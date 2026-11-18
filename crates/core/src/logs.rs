//! Parseo de los logs de ejecucion de Rocketbot.
//!
//! El log real es texto plano: una cabecera por linea y, dentro del mensaje, un
//! bloque de peticion que identifica el nodo que el runner estaba ejecutando.
//!
//! ```text
//! 2026-11-17 12:43:47.586814 - rocketbot - INFO - request: ML_06_Legacy : line 1 - rpasystem : log_message  id: 24c9916d-469e-4bbc-85ee-392feee643a7 - INFO - INICIO subrobot
//! ```
//!
//! # Por que se hereda la ultima peticion
//!
//! La linea de error no dice que comando del robot fallo: `EXCEPTION IN (<string>
//! Rocketbot, L_91 "")` nombra el modulo interno del runner, no el nodo del
//! robot. La unica pista de atribucion del log es la ultima `request:` emitida.
//! [`parse_text`] la hereda en los errores para no perder esa pista, pero es una
//! **pista, no una certeza**: si entre la peticion y el error corre otra cosa, la
//! atribucion se equivoca. El motor la cruza despues con el snapshot que estaba
//! vivo en ese instante ([`crate::versions::VersionIndex::live_at`]).
//!
//! # Tolerancia al encoding
//!
//! Los logs reales tienen bytes sueltos que no son UTF-8 (mensajes de usuario
//! escritos en Windows-1252). El parseo trabaja sobre `&str` ya decodificado
//! (`from_utf8_lossy` en el borde de lectura) y nunca corta por indice de byte a
//! ciegas: todo corte sale de [`str::find`], que cae siempre en frontera de
//! caracter. Un byte invalido se degrada a `U+FFFD` y la linea sigue siendo
//! parseable.
//!
//! # Cache de ficheros parseados
//!
//! Un barrido pide los **mismos** ficheros una vez por robot: la bandeja recorre
//! 32 instancias sobre los mismos 13 logs y lee 416 veces el mismo contenido, con
//! el mismo resultado. [`parse_file_cached`] guarda las entradas ya parseadas por
//! `(ruta, mtime, tamano)` y devuelve la lista **compartida** (`Arc`, sin copiarla)
//! mientras el fichero no cambie.
//!
//! La invalidacion mira fecha **y** tamano porque las dos cambian por motivos
//! distintos: un log que crece durante la ejecucion cambia de tamano (y el `mtime`
//! puede tardar en refrescarse), y un log que se reescribe -incluso mas pequeno-
//! cambia de fecha. Un fichero ilegible devuelve `None` y **no se cachea**: el
//! fallo puede ser transitorio (un log a medio escribir) y memorizarlo lo volveria
//! permanente.
//!
//! El cache es **de proceso** y tiene tope (`CACHE_MAX_ENTRIES`): lo usan la UI y
//! el servidor MCP, que viven horas, y un cache sin limite seria una fuga de
//! memoria lenta. Se desaloja el fichero usado menos recientemente, que en un
//! barrido es el de una pasada anterior y no el que se acaba de leer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use chrono::NaiveDateTime;

/// Separador de los campos de cabecera. La fecha no lleva espacios alrededor de
/// sus guiones, asi que el primer `" - "` cierra el timestamp sin ambiguedad.
const SEP: &str = " - ";

/// Prefijo del bloque de peticion dentro del mensaje.
const REQUEST: &str = "request:";

/// Directorio de logs del Studio local.
///
/// Es la unica ruta del entorno real que conoce el motor, y la unica que decide
/// si un pack lleva seccion de errores. Vive aqui, junto al parser, porque este
/// es el modulo de I/O: los demas reciben rutas, nunca las inventan.
pub const DEFAULT_LOGS_DIR: &str = r"C:\Rocketbot\logs";

/// Peticion de ejecucion tal como la escribe el runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRef {
    /// Robot cuyo nodo se estaba ejecutando.
    pub robot: String,
    /// Numero de linea de Studio (`2.10`). No es unico en robots con Try/Catch:
    /// solo sirve como etiqueta, la direccion fiable es [`Self::cmd_id`].
    pub line: String,
    /// Grupo de comandos (`rpasystem`, `rpalogic`, `rpafile`...).
    pub module: String,
    /// Comando concreto (`log_message`, `setVar`, `for`...).
    pub command: String,
    /// UUID del nodo, que coincide con `tree::FlatNode::id`. Vacio cuando la
    /// linea no trae `id:` (pasa con comandos internos como `logic :break`).
    pub cmd_id: String,
    /// Nivel con el que Rocketbot emitio la peticion (`INFO`, `WARNING`...).
    pub option: String,
    /// Parametros ya formateados por Rocketbot, crudos: recortarlos es decision
    /// del render, no del parser.
    pub payload: String,
}

/// Una entrada del log: una cabecera ya interpretada.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// Instante con microsegundos. Es lo que permite atar el error al snapshot
    /// de codigo que estaba vivo cuando ocurrio.
    pub timestamp: NaiveDateTime,
    /// Emisor (`rocketbot`, `Rocketbot`, `zdev`, `getbot`, `robot_message`...).
    pub source: String,
    /// Nivel (`INFO`, `ERROR`...). Puede venir vacio: `robot_message` lo omite.
    pub level: String,
    /// Mensaje completo. Si la linea traia un bloque de peticion, lo conserva:
    /// es evidencia util para el agente y para el diff.
    pub message: String,
    /// Peticion propia o heredada de la ultima vista. `None` en lineas sueltas
    /// (contadores de bucle, mensajes de consola) que no son de ningun nodo.
    pub request: Option<RequestRef>,
}

impl LogEntry {
    /// Nivel de error, o mensaje que empieza por `EXCEPTION`: las dos formas en
    /// que Rocketbot reporta un fallo.
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

/// Parsea una linea con cabecera. Devuelve `None` si no lo es.
///
/// Una linea sin fecha valida no es cabecera aunque empiece por `2026-`: se
/// prefiere tratarla como continuacion (`parse_text`) antes que inventar una
/// entrada con epoch, que falsearia la atribucion temporal.
///
/// ```
/// use rocketdebug_core::logs::parse_line;
///
/// let linea = "2026-11-17 12:43:47.586814 - rocketbot - INFO - request: ML_06_Legacy : line 1 - rpasystem : log_message  id: 24c9916d-469e-4bbc-85ee-392feee643a7 - INFO - INICIO subrobot";
/// let entrada = parse_line(linea).expect("es una cabecera");
/// let peticion = entrada.request.expect("trae bloque de peticion");
/// assert_eq!(peticion.command, "log_message");
/// assert_eq!(peticion.line, "1");
/// ```
pub fn parse_line(line: &str) -> Option<LogEntry> {
    // Un BOM UTF-8 al principio del fichero es habitual en logs escritos desde
    // Windows y no es whitespace: sin quitarlo, la primera cabecera se perderia.
    let text = line
        .trim_start_matches('\u{feff}')
        .trim_start()
        .trim_end_matches(['\r', '\n']);
    if text.is_empty() {
        return None;
    }
    // El primer " - " solo puede cerrar el timestamp: la fecha usa guiones sin
    // espacios y el logging no formatea la hora con separador.
    let (stamp, rest) = text.split_once(SEP)?;
    let timestamp = parse_timestamp(stamp)?;

    let mut campos = rest.splitn(3, SEP);
    let source = campos.next()?.trim();
    let level = campos.next()?.trim();
    let mensaje = campos.next().unwrap_or("");

    // El bloque de peticion se interpreta con los espacios finales intactos: con
    // la linea recortada, un payload vacio (`... -  - `) se confunde con un `-`.
    let request = parse_request(mensaje);

    Some(LogEntry {
        timestamp,
        source: source.to_string(),
        level: level.to_string(),
        message: mensaje.trim().to_string(),
        request,
    })
}

/// Parsea un log completo.
///
/// - Una entrada por linea con cabecera.
/// - Los `ERROR`/`EXCEPTION`/`FATAL` heredan el `request` de la ultima peticion.
/// - El resto de lineas sin cabecera se concatenan al mensaje de la entrada
///   anterior: asi una traza de Python queda dentro de su error y no como
///   entradas sueltas que nadie puede atribuir.
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

        // Sin cabecera: continuacion del mensaje anterior (traza, JSON partido).
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
        // Lineas sueltas antes de la primera cabecera se descartan: no hay nada a
        // lo que atribuirlas y guardarlas como entrada falsa seria peor.
    }

    out
}

/// Entradas de un robot **por referencia**, comparando sin distinguir mayusculas.
///
/// Solo se atribuyen las lineas con `request` (propia o heredada). El ruido de
/// motor (`zdev`, `getbot`) y los contadores de bucle no pertenecen a ningun
/// robot: reclamarlos por cercania es exactamente el tipo de falso positivo que
/// hace que el panel se deje de mirar.
///
/// Es la variante que no clona, y la que conviene cuando la lista viene del cache:
/// de un log entero, a un robot le toca una fraccion, y clonar todo para descartar
/// el resto era el gasto que el cache evita. Quien necesite las entradas, que
/// clone lo que sobreviva a su filtro (asi lo hace `analysis::robot_errors`).
pub fn of_robot<'a>(entries: &'a [LogEntry], robot: &str) -> impl Iterator<Item = &'a LogEntry> {
    let buscado = robot.trim().to_lowercase();
    entries.iter().filter(move |e| {
        e.request
            .as_ref()
            .is_some_and(|r| r.robot.to_lowercase() == buscado)
    })
}

/// [`of_robot`] con las entradas ya clonadas.
pub fn for_robot(entries: &[LogEntry], robot: &str) -> Vec<LogEntry> {
    of_robot(entries, robot).cloned().collect()
}

/// Ficheros `*.log` bajo `dir`, recursivo, del mas nuevo al mas viejo, como mucho
/// `max`.
///
/// Un directorio inexistente o ilegible devuelve vacio en vez de fallar: un pack
/// sin logs sigue siendo un pack valido, y "no hay logs" no es un error del
/// robot. El recorte a `max` se hace **despues** de ordenar: quedarse con los
/// primeros encontrados daria un fichero arbitrario -el que devuelva el sistema
/// de ficheros-, no el mas reciente.
///
/// Se ordena por fecha de modificacion porque el nombre no la garantiza: el
/// runner nombra `logfile_YYMMDD_HHMMSS.log` con la hora de **arranque**, asi que
/// un fichero abierto toda la tarde es el mas nuevo por contenido y no por nombre.
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
            // `file_type` no sigue enlaces: contar un enlace como directorio
            // abriria la puerta a un bucle en el recorrido de `dir`.
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
            // Un fichero sin fecha legible no se descarta: se le da epoch para
            // que quede al final del orden, que es donde merece estar, en vez de
            // desaparecer del informe.
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

/// Contenido de un log ya decodificado, o `None` si no se puede leer.
///
/// La decodificacion ocurre en el borde de lectura y no en [`parse_text`]: asi el
/// parser trabaja siempre sobre `&str` y un log con bytes sueltos de Windows-1252
/// (mensajes de usuario, rutas) se degrada a `U+FFFD` en vez de tumbar el fichero
/// entero. Un log ilegible se omite: es evidencia que falta, no un fallo del pack.
pub fn read_text(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Tope de ficheros cacheados.
///
/// 64 deja holgura sobre lo que pide un pack ([`crate::pack::DEFAULT_LOG_FILES`] =
/// 20 ficheros) y varios dias de logs, y sobre todo acota la memoria de un proceso
/// que no muere entre peticiones: el cache es de proceso y sin tope creceria con
/// cada dia nuevo de logs.
const CACHE_MAX_ENTRIES: usize = 64;

/// Cache de proceso, `OnceLock` porque solo hay `std` (sin dependencias nuevas).
///
/// El candado protege el mapa, **no** el parseo: [`parse_file_cached`] lo suelta
/// mientras lee y parsea, asi que dos lectores concurrentes del mismo fichero
/// pueden parsearlo dos veces. Se acepta: el resultado es identico y serializar
/// toda la I/O de logs detras de un candado global costaria mas que ese trabajo
/// duplicado, que como mucho pasa en el primer acceso.
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

/// Identidad de una version concreta de un fichero.
///
/// La ruta sola no basta (un log se reescribe) y el `mtime` solo tampoco (un log
/// que crece durante la ejecucion puede cambiar de tamano antes de que el sistema
/// de ficheros refresque la fecha): la clave lleva los tres.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    path: PathBuf,
    /// Fecha de modificacion: cambio garantizado al reescribir el fichero.
    modified: SystemTime,
    /// Tamano en bytes: cambio garantizado cuando el runner sigue escribiendo.
    len: u64,
}

impl CacheKey {
    /// Clave de la version actual del fichero; `None` si no se puede leer su
    /// metadata, porque entonces no hay forma de saber si el contenido caduco.
    fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            path: path.to_path_buf(),
            modified: meta.modified().ok()?,
            len: meta.len(),
        })
    }
}

/// Una entrada del cache y cuando se uso por ultima vez.
struct Cached {
    /// Entradas ya parseadas. Se comparte en vez de copiarse: el `Arc` es el que
    /// hace que reutilizar el parseo sea barato de verdad.
    entries: Arc<[LogEntry]>,
    /// Reloj del ultimo uso: decide el desalojo. No es la fecha de insercion para
    /// que un fichero muy consultado no se desaloje por haber entrado pronto.
    used: u64,
}

/// Estado del cache: entradas vivas, reloj de uso y aciertos acumulados.
#[derive(Default)]
struct Cache {
    files: HashMap<CacheKey, Cached>,
    /// Reloj monotono: sube en cada consulta, asi que `used` siempre crece.
    tick: u64,
    /// Aciertos desde el arranque o desde [`clear_cache`]: diagnostico y tests.
    hits: usize,
}

impl Cache {
    /// Busca la version exacta del fichero y anota el acierto.
    fn lookup(&mut self, key: &CacheKey) -> Option<Arc<[LogEntry]>> {
        self.tick += 1;
        let tick = self.tick;
        let hit = self.files.get_mut(key)?;
        hit.used = tick;
        // Clonar el `Arc` no copia las entradas: solo sube el contador de refs.
        let entries = Arc::clone(&hit.entries);
        self.hits += 1;
        Some(entries)
    }

    /// Guarda una version del fichero y, si el cache esta lleno, desaloja la menos
    /// usada.
    fn insert(&mut self, key: CacheKey, entries: Arc<[LogEntry]>) {
        // Reemplazar una clave que ya existe no anade una entrada: si se desalojara
        // aqui, una carrera entre dos hilos que parsean el mismo fichero tiraria
        // una entrada que se esta volviendo a insertar.
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

/// Entradas de un fichero de log, cacheadas por (ruta, mtime, tamano).
/// El vigilante y la bandeja barren los mismos ficheros una vez por robot: sin
/// cache, el coste crece con robots x ficheros y el resultado es el mismo.
///
/// Devuelve la lista compartida: el llamador no debe mutarla (es un `Arc<[..]>`, no
/// puede) y no la copia. Si el fichero no se puede leer, `None` y **sin cachear**:
/// un fallo de lectura puede ser transitorio y el cache no puede convertirlo en
/// permanente.
pub fn parse_file_cached(path: &Path) -> Option<Arc<[LogEntry]>> {
    let Some(key) = CacheKey::of(path) else {
        // Sin fecha ni tamano fiables no hay con que invalidar: se parsea cada vez
        // antes que arriesgarse a servir contenido que ya no esta en disco.
        return read_text(path).map(|text| parse_text(&text).into());
    };

    if let Some(entries) = with_cache(|cache| cache.lookup(&key)) {
        return Some(entries);
    }

    // Fuera del candado: leer y parsear sin bloquear a los demas hilos.
    let entries: Arc<[LogEntry]> = parse_text(&read_text(path)?).into();
    with_cache(|cache| cache.insert(key, Arc::clone(&entries)));
    Some(entries)
}

/// Vacia el cache y reinicia el contador de aciertos.
///
/// Es lo que necesita un test para medir en aislamiento y lo que puede querer un
/// llamador para liberar memoria (por ejemplo el vigilante entre pasadas). El
/// contador se reinicia porque los aciertos son diagnostico, no estado: un cache
/// vacio que dijera "12 aciertos" mentiria sobre lo que tiene dentro.
pub fn clear_cache() {
    with_cache(|cache| *cache = Cache::default());
}

/// `(entradas, aciertos)` del cache: cuantos ficheros guarda y cuantas veces se
/// reutilizo un parseo.
pub fn cache_stats() -> (usize, usize) {
    with_cache(|cache| (cache.files.len(), cache.hits))
}

/// Acceso al cache con recuperacion del candado envenenado.
///
/// Un hilo que entra en panico con el candado tomado no deja el mapa a medias (se
/// inserta un `Arc` ya construido), asi que se recupera el dato en vez de propagar
/// el panico como si fuera un fallo de lectura del log.
fn with_cache<T>(f: impl FnOnce(&mut Cache) -> T) -> T {
    let mut guard = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

/// Turno para los tests que miden el cache.
///
/// [`cache_stats`] cuenta un estado **global al proceso** y los tests de un crate
/// corren en paralelo: sin este candado, un test mediria los aciertos de otro. Se
/// usa desde `logs` y desde `analysis`.
#[cfg(test)]
pub(crate) fn cache_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static TURNO: Mutex<()> = Mutex::new(());
    TURNO
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Cabecera estricta: la fecha tiene que ser real. `db::parse_date` degrada a
/// epoch en silencio, y aqui eso moveria la evidencia al 1 de enero de 1970.
fn parse_timestamp(raw: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(raw.trim(), "%Y-%m-%d %H:%M:%S%.f").ok()
}

/// Interpreta el bloque `request:` del mensaje. Cualquier desviacion del formato
/// devuelve `None` y la linea queda como mensaje normal: una peticion a medias
/// no debe romper el parseo del log entero.
fn parse_request(message: &str) -> Option<RequestRef> {
    let message = message.trim_start();
    let prefijo = message.get(..REQUEST.len())?;
    if !prefijo.eq_ignore_ascii_case(REQUEST) {
        return None;
    }
    let body = message[REQUEST.len()..].trim_start();

    // "<robot> : line <linea> - <modulo> : <comando> ..."
    let (robot, rest) = body.split_once(" : line ")?;
    let robot = robot.trim();
    if robot.is_empty() {
        return None;
    }

    let (line, rest) = rest.split_once(SEP)?;
    let line = line.trim();
    if !is_line_label(line) {
        // Descarta prosa que empieza por `request:` sin ser una peticion, como
        // `request: Stop on error boom`.
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

/// `line` de Studio: digitos separados por puntos (`1`, `2.10`, `1.1.1`).
/// Se exige forma numerica para no confundir prosa con una peticion.
fn is_line_label(line: &str) -> bool {
    !line.is_empty()
        && line.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && line.bytes().any(|b| b.is_ascii_digit())
}

/// Separa comando, `cmd_id` y resto de la linea.
///
/// El `id:` no siempre esta: los comandos internos del runner emiten
/// `logic :break -  - ` sin UUID. Hay que aceptarlo en vez de perder la peticion.
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
            // Un `-` en vez de UUID es un id vacio, no un identificador.
            let cmd_id = if raw_id == "-" { "" } else { raw_id };
            return (command, cmd_id.to_string(), &id_part[id_end..]);
        }
        search = at + "id:".len();
    }

    // Sin `id:`: el comando termina en el primer separador.
    match rest.find(SEP) {
        Some(idx) => (rest[..idx].trim().to_string(), String::new(), &rest[idx..]),
        None => (rest.trim().to_string(), String::new(), ""),
    }
}

/// Campos que siguen al comando (o al `cmd_id`): `<opcion> - <payload>`.
fn tail_fields(tail: &str) -> (String, String) {
    let Some(at) = tail.find("- ") else {
        // La linea se corto antes de que Rocketbot escribiera los campos.
        return (String::new(), String::new());
    };
    let rest = &tail[at + 2..];
    match rest.split_once(SEP) {
        Some((option, payload)) => (option.trim().to_string(), payload.trim().to_string()),
        None => {
            let option = rest.trim();
            // Payload vacio con el espacio final recortado por el emisor: el
            // resto es solo el separador y no un valor.
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

    /// Muestra sintetica que copia las formas reales medidas en
    /// `C:\Rocketbot\logs\261117`: 21 cabeceras, dos lineas de traza y los
    /// defectos tipicos (NameError del robot, zdev, getbot, datatransform).
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
        // fecha invalida: no se inventa una entrada en 1970
        assert!(parse_line("2026-13-99 12:43:47.000000 - rocketbot - INFO - hola").is_none());
    }

    #[test]
    fn acepta_crlf_y_espacios_iniciales() {
        let e = parse_line("  2026-11-17 12:43:47.000000 - rocketbot - INFO - hola\r")
            .expect("tolera CRLF");
        assert_eq!(e.message, "hola");
        assert_eq!(e.timestamp.to_string(), "2026-11-17 12:43:47");
        // BOM UTF-8 al inicio del fichero.
        let con_bom = parse_line("\u{feff}2026-11-17 12:43:47.000000 - rocketbot - INFO - hola")
            .expect("tolera BOM");
        assert_eq!(con_bom.message, "hola");
    }

    #[test]
    fn peticion_sin_id_se_acepta() {
        // Forma real de los comandos internos: `logic :break -  - `.
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

        // La misma linea con el espacio final recortado no debe producir un
        // campo `option == "-"`.
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
        // La traza no genera entradas propias: solo hay 20 cabeceras.
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
        // El contador de bucle no esta asociado a ningun nodo y no se reclama.
        assert!(!de_ml.iter().any(|e| e.message.starts_with("For count")));

        assert_eq!(for_robot(&entradas, "MCP_SELFTEST").len(), 1);
        assert!(for_robot(&entradas, "NO_EXISTE").is_empty());
    }

    #[test]
    fn tolera_bytes_no_utf8_ya_decodificados() {
        // Un log real pasa por `from_utf8_lossy`: acentos en Windows-1252 y bytes
        // sueltos llegan como `U+FFFD`, y el parseo no puede romperse por ello.
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

    /// Fija la fecha de modificacion: el orden de `recent_files` no puede
    /// depender de la resolucion del reloj ni del orden de escritura.
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
        // Un fichero que no es `*.log` no es evidencia: no se lista.
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
        // Un directorio vacio tampoco es un error.
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

    /// Cabecera valida de una peticion del robot: la forma minima que pasa por
    /// [`parse_line`] sin depender de ningun fichero del entorno.
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

        // El runner sigue escribiendo: el tamano cambia y la clave ya no vale.
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

        // Mismo contenido y mismo tamano: solo cambia la fecha. Sin el `mtime` en
        // la clave, este fichero se serviria del cache para siempre.
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
        // Otro test pudo dejar entradas: la medida empieza siempre del mismo sitio.
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

        // Y sigue funcionando despues: el fichero se vuelve a parsear.
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
        // Al primero no se vuelve a tocar: es el candidato al desalojo.
        for path in &rutas {
            parseadas.push(parse_file_cached(path).expect("leo el log"));
        }
        assert_eq!(
            cache_stats().0,
            CACHE_MAX_ENTRIES,
            "el cache no puede crecer por encima del tope"
        );

        // El primero salio del cache: se reparsea en vez de reutilizarse.
        let readmitido = parse_file_cached(&rutas[0]).expect("leo el log");
        assert!(
            !Arc::ptr_eq(&parseadas[0], &readmitido),
            "se desaloja la entrada usada menos recientemente"
        );
        // Y el ultimo, que si se acaba de usar, sigue dentro.
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

        // Si el fichero aparece despues, se lee: el fallo no se memorizo.
        std::fs::write(&path, linea("alfa", "hola")).expect("escribo el log");
        let entradas = parse_file_cached(&path).expect("ahora si se lee");
        assert_eq!(entradas.len(), 1);
        assert_eq!(cache_stats(), (1, 0));
    }
}
