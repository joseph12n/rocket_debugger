use std::path::{Path, PathBuf};

use base64::Engine as _;
use rocketdebug_core::{build_from_conn, db, Pack};

const CIFRADO: &str = "gAAAAABq7LyRAghP1xNXRUIBkRfDi6ioMMqj_-oPnZ5xvkmSw_Jb1B41VjB_\
                      0Yq-KLSS9s_mV8pv3cJ8R2Q==";

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

fn robot_con_vars(vars: serde_json::Value) -> String {
    robot_con("Fixture de redaccion", vars)
}

fn robot_con(descripcion: &str, vars: serde_json::Value) -> String {
    serde_json::json!({
        "project": {
            "profile": {"name": "credenciales", "description": descripcion},
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

fn robot_con_ruta_en_params() -> String {
    serde_json::json!({
        "project": {
            "profile": {"name": "rutas", "description": "Fixture de rutas en parametros"},
            "commands": [
                {
                    "father": "setVar",
                    "command": r#""C:/Users/josep/OneDrive/Documentos/mercadolibre_" + datetime.now().strftime("%Y%m%d")"#,
                    "group": "system",
                    "id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                    "line": 1,
                    "var": "ruta_salida",
                    "children": [],
                    "else": []
                },
                {
                    "father": "log_message",
                    "command": r"C:\Users\josep\Downloads\reportes\resumen.xlsx",
                    "group": "system",
                    "id": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                    "line": 2,
                    "option": "INFO",
                    "children": [],
                    "else": []
                }
            ],
            "vars": [{"name": "ruta_salida", "data": null}]
        }
    })
    .to_string()
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

    assert_eq!(pack.vars[0].value.as_deref(), Some("operador1"));
    assert!(md.contains("operador1"), "{md}");
    assert!(md.contains("- contador = 7"), "{md}");
    assert!(!md.contains("(oculto)"), "{md}");
}

#[test]
fn el_render_no_filtra_rutas_absolutas_del_usuario() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let json = robot_con_vars(serde_json::json!([
        {"name": "chrome_profile", "data": "C:/Users/josep/Downloads/perfil_chrome"},
        {"name": "carpeta_salida", "data": r"C:\Users\josep\Downloads\reportes"},
        {"name": "reporte", "data": "reporte_ml.xlsx"},
        {"name": "url", "data": "https://ejemplo.test/api"}
    ]));
    let pack = pack_de(tmp.path(), &json);
    let md = pack.render_markdown();

    assert!(
        !md.contains("C:/Users"),
        "ruta con barra normal filtrada:\n{md}"
    );
    assert!(
        !md.contains(r"C:\Users"),
        "ruta con barra invertida filtrada:\n{md}"
    );
    assert!(
        !md.contains("josep"),
        "el nombre del usuario no debe salir:\n{md}"
    );

    assert!(md.contains("(ruta)"), "falta el marcador de ruta:\n{md}");
    assert!(md.contains("- chrome_profile = (ruta)"), "{md}");
    assert!(md.contains("- carpeta_salida = (ruta)"), "{md}");
    assert!(md.contains("reporte_ml.xlsx"), "{md}");
    assert!(md.contains("https://ejemplo.test/api"), "{md}");
    assert_eq!(pack.vars[0].value.as_deref(), Some("(ruta)"));
}

#[test]
fn el_pack_no_filtra_el_usuario_de_los_parametros() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let pack = pack_de(tmp.path(), &robot_con_ruta_en_params());

    assert!(
        pack.flow[0].params.contains("josep"),
        "el pack no deberia destruir el parametro: {}",
        pack.flow[0].params
    );

    let md = pack.render_markdown();
    assert!(!md.contains("josep"), "el usuario se escapo:\n{md}");
    assert!(!md.contains("C:/Users"), "ruta Windows filtrada:\n{md}");
    assert!(!md.contains(r"C:\Users"), "ruta Windows filtrada:\n{md}");

    assert!(
        md.contains("%USERPROFILE%/OneDrive/Documentos/mercadolibre_"),
        "falta la ruta enmascarada del parametro:\n{md}"
    );
    assert!(
        md.contains(r"%USERPROFILE%\Downloads\reportes\resumen.xlsx"),
        "falta la ruta enmascarada del comando:\n{md}"
    );
    assert!(
        md.contains("RUTA_ABSOLUTA"),
        "el hallazgo que cita la ruta debe seguir ahi:\n{md}"
    );
}

#[test]
fn la_descripcion_del_perfil_no_publica_el_usuario() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let json = robot_con(
        "Reporte diario: exporta a C:/Users/josep/OneDrive/Documentos y avisa por correo",
        serde_json::json!([]),
    );
    let pack = pack_de(tmp.path(), &json);

    assert!(
        pack.description
            .as_deref()
            .unwrap_or_default()
            .contains("josep"),
        "el pack no deberia destruir la descripcion: {:?}",
        pack.description
    );

    let md = pack.render_markdown();
    assert!(
        !md.contains("josep"),
        "el usuario se escapo por la descripcion:\n{md}"
    );
    assert!(
        !md.contains("C:/Users"),
        "la ruta de la descripcion se publico entera:\n{md}"
    );
    assert!(
        md.contains(
            "> Reporte diario: exporta a %USERPROFILE%/OneDrive/Documentos y avisa por correo"
        ),
        "falta la descripcion enmascarada en la cabecera:\n{md}"
    );
}
