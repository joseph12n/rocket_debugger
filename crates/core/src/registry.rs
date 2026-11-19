use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::NaiveDateTime;
use rusqlite::{params, Connection, TransactionBehavior};

use crate::error::{Error, Result};

const FILE_NAME: &str = "problems.db";

const APP_DIR: &str = "rocket_debugger";

pub const HOME_ENV: &str = "ROCKETDEBUG_HOME";

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const TS_FMT: &str = "%Y-%m-%dT%H:%M:%S%.9f";

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS signatures (
    id          INTEGER PRIMARY KEY,
    signature   TEXT    NOT NULL UNIQUE,
    occurrences INTEGER NOT NULL DEFAULT 0,
    first_seen  TEXT    NOT NULL,
    last_seen   TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS observations (
    id           INTEGER PRIMARY KEY,
    signature_id INTEGER NOT NULL REFERENCES signatures(id),
    robot        TEXT    NOT NULL,
    db_label     TEXT    NOT NULL,
    version      INTEGER,
    line         TEXT    NOT NULL,
    message      TEXT    NOT NULL,
    seen_at      TEXT    NOT NULL
);

-- La identidad incluye `version`, que puede no conocerse. En un UNIQUE normal
-- los NULL son distintos entre si, asi que dos observaciones sin version nunca
-- colisionarian y el motor volveria a contar el mismo log en cada barrido.
-- COALESCE lleva el \"no se sabe\" a un valor concreto para que la unicidad
-- tambien aplique ahi; -1 no colisiona porque las versiones de `robot.db` son
-- ids positivos.
CREATE UNIQUE INDEX IF NOT EXISTS observations_identity
    ON observations (signature_id, robot, COALESCE(version, -1), line, seen_at);

CREATE TABLE IF NOT EXISTS fixes (
    id           INTEGER PRIMARY KEY,
    signature_id INTEGER NOT NULL REFERENCES signatures(id),
    robot        TEXT    NOT NULL,
    from_version INTEGER,
    to_version   INTEGER,
    summary      TEXT    NOT NULL,
    detail       TEXT    NOT NULL,
    note         TEXT    NOT NULL,
    recorded_at  TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS fixes_by_signature ON fixes (signature_id);
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub signature: String,
    pub robot: String,
    pub db_label: String,
    pub version: Option<i64>,
    pub line: String,
    pub message: String,
    pub seen_at: NaiveDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixRecord {
    pub signature: String,
    pub robot: String,
    pub from_version: Option<i64>,
    pub to_version: Option<i64>,
    pub summary: String,
    pub detail: String,
    pub note: String,
    pub recorded_at: NaiveDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureStats {
    pub signature: String,
    pub occurrences: usize,
    pub first_seen: NaiveDateTime,
    pub last_seen: NaiveDateTime,
    pub robots: Vec<String>,
    pub fixes: usize,
}

#[derive(Debug)]
pub struct Registry {
    conn: Connection,
}

impl Registry {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir).map_err(|source| Error::Open {
                    path: path.to_path_buf(),
                    source: io_error(source),
                })?;
            }
        }
        let conn = Connection::open(path).map_err(|source| Error::Open {
            path: path.to_path_buf(),
            source,
        })?;
        Self::init(conn)
    }

    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn record_observation(&mut self, o: &Observation) -> Result<bool> {
        let seen_at = format_ts(o.seen_at);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let signature_id = ensure_signature(&tx, &o.signature, &seen_at)?;

        let already: bool = tx.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM observations
                  WHERE signature_id = ?1 AND robot = ?2 AND version IS ?3
                    AND line = ?4 AND seen_at = ?5)",
            params![signature_id, o.robot, o.version, o.line, seen_at],
            |r| r.get(0),
        )?;
        if already {
            tx.commit()?;
            return Ok(false);
        }

        tx.execute(
            "INSERT INTO observations
                 (signature_id, robot, db_label, version, line, message, seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                signature_id,
                o.robot,
                o.db_label,
                o.version,
                o.line,
                o.message,
                seen_at
            ],
        )?;
        tx.execute(
            "UPDATE signatures
                SET occurrences = occurrences + 1,
                    first_seen  = MIN(first_seen, ?2),
                    last_seen   = MAX(last_seen, ?2)
              WHERE id = ?1",
            params![signature_id, seen_at],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn record_fix(&mut self, f: &FixRecord) -> Result<i64> {
        let recorded_at = format_ts(f.recorded_at);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let signature_id = ensure_signature(&tx, &f.signature, &recorded_at)?;
        tx.execute(
            "INSERT INTO fixes
                 (signature_id, robot, from_version, to_version, summary, detail, note, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                signature_id,
                f.robot,
                f.from_version,
                f.to_version,
                f.summary,
                f.detail,
                f.note,
                recorded_at
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SignatureStats>> {
        let needle = query.trim().to_lowercase();

        let mut stmt = self.conn.prepare(
            "SELECT id, signature, occurrences, first_seen, last_seen
               FROM signatures
              ORDER BY occurrences DESC, last_seen DESC, signature",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let firmas = rows.collect::<rusqlite::Result<Vec<_>>>()?;

        let mut robots: HashMap<i64, Vec<String>> = HashMap::new();
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT signature_id, robot FROM observations ORDER BY signature_id, robot",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (id, robot) = row?;
            robots.entry(id).or_default().push(robot);
        }

        let mut fixes: HashMap<i64, usize> = HashMap::new();
        let mut stmt = self
            .conn
            .prepare("SELECT signature_id, COUNT(*) FROM fixes GROUP BY signature_id")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (id, n) = row?;
            fixes.insert(id, n as usize);
        }

        let mut out = Vec::new();
        for (id, signature, occurrences, first_seen, last_seen) in firmas {
            if out.len() >= limit {
                break;
            }
            let robots_de_la_firma = robots.get(&id).cloned().unwrap_or_default();
            let coincide = needle.is_empty()
                || signature.to_lowercase().contains(&needle)
                || robots_de_la_firma
                    .iter()
                    .any(|r| r.to_lowercase().contains(&needle));
            if !coincide {
                continue;
            }
            out.push(SignatureStats {
                signature,
                occurrences: occurrences as usize,
                first_seen: parse_ts(&first_seen),
                last_seen: parse_ts(&last_seen),
                robots: robots_de_la_firma,
                fixes: fixes.get(&id).copied().unwrap_or(0),
            });
        }
        Ok(out)
    }

    pub fn counts(&self) -> Result<(usize, usize)> {
        let (firmas, fixes): (i64, i64) = self.conn.query_row(
            "SELECT (SELECT COUNT(*) FROM signatures), (SELECT COUNT(*) FROM fixes)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((firmas as usize, fixes as usize))
    }
}

pub fn default_path() -> PathBuf {
    resolve_path(std::env::var_os(HOME_ENV), platform_base())
}

fn resolve_path(home: Option<OsString>, base: PathBuf) -> PathBuf {
    match home.as_deref().filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir).join(FILE_NAME),
        None => base.join(FILE_NAME),
    }
}

#[cfg(windows)]
fn platform_base() -> PathBuf {
    match std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir).join(APP_DIR),
        None => std::env::temp_dir().join(APP_DIR),
    }
}

#[cfg(not(windows))]
fn platform_base() -> PathBuf {
    match std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir)
            .join(".local")
            .join("share")
            .join(APP_DIR),
        None => std::env::temp_dir().join(APP_DIR),
    }
}

fn ensure_signature(tx: &Connection, signature: &str, at: &str) -> Result<i64> {
    tx.execute(
        "INSERT INTO signatures (signature, occurrences, first_seen, last_seen)
         VALUES (?1, 0, ?2, ?2)
         ON CONFLICT(signature) DO NOTHING",
        params![signature, at],
    )?;
    let id = tx.query_row(
        "SELECT id FROM signatures WHERE signature = ?1",
        params![signature],
        |r| r.get(0),
    )?;
    Ok(id)
}

fn format_ts(t: NaiveDateTime) -> String {
    format!(
        "{}.{:09}",
        t.format("%Y-%m-%dT%H:%M:%S"),
        t.and_utc().timestamp_subsec_nanos()
    )
}

fn parse_ts(s: &str) -> NaiveDateTime {
    NaiveDateTime::parse_from_str(s, TS_FMT)
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f"))
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f"))
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
        .unwrap_or_default()
}

fn io_error(e: std::io::Error) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
        Some(e.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: &str = "2026-11-17 12:00:00";
    const FIRMA_NOMBRE: &str = "NameError:name:{var}:not_defined";
    const FIRMA_CLAVE: &str = "KeyError:<key>";

    fn obs(
        signature: &str,
        robot: &str,
        version: Option<i64>,
        line: &str,
        seen_at: &str,
    ) -> Observation {
        Observation {
            signature: signature.to_string(),
            robot: robot.to_string(),
            db_label: "entrega/robot_run.db".to_string(),
            version,
            line: line.to_string(),
            message: format!("mensaje de {signature}"),
            seen_at: crate::db::parse_date(seen_at),
        }
    }

    fn fix(
        signature: &str,
        robot: &str,
        from: Option<i64>,
        to: Option<i64>,
        recorded_at: &str,
    ) -> FixRecord {
        FixRecord {
            signature: signature.to_string(),
            robot: robot.to_string(),
            from_version: from,
            to_version: to,
            summary: "asignar 1 en vez de contador + 1".to_string(),
            detail: "setVar contador = contador + 1 se lee a si misma".to_string(),
            note: "verificado en studio".to_string(),
            recorded_at: crate::db::parse_date(recorded_at),
        }
    }

    #[test]
    fn los_instantes_se_guardan_en_ancho_fijo_y_ordenan_como_el_tiempo() {
        let t = crate::db::parse_date("2026-11-17 12:49:22");
        assert_eq!(format_ts(t), "2026-11-17T12:49:22.000000000");
        assert_eq!(parse_ts(&format_ts(t)), t);
        let antes = format_ts(crate::db::parse_date("2026-11-17 12:49:22"));
        let despues = format_ts(crate::db::parse_date("2026-11-17 12:49:22.5"));
        assert!(antes < despues, "{antes} no ordena antes de {despues}");
        assert_eq!(parse_ts("no-es-fecha"), NaiveDateTime::default());
    }

    #[test]
    fn la_misma_version_escaneada_dos_veces_no_suma() {
        let mut registro = Registry::in_memory().expect("registro");
        let obs = Observation {
            signature: "NameError:name:{var}:not_defined".to_string(),
            robot: "ML_06_Legacy".to_string(),
            db_label: "test\\robot.db".to_string(),
            version: Some(25),
            line: "2.10".to_string(),
            message: "NameError: name 'contador' is not defined".to_string(),
            seen_at: crate::db::parse_date("2026-11-17 12:47:10"),
        };
        assert!(registro.record_observation(&obs).expect("primera"));
        assert!(
            !registro.record_observation(&obs).expect("segunda"),
            "la misma version no debe sumar dos veces"
        );
        let stats = registro.search("NameError", 5).expect("busqueda");
        assert_eq!(stats[0].occurrences, 1);
    }

    #[test]
    fn tres_observaciones_de_dos_firmas_y_la_repetida_no_suma() {
        let mut reg = Registry::in_memory().unwrap();
        let primera = obs(
            FIRMA_NOMBRE,
            "DEMO_ciclo",
            Some(59),
            "2",
            "2026-11-17 12:49:22",
        );
        let segunda = obs(
            FIRMA_NOMBRE,
            "DEMO_ciclo",
            Some(59),
            "2",
            "2026-11-17 12:51:00",
        );
        let otra = obs(FIRMA_CLAVE, "reporteMercadolibre", Some(7), "3.1", T0);

        assert!(reg.record_observation(&primera).unwrap(), "primera vez");
        assert!(reg.record_observation(&segunda).unwrap(), "otro instante");
        assert!(reg.record_observation(&otra).unwrap(), "otra firma");

        assert!(
            !reg.record_observation(&segunda).unwrap(),
            "la observacion repetida devuelve false"
        );

        let stats = reg.search("name:{var}", 10).unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(
            stats[0].occurrences, 2,
            "el contador no sube con el duplicado"
        );
        assert_eq!(
            stats[0].first_seen,
            crate::db::parse_date("2026-11-17 12:49:22"),
            "first_seen conserva el primer avistamiento"
        );
        assert_eq!(
            stats[0].last_seen,
            crate::db::parse_date("2026-11-17 12:51:00")
        );
    }

    #[test]
    fn una_version_desconocida_tambien_es_idempotente() {
        let mut reg = Registry::in_memory().unwrap();
        let sin_version = obs("Error:Incorrect padding", "ruido", None, "-", T0);
        assert!(reg.record_observation(&sin_version).unwrap());
        assert!(!reg.record_observation(&sin_version).unwrap());
        assert_eq!(reg.search("", 10).unwrap()[0].occurrences, 1);
    }

    #[test]
    fn search_encuentra_por_trozo_de_firma_y_de_robot() {
        let mut reg = Registry::in_memory().unwrap();
        reg.record_observation(&obs(FIRMA_NOMBRE, "DEMO_ciclo", Some(59), "2", T0))
            .unwrap();
        reg.record_observation(&obs(FIRMA_CLAVE, "reporteMercadolibre", Some(7), "3.1", T0))
            .unwrap();
        reg.record_observation(&obs(FIRMA_CLAVE, "ventas_mensuales", Some(3), "1", T0))
            .unwrap();

        let por_firma = reg.search("nameerror", 10).unwrap();
        assert_eq!(por_firma.len(), 1);
        assert_eq!(por_firma[0].signature, FIRMA_NOMBRE);

        let por_robot = reg.search("MERCADO", 10).unwrap();
        assert_eq!(por_robot.len(), 1);
        assert_eq!(por_robot[0].signature, FIRMA_CLAVE);
        assert_eq!(
            por_robot[0].robots,
            vec!["reporteMercadolibre", "ventas_mensuales"],
            "todos los robots de la firma, en orden"
        );

        assert_eq!(reg.search("ventas", 10).unwrap().len(), 1);

        assert_eq!(reg.search("", 10).unwrap().len(), 2);
        assert_eq!(reg.search("   ", 10).unwrap().len(), 2);

        assert!(reg.search("no-existe", 10).unwrap().is_empty());
    }

    #[test]
    fn search_ordena_por_ocurrencias_y_respeta_el_limite() {
        let mut reg = Registry::in_memory().unwrap();
        reg.record_observation(&obs("firma_una", "r1", Some(1), "1", "2026-11-17 12:00:00"))
            .unwrap();
        for minuto in 1..=3 {
            let visto = format!("2026-11-17 12:0{minuto}:00");
            reg.record_observation(&obs("firma_tres", "r1", Some(1), "1", &visto))
                .unwrap();
        }

        let todas = reg.search("firma", 10).unwrap();
        assert_eq!(todas.len(), 2);
        assert_eq!(todas[0].signature, "firma_tres", "la mas vista va primero");
        assert_eq!(todas[0].occurrences, 3);
        assert_eq!(todas[1].signature, "firma_una");
        assert_eq!(todas[1].occurrences, 1);

        let tope = reg.search("firma", 1).unwrap();
        assert_eq!(tope.len(), 1);
        assert_eq!(tope[0].signature, "firma_tres");
        assert!(
            reg.search("firma", 0).unwrap().is_empty(),
            "el tope es literal"
        );
    }

    #[test]
    fn un_fix_cuenta_en_las_estadisticas_y_no_inventa_avistamientos() {
        let mut reg = Registry::in_memory().unwrap();
        reg.record_observation(&obs(FIRMA_NOMBRE, "DEMO_ciclo", Some(59), "2", T0))
            .unwrap();

        let id = reg
            .record_fix(&fix(
                FIRMA_NOMBRE,
                "DEMO_ciclo",
                Some(59),
                Some(60),
                "2026-11-17 13:00:00",
            ))
            .unwrap();
        assert!(id > 0, "devuelve el id del fix");

        let stats = reg.search("nameerror", 10).unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].fixes, 1);
        assert_eq!(stats[0].occurrences, 1);

        reg.record_fix(&fix(
            "TimeoutError:esperando_ventana",
            "otro_robot",
            None,
            Some(3),
            "2026-11-17 13:05:00",
        ))
        .unwrap();
        let huerfana = reg.search("timeout", 10).unwrap();
        assert_eq!(huerfana.len(), 1);
        assert_eq!(huerfana[0].occurrences, 0);
        assert_eq!(huerfana[0].fixes, 1);
        assert!(huerfana[0].robots.is_empty());

        assert_eq!(reg.counts().unwrap(), (2, 2));
    }

    #[test]
    fn counts_de_un_registro_vacio() {
        let reg = Registry::in_memory().unwrap();
        assert_eq!(reg.counts().unwrap(), (0, 0));
        assert!(reg.search("", 10).unwrap().is_empty());
    }

    #[test]
    fn un_query_con_comillas_o_drop_table_no_rompe_el_esquema() {
        let mut reg = Registry::in_memory().unwrap();
        let venenosa = "NameError: name 'x'; DROP TABLE observations; --";
        let robot = "robot'; DROP TABLE fixes; --";
        reg.record_observation(&obs(venenosa, robot, None, "1", T0))
            .unwrap();

        let hits = reg.search("'; DROP TABLE observations; --", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].signature, venenosa);
        assert_eq!(hits[0].robots, vec![robot]);

        assert_eq!(reg.counts().unwrap(), (1, 0));
        assert!(reg
            .record_observation(&obs(venenosa, "otro_robot", None, "1", T0))
            .unwrap());

        assert!(reg.search("%", 10).unwrap().is_empty());
        assert_eq!(reg.search("_", 10).unwrap().len(), 1);
    }

    #[test]
    fn open_crea_el_esquema_y_abrir_dos_veces_no_falla() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("datos-nuevos").join("problems.db");
        assert!(!path.exists());

        {
            let mut reg = Registry::open(&path).unwrap();
            reg.record_observation(&obs("firma", "r1", Some(1), "1", T0))
                .unwrap();
        }
        assert!(path.exists(), "el fichero se crea al abrir");

        let reg = Registry::open(&path).unwrap();
        assert_eq!(reg.counts().unwrap(), (1, 0));
        assert_eq!(reg.search("firma", 10).unwrap()[0].occurrences, 1);

        drop(reg);
        let mut reg = Registry::open(&path).unwrap();
        assert!(!reg
            .record_observation(&obs("firma", "r1", Some(1), "1", T0))
            .unwrap());
    }

    #[test]
    fn el_registro_aplica_las_claves_foraneas() {
        let reg = Registry::in_memory().unwrap();
        let activas: i64 = reg
            .conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(activas, 1, "las REFERENCES declaradas se cumplen");
    }

    #[test]
    fn default_path_termina_en_problems_db_y_respeta_el_override() {
        let base = PathBuf::from("C:\\datos");
        assert_eq!(resolve_path(None, base.clone()), base.join(FILE_NAME));
        assert_eq!(
            resolve_path(Some(OsString::new()), base.clone()),
            base.join(FILE_NAME),
            "una variable definida pero vacia no cuenta"
        );
        let override_dir = PathBuf::from("C:\\registro");
        assert_eq!(
            resolve_path(Some(override_dir.clone().into_os_string()), base),
            override_dir.join(FILE_NAME)
        );

        assert_eq!(
            default_path().file_name().and_then(|n| n.to_str()),
            Some(FILE_NAME),
            "sin entorno raro, el fichero se llama siempre igual"
        );
    }
}
