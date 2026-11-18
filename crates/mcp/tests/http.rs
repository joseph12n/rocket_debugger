//! Transporte HTTP: lo que un cliente (la app o el agente) puede dar por hecho.
//!
//! Cada test arranca un `serve` real sobre `127.0.0.1:0` (el SO elige el puerto)
//! y habla por `TcpStream`. Nada sale a la red ni toca `C:\Rocketbot`: el entorno
//! es [`Settings::hermetic`] y, cuando hace falta una db, es la temporal de
//! `common`.

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use rocketdebug_mcp::{handle_line_with, serve, Settings, MAX_BODY_BYTES};
use serde_json::json;

use common::{call_request, demo_project, make_db, parse_response, TempDir};

/// Peticion sin cuerpo, tal cual la manda un cliente HTTP/1.1.
const GET_HEALTH: &[u8] = b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
const GET_UNKNOWN: &[u8] = b"GET /otra HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";

/// Arranca el servidor en un puerto que asigna el SO y devuelve ese puerto.
///
/// El hilo se queda vivo: `serve` no tiene parada ordenada porque su unico final
/// es un error fatal del listener. El proceso de test lo termina con el.
fn start(settings: Settings) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("ligo un puerto efimero");
    let port = listener.local_addr().expect("direccion local").port();
    let _ = std::thread::spawn(move || {
        let _ = serve(listener, settings);
    });
    port
}

/// Respuesta HTTP ya separada en estado, cabeceras y cuerpo.
#[derive(Debug)]
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn json(&self) -> serde_json::Value {
        parse_response(&self.body)
    }
}

fn parse(raw: &[u8]) -> Response {
    let text = String::from_utf8(raw.to_vec()).expect("la respuesta es UTF-8");
    let (head, body) = text
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("la respuesta no separa cabecera y cuerpo: {text}"));

    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .unwrap_or_else(|| panic!("sin linea de estado"));
    let status = status_line
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("la linea de estado no trae codigo: {status_line}"));
    let headers = lines
        .filter_map(|line| {
            line.split_once(':')
                .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        })
        .collect();

    Response {
        status,
        headers,
        body: body.to_string(),
    }
}

/// Manda los trozos por una conexion y lee la respuesta hasta el cierre.
fn exchange(port: u16, pieces: &[&[u8]]) -> Response {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("conecto al servidor");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("plazo de lectura");
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .expect("plazo de escritura");

    for piece in pieces {
        stream.write_all(piece).expect("escribo el trozo");
        stream.flush().expect("vacio el buffer");
        // Un respiro entre trozos: persigue que el servidor haga lecturas
        // separadas y no una sola coalescida, que es lo que ejercita el test.
        std::thread::sleep(Duration::from_millis(5));
    }

    let mut raw = Vec::new();
    // El servidor cierra al responder (`Connection: close`): leer hasta EOF.
    stream.read_to_end(&mut raw).expect("leo hasta el cierre");
    parse(&raw)
}

/// Peticion completa con `Content-Length` y el cuerpo pegado detras.
fn request(method: &str, path: &str, body: &str) -> Vec<u8> {
    let mut bytes = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body.as_bytes());
    bytes
}

#[test]
fn initialize_por_http_contesta_lo_mismo_que_stdio() {
    let settings = Settings::hermetic();
    let port = start(settings.clone());
    let line = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"},
        },
    })
    .to_string();

    let request = request("POST", "/mcp", &line);
    let response = exchange(port, &[&request]);

    assert_eq!(response.status, 200);
    assert_eq!(response.header("content-type"), Some("application/json"));
    assert_eq!(
        response.header("connection"),
        Some("close"),
        "una peticion por conexion: no hay keep-alive"
    );

    let value = response.json();
    assert_eq!(value["jsonrpc"], "2.0");
    assert_eq!(value["id"], 1);
    assert_eq!(value["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(value["result"]["serverInfo"]["name"], "rocket_debugger");
    assert_eq!(
        value["result"]["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );

    // El cuerpo es exactamente lo que devuelve el despacho comun: si el
    // transporte duplicara el despacho, este assert se rompe.
    assert_eq!(
        response.body,
        handle_line_with(&line, &settings).expect("initialize responde")
    );
}

#[test]
fn health_contesta_200_con_ok_y_su_identidad() {
    let port = start(Settings::hermetic());
    let response = exchange(port, &[GET_HEALTH]);

    assert_eq!(response.status, 200);
    assert_eq!(response.header("content-type"), Some("application/json"));

    let value = response.json();
    assert_eq!(value["ok"], true);
    assert_eq!(value["name"], "rocket_debugger");
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn un_metodo_o_una_ruta_que_no_toca_dan_405_y_404() {
    let port = start(Settings::hermetic());

    let put = request("PUT", "/mcp", "{}");
    let response = exchange(port, &[&put]);
    assert_eq!(response.status, 405);
    assert_eq!(
        response.header("allow"),
        Some("POST"),
        "el 405 dice que metodo si vale"
    );

    let response = exchange(port, &[GET_UNKNOWN]);
    assert_eq!(response.status, 404);
}

#[test]
fn un_cuerpo_de_mas_de_un_mib_da_413() {
    let port = start(Settings::hermetic());

    // Solo la cabecera: el servidor tiene que rechazar por la longitud
    // declarada, no quedarse leyendo un cuerpo que no cabe en memoria.
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n",
        MAX_BODY_BYTES + 1
    );

    let response = exchange(port, &[head.as_bytes()]);
    assert_eq!(response.status, 413);
}

#[test]
fn una_notificacion_da_202_y_cuerpo_vacio() {
    let port = start(Settings::hermetic());
    let line = json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string();

    let request = request("POST", "/mcp", &line);
    let response = exchange(port, &[&request]);

    assert_eq!(response.status, 202);
    assert!(
        response.body.is_empty(),
        "una notificacion no lleva respuesta, llego {:?}",
        response.body
    );
    assert_eq!(response.header("content-length"), Some("0"));
}

#[test]
fn un_cuerpo_troceado_y_con_acentos_se_atiende_igual() {
    let port = start(Settings::hermetic());
    let line = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "initialize",
        "params": {"clientInfo": {"name": "sesión de José: ñandú"}},
    })
    .to_string();
    let request = request("POST", "/mcp", &line);

    // Cortes a mitad de la cabecera, a mitad del JSON y a mitad de un caracter
    // UTF-8 de dos bytes: el servidor no puede decodificar hasta tenerlos todos.
    let inside_char = request
        .iter()
        .position(|byte| *byte == 0xC3)
        .expect("el cuerpo lleva acentos")
        + 1;
    let mut pieces: Vec<&[u8]> = Vec::new();
    let mut from = 0;
    for cut in [11usize, 63, inside_char, request.len()] {
        let to = cut.min(request.len());
        if to > from {
            pieces.push(&request[from..to]);
            from = to;
        }
    }

    let response = exchange(port, &pieces);

    assert_eq!(response.status, 200);
    let value = response.json();
    assert_eq!(value["id"], 7);
    assert_eq!(value["result"]["serverInfo"]["name"], "rocket_debugger");
}

#[test]
fn una_tool_por_http_usa_el_entorno_del_servidor() {
    // La db la resuelve el servidor al arrancar, no el cliente en cada mensaje:
    // el agente pide la tool y la fachada lee la db que se le dio al servir.
    let tmp = TempDir::new("http-db");
    let project = demo_project();
    let db = make_db(
        tmp.path(),
        "demo",
        &[(1, "2026-11-17 10:00:00", project.as_str())],
    );
    let port = start(Settings {
        env_db: Some(db),
        logs: None,
        registry: None,
        workspace: None,
    });

    let line = call_request(3, "list_robots", json!({}));
    let request = request("POST", "/mcp", &line);
    let response = exchange(port, &[&request]);

    assert_eq!(response.status, 200);
    let value = response.json();
    assert_eq!(value["id"], 3);
    let text = value["result"]["content"][0]["text"]
        .as_str()
        .expect("la tool devuelve texto");
    assert!(text.contains("demo"), "el listado dice el robot:\n{text}");
}

// ---------------------------------------------------------------- UI

/// Servidor con una db temporal de un robot (`demo`).
fn start_ui(nombre: &str) -> (TempDir, u16) {
    let tmp = TempDir::new(nombre);
    let db = make_db(
        tmp.path(),
        "demo",
        &[(1, "2026-11-17 10:00:00", demo_project().as_str())],
    );
    let mut settings = Settings::hermetic();
    settings.env_db = Some(db);
    (tmp, start(settings))
}

const GET_RAIZ: &[u8] = b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
const GET_REVIEW: &[u8] = b"GET /api/review HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
const GET_PACK: &[u8] = b"GET /api/pack?robot=demo HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
const GET_PACK_SIN_ROBOT: &[u8] = b"GET /api/pack HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
const POST_REVIEW: &[u8] =
    b"POST /api/review HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 2\r\n\r\n{}";

#[test]
fn la_raiz_sirve_la_pagina_de_la_ui() {
    let (_tmp, port) = start_ui("uiraiz");
    let response = exchange(port, &[GET_RAIZ]);
    assert_eq!(response.status, 200);
    let tipo = response.header("Content-Type").unwrap_or_default();
    assert!(tipo.starts_with("text/html"), "content-type: {tipo}");
    let cuerpo = &response.body;
    assert!(
        cuerpo.to_ascii_lowercase().contains("<!doctype html"),
        "no parece una pagina HTML"
    );
    assert!(
        cuerpo.contains("/app.js") && cuerpo.contains("/app.css"),
        "la pagina carga sus assets por separado (CSP sin unsafe-inline)"
    );
    assert!(
        !cuerpo.contains("unsafe-inline"),
        "la CSP no puede permitir scripts inline"
    );

    // Y los assets se sirven con su tipo: sin esto el navegador no ejecuta el JS.
    let js = exchange(port, &[b"GET /app.js HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"]);
    assert_eq!(js.status, 200);
    assert_eq!(
        js.header("Content-Type"),
        Some("text/javascript; charset=utf-8")
    );
    assert!(
        js.body.contains("/api/review"),
        "el JS es el que pide la API"
    );

    let css = exchange(port, &[b"GET /app.css HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"]);
    assert_eq!(css.status, 200);
    assert_eq!(css.header("Content-Type"), Some("text/css; charset=utf-8"));
}

#[test]
fn api_review_devuelve_filas_y_totales() {
    let (_tmp, port) = start_ui("uiapi");
    let response = exchange(port, &[GET_REVIEW]);
    assert_eq!(response.status, 200);
    assert_eq!(response.header("Content-Type"), Some("application/json"));
    let cuerpo: serde_json::Value = serde_json::from_str(&response.body).expect("json valido");
    assert_eq!(cuerpo["totals"]["robots"], 1, "{cuerpo}");
    let filas = cuerpo["rows"].as_array().expect("rows es un array");
    assert_eq!(filas.len(), 1, "{cuerpo}");
    assert_eq!(filas[0]["robot"], "demo", "{cuerpo}");
}

#[test]
fn api_pack_devuelve_el_markdown_y_exige_robot() {
    let (_tmp, port) = start_ui("uipack");
    let response = exchange(port, &[GET_PACK]);
    assert_eq!(response.status, 200);
    let tipo = response.header("Content-Type").unwrap_or_default();
    assert!(tipo.starts_with("text/plain"), "content-type: {tipo}");
    assert!(response.body.contains("# demo"), "{}", response.body);

    // Sin `robot` es culpa del cliente (400), no un 500.
    let sin = exchange(port, &[GET_PACK_SIN_ROBOT]);
    assert_eq!(sin.status, 400, "{}", sin.body);
}

#[test]
fn api_review_solo_atiende_get() {
    let (_tmp, port) = start_ui("uimetodo");
    let response = exchange(port, &[POST_REVIEW]);
    assert_eq!(response.status, 405);
}
