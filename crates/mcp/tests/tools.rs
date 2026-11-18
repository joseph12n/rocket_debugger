//! Tools contra una db temporal.
//!
//! Ninguno de estos tests toca `C:\Rocketbot`: la db se crea en un directorio
//! temporal y el entorno que se pasa es `Settings::hermetic()` (sin
//! `ROCKETBOT_DB` y sin logs), asi que el resultado no cambia segun la maquina.

mod common;

use std::io::Write as _;
use std::process::{Command, Stdio};

use rocketdebug_mcp::{handle_line_with, Settings};
use serde_json::{json, Value};

use common::{call_request, demo_project, is_error, make_db, parse_response, tool_text, TempDir};

/// Ejecuta una `tools/call` y devuelve la respuesta ya parseada.
fn call(settings: &Settings, id: i64, tool: &str, args: Value) -> Value {
    let linea = call_request(id, tool, args);
    let respuesta = handle_line_with(&linea, settings)
        .unwrap_or_else(|| panic!("`{tool}` tiene que responder"));
    parse_response(&respuesta)
}

#[test]
fn flow_diff_dice_que_cambio_entre_dos_snapshots() {
    let tmp = TempDir::new("diff");
    let antes = proyecto_con_comando("1");
    let despues = proyecto_con_comando("2");
    let db = make_db(
        tmp.path(),
        "demo",
        &[
            (1, "2026-11-17 10:00:00", antes.as_str()),
            (2, "2026-11-17 11:00:00", despues.as_str()),
        ],
    );

    let respuesta = call(
        &Settings::hermetic(),
        1,
        "flow_diff",
        json!({"robot": "demo", "db": db, "from": 1, "to": 2}),
    );
    assert!(!is_error(&respuesta), "{respuesta}");
    let texto = tool_text(&respuesta);
    assert!(texto.contains("1 cambiado"), "resumen del diff:\n{texto}");
    assert!(texto.contains("~ L1 setVar"), "linea del cambio:\n{texto}");

    // El mismo snapshot contra si mismo no inventa cambios.
    let igual = call(
        &Settings::hermetic(),
        2,
        "flow_diff",
        json!({"robot": "demo", "db": db, "from": 1, "to": 1}),
    );
    assert!(
        tool_text(&igual).contains("sin cambios"),
        "{}",
        tool_text(&igual)
    );
}

/// Un proyecto de un solo nodo estable (`id` fijo) con el comando variable: es lo
/// minimo para que el diff vea `Changed` y no `Added` + `Removed`.
fn proyecto_con_comando(cmd: &str) -> String {
    format!(
        r#"{{"project":{{"commands":[{{"father":"setVar","command":"{cmd}","group":"system",
        "id":"nodo-fijo","line":1,"var":"x","children":[],"else":[]}}],"vars":[],
        "profile":{{"name":"demo"}}}}}}"#
    )
}

// ---------------------------------------------------------------- registro

/// Settings hermetico con un registro propio: es lo minimo para ejercitar las
/// tools `problems_*` sin depender de `%LOCALAPPDATA%` ni de `C:\Rocketbot`.
fn con_registro(dir: &std::path::Path) -> (Settings, std::path::PathBuf) {
    let ruta = dir.join("problems.db");
    let mut settings = Settings::hermetic();
    settings.registry = Some(ruta.clone());
    (settings, ruta)
}

#[test]
fn las_tools_del_registro_dicen_que_falta_si_no_hay_registro() {
    let respuesta = call(&Settings::hermetic(), 1, "problems_search", json!({}));
    assert!(is_error(&respuesta), "{respuesta}");
    let texto = tool_text(&respuesta);
    assert!(
        texto.contains("ROCKETDEBUG_REGISTRY"),
        "el mensaje tiene que decir como configurarlo:\n{texto}"
    );
}

#[test]
fn problems_search_encuentra_lo_sembrado_y_cuenta_los_fixes() {
    use rocketdebug_core::db::parse_date;
    use rocketdebug_core::registry::{FixRecord, Observation, Registry};

    let tmp = TempDir::new("search");
    let (settings, ruta) = con_registro(tmp.path());

    // Se siembra el registro por la puerta del motor, no por SQL a mano.
    let mut registro = Registry::open(&ruta).expect("registro");
    let firma = "NameError:name:{var}:not_defined";
    for robot in ["orq", "hijo"] {
        registro
            .record_observation(&Observation {
                signature: firma.to_string(),
                robot: robot.to_string(),
                db_label: "test\\robot.db".to_string(),
                version: Some(1),
                line: "2".to_string(),
                message: "NameError: name 'contador' is not defined".to_string(),
                seen_at: parse_date("2026-11-17 12:49:22"),
            })
            .expect("observacion");
    }
    registro
        .record_fix(&FixRecord {
            signature: firma.to_string(),
            robot: "orq".to_string(),
            from_version: Some(1),
            to_version: Some(2),
            summary: "1 cambiado".to_string(),
            detail: "~ L2 setVar  0 → 1".to_string(),
            note: "era una asignacion a si misma".to_string(),
            recorded_at: parse_date("2026-11-17 13:00:00"),
        })
        .expect("fix");

    let respuesta = call(
        &settings,
        1,
        "problems_search",
        json!({"query": "NameError"}),
    );
    assert!(!is_error(&respuesta), "{respuesta}");
    let texto = tool_text(&respuesta);
    assert!(texto.contains(firma), "la firma aparece:\n{texto}");
    assert!(texto.contains("2 avistamientos"), "{texto}");
    assert!(texto.contains("1 fixes"), "{texto}");
    assert!(texto.contains("hijo") && texto.contains("orq"), "{texto}");

    // Busqueda por robot y busqueda vacia.
    let por_robot = call(&settings, 2, "problems_search", json!({"query": "hijo"}));
    assert!(
        tool_text(&por_robot).contains(firma),
        "{}",
        tool_text(&por_robot)
    );
    let todo = call(&settings, 3, "problems_search", json!({}));
    assert!(tool_text(&todo).contains(firma), "{}", tool_text(&todo));
    let nada = call(
        &settings,
        4,
        "problems_search",
        json!({"query": "no-existe-esto"}),
    );
    assert!(tool_text(&nada).contains("sin coincidencias"));
}

#[test]
fn problems_record_sin_logs_no_inventa_observaciones() {
    let tmp = TempDir::new("record");
    let db = make_db(
        tmp.path(),
        "demo",
        &[(1, "2026-11-17 10:00:00", demo_project().as_str())],
    );
    let (settings, _) = con_registro(tmp.path());

    let respuesta = call(
        &settings,
        1,
        "problems_record",
        json!({"robot": "demo", "db": db}),
    );
    assert!(!is_error(&respuesta), "{respuesta}");
    let texto = tool_text(&respuesta);
    assert!(texto.contains("0 errores vistos"), "{texto}");
    assert!(texto.contains("0 observaciones nuevas"), "{texto}");

    // El registro se creo y la busqueda ya no falla por falta de fichero.
    let buscado = call(&settings, 2, "problems_search", json!({}));
    assert!(!is_error(&buscado), "{buscado}");
    assert!(tool_text(&buscado).contains("sin coincidencias"));
}

#[test]
fn problems_fix_guarda_el_diff_entre_dos_versiones() {
    let tmp = TempDir::new("fix");
    let antes = proyecto_con_comando("1");
    let despues = proyecto_con_comando("2");
    let db = make_db(
        tmp.path(),
        "demo",
        &[
            (1, "2026-11-17 10:00:00", antes.as_str()),
            (2, "2026-11-17 11:00:00", despues.as_str()),
        ],
    );
    let (settings, _) = con_registro(tmp.path());

    let respuesta = call(
        &settings,
        1,
        "problems_fix",
        json!({
            "robot": "demo",
            "signature": "NameError:name:{var}:not_defined",
            "from": 1,
            "to": 2,
            "note": "inicializar antes de usar",
            "db": db,
        }),
    );
    assert!(!is_error(&respuesta), "{respuesta}");
    let texto = tool_text(&respuesta);
    assert!(texto.contains("fix #1 registrado"), "{texto}");
    assert!(
        texto.contains("1 cambiado"),
        "el resumen sale del diff:\n{texto}"
    );

    // Y queda consultable.
    let buscado = call(
        &settings,
        2,
        "problems_search",
        json!({"query": "NameError"}),
    );
    assert!(
        tool_text(&buscado).contains("1 fixes"),
        "{}",
        tool_text(&buscado)
    );
}

#[test]
fn problems_fix_exige_las_dos_versiones() {
    let tmp = TempDir::new("fixargs");
    let (settings, _) = con_registro(tmp.path());
    let respuesta = call(
        &settings,
        1,
        "problems_fix",
        json!({"robot": "demo", "signature": "x", "from": 1}),
    );
    assert!(
        respuesta["error"].is_object(),
        "falta `to` es error de protocolo: {respuesta}"
    );
}

#[test]
fn list_robots_dice_que_db_leyo_y_que_robots_tiene() {
    let tmp = TempDir::new("list");
    let proyecto = demo_project();
    let db = make_db(
        tmp.path(),
        "demo",
        &[(1, "2026-11-17 10:00:00", proyecto.as_str())],
    );

    let respuesta = call(&Settings::hermetic(), 1, "list_robots", json!({"db": db}));
    assert!(!is_error(&respuesta), "{respuesta}");

    let texto = tool_text(&respuesta);
    assert!(
        texto.contains(&*db.to_string_lossy()),
        "el listado dice que db se leyo:\n{texto}"
    );
    assert!(texto.contains("1 robot"), "{texto}");
    assert!(texto.contains("demo"), "{texto}");
    assert!(texto.contains("1 version"), "{texto}");
}

#[test]
fn flow_pack_emite_el_render_markdown_del_robot() {
    let tmp = TempDir::new("pack");
    let proyecto = demo_project();
    let db = make_db(
        tmp.path(),
        "demo",
        &[(1, "2026-11-17 10:00:00", proyecto.as_str())],
    );

    let respuesta = call(
        &Settings::hermetic(),
        1,
        "flow_pack",
        json!({"db": db, "robot": "demo"}),
    );
    assert!(!is_error(&respuesta), "{respuesta}");

    let texto = tool_text(&respuesta);
    assert!(
        texto.contains("# demo"),
        "el entregable es el render del pack:\n{texto}"
    );
    assert!(
        texto.starts_with("# demo  (snapshot 1, 1 versiones)"),
        "cabecera del pack:\n{texto}"
    );
    assert!(texto.contains("## Flujo"), "{texto}");
}

#[test]
fn flow_pack_respeta_el_snapshot_pedido() {
    let tmp = TempDir::new("snapshot");
    let proyecto = demo_project();
    let db = make_db(
        tmp.path(),
        "demo",
        &[
            (10, "2026-11-17 10:00:00", proyecto.as_str()),
            (11, "2026-11-17 11:00:00", proyecto.as_str()),
        ],
    );

    let respuesta = call(
        &Settings::hermetic(),
        1,
        "flow_pack",
        json!({"db": db, "robot": "demo", "snapshot": 10}),
    );
    assert!(!is_error(&respuesta), "{respuesta}");
    assert!(
        tool_text(&respuesta).starts_with("# demo  (snapshot 10, 2 versiones)"),
        "{}",
        tool_text(&respuesta)
    );
}

#[test]
fn un_robot_inexistente_es_error_de_negocio_y_el_servidor_sigue() {
    let tmp = TempDir::new("missing");
    let proyecto = demo_project();
    let db = make_db(
        tmp.path(),
        "demo",
        &[(1, "2026-11-17 10:00:00", proyecto.as_str())],
    );

    let respuesta = call(
        &Settings::hermetic(),
        1,
        "flow_pack",
        json!({"db": db, "robot": "no_existe"}),
    );

    assert!(
        is_error(&respuesta),
        "robot inexistente es isError, no error de protocolo: {respuesta}"
    );
    assert!(
        respuesta["error"].is_null(),
        "no puede llegar como error JSON-RPC: {respuesta}"
    );
    let texto = tool_text(&respuesta);
    assert!(
        texto.contains("no_existe"),
        "el mensaje tiene que nombrar al robot:\n{texto}"
    );
    assert!(
        texto.contains("db:"),
        "y decir que db se consulto:\n{texto}"
    );

    // La linea siguiente se sigue atendiendo: el fallo no envenena el servidor.
    // No se fija el numero de tools a proposito: lo que se prueba aqui es que el
    // servidor sigue vivo, no el catalogo (para eso esta el test del catalogo).
    let linea = common::request(2, "tools/list", json!({}));
    let siguiente = parse_response(
        &handle_line_with(&linea, &Settings::hermetic()).expect("tools/list responde"),
    );
    let tools = siguiente["result"]["tools"]
        .as_array()
        .expect("tools es un array");
    assert!(!tools.is_empty(), "{siguiente}");
}

#[test]
fn una_db_que_no_existe_da_un_error_legible() {
    let tmp = TempDir::new("nodatabase");
    let ruta = tmp.path().join("no_existe.db");
    assert!(!ruta.exists());

    let respuesta = call(&Settings::hermetic(), 1, "list_robots", json!({"db": ruta}));
    assert!(is_error(&respuesta), "{respuesta}");
    let texto = tool_text(&respuesta);
    assert!(
        texto.contains("no_existe.db"),
        "el mensaje nombra la ruta pedida:\n{texto}"
    );
}

#[test]
fn robot_errors_sin_logs_avisa_de_que_falta_evidencia() {
    let tmp = TempDir::new("errors");
    let proyecto = demo_project();
    let db = make_db(
        tmp.path(),
        "demo",
        &[(1, "2026-11-17 10:00:00", proyecto.as_str())],
    );

    let respuesta = call(
        &Settings::hermetic(),
        1,
        "robot_errors",
        json!({"db": db, "robot": "demo"}),
    );
    assert!(
        !is_error(&respuesta),
        "no tener logs no es un fallo de negocio: {respuesta}"
    );

    let texto = tool_text(&respuesta);
    assert!(texto.contains("sin datos de errores"), "{texto}");
    assert!(texto.contains("demo"), "{texto}");
    assert!(
        !texto.contains("## Errores"),
        "sin logs no se inventa una seccion:\n{texto}"
    );
}

/// El binario de verdad: `main` lee lineas, despacha y escribe.
///
/// Es el unico test que arranca un proceso, y comprueba justo lo que el
/// despachador no puede probar: que un fallo de negocio no mata el servidor, que
/// la sesion sigue atendiendo la linea siguiente, que stdout lleva solo una
/// respuesta por peticion y que `ROCKETBOT_DB` se usa cuando la peticion no trae
/// `db`. El entorno del hijo es la db temporal: no toca `C:\Rocketbot`.
#[test]
fn el_proceso_sobrevive_a_un_robot_inexistente_y_sigue_hablando() {
    let tmp = TempDir::new("stdio");
    let proyecto = demo_project();
    let db = make_db(
        tmp.path(),
        "demo",
        &[(1, "2026-11-17 10:00:00", proyecto.as_str())],
    );

    let mut hijo = Command::new(env!("CARGO_BIN_EXE_rocketdebug-mcp"))
        // Sin `db` en las peticiones: la ruta tiene que salir de aqui.
        .env("ROCKETBOT_DB", &db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("arranco el binario");

    {
        let mut entrada = hijo.stdin.take().expect("stdin del hijo");
        let peticiones = [
            common::request(
                1,
                "initialize",
                json!({"protocolVersion": "2024-11-05", "capabilities": {}}),
            ),
            common::call_request(2, "flow_pack", json!({"robot": "no_existe"})),
            common::request(3, "tools/list", json!({})),
        ];
        for linea in peticiones {
            writeln!(entrada, "{linea}").expect("escribo una peticion");
        }
    } // Al soltar stdin, el hijo ve EOF y termina.

    let salida = hijo.wait_with_output().expect("espero al hijo");
    assert!(
        salida.status.success(),
        "el servidor termino mal: {salida:?}"
    );

    let stdout = String::from_utf8_lossy(&salida.stdout);
    let lineas: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lineas.len(),
        3,
        "una respuesta por peticion y nada mas:\n{stdout}"
    );

    let initialize = parse_response(lineas[0]);
    assert_eq!(initialize["result"]["protocolVersion"], "2024-11-05");

    let fallo = parse_response(lineas[1]);
    assert!(is_error(&fallo), "{fallo}");
    let texto = tool_text(&fallo);
    assert!(texto.contains("no_existe"), "{texto}");
    assert!(
        texto.contains(&*db.to_string_lossy()),
        "la db del fallo es la de ROCKETBOT_DB:\n{texto}"
    );

    let listado = parse_response(lineas[2]);
    let tools = listado["result"]["tools"]
        .as_array()
        .expect("tools es un array");
    assert!(
        !tools.is_empty(),
        "el proceso sigue atendiendo despues del fallo: {listado}"
    );
}
