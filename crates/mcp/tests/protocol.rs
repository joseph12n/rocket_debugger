mod common;

use rocketdebug_mcp::handle_line;
use serde_json::json;

use common::{call_request, parse_response, request};

fn respuesta(linea: &str) -> serde_json::Value {
    parse_response(&handle_line(linea).unwrap_or_else(|| panic!("tiene que responder: {linea}")))
}

#[test]
fn initialize_devuelve_protocolo_y_capacidades() {
    let linea = request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"},
        }),
    );
    let value = respuesta(&linea);

    assert_eq!(value["jsonrpc"], "2.0");
    assert_eq!(value["id"], 1);
    assert_eq!(value["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(value["result"]["capabilities"], json!({"tools": {}}));
    assert_eq!(value["result"]["serverInfo"]["name"], "rocket_debugger");
    assert_eq!(
        value["result"]["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION"),
        "la version del servidor es la del crate"
    );
    assert!(value["error"].is_null());
}

#[test]
fn tools_list_devuelve_las_diez_tools_con_schema_valido() {
    let value = respuesta(&request(2, "tools/list", json!({})));
    let tools = value["result"]["tools"]
        .as_array()
        .expect("tools es un array");

    let nombres: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("toda tool tiene nombre"))
        .collect();
    assert_eq!(
        nombres,
        vec![
            "list_robots",
            "flow_pack",
            "robot_errors",
            "flow_diff",
            "run_robot",
            "run_status",
            "problems_search",
            "problems_record",
            "problems_fix",
            "ui_render"
        ]
    );

    for tool in tools {
        assert!(
            !tool["description"].as_str().unwrap_or("").is_empty(),
            "toda tool explica para que sirve: {tool}"
        );
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
        assert!(
            tool["inputSchema"]["properties"].is_object(),
            "el schema declara properties: {tool}"
        );
    }

    let pack = tools
        .iter()
        .find(|tool| tool["name"] == "flow_pack")
        .expect("flow_pack esta en el catalogo");
    assert_eq!(pack["inputSchema"]["required"], json!(["robot"]));
    assert_eq!(pack["inputSchema"]["properties"]["robot"]["type"], "string");
    assert_eq!(
        pack["inputSchema"]["properties"]["snapshot"]["type"],
        "integer"
    );
    assert_eq!(pack["inputSchema"]["properties"]["db"]["type"], "string");

    let listado = tools
        .iter()
        .find(|tool| tool["name"] == "list_robots")
        .expect("list_robots esta en el catalogo");
    assert_eq!(
        listado["inputSchema"]["required"],
        json!([]),
        "listar robots no exige argumentos"
    );
}

#[test]
fn un_metodo_desconocido_da_menos_32601() {
    let value = respuesta(&request(9, "no/existe", json!({})));

    assert_eq!(value["error"]["code"], -32601);
    assert_eq!(value["id"], 9);
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("no/existe"),
        "el error dice que metodo no existe: {value}"
    );
}

#[test]
fn la_linea_vacia_se_ignora_y_la_basura_da_menos_32700() {
    assert!(
        handle_line("").is_none(),
        "una linea vacia no es un mensaje"
    );
    assert!(
        handle_line("   \r\n").is_none(),
        "espacios y CR tampoco son un mensaje"
    );

    let value = respuesta("esto no es json {{{");
    assert_eq!(value["error"]["code"], -32700);
    assert!(value["id"].is_null());
}

#[test]
fn una_peticion_sin_method_da_menos_32600() {
    let linea = json!({"jsonrpc": "2.0", "id": 4}).to_string();
    let value = respuesta(&linea);
    assert_eq!(value["error"]["code"], -32600);
}

#[test]
fn ping_responde_sin_cuerpo() {
    let value = respuesta(&request(3, "ping", json!({})));
    assert_eq!(value["result"], json!({}));
    assert_eq!(value["id"], 3);
}

#[test]
fn las_notificaciones_nunca_se_responden() {
    let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string();
    assert!(
        handle_line(&initialized).is_none(),
        "notifications/initialized es un acuse: no lleva respuesta"
    );

    let desconocida = json!({"jsonrpc": "2.0", "method": "no/existe"}).to_string();
    assert!(
        handle_line(&desconocida).is_none(),
        "una notificacion desconocida tampoco se responde"
    );
}

#[test]
fn tools_call_sin_name_da_menos_32602() {
    let value = respuesta(&request(5, "tools/call", json!({"arguments": {}})));
    assert_eq!(value["error"]["code"], -32602);
}

#[test]
fn tools_call_con_una_tool_desconocida_da_menos_32602() {
    let value = respuesta(&call_request(6, "rm_rf", json!({})));
    assert_eq!(value["error"]["code"], -32602);
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("rm_rf"),
        "{value}"
    );
}

#[test]
fn flow_pack_sin_robot_da_menos_32602() {
    let value = respuesta(&call_request(7, "flow_pack", json!({"db": "x.db"})));
    assert_eq!(value["error"]["code"], -32602);
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("robot"),
        "{value}"
    );
}

#[test]
fn los_argumentos_que_no_son_objeto_dan_menos_32602() {
    let value = respuesta(&call_request(8, "list_robots", json!([1, 2])));
    assert_eq!(value["error"]["code"], -32602);
}
