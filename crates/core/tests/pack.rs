//! Tests de integracion del motor contra fixtures sinteticos.
//!
//! Ningun test de este fichero toca `C:\Rocketbot`. Los datos reales se prueban
//! aparte en `real_data`, marcado `#[ignore]`.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use rocketdebug_core::{build_from_conn, db};

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
