use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::Connection;

use crate::attribution::{self, NodeRef, RawError, SnapshotView, Verdict};
use crate::db;
use crate::error::Result;
use crate::logs;
use crate::signature::{self, ErrorKind};
use crate::tree;

pub const ERROR_MESSAGE_MAX: usize = 160;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorItem {
    pub line: String,
    pub command: String,
    pub message: String,
    pub occurrences: usize,
    pub signature: String,
    pub version: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErrorReport {
    pub live: Vec<ErrorItem>,
    pub payload_changed: Vec<ErrorItem>,
    pub historical: Vec<ErrorItem>,
    pub platform: Vec<ErrorItem>,
    pub noise: Vec<ErrorItem>,
}

impl ErrorReport {
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    pub fn total(&self) -> usize {
        self.live.len()
            + self.payload_changed.len()
            + self.historical.len()
            + self.platform.len()
            + self.noise.len()
    }
}

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

pub(crate) fn snapshot_views(conn: &Connection, robot: &str) -> Result<Vec<SnapshotView>> {
    let mut out = Vec::new();
    for snap in db::load_snapshots(conn, robot)? {
        let root = db::project_root(&snap.project);
        let nodes = tree::flatten(db::commands_of(root))
            .into_iter()
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Bucket {
    Live = 0,
    PayloadChanged = 1,
    Historical = 2,
    Platform = 3,
    Noise = 4,
}

impl Bucket {
    fn index(self) -> usize {
        self as usize
    }

    pub(super) fn from_verdict(verdict: Verdict) -> Self {
        match verdict {
            Verdict::Vivo => Bucket::Live,
            Verdict::PayloadCambiado => Bucket::PayloadChanged,
            Verdict::Historico | Verdict::Desconocido => Bucket::Historical,
        }
    }
}

pub(super) fn bucket_of(message: &str) -> Option<Bucket> {
    if signature::is_engine_noise(message) {
        return Some(Bucket::Noise);
    }
    if signature::classify(message) == ErrorKind::Module {
        return Some(Bucket::Platform);
    }
    None
}

fn robot_errors(robot: &str, logs_dir: &Path, max_files: usize) -> Vec<logs::LogEntry> {
    let mut out = Vec::new();
    for path in logs::recent_files(logs_dir, max_files) {
        let Some(entries) = logs::parse_file_cached(&path) else {
            continue;
        };
        out.extend(
            logs::of_robot(entries.as_ref(), robot)
                .filter(|entry| entry.is_error())
                .cloned(),
        );
    }
    out.sort_by_key(|e| e.timestamp);
    out
}

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
        assert_eq!(
            bucket_of(r#"EXCEPTION IN (<string> zdev, L_1375 ""): Error: Incorrect padding"#),
            Some(Bucket::Noise)
        );
        assert_eq!(
            bucket_of(r#"EXCEPTION IN (<string> datatransform, L_212 ""): AttributeError: x"#),
            Some(Bucket::Platform)
        );
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

    fn log_de(robot: &str) -> String {
        format!(
            "2026-11-17 12:43:47.586814 - rocketbot - INFO - request: {robot} : line 1 - rpasystem \
             : log_message  id: {robot}-1 - INFO - hola\n\
             2026-11-17 12:43:48.000000 - rocketbot - ERROR - EXCEPTION IN (<string> Rocketbot, \
             L_91 \"\"): NameError: name 'x' is not defined\n"
        )
    }

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

        let otra_vez = errors_for(&conn, "alfa", &logs_dir, 20).expect("informe de alfa otra vez");
        assert_eq!(otra_vez, alfa, "el cache no cambia ni un cubo del informe");
        assert_eq!(
            logs::cache_stats(),
            (2, 4),
            "dos aciertos mas, ningun parseo"
        );
    }

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
