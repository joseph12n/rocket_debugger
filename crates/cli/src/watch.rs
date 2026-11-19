//! Vigilante del registro: lo alimenta sin que nadie se acuerde de hacerlo.
//!
//! Solo lee la db; el unico fichero que escribe es el registro. La primera pasada
//! analiza todos los robots y las siguientes solo los que cambiaron de `latest_id`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use rocketdebug_core::analysis::{self, Analysis};
use rocketdebug_core::db;
use rocketdebug_core::registry::{Observation, Registry};

/// Segundos entre pasadas si no se pide otro intervalo (30 s).
pub const DEFAULT_INTERVAL_SECS: i64 = 30;

/// Variable de entorno que fija la ruta del registro cuando no se pasa `--registry`.
pub const REGISTRY_ENV: &str = "ROCKETDEBUG_REGISTRY";

/// Lo que el comando necesita, ya resuelto por el CLI.
#[derive(Debug)]
pub struct WatchOptions {
    pub db: PathBuf,
    pub workspace: Option<PathBuf>,
    pub interval: Duration,
    pub once: bool,
    pub registry: PathBuf,
    pub log_source: Option<(PathBuf, usize)>,
}

/// Valida `--interval` y lo convierte en duracion.
///
/// Cero o negativo se rechaza: no es "lo mas rapido", es un bucle en vacio.
pub fn interval_from_secs(secs: i64) -> anyhow::Result<Duration> {
    if secs <= 0 {
        bail!("--interval {secs}: tiene que ser >= 1 segundo (0 o negativo gira en vacio)");
    }
    Ok(Duration::from_secs(secs as u64))
}

/// Ruta del registro: `--registry`, o `ROCKETDEBUG_REGISTRY`, o la del motor.
pub fn resolve_registry(flag: Option<PathBuf>) -> PathBuf {
    flag.or_else(|| {
        std::env::var_os(REGISTRY_ENV)
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
    })
    .unwrap_or_else(rocketdebug_core::registry::default_path)
}

/// Ejecuta `rocketdebug watch`.
///
/// Novedades por stdout; avisos por stderr, para leer stdout sin filtrar.
pub fn run(options: &WatchOptions, out: &mut dyn Write) -> anyhow::Result<()> {
    let mut registry = Registry::open(&options.registry)
        .with_context(|| format!("abriendo el registro {}", options.registry.display()))?;

    let descripcion = match &options.workspace {
        None => format!("db {}", crate::db_label(&options.db)),
        Some(root) => format!(
            "workspace {} ({})",
            root.display(),
            dbs_label(fuentes(options)?.len())
        ),
    };
    writeln!(out, "{}", cabecera(&descripcion, options))?;

    let mut anterior: BTreeMap<String, i64> = BTreeMap::new();
    loop {
        let arranque = Instant::now();
        let estado = pasada(options, &anterior, &mut registry, out)?;
        anterior = estado.ids;

        if options.once {
            eprintln!(
                "watch: una pasada: {} analizados en {:.1} s ({} leidas)",
                robots_label(estado.analizados),
                arranque.elapsed().as_secs_f64(),
                dbs_label(estado.dbs_leidas)
            );
            if estado.dbs_leidas == 0 {
                bail!("no pude leer ninguna db ({descripcion}): revisa --db o --workspace");
            }
            return Ok(());
        }

        std::thread::sleep(options.interval);
    }
}

/// Una pasada: mira las dbs, analiza lo que cambio y lo registra.
/// Devuelve el estado con el que se compara la pasada siguiente.
fn pasada(
    options: &WatchOptions,
    anterior: &BTreeMap<String, i64>,
    registry: &mut Registry,
    out: &mut dyn Write,
) -> anyhow::Result<EstadoPasada> {
    let vigiladas = fuentes(options)?;
    let varias = vigiladas.len() > 1;
    let lee_logs = options.log_source.is_some();
    let log_source = options
        .log_source
        .as_ref()
        .map(|(dir, max_files)| (dir.as_path(), *max_files));
    // Un instante por pasada: repetir el barrido en el mismo segundo es idempotente.
    let visto_en = ahora();

    let mut ids = BTreeMap::new();
    let mut dbs_leidas = 0usize;
    let mut analizados = 0usize;

    for fuente in &vigiladas {
        let conn = match db::open_readonly(&fuente.path) {
            Ok(conn) => conn,
            // Una db ilegible se avisa y se omite: no aborta la pasada.
            Err(e) => {
                eprintln!("aviso: {}: {e}", fuente.label);
                continue;
            }
        };
        let resumenes = match db::list_robots(&conn) {
            Ok(resumenes) => resumenes,
            Err(e) => {
                eprintln!("aviso: {}: {e}", fuente.label);
                continue;
            }
        };
        dbs_leidas += 1;

        let ahora_ids: Vec<(String, i64)> = resumenes
            .iter()
            .map(|resumen| (fuente.clave(&resumen.name), resumen.latest_id))
            .collect();
        let cambiados: BTreeSet<String> =
            robots_cambiados(anterior, &ahora_ids).into_iter().collect();
        ids.extend(ahora_ids.iter().cloned());

        for (resumen, (clave, _)) in resumenes.iter().zip(ahora_ids.iter()) {
            if !cambiados.contains(clave) {
                continue;
            }
            let nombre = if varias {
                format!("{}\\{}", fuente.label, resumen.name)
            } else {
                resumen.name.clone()
            };
            let analisis = match log_source {
                Some(source) => {
                    analysis::analyse_with_logs(&conn, &fuente.label, &resumen.name, Some(source))
                }
                None => analysis::analyse(&conn, &fuente.label, &resumen.name),
            };
            match analisis {
                Ok(analisis) => {
                    let recuento = registrar(
                        registry,
                        &conn,
                        &fuente.label,
                        &resumen.name,
                        &analisis,
                        visto_en,
                    );
                    writeln!(out, "{}", linea_robot(&nombre, &recuento, lee_logs))?;
                    analizados += 1;
                }
                // Un robot ilegible se avisa, pero cuenta como visto: se reintenta al guardarlo, no cada pasada.
                Err(e) => eprintln!("aviso: {nombre}: {e}"),
            }
        }
    }

    Ok(EstadoPasada {
        ids,
        dbs_leidas,
        analizados,
    })
}

struct EstadoPasada {
    ids: BTreeMap<String, i64>,
    dbs_leidas: usize,
    analizados: usize,
}

/// Registra los errores de un analisis; omite lo que el robot escribe con `log_message`.
fn registrar(
    registry: &mut Registry,
    conn: &rusqlite::Connection,
    label: &str,
    robot: &str,
    analysis: &Analysis,
    respaldo: chrono::NaiveDateTime,
) -> Recuento {
    // La hora es la del snapshot observado: con la del reloj, cada pasada contaria la misma version como nueva.
    let visto_en = rocketdebug_core::db::latest_created_at(conn, robot)
        .ok()
        .flatten()
        .unwrap_or(respaldo);
    let mut recuento = Recuento::default();
    for item in analysis
        .errors
        .live
        .iter()
        .chain(analysis.errors.payload_changed.iter())
        .chain(analysis.errors.historical.iter())
        .chain(analysis.errors.platform.iter())
    {
        if item.command == "log_message" {
            recuento.omitidas += 1;
            continue;
        }
        let observacion = Observation {
            signature: item.signature.clone(),
            robot: robot.to_string(),
            db_label: label.to_string(),
            version: item.version,
            line: item.line.clone(),
            message: item.message.clone(),
            seen_at: visto_en,
        };
        match registry.record_observation(&observacion) {
            Ok(true) => recuento.nuevas += 1,
            Ok(false) => recuento.conocidas += 1,
            Err(e) => eprintln!("aviso: {robot}: no pude registrar la firma: {e}"),
        }
    }
    recuento
}

/// Lo que una pasada dejo en el registro para un robot.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Recuento {
    nuevas: usize,
    conocidas: usize,
    omitidas: usize,
}

impl Recuento {
    /// Texto de novedades; con cero dice por que (con `--no-logs` no se leyo el log).
    fn resumen(&self, lee_logs: bool) -> String {
        let mut texto = format!(
            "{}, {}",
            cuenta(self.nuevas, "nueva", "nuevas"),
            cuenta(self.conocidas, "ya conocida", "ya conocidas")
        );
        if self.nuevas == 0 && self.conocidas == 0 {
            if !lee_logs {
                texto.push_str(" (--no-logs: no se leyeron logs, 0 observaciones)");
            } else if self.omitidas > 0 {
                texto.push_str(&format!(
                    " (sin defectos que registrar: {} de log_message omitidos)",
                    cuenta(self.omitidas, "mensaje", "mensajes")
                ));
            } else {
                texto.push_str(" (sin errores que registrar)");
            }
        } else if self.omitidas > 0 {
            texto.push_str(&format!(
                " ({} de log_message omitidos)",
                cuenta(self.omitidas, "mensaje", "mensajes")
            ));
        }
        texto
    }
}

fn linea_robot(nombre: &str, recuento: &Recuento, lee_logs: bool) -> String {
    format!("{nombre}: {}", recuento.resumen(lee_logs))
}

/// `1 nueva` / `N nuevas`: la salida la lee una persona, no un parser.
fn cuenta(n: usize, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {plural}")
    }
}

/// Linea de arranque: que se vigila, que registro se escribe y cada cuanto.
fn cabecera(descripcion: &str, options: &WatchOptions) -> String {
    let mut texto = format!(
        "watch: {descripcion} | registro {} | cada {} s",
        options.registry.display(),
        options.interval.as_secs()
    );
    if options.once {
        texto.push_str(" | una pasada (--once)");
    }
    if options.log_source.is_none() {
        texto.push_str(
            "\nwatch: --no-logs: no se leen logs; el registro no recibira \
             observaciones de ejecucion",
        );
    }
    texto
}

/// Claves que hay que analizar: nuevas o con `latest_id` distinto.
///
/// El orden de salida es el de `ahora`, no el del mapa interno.
pub fn robots_cambiados(antes: &BTreeMap<String, i64>, ahora: &[(String, i64)]) -> Vec<String> {
    ahora
        .iter()
        .filter(|(clave, id)| antes.get(clave) != Some(id))
        .map(|(clave, _)| clave.clone())
        .collect()
}

/// Hora del sello; `chrono` va sin `clock`, el reloj lo pone quien llama.
fn ahora() -> chrono::NaiveDateTime {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|d| d.naive_utc())
        .unwrap_or_default()
}

/// Dbs que se vigilan: la de `--db` (exista o no), o las del workspace.
fn fuentes(options: &WatchOptions) -> anyhow::Result<Vec<Fuente>> {
    match &options.workspace {
        None => Ok(vec![Fuente {
            label: crate::db_label(&options.db),
            path: options.db.clone(),
        }]),
        Some(root) => {
            let paths = db::discover_databases(root)
                .with_context(|| format!("descubriendo bases en {}", root.display()))?;
            Ok(paths
                .into_iter()
                .map(|path| Fuente {
                    label: etiqueta_relativa(root, &path),
                    path,
                })
                .collect())
        }
    }
}

/// Una db vigilada: donde esta y como se llama en la salida.
struct Fuente {
    path: PathBuf,
    label: String,
}

impl Fuente {
    /// Clave del estado: lleva la etiqueta delante para separar homonimos entre dbs.
    fn clave(&self, robot: &str) -> String {
        format!("{}\\{robot}", self.label)
    }
}

/// Etiqueta de una db del workspace: su ruta relativa a la raiz.
/// Unica (dos entregas anidadas no colisionan) y sin la ruta absoluta del usuario.
fn etiqueta_relativa(root: &Path, path: &Path) -> String {
    // Se compara texto con `/` normalizado: `Path` solo trata `\` como separador en Windows.
    let normal = |p: &Path| p.to_string_lossy().replace('\\', "/");
    let (root_n, path_n) = (normal(root), normal(path));
    let raiz = root_n.trim_end_matches('/');
    match path_n
        .strip_prefix(raiz)
        .map(|resto| resto.trim_start_matches('/'))
    {
        Some(relativa) if !relativa.is_empty() && relativa != path_n => relativa.replace('/', "\\"),
        _ => crate::db_label(path),
    }
}

fn robots_label(count: usize) -> String {
    if count == 1 {
        "1 robot".to_string()
    } else {
        format!("{count} robots")
    }
}

fn dbs_label(count: usize) -> String {
    if count == 1 {
        "1 db".to_string()
    } else {
        format!("{count} dbs")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use base64::Engine as _;

    /// Sufijo unico por directorio: dos tests en paralelo no pueden compartir ruta.
    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// Directorio temporal que se borra solo.
    ///
    /// No se usa `tempfile` a proposito: son `create_dir_all` + `remove_dir_all`, y
    /// el CLI no necesita una dependencia mas para eso.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rocketdebug-cli-watch-{tag}-{}-{nanos}-{seq}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("creo el directorio temporal");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            // En Windows un fichero aun abierto no se puede borrar: si falla, el
            // directorio queda en %TEMP% y el test no falla por la limpieza.
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Proyecto minimo que el motor sabe decodificar y analizar.
    const ROBOT: &str = r#"{"project":{"commands":[{"father":"log_message","line":"1","command":"__NAME__","children":[],"else":[]}],"vars":[],"profile":{"name":"__NAME__"}}}"#;

    /// Crea un `robot.db` con la tabla `bots` que lee el motor.
    ///
    /// Mismo patron que `crates/cli/tests/review.rs`: `id`, `name`, `created_at` y
    /// `data` (base64 del JSON del proyecto).
    fn make_db(dir: &Path, robots: &[&str]) -> PathBuf {
        std::fs::create_dir_all(dir).expect("creo la carpeta de la db");
        let path = dir.join("robot.db");
        let conn = rusqlite::Connection::open(&path).expect("creo la db temporal");
        crear_tabla(&conn);
        for (indice, name) in robots.iter().enumerate() {
            insertar(
                &conn,
                indice as i64 + 1,
                name,
                &ROBOT.replace("__NAME__", name),
            );
        }
        path
    }

    fn crear_tabla(conn: &rusqlite::Connection) {
        conn.execute_batch(
            "CREATE TABLE bots (
                id INTEGER PRIMARY KEY,
                name TEXT,
                created_at TEXT,
                data TEXT
            );",
        )
        .expect("creo la tabla bots");
    }

    fn insertar(conn: &rusqlite::Connection, id: i64, name: &str, json: &str) {
        let b64 = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
        conn.execute(
            "INSERT INTO bots (id, name, created_at, data) \
             VALUES (?1, ?2, '2026-11-17 10:00:00', ?3)",
            rusqlite::params![id, name, b64],
        )
        .expect("inserto el snapshot");
    }

    /// Guarda una version nueva: es lo que hace Studio al pulsar "guardar".
    fn guardar_otra_version(db: &Path, id: i64, name: &str) {
        let conn = rusqlite::Connection::open(db).expect("abro la db temporal");
        insertar(&conn, id, name, &ROBOT.replace("__NAME__", name));
    }

    fn options(db: &Path, registry: &Path) -> WatchOptions {
        WatchOptions {
            db: db.to_path_buf(),
            workspace: None,
            interval: Duration::from_secs(30),
            once: true,
            registry: registry.to_path_buf(),
            log_source: None,
        }
    }

    #[test]
    fn detecta_nuevos_cambiados_y_desaparecidos() {
        let antes: BTreeMap<String, i64> = [
            ("alpha".to_string(), 1),
            ("beta".to_string(), 7),
            ("gamma".to_string(), 3),
        ]
        .into_iter()
        .collect();
        let ahora = vec![
            ("alpha".to_string(), 1), // igual: no se toca
            ("beta".to_string(), 9),  // guardado nuevo
            ("delta".to_string(), 2), // robot nuevo en la db
                                      // gamma desaparecio: no hay nada que analizar y no puede salir
        ];

        assert_eq!(robots_cambiados(&antes, &ahora), vec!["beta", "delta"]);
    }

    #[test]
    fn la_primera_pasada_devuelve_todos_los_robots() {
        let antes = BTreeMap::new();
        let ahora = vec![("alpha".to_string(), 1), ("beta".to_string(), 2)];
        assert_eq!(robots_cambiados(&antes, &ahora), vec!["alpha", "beta"]);
    }

    #[test]
    fn sin_cambios_no_hay_nada_que_analizar() {
        let antes: BTreeMap<String, i64> = [("alpha".to_string(), 1), ("beta".to_string(), 2)]
            .into_iter()
            .collect();
        let ahora = vec![("alpha".to_string(), 1), ("beta".to_string(), 2)];
        assert!(robots_cambiados(&antes, &ahora).is_empty());
        // Y un robot desaparecido tampoco se anuncia como cambio.
        let ahora = vec![("alpha".to_string(), 1)];
        assert!(robots_cambiados(&antes, &ahora).is_empty());
    }

    #[test]
    fn un_intervalo_de_cero_o_negativo_se_rechaza_con_un_mensaje() {
        for secs in [0, -1, -30] {
            let error = interval_from_secs(secs).expect_err("tiene que rechazarse");
            let texto = error.to_string();
            assert!(texto.contains("--interval"), "{texto}");
            assert!(texto.contains(">= 1 segundo"), "{texto}");
        }
        assert_eq!(interval_from_secs(1).unwrap(), Duration::from_secs(1));
        assert_eq!(
            interval_from_secs(DEFAULT_INTERVAL_SECS).unwrap(),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn la_cabecera_lleva_db_registro_e_intervalo() {
        let dir = TempDir::new("cabecera");
        let db = dir.path().join("robot.db");
        let registro = dir.path().join("problems.db");
        let mut opciones = options(&db, &registro);
        opciones.once = false;
        let texto = cabecera("db entrega\\robot.db", &opciones);
        assert!(
            texto.starts_with("watch: db entrega\\robot.db | registro "),
            "{texto}"
        );
        assert!(texto.contains("problems.db"), "{texto}");
        assert!(texto.contains("cada 30 s"), "{texto}");
        assert!(
            !texto.contains("--once"),
            "sin `--once` no se anuncia una pasada unica: {texto}"
        );
        // Sin logs lo dice: no habra observaciones y no se puede fingir que si.
        assert!(texto.contains("--no-logs"), "{texto}");
        assert!(texto.contains("no recibira observaciones"), "{texto}");

        // Con `--once` y logs, ni lo uno ni lo otro.
        opciones.once = true;
        opciones.log_source = Some((dir.path().to_path_buf(), 20));
        let texto = cabecera("db entrega\\robot.db", &opciones);
        assert!(texto.contains("| una pasada (--once)"), "{texto}");
        assert!(!texto.contains("--no-logs"), "{texto}");
    }

    #[test]
    fn la_linea_de_un_robot_dice_lo_que_paso() {
        let recuento = Recuento {
            nuevas: 3,
            conocidas: 2,
            omitidas: 0,
        };
        assert_eq!(
            linea_robot("DEMO_ciclo", &recuento, true),
            "DEMO_ciclo: 3 nuevas, 2 ya conocidas"
        );
        // Singular: la salida la lee una persona.
        let una = Recuento {
            nuevas: 1,
            conocidas: 1,
            omitidas: 0,
        };
        assert_eq!(
            linea_robot("uno", &una, true),
            "uno: 1 nueva, 1 ya conocida"
        );
        // Sin defectos, con logs y solo mensajes del robot.
        let solo_logs = Recuento {
            nuevas: 0,
            conocidas: 0,
            omitidas: 2,
        };
        assert_eq!(
            linea_robot("charla", &solo_logs, true),
            "charla: 0 nuevas, 0 ya conocidas \
             (sin defectos que registrar: 2 mensajes de log_message omitidos)"
        );
        // Sin logs no se puede afirmar que el robot este limpio.
        let nada = Recuento::default();
        assert_eq!(
            linea_robot("mudo", &nada, false),
            "mudo: 0 nuevas, 0 ya conocidas (--no-logs: no se leyeron logs, 0 observaciones)"
        );
    }

    #[test]
    fn el_registro_explicito_manda() {
        let path = PathBuf::from("C:\\datos\\problems.db");
        assert_eq!(resolve_registry(Some(path.clone())), path);
        // Sin flag, el camino por defecto del motor termina siempre igual.
        assert_eq!(
            resolve_registry(None).file_name().and_then(|n| n.to_str()),
            Some("problems.db")
        );
    }

    #[test]
    fn una_pasada_crea_el_registro_y_dice_que_no_registro_nada() {
        // El caso que se pone en una tarea programada: db temporal y `--once`.
        let dir = TempDir::new("once");
        let db = make_db(dir.path(), &["alpha"]);
        let registro = dir.path().join("problems.db");
        let opciones = options(&db, &registro);
        assert!(!registro.exists(), "aun no hay registro");

        let mut salida = Vec::new();
        run(&opciones, &mut salida).expect("el comando sale con exito (exit 0)");

        let texto = String::from_utf8(salida).expect("la salida es utf-8");
        assert!(
            registro.exists(),
            "el fichero del registro se crea al abrir"
        );
        assert!(texto.starts_with("watch: db "), "{texto}");
        assert!(texto.contains("| registro "), "{texto}");
        assert!(texto.contains("cada 30 s"), "{texto}");
        assert!(texto.contains("--no-logs"), "{texto}");
        // Sin logs: cero observaciones, y se dice; no se finge un robot limpio.
        assert!(
            texto.contains(
                "alpha: 0 nuevas, 0 ya conocidas (--no-logs: no se leyeron logs, 0 observaciones)"
            ),
            "{texto}"
        );

        // Y el registro esta vacio de verdad, no solo en el texto.
        let registro = Registry::open(&dir.path().join("problems.db")).expect("abro el registro");
        assert_eq!(registro.counts().expect("cuento"), (0, 0));
    }

    #[test]
    fn la_segunda_pasada_sin_cambios_calla_y_la_tercera_solo_analiza_lo_guardado() {
        let dir = TempDir::new("bucle");
        let db = make_db(dir.path(), &["alpha", "beta"]);
        let registro = dir.path().join("problems.db");
        let opciones = options(&db, &registro);
        let mut reg = Registry::open(&registro).expect("abro el registro");

        // Primera pasada: todos los robots, y se dice.
        let mut primera = Vec::new();
        let estado = pasada(&opciones, &BTreeMap::new(), &mut reg, &mut primera)
            .expect("la primera pasada no falla");
        let texto = String::from_utf8(primera).expect("utf-8");
        assert_eq!(estado.analizados, 2, "{texto}");
        assert!(texto.contains("alpha: "), "{texto}");
        assert!(texto.contains("beta: "), "{texto}");

        // Segunda pasada sin tocar la db: silencio (nada de lineas en stdout).
        let mut segunda = Vec::new();
        let estado_igual = pasada(&opciones, &estado.ids, &mut reg, &mut segunda)
            .expect("la segunda pasada no falla");
        assert!(
            segunda.is_empty(),
            "sin cambios no se imprime nada: {}",
            String::from_utf8_lossy(&segunda)
        );
        assert_eq!(estado_igual.analizados, 0);
        assert_eq!(estado_igual.ids, estado.ids);

        // Se guarda una version nueva de beta: la tercera pasada solo mira beta.
        guardar_otra_version(&db, 3, "beta");
        let mut tercera = Vec::new();
        let estado_cambio = pasada(&opciones, &estado_igual.ids, &mut reg, &mut tercera)
            .expect("la tercera pasada no falla");
        let texto = String::from_utf8(tercera).expect("utf-8");
        assert_eq!(estado_cambio.analizados, 1, "{texto}");
        assert!(texto.contains("beta: "), "{texto}");
        assert!(
            !texto.contains("alpha: "),
            "alpha no cambio y no se vuelve a analizar: {texto}"
        );
    }

    #[test]
    fn un_robot_ilegible_no_aborta_la_pasada() {
        let dir = TempDir::new("roto");
        let db = make_db(dir.path(), &["alpha"]);
        // Una fila que no se puede decodificar: el listado la ve, el analisis no.
        let conn = rusqlite::Connection::open(&db).expect("abro la db temporal");
        insertar(&conn, 2, "roto", "esto-no-es-base64");
        drop(conn);

        let registro = dir.path().join("problems.db");
        let mut reg = Registry::open(&registro).expect("abro el registro");
        let mut salida = Vec::new();
        let estado = pasada(
            &options(&db, &registro),
            &BTreeMap::new(),
            &mut reg,
            &mut salida,
        )
        .expect("un robot roto no puede tumbar el vigilante");

        let texto = String::from_utf8(salida).expect("utf-8");
        assert!(
            texto.contains("alpha: "),
            "el robot bueno se analiza igual: {texto}"
        );
        assert!(
            !texto.contains("roto: "),
            "del robot ilegible no se inventa una linea: {texto}"
        );
        // Cuenta como visto: se reintenta cuando lo guarden, no en cada vuelta.
        assert_eq!(estado.analizados, 1);
        assert_eq!(estado.ids.len(), 2);
    }

    #[test]
    fn un_workspace_vigila_cada_db_con_su_etiqueta_relativa() {
        let dir = TempDir::new("workspace");
        make_db(&dir.path().join("entrega_a"), &["alpha"]);
        make_db(&dir.path().join("entrega_b"), &["beta"]);
        let registro = dir.path().join("problems.db");
        let mut opciones = options(&dir.path().join("nada.db"), &registro);
        opciones.workspace = Some(dir.path().to_path_buf());

        let mut reg = Registry::open(&registro).expect("abro el registro");
        let mut salida = Vec::new();
        let estado = pasada(&opciones, &BTreeMap::new(), &mut reg, &mut salida)
            .expect("la pasada del workspace no falla");

        let texto = String::from_utf8(salida).expect("utf-8");
        // La etiqueta es la ruta relativa a la raiz: se distinguen las dos entregas y
        // no se filtra la ruta absoluta del temporal.
        assert!(
            texto.contains("entrega_a\\robot.db\\alpha: "),
            "falta la etiqueta de la primera db: {texto}"
        );
        assert!(
            texto.contains("entrega_b\\robot.db\\beta: "),
            "falta la etiqueta de la segunda db: {texto}"
        );
        assert!(
            !texto.contains(&dir.path().display().to_string()),
            "la salida filtra la ruta absoluta: {texto}"
        );
        assert_eq!(estado.analizados, 2);
        assert_eq!(estado.dbs_leidas, 2);

        // Un robot homonimo en dos dbs son dos robots distintos en el estado.
        make_db(&dir.path().join("entrega_c"), &["alpha"]);
        let mut salida = Vec::new();
        let estado = pasada(&opciones, &estado.ids, &mut reg, &mut salida)
            .expect("la pasada con la entrega nueva no falla");
        let texto = String::from_utf8(salida).expect("utf-8");
        assert_eq!(
            estado.analizados, 1,
            "solo el robot de la entrega nueva: {texto}"
        );
        assert!(texto.contains("entrega_c\\robot.db\\alpha: "), "{texto}");
    }

    #[test]
    fn etiqueta_relativa_no_colisiona_entre_entregas_anidadas() {
        let root = Path::new("C:\\proyectos");
        let una = etiqueta_relativa(root, Path::new("C:\\proyectos\\a\\entrega\\robot.db"));
        let otra = etiqueta_relativa(root, Path::new("C:\\proyectos\\b\\entrega\\robot.db"));
        assert_eq!(una, "a\\entrega\\robot.db");
        assert_eq!(otra, "b\\entrega\\robot.db");
        assert_ne!(
            una, otra,
            "dos dbs con el mismo fichero no pueden compartir clave"
        );
        // Y la etiqueta no lleva la ruta absoluta de la raiz.
        assert!(!una.contains("C:"), "{una}");

        // La misma funcion en forma posix: el separador del host no puede cambiar
        // el resultado (CI compila en Linux y esto lo detecto alli).
        let posix = etiqueta_relativa(
            Path::new("/home/u/proyectos"),
            Path::new("/home/u/proyectos/a/entrega/robot.db"),
        );
        assert_eq!(posix, "a\\entrega\\robot.db");
        assert!(!posix.contains("/home"), "{posix}");
    }
}
