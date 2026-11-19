use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use chrono::NaiveDateTime;

const SEP: &str = " - ";

const REQUEST: &str = "request:";

pub const DEFAULT_LOGS_DIR: &str = r"C:\Rocketbot\logs";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRef {
    pub robot: String,
    pub line: String,
    pub module: String,
    pub command: String,
    pub cmd_id: String,
    pub option: String,
    pub payload: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub timestamp: NaiveDateTime,
    pub source: String,
    pub level: String,
    pub message: String,
    pub request: Option<RequestRef>,
}

impl LogEntry {
    pub fn is_error(&self) -> bool {
        let level = self.level.trim();
        let nivel_malo = ["ERROR", "EXCEPTION", "FATAL"]
            .iter()
            .any(|w| level.eq_ignore_ascii_case(w));
        let mensaje_malo = self
            .message
            .trim_start()
            .get(.."EXCEPTION".len())
            .is_some_and(|p| p.eq_ignore_ascii_case("EXCEPTION"));
        nivel_malo || mensaje_malo
    }
}

pub fn parse_line(line: &str) -> Option<LogEntry> {
    let text = line
        .trim_start_matches('\u{feff}')
        .trim_start()
        .trim_end_matches(['\r', '\n']);
    if text.is_empty() {
        return None;
    }
    let (stamp, rest) = text.split_once(SEP)?;
    let timestamp = parse_timestamp(stamp)?;

    let mut campos = rest.splitn(3, SEP);
    let source = campos.next()?.trim();
    let level = campos.next()?.trim();
    let mensaje = campos.next().unwrap_or("");

    let request = parse_request(mensaje);

    Some(LogEntry {
        timestamp,
        source: source.to_string(),
        level: level.to_string(),
        message: mensaje.trim().to_string(),
        request,
    })
}

pub fn parse_text(text: &str) -> Vec<LogEntry> {
    let mut out: Vec<LogEntry> = Vec::new();
    let mut ultima_peticion: Option<usize> = None;

    for raw in text.lines() {
        if let Some(mut entrada) = parse_line(raw) {
            if entrada.request.is_some() {
                ultima_peticion = Some(out.len());
            } else if entrada.is_error() {
                if let Some(idx) = ultima_peticion {
                    entrada.request = out[idx].request.clone();
                }
            }
            out.push(entrada);
            continue;
        }

        let continuacion = raw.trim_end();
        if continuacion.trim().is_empty() {
            continue;
        }
        if let Some(ultima) = out.last_mut() {
            if !ultima.message.is_empty() {
                ultima.message.push('\n');
            }
            ultima.message.push_str(continuacion);
        }
    }

    out
}

pub fn of_robot<'a>(entries: &'a [LogEntry], robot: &str) -> impl Iterator<Item = &'a LogEntry> {
    let buscado = robot.trim().to_lowercase();
    entries.iter().filter(move |e| {
        e.request
            .as_ref()
            .is_some_and(|r| r.robot.to_lowercase() == buscado)
    })
}

pub fn for_robot(entries: &[LogEntry], robot: &str) -> Vec<LogEntry> {
    of_robot(entries, robot).cloned().collect()
}

pub fn recent_files(dir: &Path, max: usize) -> Vec<PathBuf> {
    if max == 0 {
        return Vec::new();
    }

    let mut encontrados: Vec<(SystemTime, PathBuf)> = Vec::new();
    let mut pendientes = vec![dir.to_path_buf()];
    while let Some(actual) = pendientes.pop() {
        let Ok(entradas) = std::fs::read_dir(&actual) else {
            continue;
        };
        for entrada in entradas.flatten() {
            let Ok(tipo) = entrada.file_type() else {
                continue;
            };
            if tipo.is_dir() {
                pendientes.push(entrada.path());
                continue;
            }
            if !tipo.is_file() {
                continue;
            }
            let ruta = entrada.path();
            let es_log = ruta
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("log"));
            if !es_log {
                continue;
            }
            let modificado = entrada
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            encontrados.push((modificado, ruta));
        }
    }

    encontrados.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    encontrados
        .into_iter()
        .take(max)
        .map(|(_, ruta)| ruta)
        .collect()
}

pub fn read_text(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

const CACHE_MAX_ENTRIES: usize = 64;

static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    path: PathBuf,
    modified: SystemTime,
    len: u64,
}

impl CacheKey {
    fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            path: path.to_path_buf(),
            modified: meta.modified().ok()?,
            len: meta.len(),
        })
    }
}

struct Cached {
    entries: Arc<[LogEntry]>,
    used: u64,
}

#[derive(Default)]
struct Cache {
    files: HashMap<CacheKey, Cached>,
    tick: u64,
    hits: usize,
}

impl Cache {
    fn lookup(&mut self, key: &CacheKey) -> Option<Arc<[LogEntry]>> {
        self.tick += 1;
        let tick = self.tick;
        let hit = self.files.get_mut(key)?;
        hit.used = tick;
        let entries = Arc::clone(&hit.entries);
        self.hits += 1;
        Some(entries)
    }

    fn insert(&mut self, key: CacheKey, entries: Arc<[LogEntry]>) {
        if self.files.len() >= CACHE_MAX_ENTRIES && !self.files.contains_key(&key) {
            let menos_usada = self
                .files
                .iter()
                .min_by_key(|(_, cached)| cached.used)
                .map(|(key, _)| key.clone());
            if let Some(vieja) = menos_usada {
                self.files.remove(&vieja);
            }
        }
        self.tick += 1;
        self.files.insert(
            key,
            Cached {
                entries,
                used: self.tick,
            },
        );
    }
}

pub fn parse_file_cached(path: &Path) -> Option<Arc<[LogEntry]>> {
    let Some(key) = CacheKey::of(path) else {
        return read_text(path).map(|text| parse_text(&text).into());
    };

    if let Some(entries) = with_cache(|cache| cache.lookup(&key)) {
        return Some(entries);
    }

    let entries: Arc<[LogEntry]> = parse_text(&read_text(path)?).into();
    with_cache(|cache| cache.insert(key, Arc::clone(&entries)));
    Some(entries)
}

pub fn clear_cache() {
    with_cache(|cache| *cache = Cache::default());
}

pub fn cache_stats() -> (usize, usize) {
    with_cache(|cache| (cache.files.len(), cache.hits))
}

fn with_cache<T>(f: impl FnOnce(&mut Cache) -> T) -> T {
    let mut guard = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

#[cfg(test)]
pub(crate) fn cache_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static TURNO: Mutex<()> = Mutex::new(());
    TURNO
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn parse_timestamp(raw: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(raw.trim(), "%Y-%m-%d %H:%M:%S%.f").ok()
}

fn parse_request(message: &str) -> Option<RequestRef> {
    let message = message.trim_start();
    let prefijo = message.get(..REQUEST.len())?;
    if !prefijo.eq_ignore_ascii_case(REQUEST) {
        return None;
    }
    let body = message[REQUEST.len()..].trim_start();

    let (robot, rest) = body.split_once(" : line ")?;
    let robot = robot.trim();
    if robot.is_empty() {
        return None;
    }

    let (line, rest) = rest.split_once(SEP)?;
    let line = line.trim();
    if !is_line_label(line) {
        return None;
    }

    let (module, rest) = rest.trim_start().split_once(':')?;
    let module = module.trim();
    if module.is_empty() {
        return None;
    }

    let (command, cmd_id, tail) = split_command_id(rest.trim_start());
    let (option, payload) = tail_fields(tail);

    Some(RequestRef {
        robot: robot.to_string(),
        line: line.to_string(),
        module: module.to_string(),
        command,
        cmd_id,
        option,
        payload,
    })
}

fn is_line_label(line: &str) -> bool {
    !line.is_empty()
        && line.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && line.bytes().any(|b| b.is_ascii_digit())
}

fn split_command_id(rest: &str) -> (String, String, &str) {
    let bytes = rest.as_bytes();
    let mut search = 0;
    while let Some(rel) = rest[search..].find("id:") {
        let at = search + rel;
        let prev_ok = at > 0 && bytes[at - 1] == b' ';
        let after = at + "id:".len();
        let next_ok = after >= bytes.len() || bytes[after] == b' ';
        if prev_ok && next_ok {
            let command = rest[..at].trim_end().to_string();
            let id_part = rest[after..].trim_start();
            let id_end = id_part.find(char::is_whitespace).unwrap_or(id_part.len());
            let raw_id = &id_part[..id_end];
            let cmd_id = if raw_id == "-" { "" } else { raw_id };
            return (command, cmd_id.to_string(), &id_part[id_end..]);
        }
        search = at + "id:".len();
    }

    match rest.find(SEP) {
        Some(idx) => (rest[..idx].trim().to_string(), String::new(), &rest[idx..]),
        None => (rest.trim().to_string(), String::new(), ""),
    }
}

fn tail_fields(tail: &str) -> (String, String) {
    let Some(at) = tail.find("- ") else {
        return (String::new(), String::new());
    };
    let rest = &tail[at + 2..];
    match rest.split_once(SEP) {
        Some((option, payload)) => (option.trim().to_string(), payload.trim().to_string()),
        None => {
            let option = rest.trim();
            if option == "-" {
                (String::new(), String::new())
            } else {
                (option.to_string(), String::new())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MUESTRA: &str = include_str!("../../../fixtures/logs/sample.log");

    fn entradas() -> Vec<LogEntry> {
        parse_text(MUESTRA)
    }

    #[test]
    fn parsea_cabecera_y_bloque_de_peticion() {
        let linea = "2026-11-17 12:43:47.586814 - rocketbot - INFO - request: ML_06_Legacy : line 1 - rpasystem : log_message  id: 24c9916d-469e-4bbc-85ee-392feee643a7 - INFO - INICIO subrobot";
        let e = parse_line(linea).expect("es una cabecera");
        assert_eq!(e.timestamp.to_string(), "2026-11-17 12:43:47.586814");
        assert_eq!(e.source, "rocketbot");
        assert_eq!(e.level, "INFO");
        assert!(e.message.starts_with("request: ML_06_Legacy"));
        let p = e.request.expect("trae peticion");
        assert_eq!(p.robot, "ML_06_Legacy");
        assert_eq!(p.line, "1");
        assert_eq!(p.module, "rpasystem");
        assert_eq!(p.command, "log_message");
        assert_eq!(p.cmd_id, "24c9916d-469e-4bbc-85ee-392feee643a7");
        assert_eq!(p.option, "INFO");
        assert_eq!(p.payload, "INICIO subrobot");
    }

    #[test]
    fn el_nivel_puede_venir_vacio() {
        let e =
            parse_line("2026-11-17 12:43:47.602981 - robot_message -  - procesando validacion 1")
                .expect("es una cabecera");
        assert_eq!(e.source, "robot_message");
        assert_eq!(e.level, "");
        assert_eq!(e.message, "procesando validacion 1");
        assert!(e.request.is_none());
    }

    #[test]
    fn linea_sin_cabecera_no_es_entrada() {
        assert!(parse_line("Traceback (most recent call last):").is_none());
        assert!(parse_line("  File \"<string>\", line 91, in run").is_none());
        assert!(parse_line("").is_none());
        assert!(parse_line("2026-11-17 12:43:47 - solo fecha y fuente").is_none());
        assert!(parse_line("2026-13-99 12:43:47.000000 - rocketbot - INFO - hola").is_none());
    }

    #[test]
    fn acepta_crlf_y_espacios_iniciales() {
        let e = parse_line("  2026-11-17 12:43:47.000000 - rocketbot - INFO - hola\r")
            .expect("tolera CRLF");
        assert_eq!(e.message, "hola");
        assert_eq!(e.timestamp.to_string(), "2026-11-17 12:43:47");
        let con_bom = parse_line("\u{feff}2026-11-17 12:43:47.000000 - rocketbot - INFO - hola")
            .expect("tolera BOM");
        assert_eq!(con_bom.message, "hola");
    }

    #[test]
    fn peticion_sin_id_se_acepta() {
        let e = parse_line(
            "2026-11-17 12:49:27.100400 - rocketbot - INFO - request: ML_06_Legacy : line 1.1.1 - logic :break -  - ",
        )
        .expect("es una cabecera");
        let p = e.request.expect("la peticion se conserva sin UUID");
        assert_eq!(p.line, "1.1.1");
        assert_eq!(p.module, "logic");
        assert_eq!(p.command, "break");
        assert_eq!(p.cmd_id, "");
        assert_eq!(p.option, "");
        assert_eq!(p.payload, "");

        let recortada = parse_line(
            "2026-11-17 12:49:27.100400 - rocketbot - INFO - request: ML_06_Legacy : line 1.1.1 - logic :break -  -",
        )
        .expect("es una cabecera");
        let p = recortada.request.expect("la peticion se conserva");
        assert_eq!(p.option, "");
        assert_eq!(p.payload, "");
    }

    #[test]
    fn prosa_que_empieza_por_request_no_es_peticion() {
        let e = parse_line(
            "2026-11-17 12:49:27.200500 - rocketbot - ERROR - request: Stop on error boom",
        )
        .expect("la cabecera si es valida");
        assert_eq!(e.message, "request: Stop on error boom");
        assert!(e.request.is_none());
    }

    #[test]
    fn la_muestra_tiene_una_entrada_por_cabecera() {
        let entradas = entradas();
        assert_eq!(entradas.len(), 21, "una entrada por cabecera");
        assert_eq!(
            MUESTRA.lines().count(),
            23,
            "21 cabeceras + 2 lineas de traza"
        );
        assert_eq!(
            entradas.iter().filter(|e| e.request.is_some()).count(),
            19,
            "dos lineas no son de ningun nodo (contador de bucle y mensaje de consola)"
        );
    }

    #[test]
    fn parse_text_hereda_el_request_de_los_errores() {
        let entradas = entradas();
        let name_error = entradas
            .iter()
            .find(|e| e.message.contains("NameError: name 'contador'"))
            .expect("el NameError esta en la muestra");
        assert_eq!(name_error.level, "ERROR");
        let p = name_error
            .request
            .as_ref()
            .expect("hereda la ultima peticion vista");
        assert_eq!(p.robot, "ML_06_Legacy");
        assert_eq!(p.line, "2.10");
        assert_eq!(p.command, "evaluateIf");
    }

    #[test]
    fn parse_text_concatena_lineas_sin_cabecera() {
        let entradas = entradas();
        let name_error = entradas
            .iter()
            .find(|e| e.message.contains("NameError: name 'contador'"))
            .expect("el NameError esta en la muestra");
        assert!(name_error
            .message
            .contains("Traceback (most recent call last):"));
        assert!(name_error
            .message
            .contains("File \"<string>\", line 91, in run"));
        assert!(!entradas.iter().any(|e| e.message.starts_with("Traceback")));
    }

    #[test]
    fn for_robot_ignora_mayusculas_y_excluye_lo_no_atribuido() {
        let entradas = entradas();
        let de_ml = for_robot(&entradas, "ml_06_legacy");
        assert_eq!(de_ml.len(), 18);
        assert!(de_ml.iter().all(|e| e
            .request
            .as_ref()
            .is_some_and(|r| r.robot == "ML_06_Legacy")));
        assert!(!de_ml.iter().any(|e| e.message.starts_with("For count")));

        assert_eq!(for_robot(&entradas, "MCP_SELFTEST").len(), 1);
        assert!(for_robot(&entradas, "NO_EXISTE").is_empty());
    }

    #[test]
    fn tolera_bytes_no_utf8_ya_decodificados() {
        let crudo: &[u8] = b"2026-11-17 12:43:47.000000 - rocketbot - INFO - mensaje con acento \xf1 y \xff roto\n2026-11-17 12:43:48.000000 - zdev - ERROR - EXCEPTION IN (<string> zdev, L_1375 \"\"): Error: Incorrect padding\n";
        let texto = String::from_utf8_lossy(crudo);
        let entradas = parse_text(&texto);
        assert_eq!(entradas.len(), 2);
        assert!(entradas[0].message.contains("mensaje con acento"));
        assert!(entradas[0].message.contains('\u{fffd}'));
        assert_eq!(entradas[1].source, "zdev");
        assert!(entradas[1].is_error());
    }

    #[test]
    fn texto_vacio_o_solo_basura_no_panica() {
        assert!(parse_text("").is_empty());
        assert!(parse_text("\n\n\r\n").is_empty());
        assert!(parse_text("basura sin fecha\nmas basura").is_empty());
    }

    fn fechar(path: &Path, segundos: u64) {
        use std::time::{Duration, UNIX_EPOCH};

        let cuando = UNIX_EPOCH + Duration::from_secs(1_760_000_000 + segundos);
        let fichero = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("abro para fechar");
        fichero.set_modified(cuando).expect("fijo la fecha");
    }

    #[test]
    fn recent_files_ordena_por_fecha_descendente_y_recorta() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dia = tmp.path().join("261117");
        std::fs::create_dir_all(&dia).expect("creo el subdirectorio del dia");
        let viejo = tmp.path().join("logfile_viejo.log");
        let medio = dia.join("logfile_medio.log");
        let nuevo = dia.join("logfile_nuevo.log");
        for p in [&viejo, &medio, &nuevo] {
            std::fs::write(p, b"2026-11-17 12:43:47.000000 - rocketbot - INFO - hola\n")
                .expect("escribo el log");
        }
        std::fs::write(dia.join("notas.txt"), b"x").expect("escribo notas");
        fechar(&viejo, 10);
        fechar(&medio, 20);
        fechar(&nuevo, 30);

        assert_eq!(
            recent_files(tmp.path(), 10),
            vec![nuevo.clone(), medio.clone(), viejo.clone()],
            "del mas nuevo al mas viejo, y recursivo"
        );
        assert_eq!(
            recent_files(tmp.path(), 2),
            vec![nuevo, medio],
            "el recorte se hace despues de ordenar, no durante el recorrido"
        );
    }

    #[test]
    fn recent_files_sin_directorio_devuelve_vacio() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(recent_files(&tmp.path().join("no_existe"), 10).is_empty());
        assert!(recent_files(tmp.path(), 0).is_empty());
        assert!(recent_files(tmp.path(), 10).is_empty());
    }

    #[test]
    fn read_text_degrada_los_bytes_invalidos_y_sigue_parseando() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("logfile.log");
        std::fs::write(
            &p,
            b"2026-11-17 12:43:47.000000 - rocketbot - INFO - acento \xf1 y roto \xff\n",
        )
        .expect("escribo el log");

        let texto = read_text(&p).expect("leo el log");
        assert!(texto.contains('\u{fffd}'), "el byte suelto se degrada");
        assert_eq!(
            parse_text(&texto).len(),
            1,
            "el fichero sigue siendo parseable"
        );
        assert!(read_text(&tmp.path().join("no_existe.log")).is_none());
    }

    fn linea(robot: &str, mensaje: &str) -> String {
        format!(
            "2026-11-17 12:43:47.586814 - rocketbot - INFO - request: {robot} : line 1 - rpasystem \
             : log_message  id: {robot}-1 - INFO - {mensaje}\n"
        )
    }

    #[test]
    fn el_cache_reutiliza_el_parseo_y_cuenta_un_acierto() {
        let _turno = cache_test_guard();
        clear_cache();

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("logfile.log");
        std::fs::write(&path, linea("alfa", "hola")).expect("escribo el log");

        let primera = parse_file_cached(&path).expect("leo el log");
        assert_eq!(primera.len(), 1);
        assert_eq!(
            cache_stats(),
            (1, 0),
            "la primera pasada parsea y guarda, no acierta"
        );

        let segunda = parse_file_cached(&path).expect("leo el log");
        assert_eq!(segunda.len(), 1, "mismo contenido");
        assert_eq!(segunda[0].message, primera[0].message);
        assert!(
            Arc::ptr_eq(&primera, &segunda),
            "el segundo acceso tiene que reutilizar la lista cacheada, no reparsearla"
        );
        assert_eq!(cache_stats(), (1, 1), "un acierto mas");
    }

    #[test]
    fn un_log_que_crece_se_reparsea() {
        let _turno = cache_test_guard();
        clear_cache();

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("logfile.log");
        let mut texto = linea("alfa", "uno");
        std::fs::write(&path, &texto).expect("escribo el log");
        let primera = parse_file_cached(&path).expect("leo el log");
        assert_eq!(primera.len(), 1);

        texto.push_str(&linea("alfa", "dos"));
        std::fs::write(&path, &texto).expect("anado una linea");

        let segunda = parse_file_cached(&path).expect("leo el log");
        assert_eq!(segunda.len(), 2, "el fichero crecido se reparsea");
        assert!(segunda[1].message.contains("dos"), "se ve la linea nueva");
        assert!(
            !Arc::ptr_eq(&primera, &segunda),
            "no se sirve la lista vieja"
        );
    }

    #[test]
    fn un_log_retocado_sin_cambiar_de_tamano_se_reparsea() {
        let _turno = cache_test_guard();
        clear_cache();

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("logfile.log");
        std::fs::write(&path, linea("alfa", "uno")).expect("escribo el log");
        let primera = parse_file_cached(&path).expect("leo el log");

        fechar(&path, 42);
        let segunda = parse_file_cached(&path).expect("leo el log");
        assert_eq!(segunda.len(), 1);
        assert!(
            !Arc::ptr_eq(&primera, &segunda),
            "un fichero retocado se reparsea aunque pese lo mismo"
        );
    }

    #[test]
    fn clear_cache_deja_el_cache_vacio() {
        let _turno = cache_test_guard();
        clear_cache();

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("logfile.log");
        std::fs::write(&path, linea("alfa", "hola")).expect("escribo el log");
        parse_file_cached(&path).expect("leo el log");
        parse_file_cached(&path).expect("leo el log");
        let (entradas, aciertos) = cache_stats();
        assert_eq!(entradas, 1);
        assert_eq!(aciertos, 1);

        clear_cache();
        assert_eq!(cache_stats(), (0, 0), "sin entradas y sin aciertos");

        let otra_vez = parse_file_cached(&path).expect("leo el log");
        assert_eq!(otra_vez.len(), 1);
        assert_eq!(cache_stats(), (1, 0));
    }

    #[test]
    fn el_cache_tiene_tope_y_desaloja_el_menos_usado() {
        let _turno = cache_test_guard();
        clear_cache();

        let tmp = tempfile::tempdir().expect("tempdir");
        let mut rutas = Vec::new();
        let mut parseadas = Vec::new();
        for i in 0..=CACHE_MAX_ENTRIES {
            let path = tmp.path().join(format!("logfile_{i}.log"));
            std::fs::write(&path, linea("alfa", "hola")).expect("escribo el log");
            rutas.push(path);
        }
        for path in &rutas {
            parseadas.push(parse_file_cached(path).expect("leo el log"));
        }
        assert_eq!(
            cache_stats().0,
            CACHE_MAX_ENTRIES,
            "el cache no puede crecer por encima del tope"
        );

        let readmitido = parse_file_cached(&rutas[0]).expect("leo el log");
        assert!(
            !Arc::ptr_eq(&parseadas[0], &readmitido),
            "se desaloja la entrada usada menos recientemente"
        );
        let ultimo = parse_file_cached(&rutas[CACHE_MAX_ENTRIES]).expect("leo el log");
        assert!(
            Arc::ptr_eq(&parseadas[CACHE_MAX_ENTRIES], &ultimo),
            "el desalojo no puede tirar la entrada recien usada"
        );
        assert_eq!(cache_stats().0, CACHE_MAX_ENTRIES);
    }

    #[test]
    fn un_fichero_ilegible_no_se_cachea() {
        let _turno = cache_test_guard();
        clear_cache();

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("no_existe.log");
        assert!(parse_file_cached(&path).is_none());
        assert_eq!(
            cache_stats(),
            (0, 0),
            "un fallo de lectura no entra al cache"
        );

        std::fs::write(&path, linea("alfa", "hola")).expect("escribo el log");
        let entradas = parse_file_cached(&path).expect("ahora si se lee");
        assert_eq!(entradas.len(), 1);
        assert_eq!(cache_stats(), (1, 0));
    }
}
