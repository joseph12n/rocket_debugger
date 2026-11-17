//! Utilidades compartidas por los tests de integracion.
//!
//! Ningun test toca `C:\Rocketbot`: las dbs son temporales y el entorno que se
//! pasa a la fachada es [`rocketdebug_mcp::Settings::hermetic`].
//!
//! `#![allow(dead_code)]`: cada binario de test usa un subconjunto de estas
//! funciones, y sin esto `clippy -D warnings` se queja de las que le sobran.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;

/// Sufijo unico por directorio: dos tests en paralelo no pueden compartir ruta.
static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Directorio temporal que se borra solo.
///
/// No se usa `tempfile` a proposito: no hace falta una dependencia mas para
/// `create_dir_all` + `remove_dir_all`, y el workspace ya tiene las justas.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rocketdebug-mcp-{tag}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("creo el directorio temporal");
        Self { path }
    }

    pub fn path(&self) -> &Path {
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

/// Crea un `robot.db` con la tabla `bots` que lee el motor.
///
/// Es el mismo patron que `crates/core/tests/pack.rs`: `id`, `name`,
/// `created_at` y `data` (base64 del JSON del proyecto).
pub fn make_db(dir: &Path, robot: &str, rows: &[(i64, &str, &str)]) -> PathBuf {
    let path = dir.join("robot.db");
    let conn = rusqlite::Connection::open(&path).expect("creo la db temporal");
    conn.execute_batch(
        "CREATE TABLE bots (
            id INTEGER PRIMARY KEY,
            name TEXT,
            created_at TEXT,
            data TEXT
        );",
    )
    .expect("creo la tabla bots");

    for (id, created_at, json) in rows {
        let b64 = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
        conn.execute(
            "INSERT INTO bots (id, name, created_at, data) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, robot, created_at, b64],
        )
        .expect("inserto el snapshot");
    }

    // La conexion se cierra aqui: el `TempDir` se borra despues que este valor.
    drop(conn);
    path
}

/// Proyecto minimo: dos comandos y una variable, suficiente para que el pack
/// tenga cabecera, flujo y variables.
pub fn demo_project() -> String {
    serde_json::json!({
        "project": {
            "commands": [
                {
                    "father": "log_message",
                    "command": "INICIO demo",
                    "group": "system",
                    "id": "11111111-1111-1111-1111-111111111111",
                    "line": 1,
                    "option": "INFO",
                    "children": [],
                    "else": [],
                },
                {
                    "father": "setVar",
                    "command": "\"hola\"",
                    "group": "system",
                    "id": "22222222-2222-2222-2222-222222222222",
                    "line": 2,
                    "var": "saludo",
                    "children": [],
                    "else": [],
                },
            ],
            "vars": [{"name": "saludo", "data": null, "type": "string"}],
            "profile": {"name": "demo", "description": "Robot de prueba del MCP"},
            "ifs": [],
            "modules": [],
            "expose": {},
        },
    })
    .to_string()
}

/// Linea JSON-RPC de peticion.
pub fn request(id: i64, method: &str, params: serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
    .to_string()
}

/// Linea JSON-RPC de `tools/call`.
pub fn call_request(id: i64, tool: &str, arguments: serde_json::Value) -> String {
    request(
        id,
        "tools/call",
        serde_json::json!({"name": tool, "arguments": arguments}),
    )
}

pub fn parse_response(response: &str) -> serde_json::Value {
    serde_json::from_str(response)
        .unwrap_or_else(|source| panic!("la respuesta no es JSON ({source}): {response}"))
}

/// Texto de `result.content[0].text`, el unico contenido que devuelve la fachada.
pub fn tool_text(response: &serde_json::Value) -> &str {
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("la respuesta no trae content[0].text: {response}"))
}

/// `true` si la tool declaro un fallo de negocio.
pub fn is_error(response: &serde_json::Value) -> bool {
    response["result"]["isError"]
        .as_bool()
        .unwrap_or_else(|| panic!("la respuesta no trae result.isError: {response}"))
}
