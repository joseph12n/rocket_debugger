//! Lectura de `robot.db`.
//!
//! La tabla `bots` es un **historial**: cada guardado en Studio inserta una fila
//! nueva en vez de actualizar. Por eso `ORDER BY id` da la evolucion del robot y
//! `created_at` da los intervalos temporales que usa la atribucion de errores.

use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::error::{Error, Result};

/// Formato de `bots.created_at` observado en el entorno real.
pub const DATE_FMT: &str = "%Y-%m-%d %H:%M:%S";

/// Base de datos por defecto del Studio local.
pub const DEFAULT_DB: &str = r"C:\Rocketbot\robot.db";

#[derive(Debug, Clone)]
pub struct RobotSummary {
    pub name: String,
    pub versions: usize,
    pub latest_id: i64,
}

/// Una fila de `bots`: un snapshot inmutable del robot.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub id: i64,
    pub name: String,
    pub created_at: NaiveDateTime,
    /// Base64 tal cual esta en la base de datos. Se conserva para medir el
    /// tamano crudo real sin volver a consultar.
    pub raw_b64: String,
    /// Proyecto ya decodificado (`project` o la raiz si no hay envoltura).
    pub project: Value,
}

/// `created_at` del ultimo snapshot del robot, si lo tiene.
///
/// Es la hora con la que se registra una observacion en el historial de problemas.
/// Si se usara el reloj del barrido, dos escaneos de la **misma** version contarian
/// como dos avistamientos y el contador dejaria de significar nada: la identidad de
/// una observacion incluye el instante, asi que ese instante tiene que ser estable.
pub fn latest_created_at(conn: &Connection, name: &str) -> Result<Option<NaiveDateTime>> {
    let mut stmt =
        conn.prepare("SELECT created_at FROM bots WHERE name = ?1 ORDER BY id DESC LIMIT 1")?;
    let mut rows = stmt.query([name])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let created: Option<String> = row.get(0).ok();
    Ok(created
        .as_deref()
        .map(parse_date)
        .filter(|d| *d != NaiveDateTime::default()))
}

/// `(id, created_at)` del ultimo snapshot del robot, sin decodificar el JSON.
///
/// El broker de ejecucion lo usa para decir contra que version se atribuyeron
/// los errores. Decodificar todas las versiones solo para leer la ultima
/// costaria lo mismo que construir el pack entero.
pub fn latest_snapshot(conn: &Connection, name: &str) -> Result<Option<(i64, NaiveDateTime)>> {
    let mut stmt =
        conn.prepare("SELECT id, created_at FROM bots WHERE name = ?1 ORDER BY id DESC LIMIT 1")?;
    let mut rows = stmt.query([name])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let id: i64 = row.get(0)?;
    let created: Option<String> = row.get(1).ok();
    Ok(Some((
        id,
        created.as_deref().map(parse_date).unwrap_or_default(),
    )))
}

/// Abre en solo lectura. La herramienta nunca escribe `robot.db`.
pub fn open_readonly(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|source| {
        Error::Open {
            path: path.to_path_buf(),
            source,
        }
    })
}

/// Tolerante: si el formato cambia, se degrada a epoch en vez de fallar.
pub fn parse_date(s: &str) -> NaiveDateTime {
    let t = s.trim();
    NaiveDateTime::parse_from_str(t, DATE_FMT)
        .or_else(|_| NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S%.f"))
        .or_else(|_| NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S"))
        .unwrap_or_default()
}

pub fn list_robots(conn: &Connection) -> Result<Vec<RobotSummary>> {
    let mut stmt = conn.prepare(
        "SELECT name, COUNT(*) AS n, MAX(id) AS latest FROM bots GROUP BY name ORDER BY name",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(RobotSummary {
            name: r.get(0)?,
            versions: r.get::<_, i64>(1)? as usize,
            latest_id: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Todos los snapshots de un robot, en orden cronologico de guardado.
pub fn load_snapshots(conn: &Connection, name: &str) -> Result<Vec<Snapshot>> {
    let mut stmt =
        conn.prepare("SELECT id, name, created_at, data FROM bots WHERE name = ?1 ORDER BY id")?;
    let mut rows = stmt.query([name])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let name: String = row.get(1)?;
        let created: Option<String> = row.get(2).ok();
        // `data` es TEXT (base64) en las bases reales, pero un export puede
        // dejarlo como BLOB. Se acepta cualquiera de las dos formas.
        let bytes: Vec<u8> = match row.get_ref(3)? {
            rusqlite::types::ValueRef::Null => Vec::new(),
            rusqlite::types::ValueRef::Text(t) => t.to_vec(),
            rusqlite::types::ValueRef::Blob(b) => b.to_vec(),
            _ => Vec::new(),
        };
        out.push(Snapshot {
            id,
            name,
            created_at: created.as_deref().map(parse_date).unwrap_or_default(),
            project: decode_project(id, &bytes)?,
            raw_b64: String::from_utf8_lossy(&bytes).into_owned(),
        });
    }
    if out.is_empty() {
        return Err(Error::RobotNotFound(name.to_string()));
    }
    Ok(out)
}

pub fn load_snapshot(conn: &Connection, name: &str, id: i64) -> Result<Snapshot> {
    load_snapshots(conn, name)?
        .into_iter()
        .find(|s| s.id == id)
        .ok_or(Error::SnapshotNotFound(id))
}

/// `bots.data` es base64 de un JSON. Se acepta tambien JSON plano porque el
/// formato no esta documentado y no conviene romperse por una variante.
fn decode_project(id: i64, bytes: &[u8]) -> Result<Value> {
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Value::Null);
    }
    if trimmed.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            return Ok(v);
        }
    }
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .map_err(|e| Error::Decode {
            id,
            reason: e.to_string(),
        })?;
    serde_json::from_slice::<Value>(&decoded).map_err(|source| Error::Json { id, source })
}

/// El proyecto vive bajo la clave `project`; si algun dia no esta, la raiz sirve.
pub fn project_root(json: &Value) -> &Value {
    json.get("project").unwrap_or(json)
}

/// Descubre bases de datos validas bajo una raiz.
///
/// Se excluyen `backups/` (copias de trabajo que duplicarian cada robot) y las
/// bases auxiliares de Studio que comparten nombre con `robot.db`.
pub fn discover_databases(root: &Path) -> Result<Vec<PathBuf>> {
    const SKIP_FILES: [&str; 3] = ["analytics.db", "files.db", "nodb.db"];
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if name.eq_ignore_ascii_case("backup") || name.eq_ignore_ascii_case("backups") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            let fname = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !fname.ends_with(".db") || SKIP_FILES.contains(&fname.as_str()) {
                continue;
            }
            if has_bots_table(&path) {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Etiqueta de una db sin ruta absoluta: `BBRE2\robot_run.db`.
///
/// El pack y los informes se comparten (agente, incidencia, UI), asi que no
/// llevan el arbol de directorios del desarrollador. Se normalizan los dos
/// separadores a proposito: `Path` solo reconoce `\` en Windows y la etiqueta no
/// puede depender del host que la construye.
pub fn label(path: &Path) -> String {
    let normalizado = path.to_string_lossy().replace('\\', "/");
    let mut partes: Vec<&str> = normalizado.split('/').filter(|p| !p.is_empty()).collect();

    let Some(file) = partes.pop() else {
        return "robot.db".to_string();
    };
    match partes.pop() {
        Some(dir) => format!("{dir}\\{file}"),
        None => file.to_string(),
    }
}

fn has_bots_table(path: &Path) -> bool {
    let Ok(conn) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return false;
    };
    let cols: rusqlite::Result<Vec<String>> = (|| {
        let mut stmt = conn.prepare("PRAGMA table_info(bots)")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        rows.collect()
    })();
    matches!(cols, Ok(c) if c.iter().any(|x| x == "name") && c.iter().any(|x| x == "data"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsea_fechas_del_formato_real() {
        let d = parse_date("2026-11-17 12:59:37");
        assert_eq!(d.to_string(), "2026-11-17 12:59:37");
    }

    #[test]
    fn fecha_rota_no_panica() {
        assert_eq!(parse_date("no-es-fecha").to_string(), "1970-01-01 00:00:00");
    }

    #[test]
    fn project_root_acepta_ambas_formas() {
        let con = serde_json::json!({"project": {"commands": []}});
        assert!(project_root(&con).get("commands").is_some());
        let sin = serde_json::json!({"commands": []});
        assert!(project_root(&sin).get("commands").is_some());
    }
}
