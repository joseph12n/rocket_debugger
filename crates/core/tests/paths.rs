use std::path::{Path, PathBuf};

use base64::Engine as _;
use rocketdebug_core::db;
use rocketdebug_core::pack::build_from_conn_with_logs_and_paths;
use rocketdebug_core::paths::{LocalPaths, NoPaths};
use serde_json::{json, Value};

fn make_db(dir: &Path, robot: &str, project: &str) -> PathBuf {
    let path = dir.join("robot.db");
    let conn = rusqlite::Connection::open(&path).expect("creo la db");
    conn.execute_batch(
        "CREATE TABLE bots (
            id INTEGER PRIMARY KEY,
            name TEXT,
            created_at TEXT,
            data TEXT
        );",
    )
    .expect("creo la tabla");
    let b64 = base64::engine::general_purpose::STANDARD.encode(project.as_bytes());
    conn.execute(
        "INSERT INTO bots (id, name, created_at, data) VALUES (1, ?1, '2026-11-17 10:00:00', ?2)",
        rusqlite::params![robot, b64],
    )
    .expect("inserto el snapshot");
    drop(conn);
    path
}

fn project(remote_base: &str, existing: &str) -> String {
    json!({
        "project": {
            "commands": [
                {
                    "father": "log_message", "command": "INICIO", "group": "system",
                    "id": "n1", "line": 1, "children": [], "else": []
                },
                {
                    "father": "setVar", "command": "{ruta_salida}", "group": "system",
                    "id": "n2", "line": 2, "var": "salida", "children": [], "else": []
                },
                {
                    "father": "setVar", "command": "{carpeta_salida}", "group": "system",
                    "id": "n3", "line": 3, "var": "carpeta", "children": [], "else": []
                },
                {
                    "father": "savexlsx", "command": format!("{remote_base}/ENTREGA_BBRE2/otro.xlsx"),
                    "group": "files", "id": "n4", "line": 4, "children": [], "else": []
                },
                {
                    "father": "log_message", "command": "C:/Rocketbot", "group": "system",
                    "id": "n5", "line": 5, "children": [], "else": []
                },
                {
                    "father": "log_message", "command": "https://ejemplo.com/C:/no-es-ruta",
                    "group": "system", "id": "n6", "line": 6, "children": [], "else": []
                },
                {
                    "father": "setVar", "command": "{base_path}/x.xlsx", "group": "system",
                    "id": "n7", "line": 7, "var": "dinamica", "children": [], "else": []
                }
            ],
            "vars": [
                {"name": "ruta_salida", "data": format!("{remote_base}/ENTREGA_BBRE2/mercadolibre.xlsx")},
                {"name": "carpeta_salida", "data": format!("{remote_base}/ENTREGA_BBRE2")},
                {"name": "rb_home", "data": "C:/Rocketbot"},
                {"name": "programa", "data": "C:/Program Files/MariaDB/bin/mariadb-dump.exe"},
                {"name": "existente", "data": existing},
                {"name": "dinamica", "data": "{base_path}/x.xlsx"}
            ],
            "profile": {"name": "demo", "description": "rutas migradas"},
            "ifs": [],
            "modules": [],
            "expose": {}
        }
    })
    .to_string()
}

fn rulas(pack: &rocketdebug_core::Pack) -> Vec<&rocketdebug_core::rules::Finding> {
    pack.analysis
        .findings
        .iter()
        .filter(|f| f.rule_id == "RUTA_MIGRADA")
        .collect()
}

#[test]
fn agrupa_las_rutas_de_la_misma_carpeta_y_propone_el_remapeo() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote").join("Downloads");
    let local = tmp
        .path()
        .join("local")
        .join("Downloads")
        .join("ENTREGA_BBRE2");
    let existing = tmp.path().join("local").join("archivo.xlsx");
    std::fs::create_dir_all(&remote).expect("remoto");
    std::fs::create_dir_all(&local).expect("local");
    std::fs::write(&existing, "x").expect("existente");

    let db = make_db(
        tmp.path(),
        "demo",
        &project(&remote.to_string_lossy(), &existing.to_string_lossy()),
    );
    let conn = db::open_readonly(&db).expect("abro");
    let probe = LocalPaths::with_roots(vec![tmp.path().join("local")]);
    let pack =
        build_from_conn_with_logs_and_paths(&conn, "demo", None, "prueba\\robot.db", None, &probe)
            .expect("pack");

    let mut findings = rulas(&pack);
    assert_eq!(findings.len(), 1, "{findings:?}");
    let finding = findings.remove(0);
    assert_eq!(
        finding.line, "2",
        "la linea es la del primer uso de la variable"
    );
    assert!(finding.message.contains("ENTREGA_BBRE2"), "{finding:?}");
    assert!(finding.message.contains("ruta_salida"), "{finding:?}");
    assert!(finding.message.contains("carpeta_salida"), "{finding:?}");
    assert!(
        finding.message.contains("3 rutas"),
        "el mensaje dice cuantas rutas cubre: {finding:?}"
    );
    assert!(
        finding.fix.contains("candidato") || finding.fix.contains("remapea"),
        "{finding:?}"
    );
    assert!(finding.fix.contains("ENTREGA_BBRE2"), "{finding:?}");
    assert!(
        !finding.message.contains("C:/Rocketbot")
            && !finding.message.contains("Program Files")
            && !finding.message.contains("ejemplo.com")
            && !finding.message.contains("{base_path}"),
        "no se marcan rutas de sistema, URLs ni plantillas: {finding:?}"
    );

    let md = pack.render_markdown();
    assert!(md.contains("RUTA_MIGRADA"), "{md}");
    assert!(md.contains("ENTREGA_BBRE2"), "{md}");
}

#[test]
fn sin_equivalente_local_lo_dice_en_el_fix() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote").join("Downloads");
    let existing = tmp.path().join("archivo.xlsx");
    std::fs::create_dir_all(&remote).expect("remoto");
    std::fs::write(&existing, "x").expect("existente");

    let db = make_db(
        tmp.path(),
        "demo",
        &project(&remote.to_string_lossy(), &existing.to_string_lossy()),
    );
    let conn = db::open_readonly(&db).expect("abro");
    let probe = LocalPaths::with_roots(vec![tmp.path().join("local-vacio")]);
    let pack =
        build_from_conn_with_logs_and_paths(&conn, "demo", None, "prueba\\robot.db", None, &probe)
            .expect("pack");

    let findings = rulas(&pack);
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(
        findings[0].fix.contains("sin equivalente local"),
        "{}",
        findings[0].fix
    );
}

#[test]
fn con_nopaths_la_regla_no_aparece() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote");
    std::fs::create_dir_all(&remote).expect("remoto");
    let db = make_db(
        tmp.path(),
        "demo",
        &project(&remote.to_string_lossy(), &remote.to_string_lossy()),
    );
    let conn = db::open_readonly(&db).expect("abro");
    let pack = build_from_conn_with_logs_and_paths(
        &conn,
        "demo",
        None,
        "prueba\\robot.db",
        None,
        &NoPaths,
    )
    .expect("pack");
    assert!(
        rulas(&pack).is_empty(),
        "sin probe la regla no se evalua y el golden no cambia"
    );
}

#[test]
fn una_ruta_que_existe_no_se_marca() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let existing = tmp.path().join("ruta_valida.xlsx");
    std::fs::write(&existing, "x").expect("existente");
    let project: Value = serde_json::from_str(&project(
        &tmp.path().join("remote").to_string_lossy(),
        &existing.to_string_lossy(),
    ))
    .expect("json");
    let mut project = project;
    for var in project["project"]["vars"].as_array_mut().expect("vars") {
        let name = var["name"].as_str().unwrap_or("");
        if name != "existente" {
            var["data"] = Value::Null;
        }
    }
    project["project"]["commands"][3]["command"] = json!("INICIO");
    let db = make_db(tmp.path(), "demo", &project.to_string());
    let conn = db::open_readonly(&db).expect("abro");
    let probe = LocalPaths::with_roots(vec![tmp.path().to_path_buf()]);
    let pack =
        build_from_conn_with_logs_and_paths(&conn, "demo", None, "prueba\\robot.db", None, &probe)
            .expect("pack");
    assert!(
        rulas(&pack).is_empty(),
        "una ruta existente no es un defecto: {:?}",
        rulas(&pack)
    );
}

#[test]
#[ignore = "requiere C:\\rocketbot_bots\\BBRE2\\robot_run.db"]
fn bbre2_detecta_entrega_bbre2_y_no_marca_rutas_validas() {
    let db_path = std::env::var("ROCKETDEBUG_REAL_DB")
        .unwrap_or_else(|_| r"C:\rocketbot_bots\BBRE2\robot_run.db".to_string());
    assert!(
        Path::new(&db_path).is_file(),
        "no encuentro {db_path}: define ROCKETDEBUG_REAL_DB"
    );

    let conn = db::open_readonly(Path::new(&db_path)).expect("abro BBRE2");
    let probe = LocalPaths::for_db(Path::new(&db_path));
    let pack = build_from_conn_with_logs_and_paths(
        &conn,
        "reporteMercadolibre",
        None,
        "BBRE2\\robot_run.db",
        None,
        &probe,
    )
    .expect("pack de reporteMercadolibre");

    let findings = rulas(&pack);
    assert!(
        !findings.is_empty(),
        "no se detecto ninguna ruta migrada en reporteMercadolibre"
    );
    for finding in &findings {
        assert!(
            finding.message.contains("ENTREGA_BBRE2"),
            "se marco una ruta que no es la migrada conocida: {finding:?}"
        );
        assert!(
            !finding.message.contains("C:/Rocketbot") && !finding.message.contains("Program Files"),
            "una ruta de sistema no se marca: {finding:?}"
        );
    }
    assert!(
        findings.iter().any(|finding| !finding.line.is_empty()),
        "la variable la usa un subrobot: la linea es la de la llamada en el padre: {findings:?}"
    );
    let md = pack.render_markdown();
    assert!(md.contains("RUTA_MIGRADA"), "{md}");
    assert!(md.contains("ENTREGA_BBRE2"), "{md}");
}
