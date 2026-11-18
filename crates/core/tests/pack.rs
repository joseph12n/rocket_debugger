//! Tests de integracion del motor contra fixtures sinteticos.
//!
//! Ningun test de este fichero toca `C:\Rocketbot`. Los datos reales se prueban
//! aparte en `real_data`, marcado `#[ignore]`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use rocketdebug_core::{build_from_conn, build_from_conn_with_logs, db, Analysis};
use serde_json::{json, Value};

fn fixture(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/robots")
        .join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("no pude leer {}: {e}", p.display()))
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/golden")
        .join(name)
}

/// Crea un `robot.db` minimo con las filas indicadas `(id, created_at, json)`.
fn make_db(dir: &Path, robot: &str, rows: &[(i64, &str, &str)]) -> PathBuf {
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
    for (id, created, json) in rows {
        let b64 = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
        conn.execute(
            "INSERT INTO bots (id, name, created_at, data) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, robot, created, b64],
        )
        .expect("inserto snapshot");
    }
    path
}

#[test]
fn render_estable_del_fixture_simple() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let json = fixture("simple.json");
    let path = make_db(
        tmp.path(),
        "simple",
        &[(1, "2026-11-17 10:00:00", json.as_str())],
    );
    let conn = db::open_readonly(&path).expect("abro");
    let pack = build_from_conn(&conn, "simple", None, "test\\robot.db").expect("construyo pack");
    let actual = pack.render_markdown();

    let gp = golden_path("simple.md");
    if std::env::var("BLESS").is_ok() {
        std::fs::create_dir_all(gp.parent().unwrap()).ok();
        std::fs::write(&gp, &actual).expect("escribo golden");
    }
    let expected = std::fs::read_to_string(&gp)
        .unwrap_or_else(|e| panic!("falta el golden {}: {e}. Corre con BLESS=1", gp.display()));
    assert_eq!(actual, expected, "el render cambio respecto al golden");
}

#[test]
fn detecta_colision_de_lineas_del_fixture_real() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let json = fixture("line_collision.json");
    let path = make_db(
        tmp.path(),
        "line_collision",
        &[(1, "2026-11-17 10:00:00", json.as_str())],
    );
    let conn = db::open_readonly(&path).expect("abro");
    let pack = build_from_conn(&conn, "line_collision", None, "test\\robot.db").expect("pack");

    // El fixture reproduce el defecto observado en ML_06_Legacy: el try y el catch
    // comparten la numeracion 2.1 y 2.2.
    assert_eq!(pack.collisions.get("2.1"), Some(&2));
    assert_eq!(pack.collisions.get("2.2"), Some(&2));
    let md = pack.render_markdown();
    assert!(
        md.contains("direccionamiento por linea NO FIABLE"),
        "el render debe avisar de la colision:\n{md}"
    );
}

#[test]
fn el_indice_de_versiones_atribuye_al_snapshot_vivo() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let simple = fixture("simple.json");
    let orch = fixture("orchestrator.json");
    let path = make_db(
        tmp.path(),
        "demo",
        &[
            (10, "2026-11-17 12:47:10", simple.as_str()),
            (11, "2026-11-17 12:52:29", orch.as_str()),
        ],
    );
    let conn = db::open_readonly(&path).expect("abro");
    let pack = build_from_conn(&conn, "demo", None, "test\\robot.db").expect("pack");

    assert_eq!(pack.versions.len(), 2);
    assert_eq!(pack.snapshot_id, 11, "por defecto se usa el ultimo");

    let t = db::parse_date("2026-11-17 12:49:22");
    assert_eq!(
        pack.versions.live_at(t).map(|v| v.id),
        Some(10),
        "a las 12:49:22 estaba vivo el snapshot 10"
    );

    // El indice debe reflejar el contenido de cada version, no del actual:
    // el hash las distingue aunque ambas tengan el mismo numero de nodos.
    assert_ne!(
        pack.versions.versions[0].hash, pack.versions.versions[1].hash,
        "cada version debe tener su propio hash de contenido"
    );
    assert!(pack.versions.versions.iter().all(|v| v.nodes > 0));
}

#[test]
fn selecciona_un_snapshot_concreto() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let simple = fixture("simple.json");
    let orch = fixture("orchestrator.json");
    let path = make_db(
        tmp.path(),
        "demo",
        &[
            (10, "2026-11-17 12:47:10", simple.as_str()),
            (11, "2026-11-17 12:52:29", orch.as_str()),
        ],
    );
    let conn = db::open_readonly(&path).expect("abro");
    let pack = build_from_conn(&conn, "demo", Some(10), "test\\robot.db").expect("pack");
    assert_eq!(pack.snapshot_id, 10);
    assert_eq!(pack.node_count(), 3);
    assert_eq!(pack.vars.len(), 1);
}

#[test]
fn robot_inexistente_da_error_claro() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let simple = fixture("simple.json");
    let path = make_db(
        tmp.path(),
        "demo",
        &[(1, "2026-11-17 10:00:00", simple.as_str())],
    );
    let conn = db::open_readonly(&path).expect("abro");
    let err = build_from_conn(&conn, "no_existe", None, "test\\robot.db").unwrap_err();
    assert!(err.to_string().contains("no_existe"), "error: {err}");
}

/// Proyecto minimo de una version del robot de prueba.
fn proyecto(commands: Vec<Value>, vars: &[&str]) -> String {
    let vars: Vec<Value> = vars
        .iter()
        .map(|name| json!({"name": name, "data": null}))
        .collect();
    json!({
        "project": {
            "commands": commands,
            "vars": vars,
            "profile": {"name": "demo"},
        }
    })
    .to_string()
}

fn nodo(father: &str, line: &str, command: &str) -> Value {
    json!({
        "father": father,
        "line": line,
        "command": command,
        "children": [],
        "else": [],
    })
}

/// Primera version del robot de prueba: **3 nodos** y dos hallazgos propios.
///
/// La ruta absoluta de la linea 2 da `RUTA_ABSOLUTA` (accionable) y `vieja` es una
/// variable declarada que no se lee en ninguna linea (`VAR_NO_USADA`).
fn version_1() -> String {
    proyecto(
        vec![
            json!({
                "father": "setVar",
                "line": "1",
                "command": "\"cafe\"",
                "var": "busqueda",
                "children": [],
                "else": [],
            }),
            nodo("setcell", "2", r"C:\datos\salida.xlsx"),
            nodo("savexlsx", "3", "{ruta_salida}"),
        ],
        &["busqueda", "ruta_salida", "vieja"],
    )
}

/// Segunda version: **4 nodos** y otro juego de hallazgos.
///
/// Ya no hay ruta absoluta ni variable muerta, y aparece un `evaluateIf` sin else
/// en la linea 4 (`IF_SIN_ELSE`). La linea 4 no existe en la v1: un hallazgo suyo
/// dentro de un pack de la v1 delata que el analisis miro otra version.
fn version_2() -> String {
    proyecto(
        vec![
            json!({
                "father": "setVar",
                "line": "1",
                "command": "\"cafe\"",
                "var": "busqueda",
                "children": [],
                "else": [],
            }),
            nodo("log_message", "2", "buscando {usada}"),
            nodo("savexlsx", "3", "{ruta_salida}"),
            nodo("evaluateIf", "4", r#""{busqueda}" == """#),
        ],
        &["busqueda", "ruta_salida", "usada"],
    )
}

/// Firmas de los hallazgos (regla + linea), accionables e higiene, en el orden del
/// analisis: distingue dos versiones sin comparar los mensajes, que llevan conteos
/// y rutas.
fn reglas(analysis: &Analysis) -> Vec<(&str, &str)> {
    analysis
        .findings
        .iter()
        .chain(&analysis.higiene)
        .map(|f| (f.rule_id.as_str(), f.line.as_str()))
        .collect()
}

/// Db con dos versiones del mismo robot: la v1 sin ruta portable y con una
/// variable muerta, la v2 con el flujo mas largo y un IF sin else.
fn db_dos_versiones(dir: &Path) -> PathBuf {
    let v1 = version_1();
    let v2 = version_2();
    make_db(
        dir,
        "demo",
        &[
            (10, "2026-11-17 10:00:00", v1.as_str()),
            (11, "2026-11-17 10:05:00", v2.as_str()),
        ],
    )
}

/// Issue #18: el pack de la v1 habla de la v1.
///
/// Antes, `--snapshot 10` renderizaba el flujo de la v1 pero el analisis seguia
/// describiendo el ultimo snapshot: dos versiones del mismo robot en el mismo
/// documento, y el lector no tenia forma de saberlo.
#[test]
fn el_pack_de_una_version_anterior_analiza_esa_version() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = db_dos_versiones(tmp.path());
    let conn = db::open_readonly(&path).expect("abro");

    let pack_v1 = build_from_conn_with_logs(&conn, "demo", Some(10), "test\\robot.db", None)
        .expect("pack de la v1");
    assert_eq!(pack_v1.snapshot_id, 10, "el pack es el de la v1");
    assert_eq!(
        pack_v1.node_count(),
        3,
        "el flujo renderizado es el de la v1: {}",
        pack_v1.render_markdown()
    );

    // El analisis describe los mismos nodos que el flujo: el indice coincide y
    // todo hallazgo con linea cae dentro del flujo pedido. Sin esto, un hallazgo
    // de la v2 (linea 4) se colaria en el pack de la v1.
    let del_indice = pack_v1
        .versions
        .versions
        .iter()
        .find(|v| v.id == 10)
        .expect("la v1 esta en el indice");
    assert_eq!(del_indice.nodes, pack_v1.node_count());
    let lineas_v1: BTreeSet<&str> = pack_v1.flow.iter().map(|n| n.line.as_str()).collect();

    assert_eq!(
        reglas(&pack_v1.analysis),
        vec![
            ("RUTA_ABSOLUTA", "2"),
            ("SIN_TRY", ""),
            ("VAR_NO_USADA", ""),
        ],
        "los hallazgos son los de la v1"
    );
    for finding in pack_v1
        .analysis
        .findings
        .iter()
        .chain(&pack_v1.analysis.higiene)
    {
        if !finding.line.is_empty() {
            assert!(
                lineas_v1.contains(finding.line.as_str()),
                "hallazgo en una linea que la v1 no tiene: {finding:?}"
            );
        }
    }
    assert_eq!(
        pack_v1.analysis.unused_declared,
        vec!["vieja".to_string()],
        "la variable muerta es la de la v1"
    );

    // Y la v2 dice otra cosa: se compara para demostrar que el pin cambia el
    // analisis, no que se trunque el mismo.
    let pack_v2 = build_from_conn_with_logs(&conn, "demo", Some(11), "test\\robot.db", None)
        .expect("pack de la v2");
    assert_eq!(pack_v2.snapshot_id, 11);
    assert_eq!(pack_v2.node_count(), 4);
    assert_eq!(
        reglas(&pack_v2.analysis),
        vec![("SIN_TRY", ""), ("IF_SIN_ELSE", "4")],
        "los hallazgos son los de la v2"
    );
    assert!(
        pack_v2.analysis.unused_declared.is_empty(),
        "la v2 usa las tres variables: {:?}",
        pack_v2.analysis.unused_declared
    );
    assert_ne!(
        reglas(&pack_v1.analysis),
        reglas(&pack_v2.analysis),
        "las dos versiones no pueden producir el mismo analisis"
    );
}

/// Sin `--snapshot` el pack sigue siendo el de la ultima version: el pin cambio la
/// coherencia del pack, no su comportamiento por defecto.
#[test]
fn sin_snapshot_el_pack_sigue_siendo_el_ultimo() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = db_dos_versiones(tmp.path());
    let conn = db::open_readonly(&path).expect("abro");

    let ultimo =
        build_from_conn_with_logs(&conn, "demo", None, "test\\robot.db", None).expect("pack");
    let explicito = build_from_conn_with_logs(&conn, "demo", Some(11), "test\\robot.db", None)
        .expect("pack de la v2");

    assert_eq!(ultimo.snapshot_id, 11, "por defecto se usa el ultimo");
    assert_eq!(ultimo.node_count(), explicito.node_count());
    assert_eq!(
        reglas(&ultimo.analysis),
        reglas(&explicito.analysis),
        "pedir el ultimo snapshot o no pedir ninguno es lo mismo"
    );
}

#[test]
fn el_pack_no_filtra_rutas_absolutas_del_usuario() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let simple = fixture("simple.json");
    let path = make_db(
        tmp.path(),
        "demo",
        &[(1, "2026-11-17 10:00:00", simple.as_str())],
    );
    let conn = db::open_readonly(&path).expect("abro");
    let pack = build_from_conn(&conn, "demo", None, "BBRE2\\robot_run.db").expect("pack");
    let md = pack.render_markdown();
    assert!(
        !md.contains(r"C:\Users"),
        "el render no debe contener rutas de usuario"
    );
}

/// Conteos de referencia medidos el 2026-11-17 sobre el entorno real.
/// Se corre a mano: `cargo test -- --ignored real_`
#[test]
#[ignore]
fn real_el_workspace_entero_parsea_y_comprime() {
    let mut total_raw = 0usize;
    let mut total_render = 0usize;
    let mut robots = 0usize;
    let mut colisiones = 0usize;

    for root in [r"C:\Rocketbot", r"C:\rocketbot_bots"] {
        let dbs = db::discover_databases(Path::new(root)).expect("descubro");
        for p in dbs {
            let Ok(conn) = db::open_readonly(&p) else {
                continue;
            };
            let Ok(list) = db::list_robots(&conn) else {
                continue;
            };
            for r in list {
                let Ok(pack) = build_from_conn(&conn, &r.name, None, "real") else {
                    continue;
                };
                let md = pack.render_markdown();
                let s = pack.sizing(&md);
                total_raw += s.raw_tokens;
                total_render += s.render_tokens;
                robots += 1;
                colisiones += pack.collisions.len();
                assert!(
                    pack.node_count() > 0 || r.name == "Google",
                    "{} quedo vacio",
                    r.name
                );
            }
        }
    }

    let ratio = total_raw as f64 / total_render.max(1) as f64;
    println!(
        "robots={robots} crudo={total_raw} render={total_render} ratio={ratio:.1}x colisiones={colisiones}"
    );
    assert!(robots >= 20, "esperaba >= 20 robots, hubo {robots}");
    assert!(ratio >= 4.0, "el ratio bajo a {ratio:.1}x");
}
