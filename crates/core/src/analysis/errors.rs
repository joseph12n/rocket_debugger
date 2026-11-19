//! Informe de errores del robot clasificados por lo que se puede hacer con
//! ellos: vivos, payload cambiado, historicos, modulo de plataforma y ruido.
//!
//! Invariante: `ERROR_MESSAGE_MAX` acota el mensaje a su primera linea.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::Connection;

use crate::attribution::{self, NodeRef, RawError, SnapshotView, Verdict};
use crate::db;
use crate::error::Result;
use crate::logs;
use crate::signature::{self, ErrorKind};
use crate::tree;

/// Tope del mensaje en caracteres; el mensaje se queda en su primera linea.
pub const ERROR_MESSAGE_MAX: usize = 160;

/// Un error del log agrupado por firma.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorItem {
    /// Linea de Studio en la version donde ocurrio; vacia si el nodo no existia.
    pub line: String,
    pub command: String,
    /// Primera linea del mensaje, acotada a [`ERROR_MESSAGE_MAX`].
    pub message: String,
    pub occurrences: usize,
    /// Firma estable (`signature::normalize`): clave de agrupacion.
    pub signature: String,
    /// `bots.id` vivo en el instante del error, si se pudo atribuir.
    pub version: Option<i64>,
}

/// Errores del robot repartidos por lo que se puede hacer con ellos.
/// `live` y `payload_changed` son accionables hoy; `historical` solo es evidencia
/// de un defecto ya corregido. `platform` y `noise` son fallos de Rocketbot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErrorReport {
    pub live: Vec<ErrorItem>,
    pub payload_changed: Vec<ErrorItem>,
    pub historical: Vec<ErrorItem>,
    pub platform: Vec<ErrorItem>,
    pub noise: Vec<ErrorItem>,
}

impl ErrorReport {
    /// Sin nada que contar: el render omite la seccion entera.
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// Firmas distintas en el informe, no ocurrencias.
    pub fn total(&self) -> usize {
        self.live.len()
            + self.payload_changed.len()
            + self.historical.len()
            + self.platform.len()
            + self.noise.len()
    }
}

/// Lee los logs del directorio y devuelve los errores del robot clasificados.
/// Un directorio sin logs da un informe vacio, no un error.
pub fn errors_for(
    conn: &Connection,
    robot: &str,
    logs_dir: &Path,
    max_files: usize,
) -> Result<ErrorReport> {
    let entries = robot_errors(robot, logs_dir, max_files);
    if entries.is_empty() {
        return Ok(ErrorReport::default());
    }

    let snapshots = snapshot_views(conn, robot)?;

    let mut raws: Vec<RawError> = Vec::with_capacity(entries.len());
    let mut por_mensaje: Vec<Option<Bucket>> = Vec::with_capacity(entries.len());
    for entry in &entries {
        // `for_robot` ya exige peticion; el `let else` evita un error sin nodo.
        let Some(request) = entry.request.as_ref() else {
            continue;
        };
        raws.push(RawError {
            timestamp: entry.timestamp,
            cmd_id: request.cmd_id.clone(),
            command: request.command.clone(),
            payload: request.payload.clone(),
            message: short_message(&entry.message),
            occurrences: 1,
            signature: signature::normalize(&entry.message),
        });
        por_mensaje.push(bucket_of(&entry.message));
    }

    let atrs = attribution::attribute(&raws, &snapshots);

    let mut cubos: [BTreeMap<String, ErrorItem>; 5] = std::array::from_fn(|_| BTreeMap::new());
    for ((raw, atr), decidido) in raws.iter().zip(&atrs).zip(&por_mensaje) {
        let bucket = decidido.unwrap_or_else(|| Bucket::from_verdict(atr.verdict));
        cubos[bucket.index()]
            .entry(raw.signature.clone())
            .and_modify(|item| {
                item.occurrences += raw.occurrences;
                // La primera linea que se pudo ubicar es la que el agente mira.
                if item.line.is_empty() {
                    if let Some(line) = &atr.line {
                        item.line = line.clone();
                    }
                }
                item.version = max_version(item.version, atr.version);
            })
            .or_insert_with(|| ErrorItem {
                line: atr.line.clone().unwrap_or_default(),
                command: raw.command.clone(),
                message: raw.message.clone(),
                occurrences: raw.occurrences,
                signature: raw.signature.clone(),
                version: atr.version,
            });
    }

    let [live, payload_changed, historical, platform, noise] = cubos;
    Ok(ErrorReport {
        live: sorted(live),
        payload_changed: sorted(payload_changed),
        historical: sorted(historical),
        platform: sorted(platform),
        noise: sorted(noise),
    })
}

/// Snapshots del robot reducidos a `cmd_id -> nodo`, para la atribucion.
/// Compartido con el broker de ejecucion para no dar lineas distintas.
pub(crate) fn snapshot_views(conn: &Connection, robot: &str) -> Result<Vec<SnapshotView>> {
    let mut out = Vec::new();
    for snap in db::load_snapshots(conn, robot)? {
        let root = db::project_root(&snap.project);
        let nodes = tree::flatten(db::commands_of(root))
            .into_iter()
            // Sin `id` el nodo no es direccionable y no puede cruzar con el log.
            .filter(|n| !n.id.is_empty())
            .map(|n| {
                (
                    n.id,
                    NodeRef {
                        line: n.line,
                        params: n.params,
                    },
                )
            })
            .collect();
        out.push(SnapshotView {
            id: snap.id,
            created_at: snap.created_at,
            nodes,
        });
    }
    Ok(out)
}

/// Cubo de clasificacion de un error.
/// El orden del `enum` es el de la cabecera del render y el indice de los cubos.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Bucket {
    Live = 0,
    PayloadChanged = 1,
    Historical = 2,
    Platform = 3,
    Noise = 4,
}

impl Bucket {
    /// Indice en la tabla de cubos: unica traduccion de "cubo" a "posicion".
    fn index(self) -> usize {
        self as usize
    }

    /// Veredicto de la atribucion -> cubo del informe.
    /// [`Verdict::Desconocido`] va con los historicos: no es accionable hoy.
    pub(super) fn from_verdict(verdict: Verdict) -> Self {
        match verdict {
            Verdict::Vivo => Bucket::Live,
            Verdict::PayloadCambiado => Bucket::PayloadChanged,
            Verdict::Historico | Verdict::Desconocido => Bucket::Historical,
        }
    }
}

/// Cubo decidible solo con el texto del error; `None` si hay que mirar el veredicto.
pub(super) fn bucket_of(message: &str) -> Option<Bucket> {
    // Ruido primero: `zdev` y `getbot` tambien son modulos, pero no son del robot.
    if signature::is_engine_noise(message) {
        return Some(Bucket::Noise);
    }
    if signature::classify(message) == ErrorKind::Module {
        return Some(Bucket::Platform);
    }
    None
}

/// Entradas de error del robot, en orden cronologico.
/// Reutiliza el cache de ficheros parseados y filtra por referencia.
fn robot_errors(robot: &str, logs_dir: &Path, max_files: usize) -> Vec<logs::LogEntry> {
    let mut out = Vec::new();
    for path in logs::recent_files(logs_dir, max_files) {
        // Un log ilegible se omite: es evidencia que falta, no un fallo del pack.
        let Some(entries) = logs::parse_file_cached(&path) else {
            continue;
        };
        out.extend(
            logs::of_robot(entries.as_ref(), robot)
                .filter(|entry| entry.is_error())
                .cloned(),
        );
    }
    // "La primera ocurrencia" solo vale en orden cronologico; la ordenacion es
    // estable y conserva el orden del runner para el mismo instante.
    out.sort_by_key(|e| e.timestamp);
    out
}

/// Primera linea del mensaje, acotada: la traza de Python no es parte del defecto.
pub(super) fn short_message(message: &str) -> String {
    let first = message.lines().next().unwrap_or("").trim();
    let mut out: String = first.chars().take(ERROR_MESSAGE_MAX).collect();
    if first.chars().count() > ERROR_MESSAGE_MAX {
        out.push('…');
    }
    out
}

pub(super) fn max_version(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Mas repetidos primero; el desempate por firma mantiene el render estable.
fn sorted(cubo: BTreeMap<String, ErrorItem>) -> Vec<ErrorItem> {
    let mut out: Vec<ErrorItem> = cubo.into_values().collect();
    out.sort_by(|a, b| {
        b.occurrences
            .cmp(&a.occurrences)
            .then_with(|| a.signature.cmp(&b.signature))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn el_mensaje_del_error_se_queda_en_la_primera_linea() {
        assert_eq!(
            short_message("NameError: name 'x' is not defined\nTraceback (most recent call last):"),
            "NameError: name 'x' is not defined",
            "la traza de Python no es parte del defecto"
        );
        assert_eq!(short_message("  con espacios  "), "con espacios");
        assert_eq!(short_message(""), "");
    }

    #[test]
    fn el_mensaje_largo_se_recorta_sin_romper_unicode() {
        let largo = "a".repeat(ERROR_MESSAGE_MAX + 10);
        let recortado = short_message(&largo);
        assert_eq!(recortado.chars().count(), ERROR_MESSAGE_MAX + 1);
        assert!(recortado.ends_with('…'));

        // Recortar por bytes partiria un caracter de dos y dejaria el pack con
        // texto invalido.
        let unicode = "ñ".repeat(ERROR_MESSAGE_MAX + 1);
        let recortado = short_message(&unicode);
        assert_eq!(recortado.chars().count(), ERROR_MESSAGE_MAX + 1);
        assert!(recortado.starts_with('ñ'));
    }

    #[test]
    fn el_veredicto_decide_el_cubo_y_lo_desconocido_no_parece_vivo() {
        assert_eq!(Bucket::from_verdict(Verdict::Vivo), Bucket::Live);
        assert_eq!(
            Bucket::from_verdict(Verdict::PayloadCambiado),
            Bucket::PayloadChanged
        );
        assert_eq!(Bucket::from_verdict(Verdict::Historico), Bucket::Historical);
        assert_eq!(
            Bucket::from_verdict(Verdict::Desconocido),
            Bucket::Historical,
            "sin version no es accionable contra el codigo de hoy"
        );
    }

    #[test]
    fn el_cubo_por_mensaje_separa_ruido_y_plataforma() {
        // Ruido de motor: no se arregla desde el robot.
        assert_eq!(
            bucket_of(r#"EXCEPTION IN (<string> zdev, L_1375 ""): Error: Incorrect padding"#),
            Some(Bucket::Noise)
        );
        // Defecto de un modulo de la plataforma: no es ruido transitorio.
        assert_eq!(
            bucket_of(r#"EXCEPTION IN (<string> datatransform, L_212 ""): AttributeError: x"#),
            Some(Bucket::Platform)
        );
        // Error del robot: hay que atribuirlo antes de colocarlo.
        assert_eq!(
            bucket_of(r#"EXCEPTION IN (<string> Rocketbot, L_91 ""): NameError: name 'x'"#),
            None
        );
    }

    #[test]
    fn la_version_del_grupo_es_la_mas_alta() {
        assert_eq!(max_version(None, None), None);
        assert_eq!(max_version(Some(25), None), Some(25));
        assert_eq!(max_version(None, Some(29)), Some(29));
        assert_eq!(max_version(Some(25), Some(29)), Some(29));
        assert_eq!(max_version(Some(29), Some(25)), Some(29));
    }

    /// Db minima con un snapshot por robot: `errors_for` atribuye cada error al
    /// snapshot vivo, y sin snapshots no habria a que atribuirlo.
    fn db_con_robots(dir: &Path, robots: &[&str]) -> Connection {
        use base64::Engine as _;

        let conn = Connection::open(dir.join("robot.db")).expect("creo la db");
        conn.execute_batch(
            "CREATE TABLE bots (
                id INTEGER PRIMARY KEY,
                name TEXT,
                created_at TEXT,
                data TEXT
            );",
        )
        .expect("creo la tabla bots");
        for (index, robot) in robots.iter().enumerate() {
            // El `id` del nodo tiene que coincidir con el `cmd_id` del log: es la
            // unica union fiable entre ejecucion y codigo.
            let project = serde_json::json!({
                "project": {
                    "commands": [{
                        "father": "log_message",
                        "line": "1",
                        "id": format!("{robot}-1"),
                        "command": "hola",
                        "children": [],
                        "else": [],
                    }],
                    "vars": [],
                }
            });
            let b64 =
                base64::engine::general_purpose::STANDARD.encode(project.to_string().as_bytes());
            conn.execute(
                "INSERT INTO bots (id, name, created_at, data) \
                 VALUES (?1, ?2, '2026-11-17 10:00:00', ?3)",
                rusqlite::params![index as i64 + 1, robot, b64],
            )
            .expect("inserto el snapshot");
        }
        conn
    }

    /// Log con una peticion del robot y un error que la hereda, como los escribe el
    /// runner: la linea de error no dice de que nodo es.
    fn log_de(robot: &str) -> String {
        format!(
            "2026-11-17 12:43:47.586814 - rocketbot - INFO - request: {robot} : line 1 - rpasystem \
             : log_message  id: {robot}-1 - INFO - hola\n\
             2026-11-17 12:43:48.000000 - rocketbot - ERROR - EXCEPTION IN (<string> Rocketbot, \
             L_91 \"\"): NameError: name 'x' is not defined\n"
        )
    }

    /// El cache no puede cambiar el informe: dos robots sobre el mismo directorio
    /// dan lo mismo que daban sin cache, y el segundo reaprovecha el parseo del
    /// primero.
    #[test]
    fn dos_robots_sobre_los_mismos_logs_conservan_el_informe_y_aprovechan_el_cache() {
        let _turno = logs::cache_test_guard();
        logs::clear_cache();

        let tmp = tempfile::tempdir().expect("tempdir");
        let conn = db_con_robots(tmp.path(), &["alfa", "beta"]);
        let logs_dir = tmp.path().join("logs");
        std::fs::create_dir_all(&logs_dir).expect("creo el directorio de logs");
        for robot in ["alfa", "beta"] {
            std::fs::write(logs_dir.join(format!("logfile_{robot}.log")), log_de(robot))
                .expect("escribo el log");
        }

        // Primera pasada: dos ficheros parseados, ningun acierto.
        let alfa = errors_for(&conn, "alfa", &logs_dir, 20).expect("informe de alfa");
        assert_eq!(
            alfa.live.len(),
            1,
            "el error hereda la peticion viva de alfa: {alfa:?}"
        );
        assert_eq!(
            logs::cache_stats(),
            (2, 0),
            "un fichero cacheado por fichero leido"
        );

        // Segundo robot, mismos ficheros: ni una lectura nueva.
        let beta = errors_for(&conn, "beta", &logs_dir, 20).expect("informe de beta");
        assert_eq!(
            logs::cache_stats(),
            (2, 2),
            "el segundo robot acierta en los dos ficheros, sin parsearlos otra vez"
        );
        assert_eq!(beta.live.len(), 1, "beta tiene su propio error: {beta:?}");
        assert_eq!(
            beta.live[0].occurrences, 1,
            "el error de alfa no puede contarse en el informe de beta"
        );

        // Y alfa, otra vez, da exactamente el mismo informe que la primera vez.
        let otra_vez = errors_for(&conn, "alfa", &logs_dir, 20).expect("informe de alfa otra vez");
        assert_eq!(otra_vez, alfa, "el cache no cambia ni un cubo del informe");
        assert_eq!(
            logs::cache_stats(),
            (2, 4),
            "dos aciertos mas, ningun parseo"
        );
    }

    /// Un directorio sin ficheros de log no entra al cache ni inventa un informe.
    #[test]
    fn sin_logs_el_informe_sale_vacio_y_el_cache_intacto() {
        let _turno = logs::cache_test_guard();
        logs::clear_cache();

        let tmp = tempfile::tempdir().expect("tempdir");
        let conn = db_con_robots(tmp.path(), &["alfa"]);
        let vacio = tmp.path().join("sin_logs");

        let informe = errors_for(&conn, "alfa", &vacio, 20).expect("informe vacio");
        assert!(informe.is_empty(), "sin logs no hay errores: {informe:?}");
        assert_eq!(logs::cache_stats(), (0, 0), "no hay ficheros que cachear");
    }
}
