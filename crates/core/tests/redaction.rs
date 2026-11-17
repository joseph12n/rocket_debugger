//! Redaccion de secretos de punta a punta: `project.vars` -> pack -> render.
//!
//! Como en `pack.rs`, ningun test toca `C:\Rocketbot`: la db se construye en un
//! directorio temporal con el mismo esquema que usa Studio.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use rocketdebug_core::{build_from_conn, db, Pack};

/// Token con la forma exacta que Rocketbot deja en `project.vars` al cifrar una
/// credencial (Fernet, base64 URL-safe, prefijo `gAAAAA`).
const CIFRADO: &str = "gAAAAABq7LyRAghP1xNXRUIBkRfDi6ioMMqj_-oPnZ5xvkmSw_Jb1B41VjB_\
                      0Yq-KLSS9s_mV8pv3cJ8R2Q==";

/// Valor visible de una variable con nombre sensible: no debe aparecer en el render.
const PASSWORD: &str = "S3cr3t0Visible42";

fn make_db(dir: &Path, json: &str) -> PathBuf {
    let path = dir.join("robot.db");
    let conn = rusqlite::Connection::open(&path).expect("abro db temporal");
    conn.execute_batch(
        "CREATE TABLE bots (
            id INTEGER PRIMARY KEY,
            name TEXT,
            created_at TEXT,
            data TEXT
        );",
    )
    .expect("creo tabla");
    let b64 = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
    conn.execute(
        "INSERT INTO bots (id, name, created_at, data) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![1i64, "credenciales", "2026-11-17 10:00:00", b64],
    )
    .expect("inserto snapshot");
    path
}

/// Envuelve las variables en un `project` minimo pero con la forma real.
fn robot_con_vars(vars: serde_json::Value) -> String {
    serde_json::json!({
        "project": {
            "profile": {"name": "credenciales", "description": "Fixture de redaccion"},
            "commands": [{
                "father": "log_message",
                "command": "INICIO credenciales",
                "group": "system",
                "id": "11111111-1111-1111-1111-111111111111",
                "line": 1,
                "option": "INFO",
                "children": [],
                "else": []
            }],
            "vars": vars
        }
    })
    .to_string()
}

fn pack_de(dir: &Path, json: &str) -> Pack {
    let path = make_db(dir, json);
    let conn = db::open_readonly(&path).expect("abro la db temporal");
    build_from_conn(&conn, "credenciales", None, "test\\robot.db").expect("construyo pack")
}

#[test]
fn el_render_no_filtra_secretos_y_los_marca() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let json = robot_con_vars(serde_json::json!([
        {"name": "destinatario", "data": CIFRADO},
        {"name": "db_password", "data": PASSWORD},
        {"name": "url", "data": "https://ejemplo.test/api"}
    ]));
    let pack = pack_de(tmp.path(), &json);
    assert_eq!(pack.vars.len(), 3);

    let md = pack.render_markdown();
    assert!(
        !md.contains("gAAAAA"),
        "el blob cifrado no debe llegar al render:\n{md}"
    );
    assert!(
        !md.contains(PASSWORD),
        "el valor del password no debe llegar al render:\n{md}"
    );
    assert!(
        md.contains("(valor cifrado)"),
        "el render debe decir que el valor esta cifrado:\n{md}"
    );
    assert!(
        md.contains("(oculto)"),
        "el render debe ocultar el valor sensible:\n{md}"
    );

    // Redactar no puede cegar el diagnostico: nombres y valores normales siguen.
    assert!(md.contains("- destinatario = (valor cifrado)"), "{md}");
    assert!(md.contains("- db_password = (oculto)"), "{md}");
    assert!(md.contains("https://ejemplo.test/api"), "{md}");
}

#[test]
fn las_variables_de_flujo_no_se_redactan_por_nombre() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let json = robot_con_vars(serde_json::json!([
        {"name": "usuario", "data": "operador1"},
        {"name": "contador", "data": "7"},
        {"name": "url", "data": "https://ejemplo.test/api"}
    ]));
    let pack = pack_de(tmp.path(), &json);
    let md = pack.render_markdown();

    // `user` no esta en la lista a proposito: sin el, el agente no sabe a quien
    // apunta la peticion que esta depurando.
    assert_eq!(pack.vars[0].value.as_deref(), Some("operador1"));
    assert!(md.contains("operador1"), "{md}");
    assert!(md.contains("- contador = 7"), "{md}");
    assert!(!md.contains("(oculto)"), "{md}");
}
