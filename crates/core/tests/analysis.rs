use std::path::{Path, PathBuf};

use base64::Engine as _;
use rocketdebug_core::analysis::{
    analyse_with_logs, analyse_with_logs_at, analyse_with_workspace_at_and_paths, load_workspace,
};
use rocketdebug_core::graph::build_edges;
use rocketdebug_core::{analyse, build_from_conn, db, Error};
use serde_json::{json, Value};

const DB: &str = r"test\robot.db";

fn node(father: &str, line: &str, command: &str) -> Value {
    json!({
        "father": father,
        "line": line,
        "command": command,
        "children": [],
        "else": [],
    })
}

fn call(line: &str, child: &str) -> Value {
    node("execRocketBotDB", line, child)
}

fn robot(commands: Vec<Value>, vars: &[&str]) -> Value {
    let vars: Vec<Value> = vars
        .iter()
        .map(|name| json!({"name": name, "data": null}))
        .collect();
    json!({
        "project": {
            "commands": commands,
            "vars": vars,
            "profile": {"name": "fixture"},
        }
    })
}

fn orquestador() -> Value {
    robot(
        vec![
            json!({
                "father": "setVar",
                "line": "1",
                "command": "\"cafe\"",
                "var": "busqueda",
                "children": [],
                "else": [],
            }),
            call("2", "hijo_web"),
            call("3", "hijo_excel"),
            call("4", "no_esta_en_la_db"),
        ],
        &["busqueda", "ruta_salida", "destinatario"],
    )
}

fn hijo_web() -> Value {
    robot(
        vec![node(
            "log_message",
            "2.1",
            "Buscando {busqueda} con {chrome_profile}",
        )],
        &[],
    )
}

fn hijo_excel() -> Value {
    robot(
        vec![node("savexlsx", "3.1", "%base_path%/{ruta_salida}")],
        &[],
    )
}

fn orq_v1() -> Value {
    robot(
        vec![
            json!({
                "father": "setVar",
                "line": "1",
                "command": "\"cafe\"",
                "var": "busqueda",
                "children": [],
                "else": [],
            }),
            call("2", "hijo_web"),
        ],
        &["busqueda"],
    )
}

fn orq_v2() -> Value {
    robot(
        vec![
            json!({
                "father": "setVar",
                "line": "1",
                "command": "\"cafe\"",
                "var": "busqueda",
                "children": [],
                "else": [],
            }),
            call("2", "hijo_web"),
            call("3", "hijo_excel"),
            node("evaluateIf", "4", "\"{busqueda}\" == \"\""),
        ],
        &["busqueda"],
    )
}

fn hijo_web_que_lee(var: &str) -> Value {
    robot(
        vec![node("log_message", "2.1", &format!("Buscando {{{var}}}"))],
        &[],
    )
}

fn db_con_versiones(dir: &Path) -> PathBuf {
    make_db(
        dir,
        vec![
            (1, "orq".to_string(), orq_v1()),
            (2, "orq".to_string(), orq_v2()),
            (10, "hijo_web".to_string(), hijo_web_que_lee("uno")),
            (11, "hijo_web".to_string(), hijo_web_que_lee("dos")),
            (12, "hijo_excel".to_string(), hijo_excel()),
        ],
    )
}

fn make_db(dir: &Path, rows: Vec<(i64, String, Value)>) -> PathBuf {
    make_db_at(dir, "robot.db", rows)
}

fn make_db_at(dir: &Path, file: &str, rows: Vec<(i64, String, Value)>) -> PathBuf {
    std::fs::create_dir_all(dir).expect("creo la carpeta de la db");
    let path = dir.join(file);
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
    for (id, name, project) in rows {
        let raw = project.to_string();
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw.as_bytes());
        conn.execute(
            "INSERT INTO bots (id, name, created_at, data) \
             VALUES (?1, ?2, '2026-11-17 10:00:00', ?3)",
            rusqlite::params![id, name, b64],
        )
        .expect("inserto snapshot");
    }
    path
}

fn workspace(dir: &Path) -> PathBuf {
    make_db(
        dir,
        vec![
            (1, "orq".to_string(), orquestador()),
            (2, "hijo_web".to_string(), hijo_web()),
            (3, "hijo_excel".to_string(), hijo_excel()),
        ],
    )
}

#[test]
fn el_workspace_compartido_da_el_mismo_analisis_que_cada_robot_por_separado() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = workspace(tmp.path());
    let conn = db::open_readonly(&path).expect("abro");

    let shared_workspace =
        rocketdebug_core::analysis::Workspace::load(&[(&conn, DB)]).expect("workspace");

    for robot in ["orq", "hijo_web", "hijo_excel"] {
        let individual = analyse_with_logs(&conn, DB, robot, None).expect("analisis individual");
        let shared = shared_workspace
            .analyse(&conn, DB, robot, None, &rocketdebug_core::paths::NoPaths)
            .expect("analisis compartido");

        assert_eq!(shared.findings, individual.findings, "{robot}: hallazgos");
        assert_eq!(shared.higiene, individual.higiene, "{robot}: higiene");
        assert_eq!(
            shared.contracts_out, individual.contracts_out,
            "{robot}: contratos de salida"
        );
        assert_eq!(
            shared.contracts_in, individual.contracts_in,
            "{robot}: contratos de entrada"
        );
        assert_eq!(
            shared.unused_declared, individual.unused_declared,
            "{robot}: variables muertas"
        );
        assert_eq!(
            shared.ambiguous_calls, individual.ambiguous_calls,
            "{robot}: llamadas ambiguas"
        );
        assert_eq!(
            shared.missing_calls, individual.missing_calls,
            "{robot}: llamadas sin destino"
        );
        assert_eq!(shared.errors, individual.errors, "{robot}: errores");
    }
}

#[test]
fn el_orquestador_ve_a_sus_hijos_y_solo_marca_lo_que_esta_muerto() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = workspace(tmp.path());
    let conn = db::open_readonly(&path).expect("abro");

    let pack = build_from_conn(&conn, "orq", None, DB).expect("pack");
    let analysis = &pack.analysis;

    assert_eq!(
        analysis.contracts_out.len(),
        2,
        "{:?}",
        analysis.contracts_out
    );
    let web = analysis
        .contracts_out
        .iter()
        .find(|c| c.child == "hijo_web")
        .expect("contrato de hijo_web");
    assert_eq!(web.parent, "orq");
    assert_eq!(web.line, "2", "la linea de la llamada en el padre");
    assert_eq!(
        web.provided,
        vec![
            "busqueda".to_string(),
            "destinatario".to_string(),
            "ruta_salida".to_string()
        ],
        "el panel del padre es lo que hereda el hijo"
    );
    assert_eq!(
        web.missing,
        vec!["chrome_profile".to_string()],
        "lo que el hijo lee y nadie le da"
    );
    assert!(analysis.contracts_in.is_empty(), "nadie llama al orq");

    assert_eq!(
        analysis.unused_declared,
        vec!["destinatario".to_string()],
        "la var que es API de un hijo no se marca"
    );

    let accionables: Vec<&str> = analysis
        .findings
        .iter()
        .map(|f| f.rule_id.as_str())
        .collect();
    let higiene: Vec<&str> = analysis
        .higiene
        .iter()
        .map(|f| f.rule_id.as_str())
        .collect();
    assert!(higiene.contains(&"SIN_TRY"), "higiene: {higiene:?}");
    assert!(higiene.contains(&"VAR_NO_USADA"), "higiene: {higiene:?}");
    assert!(
        !accionables.contains(&"LINEA_COLISION"),
        "no hay lineas repetidas: {accionables:?}"
    );

    assert!(analysis.ambiguous_calls.is_empty());
    assert_eq!(
        analysis.missing_calls,
        vec!["orq L4 -> no_esta_en_la_db".to_string()],
        "la llamada sin destino ya no se pierde en silencio"
    );

    let md = pack.render_markdown();
    assert!(
        !md.contains("## Hallazgos"),
        "sin accionables no se pinta la seccion:\n{md}"
    );
    assert!(
        md.contains("## Higiene (2: 0 alta / 1 media / 1 baja)"),
        "cabecera de higiene con recuento por severidad:\n{md}"
    );
    assert!(
        md.contains("## Contrato implicito (subrobots)"),
        "falta la seccion de contrato:\n{md}"
    );
    assert!(
        md.contains("- llama a hijo_web en L2 → provee: busqueda, destinatario, ruta_salida"),
        "linea de contrato:\n{md}"
    );
    assert!(
        md.contains("  !! el hijo espera y nadie provee: chrome_profile"),
        "el hueco del contrato se ve antes de ejecutar:\n{md}"
    );

    let pos = |needle: &str| md.find(needle).unwrap_or_else(|| panic!("falta {needle}"));
    assert!(pos("## Higiene") < pos("## Contrato implicito"));
    assert!(pos("## Contrato implicito") < pos("## Variables"));
    assert!(pos("## Variables") < pos("## Flujo"));
}

#[test]
fn el_hijo_sabe_quien_lo_llama_y_que_le_falta() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = workspace(tmp.path());
    let conn = db::open_readonly(&path).expect("abro");

    let pack = build_from_conn(&conn, "hijo_web", None, DB).expect("pack");
    assert!(
        pack.analysis.contracts_out.is_empty(),
        "hijo_web no llama a nadie"
    );
    assert_eq!(pack.analysis.contracts_in.len(), 1);

    let inbound = &pack.analysis.contracts_in[0];
    assert_eq!(inbound.parent, "orq");
    assert_eq!(inbound.line, "2", "la linea del padre que lo llama");
    assert_eq!(
        inbound.missing,
        vec!["chrome_profile".to_string()],
        "el hijo ve el mismo hueco que ve el padre"
    );

    let md = pack.render_markdown();
    assert!(
        md.contains("- lo llama orq en L2 → recibe: busqueda, destinatario, ruta_salida"),
        "linea de contrato entrante:\n{md}"
    );
}

#[test]
fn el_contrato_se_trunca_a_ocho_llamadas_por_seccion() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut rows = Vec::new();
    let mut calls = Vec::new();
    for i in 0..10 {
        calls.push(call(&(i + 1).to_string(), &format!("hijo_{i}")));
        rows.push((i, format!("hijo_{i}"), robot(Vec::new(), &[])));
    }
    rows.push((100, "orq".to_string(), robot(calls, &[])));

    let path = make_db(tmp.path(), rows);
    let conn = db::open_readonly(&path).expect("abro");
    let pack = build_from_conn(&conn, "orq", None, DB).expect("pack");

    assert_eq!(
        pack.analysis.contracts_out.len(),
        10,
        "el analisis conserva todo; el recorte es del render"
    );

    let md = pack.render_markdown();
    assert_eq!(
        md.matches("- llama a ").count(),
        8,
        "nunca se listan las 40 lineas:\n{md}"
    );
    assert!(
        md.contains("- ... y 2 llamadas mas"),
        "el recorte se declara, no se esconde:\n{md}"
    );
}

#[test]
fn un_robot_que_no_esta_en_la_db_da_error_claro() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = workspace(tmp.path());
    let conn = db::open_readonly(&path).expect("abro");

    let err = analyse(&conn, DB, "no_existe").expect_err("no hay hechos de ese robot");
    assert!(err.to_string().contains("no_existe"), "error: {err}");
}

#[test]
fn un_hijo_que_solo_esta_en_otra_db_sale_como_ambiguo() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path_a = make_db_at(
        &tmp.path().join("BBRE2"),
        "robot_run.db",
        vec![
            (1, "orq".to_string(), orquestador()),
            (2, "hijo_excel".to_string(), hijo_excel()),
        ],
    );
    let path_b = make_db_at(
        &tmp.path().join("BBRE2").join("entrega_corregida"),
        "robot_run.db",
        vec![(1, "hijo_web".to_string(), hijo_web())],
    );
    let conn_a = db::open_readonly(&path_a).expect("abro A");
    let conn_b = db::open_readonly(&path_b).expect("abro B");
    let la = r"BBRE2\robot_run.db";
    let lb = r"entrega_corregida\robot_run.db";

    let solo_a = analyse(&conn_a, la, "orq").expect("analisis de una db");
    assert!(
        solo_a.ambiguous_calls.is_empty(),
        "una db no puede saber que el hijo esta fuera"
    );
    assert_eq!(
        solo_a.contracts_out.len(),
        1,
        "solo el hijo de la misma db: {:?}",
        solo_a.contracts_out
    );

    let facts = load_workspace(&[(&conn_a, la), (&conn_b, lb)]).expect("hechos del workspace");
    let edges = build_edges(&facts);
    let web = edges
        .iter()
        .find(|edge| edge.child == "hijo_web")
        .expect("arista a hijo_web");
    assert!(
        web.ambiguous && !web.same_db,
        "solo existe fuera de la db del padre: {web:?}"
    );
    assert_eq!(web.child_db, lb);

    let ws = analyse_with_workspace_at_and_paths(
        &[(&conn_a, la), (&conn_b, lb)],
        la,
        "orq",
        None,
        None,
        &rocketdebug_core::paths::NoPaths,
    )
    .expect("analisis del workspace");
    assert_eq!(ws.ambiguous_calls, vec!["orq -> hijo_web".to_string()]);
    assert_eq!(
        ws.contracts_out.len(),
        2,
        "ahora el hijo de la otra db tiene contrato: {:?}",
        ws.contracts_out
    );
    let contrato = ws
        .contracts_out
        .iter()
        .find(|c| c.child == "hijo_web")
        .expect("contrato de hijo_web");
    assert_eq!(
        contrato.missing,
        vec!["chrome_profile".to_string()],
        "el contrato se calcula con los hechos de la otra db"
    );
    assert_eq!(
        ws.missing_calls,
        vec!["orq L4 -> no_esta_en_la_db".to_string()],
        "el robot que no esta en ninguna db sigue siendo un aviso"
    );
}

#[test]
fn load_workspace_deduplica_por_contenido_y_conserva_homonimos() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path_a = make_db_at(
        &tmp.path().join("trabajo"),
        "robot.db",
        vec![
            (1, "orq".to_string(), orquestador()),
            (2, "hijo_excel".to_string(), hijo_excel()),
        ],
    );
    let path_b = make_db_at(
        &tmp.path().join("entrega_corregida"),
        "robot.db",
        vec![
            (1, "orq".to_string(), orquestador()),
            (2, "hijo_excel".to_string(), robot(vec![], &["otra_cosa"])),
            (3, "hijo_web".to_string(), hijo_web()),
        ],
    );
    let conn_a = db::open_readonly(&path_a).expect("abro A");
    let conn_b = db::open_readonly(&path_b).expect("abro B");
    let (la, lb) = ("trabajo\\robot.db", "entrega_corregida\\robot.db");

    let facts = load_workspace(&[(&conn_a, la), (&conn_b, lb)]).expect("hechos");

    assert_eq!(
        facts.len(),
        4,
        "hechos: {:?}",
        facts.iter().map(|f| (&f.db, &f.name)).collect::<Vec<_>>()
    );

    let orq = facts.iter().find(|fact| fact.name == "orq").expect("orq");
    assert_eq!(orq.db, la, "la primera db conserva la etiqueta del hecho");
    assert_eq!(
        orq.also_in,
        vec![lb.to_string()],
        "la copia identica se anota, no se duplica"
    );

    assert_eq!(
        facts
            .iter()
            .filter(|fact| fact.name == "hijo_excel")
            .count(),
        2,
        "el mismo nombre con contenido distinto son dos hechos"
    );
}

#[test]
fn el_analisis_de_un_snapshot_anterior_no_mezcla_el_ultimo() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = db_con_versiones(tmp.path());
    let conn = db::open_readonly(&path).expect("abro");

    let v1 = analyse_with_logs_at(&conn, DB, "orq", Some(1), None).expect("analisis de la v1");
    assert_eq!(
        v1.contracts_out.len(),
        1,
        "la v1 solo llama a hijo_web: {:?}",
        v1.contracts_out
    );
    let web = &v1.contracts_out[0];
    assert_eq!(web.child, "hijo_web");
    assert_eq!(web.line, "2");
    assert_eq!(
        web.missing,
        vec!["dos".to_string()],
        "el hijo aporta su ultimo snapshot: el contrato que se romperia hoy"
    );
    assert!(
        !v1.higiene.iter().any(|f| f.rule_id == "IF_SIN_ELSE"),
        "el IF de la linea 4 solo existe en la v2: {:?}",
        v1.higiene
    );
    assert!(
        v1.missing_calls.is_empty(),
        "la v1 no llama a ningun robot inexistente: {:?}",
        v1.missing_calls
    );

    let ultima = analyse_with_logs(&conn, DB, "orq", None).expect("analisis de la ultima");
    assert_eq!(
        ultima.contracts_out.len(),
        2,
        "la ultima version llama a los dos hijos: {:?}",
        ultima.contracts_out
    );
    assert!(
        ultima
            .higiene
            .iter()
            .any(|f| f.rule_id == "IF_SIN_ELSE" && f.line == "4"),
        "la v2 tiene el IF sin else: {:?}",
        ultima.higiene
    );
    assert_eq!(
        ultima
            .contracts_out
            .iter()
            .find(|c| c.child == "hijo_web")
            .map(|c| c.missing.clone()),
        Some(vec!["dos".to_string()]),
        "el hijo se analiza igual en las dos peticiones"
    );
}

#[test]
fn un_snapshot_inexistente_no_cae_al_ultimo() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = db_con_versiones(tmp.path());
    let conn = db::open_readonly(&path).expect("abro");

    let err = analyse_with_logs_at(&conn, DB, "orq", Some(999), None).expect_err("no existe");
    assert!(matches!(err, Error::SnapshotNotFound(999)), "error: {err}");
}
