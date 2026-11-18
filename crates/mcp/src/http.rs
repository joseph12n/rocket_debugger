//! Transporte HTTP/1.1 local para la fachada MCP.
//!
//! # Por que hace falta
//!
//! stdio basta cuando el cliente arranca el servidor: es el caso de DSH. Pero el
//! producto sera una app con una sola instancia y un solo dueno del estado, y el
//! agente un cliente que se conecta a ella. Eso pide un socket.
//!
//! # HTTP minimo, no un framework
//!
//! Lo que hay aqui es lo justo para JSON-RPC de peticion/respuesta: una peticion
//! por conexion, sin `keep-alive`, sin `chunked`, sin TLS y sin pool de hilos.
//! La app habla con un agente, no con una multitud; una dependencia de servidor
//! HTTP solo traeria superficie que nadie usa.
//!
//! # Un solo despacho
//!
//! Este modulo no sabe de MCP: pasa el cuerpo por [`crate::handle_line_with`], la
//! misma funcion que usa stdio, y devuelve lo que devuelve. Un `match` propio con
//! los metodos duplicaria el protocolo y los dos transportes se separarian a la
//! primera correccion. Como consecuencia, una notificacion (sin `id`) devuelve
//! `None` tambien aqui, y eso se traduce a `202`: no hay nada que contestar.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use serde_json::json;

use rocketdebug_core::db;

use crate::Settings;

/// Cuerpo maximo de `POST /mcp`.
///
/// Un context pack no llega ni de lejos; el tope esta para que una peticion no
/// pueda reservar un `Vec` de gigabytes ni dejar un hilo leyendo para siempre.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Cabecera maxima. `Content-Length` y poco mas caben en una linea; 16 KiB es
/// margen de sobra para un cliente que ademas mande `User-Agent` y `Accept`.
const MAX_HEAD_BYTES: usize = 16 * 1024;

/// Plazo de lectura y escritura.
///
/// Un cliente que abre la conexion y no habla ocupa un hilo; sin plazo, una app
/// que no puede crear hilos se queda colgada detras de un socket mudo.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Atiende peticiones hasta que el listener se cierre. Devuelve solo al error fatal.
///
/// Cada conexion se atiende en su hilo y se cierra al responder (`Connection:
/// close`): no hay estado entre peticiones, asi que un `TcpStream` no puede
/// quedarse pegado a una sesion de otro cliente.
///
/// # Seguridad
///
/// El motor lee `robot.db` sin autenticacion: si esto escuchara en `0.0.0.0`,
/// cualquiera en la red local podria listar los robots del usuario. Por eso se
/// comprueba el `local_addr` del listener y se rechaza lo que no sea loopback,
/// aunque quien llame se haya equivocado al ligar.
pub fn serve(listener: TcpListener, settings: Settings) -> io::Result<()> {
    let address = listener.local_addr()?;
    if !is_loopback(address) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("el transporte HTTP solo atiende en loopback, y el listener esta en {address}"),
        ));
    }

    loop {
        match listener.accept() {
            Ok((stream, _peer)) => {
                let settings = settings.clone();
                // Un hilo por conexion: la app puede seguir pintando la UI
                // mientras el agente pregunta. Los hilos no se acumulan porque
                // cada conexion termina en cuanto recibe su respuesta.
                std::thread::Builder::new()
                    .name("rocketdebug-mcp-http".to_string())
                    .spawn(move || {
                        // Un fallo de esta conexion no es un fallo del servidor:
                        // el cliente se fue o mando basura. Se cierra y ya.
                        let _ = handle_connection(stream, &settings);
                    })?;
            }
            // Un `accept` que falla por una conexion concreta no puede tumbar el
            // motor: rendirse ante el primer cliente que corta el handshake
            // dejaria la app sin servidor.
            Err(error) if is_transient(&error) => continue,
            Err(error) => return Err(error),
        }
    }
}

/// La unica direccion en la que este transporte acepta hablar.
///
/// Aislado de [`serve`] para poder probar la regla sin abrir un socket.
fn is_loopback(address: SocketAddr) -> bool {
    address.ip().is_loopback()
}

/// Errores de `accept` que son de una conexion, no del servidor.
///
/// `WouldBlock` no esta en la lista a proposito: con un listener no bloqueante
/// (un fallo de quien llama, porque `bind` lo deja bloqueante) reintentar seria
/// girar al 100% de CPU sin aceptar nada. Mejor devolver el error y que se vea.
fn is_transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::Interrupted
    )
}

/// Atiende una conexion: lee una peticion, responde y cierra.
fn handle_connection(mut stream: TcpStream, settings: &Settings) -> io::Result<()> {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));

    let response = match read_request(&mut stream) {
        Ok(Some(request)) => respond(&request, settings),
        // Conexion abierta y cerrada sin escribir: un sondeo del SO o un cliente
        // que se arrepiente. No hay peticion, asi que no hay respuesta.
        Ok(None) => return Ok(()),
        Err(error) => error.into_response(),
    };

    write_response(&mut stream, &response)
}

/// Lo que el servidor necesita de una peticion.
///
/// Las cabeceras que no se miran se descartan al parsear en vez de guardarlas:
/// este servidor solo decide con el metodo, la ruta y la longitud del cuerpo.
#[derive(Debug)]
struct Request {
    method: String,
    /// Ruta sin query string (`/health?x=1` es `/health`).
    path: String,
    /// Query string sin el `?` inicial. La UI la necesita para `/api/pack?robot=…`,
    /// y descartarla en el parser (como se hacia antes) la hacia irrecuperable.
    query: String,
    /// `None` = la peticion no traia `Content-Length`, o sea que no declaro cuerpo.
    content_length: Option<usize>,
    body: Vec<u8>,
}

/// Lee una peticion completa.
///
/// `Ok(None)` = el cliente cerro sin escribir nada. `Err` = hay que contestar ese
/// estado y cerrar; no es un fallo del servidor.
///
/// La lectura es incremental a proposito: un `read` de un socket no respeta
/// ninguna frontera, ni la del final de cabecera ni la del cuerpo. Acumular hasta
/// tener lo declarado es lo unico que funciona con un cuerpo troceado.
fn read_request(reader: &mut impl Read) -> Result<Option<Request>, HttpError> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];

    let (head_end, body_start) = loop {
        if let Some(positions) = find_head_end(&buffer) {
            break positions;
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return Err(HttpError::bad_request("la cabecera es demasiado grande"));
        }
        match reader.read(&mut chunk) {
            Ok(0) => {
                if buffer.is_empty() {
                    return Ok(None);
                }
                return Err(HttpError::bad_request(
                    "la conexion se cerro antes de terminar la cabecera",
                ));
            }
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(HttpError::from_io(error)),
        }
    };

    // La cabecera de HTTP es ASCII (y por tanto UTF-8); si no lo es, no es HTTP.
    let head = std::str::from_utf8(&buffer[..head_end])
        .map_err(|_| HttpError::bad_request("la cabecera no es UTF-8"))?;
    if head_end > MAX_HEAD_BYTES {
        return Err(HttpError::bad_request("la cabecera es demasiado grande"));
    }

    let request_line = head.split('\n').next().unwrap_or_default();

    let mut fields = request_line.split_whitespace();
    let (Some(method), Some(target), Some(version)) = (fields.next(), fields.next(), fields.next())
    else {
        return Err(HttpError::bad_request(
            "la linea de peticion no es `METODO RUTA VERSION`",
        ));
    };
    if fields.next().is_some() {
        return Err(HttpError::bad_request(
            "la linea de peticion tiene campos de mas",
        ));
    }
    if !version.starts_with("HTTP/") {
        return Err(HttpError::bad_request(format!(
            "version HTTP desconocida: {version}"
        )));
    }

    // El target puede venir en forma absoluta (`http://host/mcp`) detras de un
    // proxy; aqui solo interesa la ruta. La query se guarda aparte porque las rutas
    // de la UI la usan (`/api/pack?robot=…`).
    let path = target.split(['?', '#']).next().unwrap_or(target);
    let query = target
        .split_once('?')
        .map(|(_, resto)| resto.split('#').next().unwrap_or(""))
        .unwrap_or("");

    // Las cabeceras son todas las lineas menos la de peticion; el CR final de
    // cada una se tolera porque un cliente de Windows lo manda y aqui no anade
    // informacion.
    let headers = head
        .split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .skip(1);

    let mut content_length = None;
    for line in headers {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(HttpError::bad_request(format!("cabecera sin `:`: {line}")));
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            let length = value
                .parse::<usize>()
                .map_err(|_| HttpError::bad_request(format!("Content-Length invalido: {value}")))?;
            content_length = Some(length);
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && !value.eq_ignore_ascii_case("identity")
        {
            // Soportar `chunked` seria otro parser entero (y otra forma de
            // desincronizarse) para un cliente que aqui es siempre local.
            return Err(HttpError::bad_request(
                "Transfer-Encoding no soportado: manda el cuerpo con Content-Length",
            ));
        }
    }

    let body = match content_length {
        Some(length) if length > MAX_BODY_BYTES => return Err(HttpError::too_large()),
        Some(length) => read_body(reader, &buffer, body_start, length)?,
        None => Vec::new(),
    };

    Ok(Some(Request {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        content_length,
        body,
    }))
}

/// Posicion `(final de la cabecera, inicio del cuerpo)` del separador.
///
/// Se aceptan `CRLF CRLF` (lo que manda cualquier cliente HTTP/1.1) y `LF LF`
/// (lo que manda un `printf` de prueba): el segundo no cuesta nada y evita que un
/// cliente de mano se quede esperando.
fn find_head_end(bytes: &[u8]) -> Option<(usize, usize)> {
    let crlf = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|start| (start, start + 4));
    let lf = bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|start| (start, start + 2));

    // El separador que aparece antes, no el que se comprueba antes: con una
    // cabecera terminada en LF y un cuerpo que lleve `\r\n\r\n` dentro, quedarse
    // con el CRLF CRLF partiria por el medio del cuerpo.
    match (crlf, lf) {
        (Some(crlf), Some(lf)) => Some(if crlf.0 <= lf.0 { crlf } else { lf }),
        (Some(found), None) | (None, Some(found)) => Some(found),
        (None, None) => None,
    }
}

/// Acumula el cuerpo hasta tener los bytes que declaro `Content-Length`.
fn read_body(
    reader: &mut impl Read,
    buffered: &[u8],
    start: usize,
    length: usize,
) -> Result<Vec<u8>, HttpError> {
    let mut body = Vec::with_capacity(length);

    // Parte del cuerpo puede haber llegado en la misma lectura que la cabecera.
    let available_end = (start + length).min(buffered.len());
    if start < available_end {
        body.extend_from_slice(&buffered[start..available_end]);
    }

    let mut chunk = [0u8; 8192];
    while body.len() < length {
        match reader.read(&mut chunk) {
            Ok(0) => {
                return Err(HttpError::bad_request(
                    "el cuerpo termino antes de lo que declara Content-Length",
                ));
            }
            Ok(read) => {
                // Nunca se guarda mas de lo declarado: lo que sobre de esta
                // lectura seria una peticion siguiente, y aqui no hay pipelining.
                let missing = length - body.len();
                body.extend_from_slice(&chunk[..read.min(missing)]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(HttpError::from_io(error)),
        }
    }

    Ok(body)
}

/// Fallo al leer o interpretar la peticion.
///
/// No es un error del servidor: se contesta `status` con `{"error": "..."}` y se
/// cierra. No se usa `io::Error` porque perderia el codigo HTTP que hay que
/// devolver.
#[derive(Debug)]
struct HttpError {
    status: u16,
    message: String,
}

impl HttpError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            message: message.into(),
        }
    }

    fn too_large() -> Self {
        Self {
            status: 413,
            message: format!("el cuerpo pasa del tope de {MAX_BODY_BYTES} bytes"),
        }
    }

    fn from_io(error: io::Error) -> Self {
        // Timeout o cliente que corta: el que se fue es el cliente, asi que el
        // 400 es para el si todavia escucha, no un 500 que culpe al servidor.
        Self::bad_request(format!("no pude leer la peticion: {error}"))
    }

    fn into_response(self) -> Response {
        Response::error(self.status, &self.message)
    }
}

/// Respuesta HTTP ya completa, lista para escribir.
struct Response {
    status: u16,
    /// `None` = respuesta sin cuerpo (el `202` de una notificacion).
    content_type: Option<&'static str>,
    body: Vec<u8>,
    /// Cabecera `Allow` de los `405`.
    allow: Option<&'static str>,
}

impl Response {
    fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: Some("application/json"),
            body: body.into().into_bytes(),
            allow: None,
        }
    }

    fn empty(status: u16) -> Self {
        Self {
            status,
            content_type: None,
            body: Vec::new(),
            allow: None,
        }
    }

    /// HTML de la UI. `text/html; charset=utf-8` porque la pagina lleva acentos.
    fn html(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: Some("text/html; charset=utf-8"),
            body: body.into().into_bytes(),
            allow: None,
        }
    }

    /// Texto plano: el markdown del pack, tal cual, sin escapar.
    fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: Some("text/plain; charset=utf-8"),
            body: body.into().into_bytes(),
            allow: None,
        }
    }

    /// Asset embebido con su tipo. Sirve CSS y JS de la UI.
    fn asset(content_type: &'static str, body: &'static str) -> Self {
        Self {
            status: 200,
            content_type: Some(content_type),
            body: body.as_bytes().to_vec(),
            allow: None,
        }
    }

    fn error(status: u16, message: &str) -> Self {
        Self::json(status, json!({ "error": message }).to_string())
    }

    fn method_not_allowed(allowed: &'static str) -> Self {
        let mut response = Self::error(
            405,
            &format!("metodo no soportado: esta ruta solo atiende {allowed}"),
        );
        response.allow = Some(allowed);
        response
    }
}

/// Decide la respuesta a una peticion ya leida.
///
/// Es una funcion pura (no toca el socket) para poder probar el ruteo sin red.
fn respond(request: &Request, settings: &Settings) -> Response {
    match request.path.as_str() {
        "/mcp" => {
            if request.method != "POST" {
                return Response::method_not_allowed("POST");
            }
            if request.content_length.is_none() {
                // Sin `Content-Length` no hay cuerpo declarado: contestar `202`
                // fingiria que se recibio una notificacion que nunca llego.
                return Response::error(411, "POST /mcp necesita `Content-Length`");
            }
            let Ok(body) = std::str::from_utf8(&request.body) else {
                return Response::error(400, "el cuerpo no es UTF-8: JSON-RPC exige UTF-8");
            };
            match crate::handle_line_with(body, settings) {
                Some(response) => Response::json(200, response),
                // Notificacion (sin `id`): JSON-RPC prohibe responderla. stdio
                // tampoco escribe nada; el `202` dice "recibida y no contestada".
                None => Response::empty(202),
            }
        }
        "/health" => {
            if request.method != "GET" {
                return Response::method_not_allowed("GET");
            }
            Response::json(200, health_body())
        }
        // La UI. `/` sirve la pagina autocontenida; `/api/*` son sus datos.
        // `/mcp` sigue siendo el protocolo del agente: la pagina nunca habla JSON-RPC.
        "/" | "/index.html" => {
            if request.method != "GET" {
                return Response::method_not_allowed("GET");
            }
            Response::html(200, PAGE)
        }
        // CSS y JS van en ficheros aparte, y no embebidos en el HTML, para que la CSP
        // pueda prohibir `'unsafe-inline'` de verdad. Un `unsafe-inline` en
        // `script-src` deja la defensa anti-XSS en nada: el CSP tiene que poder
        // bloquear un script inyectado.
        "/app.css" => {
            if request.method != "GET" {
                return Response::method_not_allowed("GET");
            }
            Response::asset("text/css; charset=utf-8", APP_CSS)
        }
        "/app.js" => {
            if request.method != "GET" {
                return Response::method_not_allowed("GET");
            }
            Response::asset("text/javascript; charset=utf-8", APP_JS)
        }
        "/api/review" => {
            if request.method != "GET" {
                return Response::method_not_allowed("GET");
            }
            match review_json(settings) {
                Ok(body) => Response::json(200, body),
                Err(texto) => Response::error(500, &texto),
            }
        }
        "/api/pack" => {
            if request.method != "GET" {
                return Response::method_not_allowed("GET");
            }
            let params = parse_query(&request.query);
            let Some(robot) = params.get("robot") else {
                return Response::error(400, "falta `robot` en la query");
            };
            match pack_for(settings, robot, params.get("db").map(String::as_str)) {
                Ok(texto) => Response::text(200, texto),
                Err(texto) => Response::error(404, &texto),
            }
        }
        unknown => Response::error(404, &format!("ruta desconocida: {unknown}")),
    }
}

/// La pagina de la UI, embebida: el binario es un unico fichero, sin assets sueltos.
const PAGE: &str = include_str!("../assets/index.html");
const APP_CSS: &str = include_str!("../assets/app.css");
const APP_JS: &str = include_str!("../assets/app.js");

/// Query string a pares clave/valor, con `%xx` y `+` decodificados.
fn parse_query(query: &str) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for par in query.split('&').filter(|p| !p.is_empty()) {
        let (clave, valor) = par.split_once('=').unwrap_or((par, ""));
        out.insert(percent_decode(clave), percent_decode(valor));
    }
    out
}

/// Decodifica `%xx` y `+`.
///
/// Se hace a mano para no meter una dependencia por dos rutas: la UI manda los
/// nombres de robot con `encodeURIComponent`, y sin esto un robot con espacios o
/// acentos no se encontraria.
fn percent_decode(texto: &str) -> String {
    let bytes = texto.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    // Un `%` que no es escape se deja tal cual: preferible a perder
                    // el caracter entero.
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// JSON de `GET /api/review`: la misma agregacion que el CLI, una sola verdad.
fn review_json(settings: &Settings) -> Result<String, String> {
    let logs = settings
        .logs
        .as_ref()
        .map(|(dir, max_files)| (dir.as_path(), *max_files));
    let review = match settings.workspace.as_deref() {
        Some(root) => {
            let paths = db::discover_databases(root)
                .map_err(|e| format!("no pude descubrir bases en {}: {e}", root.display()))?;
            // Mismas etiquetas unicas que el CLI: si dos entregas comparten
            // `sandbox\robot.db`, la bandeja no puede fusionarlas.
            let labels = db::labels_for(&paths);
            let mut abiertas = Vec::new();
            for (path, label) in paths.iter().zip(labels) {
                if let Ok(conn) = db::open_readonly(path) {
                    abiertas.push((conn, label));
                }
            }
            let referencias: Vec<_> = abiertas
                .iter()
                .map(|(conn, label)| (conn, label.as_str()))
                .collect();
            rocketdebug_core::review::review_workspace(&referencias, logs)
                .map_err(|e| format!("no pude analizar el workspace: {e}"))?
        }
        None => {
            let path = settings.default_db();
            let conn = db::open_readonly(&path)
                .map_err(|e| format!("no pude abrir {}: {e}", path.display()))?;
            rocketdebug_core::review::review_conn(&conn, &crate::tools::db_label(&path), logs)
                .map_err(|e| format!("no pude analizar la db: {e}"))?
        }
    };

    let rows: Vec<serde_json::Value> = review
        .rows
        .iter()
        .map(|r| {
            json!({
                "robot": r.robot,
                "db": r.db,
                "nodes": r.nodes,
                "accionables": r.accionables,
                "higiene": r.higiene,
                "vivos": r.vivos,
            })
        })
        .collect();
    Ok(json!({
        "source": review.source,
        "dbs": review.dbs,
        "rows": rows,
        "totals": {
            "robots": review.total_robots(),
            "accionables": review.total_accionables(),
            "higiene": review.total_higiene(),
            "vivos": review.total_vivos(),
        },
    })
    .to_string())
}

/// Markdown del pack para la UI.
fn pack_for(settings: &Settings, robot: &str, label: Option<&str>) -> Result<String, String> {
    let path = match label {
        Some(label) => {
            let root = settings
                .workspace
                .as_deref()
                .ok_or("la peticion pide una db concreta y el servidor no tiene workspace")?;
            let paths = db::discover_databases(root)
                .map_err(|e| format!("no pude descubrir bases: {e}"))?;
            let labels = db::labels_for(&paths);
            paths
                .into_iter()
                .zip(labels)
                .find(|(_, candidate)| candidate.as_str() == label)
                .map(|(path, _)| path)
                .ok_or_else(|| format!("no encuentro la db `{label}` en el workspace"))?
        }
        None => settings.default_db(),
    };
    crate::tools::pack_markdown(&path, robot, None, settings)
        .map_err(|e| crate::tools::describe_failure(&e, &path))
}

/// Cuerpo de `GET /health`.
///
/// `name` y `version` son los mismos que `initialize` anuncia en `serverInfo`:
/// quien pregunta por salud y quien negocia el protocolo tienen que ver la misma
/// identidad.
fn health_body() -> String {
    json!({
        "ok": true,
        "name": "rocket_debugger",
        "version": env!("CARGO_PKG_VERSION"),
    })
    .to_string()
}

/// Escribe la respuesta y cierra el lado de escritura.
fn write_response(stream: &mut TcpStream, response: &Response) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        response.status,
        reason(response.status)
    );
    if let Some(allowed) = response.allow {
        head.push_str(&format!("Allow: {allowed}\r\n"));
    }
    if let Some(content_type) = response.content_type {
        head.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
    // Sin keep-alive: una respuesta y se cierra. Asi un cliente que no entienda
    // `Connection: close` no se queda esperando una segunda respuesta que no va
    // a llegar.
    head.push_str("Connection: close\r\n\r\n");

    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()?;
    // Cerrar el lado de escritura antes de soltar el socket evita que en Windows
    // el cliente reciba un RST con la respuesta a medio leer.
    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
}

/// Frase de estado que acompana al codigo.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        _ => "Error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Peticion de ejemplo: cabecera con `Content-Length` y cuerpo con acentos y
    /// enes, que ocupan mas de un byte en UTF-8 (el caso que rompe un lector
    /// ingenuo que cuente caracteres en vez de bytes).
    const BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"sesión de José: ñandú y caña"}}}"#;

    fn request_bytes(body: &str) -> Vec<u8> {
        let mut bytes = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        bytes.extend_from_slice(body.as_bytes());
        bytes
    }

    /// Lector de prueba que entrega como mucho `chunk` bytes por `read`.
    ///
    /// Reproduce un cuerpo que llega partido sin depender del socket ni del
    /// planificador del SO: el troceado es determinista.
    struct Trickle {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
    }

    impl Trickle {
        fn new(data: Vec<u8>, chunk: usize) -> Self {
            Self {
                data,
                pos: 0,
                chunk,
            }
        }
    }

    impl Read for Trickle {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let pending = self.data.len() - self.pos;
            let count = pending.min(self.chunk).min(out.len());
            out[..count].copy_from_slice(&self.data[self.pos..self.pos + count]);
            self.pos += count;
            Ok(count)
        }
    }

    fn read_full(bytes: Vec<u8>, chunk: usize) -> Request {
        let mut reader = Trickle::new(bytes, chunk);
        read_request(&mut reader)
            .expect("la peticion se parsea")
            .expect("hay peticion")
    }

    fn request(method: &str, path: &str, body: &str) -> Request {
        Request {
            method: method.to_string(),
            path: path.to_string(),
            query: String::new(),
            content_length: Some(body.len()),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn el_tope_de_cuerpo_es_un_mib() {
        assert_eq!(MAX_BODY_BYTES, 1024 * 1024);
    }

    #[test]
    fn parsea_la_linea_de_peticion_y_las_cabeceras() {
        let request = read_full(request_bytes(BODY), 4096);

        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/mcp");
        assert_eq!(request.content_length, Some(BODY.len()));
        assert_eq!(
            String::from_utf8(request.body).expect("el cuerpo es UTF-8"),
            BODY,
            "el cuerpo llega entero y sin tocar"
        );
    }

    #[test]
    fn el_cuerpo_puede_llegar_en_varios_reads() {
        // 1 byte por lectura corta tambien dentro de la cabecera; 2 y 3 obligan
        // a acumular trozos mas grandes que un caracter UTF-8.
        for chunk in [1usize, 2, 3, 7, 64] {
            let request = read_full(request_bytes(BODY), chunk);
            assert_eq!(request.method, "POST", "trozo de {chunk}");
            assert_eq!(
                String::from_utf8(request.body).expect("el cuerpo es UTF-8"),
                BODY,
                "el cuerpo troceado de {chunk} bytes se acumula hasta estar completo"
            );
        }
    }

    #[test]
    fn un_cuerpo_mas_largo_que_lo_declarado_se_recorta() {
        // Con pipelining llegarian dos peticiones en el mismo socket: aqui solo
        // se consume la primera y lo que sobra no puede colarse en el cuerpo.
        let mut bytes = request_bytes(BODY);
        bytes.extend_from_slice(b"POST /mcp HTTP/1.1\r\n\r\n");
        let request = read_full(bytes, 4096);

        assert_eq!(
            String::from_utf8(request.body).expect("el cuerpo es UTF-8"),
            BODY
        );
    }

    #[test]
    fn acepta_el_fin_de_cabecera_solo_con_lf_y_descarta_la_query() {
        let mut reader = Trickle::new(
            b"GET /health?probe=1 HTTP/1.1\nHost: 127.0.0.1\n\n".to_vec(),
            4096,
        );
        let request = read_request(&mut reader)
            .expect("se parsea")
            .expect("hay peticion");

        assert_eq!(request.method, "GET");
        assert_eq!(
            request.path, "/health",
            "la query no forma parte de la ruta"
        );
        assert_eq!(request.content_length, None, "GET no declara cuerpo");
        assert!(request.body.is_empty());
    }

    #[test]
    fn el_fin_de_cabecera_es_el_primero_y_no_uno_de_dentro_del_cuerpo() {
        // Cabecera terminada en LF y cuerpo que lleva `\r\n\r\n` dentro: cortar
        // por el CRLF CRLF partiria el cuerpo y el JSON llegaria mutilado.
        let mut bytes = b"POST /mcp HTTP/1.1\nContent-Length: 10\n\n".to_vec();
        bytes.extend_from_slice(b"a\r\n\r\nb\r\n\r\n");
        let request = read_full(bytes, 3);

        assert_eq!(request.content_length, Some(10));
        assert_eq!(request.body, b"a\r\n\r\nb\r\n\r\n");
    }

    #[test]
    fn un_cuerpo_declarado_demasiado_grande_da_413_sin_leerlo() {
        let bytes = format!(
            "POST /mcp HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        )
        .into_bytes();

        // El `Trickle` no aporta cuerpo: si el parser intentase leerlo, el error
        // seria 400 ("termino antes"), no 413.
        let error = read_request(&mut Trickle::new(bytes, 8)).expect_err("tiene que rechazar");
        assert_eq!(error.status, 413);
        assert!(
            error.message.contains("tope"),
            "el mensaje dice el tope: {}",
            error.message
        );
    }

    #[test]
    fn un_cuerpo_truncado_da_400() {
        let bytes = b"POST /mcp HTTP/1.1\r\nContent-Length: 64\r\n\r\n{\"jsonrpc\"".to_vec();
        let error = read_request(&mut Trickle::new(bytes, 3)).expect_err("tiene que fallar");
        assert_eq!(error.status, 400);
    }

    #[test]
    fn una_cabecera_sin_dos_puntos_da_400() {
        let bytes = b"POST /mcp HTTP/1.1\r\nContent-Length: 2\r\nRota\r\n\r\n{}".to_vec();
        let error = read_request(&mut Trickle::new(bytes, 5)).expect_err("tiene que fallar");
        assert_eq!(error.status, 400);
    }

    #[test]
    fn un_content_length_que_no_es_numero_da_400() {
        let bytes = b"POST /mcp HTTP/1.1\r\nContent-Length: mucho\r\n\r\n".to_vec();
        let error = read_request(&mut Trickle::new(bytes, 9)).expect_err("tiene que fallar");
        assert_eq!(error.status, 400);
    }

    #[test]
    fn una_conexion_vacia_no_es_una_peticion() {
        assert!(
            read_request(&mut Trickle::new(Vec::new(), 4))
                .expect("sin peticion no hay error")
                .is_none(),
            "un cliente que cierra sin escribir no merece respuesta"
        );
    }

    #[test]
    fn el_ruteo_responde_200_405_y_404() {
        let settings = Settings::hermetic();

        assert_eq!(
            respond(&request("GET", "/health", ""), &settings).status,
            200
        );
        let no_permitido = respond(&request("PUT", "/mcp", "{}"), &settings);
        assert_eq!(no_permitido.status, 405);
        assert_eq!(no_permitido.allow, Some("POST"));
        assert_eq!(
            respond(&request("POST", "/health", "{}"), &settings).status,
            405
        );
        assert_eq!(respond(&request("GET", "/otra", ""), &settings).status, 404);
    }

    #[test]
    fn una_notificacion_da_202_y_sin_cuerpo() {
        let body = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let response = respond(&request("POST", "/mcp", body), &Settings::hermetic());

        assert_eq!(response.status, 202);
        assert!(response.body.is_empty(), "202 sin cuerpo");
        assert_eq!(response.content_type, None);
    }

    #[test]
    fn el_json_roto_lo_contesta_handle_line_con_200() {
        let response = respond(
            &request("POST", "/mcp", "esto no es json {{{"),
            &Settings::hermetic(),
        );

        assert_eq!(
            response.status, 200,
            "el fallo de parseo es JSON-RPC (-32700), no HTTP"
        );
        let value: serde_json::Value =
            serde_json::from_slice(&response.body).expect("la respuesta es JSON");
        assert_eq!(value["error"]["code"], -32700);
    }

    #[test]
    fn un_post_sin_content_length_da_411() {
        let sin_cuerpo = Request {
            method: "POST".to_string(),
            path: "/mcp".to_string(),
            query: String::new(),
            content_length: None,
            body: Vec::new(),
        };

        assert_eq!(respond(&sin_cuerpo, &Settings::hermetic()).status, 411);
    }

    #[test]
    fn un_cuerpo_que_no_es_utf8_da_400() {
        let mut request = request("POST", "/mcp", "");
        request.body = vec![0xff, 0xfe, 0xfd];

        assert_eq!(respond(&request, &Settings::hermetic()).status, 400);
    }

    #[test]
    fn el_health_lleva_ok_nombre_y_version() {
        let value: serde_json::Value =
            serde_json::from_str(&health_body()).expect("el cuerpo es JSON");

        assert_eq!(value["ok"], true);
        assert_eq!(value["name"], "rocket_debugger");
        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn solo_se_acepta_loopback() {
        let loopback: SocketAddr = "127.0.0.1:8080".parse().expect("direccion valida");
        let ipv6: SocketAddr = "[::1]:8080".parse().expect("direccion valida");
        let abierta: SocketAddr = "0.0.0.0:8080".parse().expect("direccion valida");

        assert!(is_loopback(loopback));
        assert!(is_loopback(ipv6));
        assert!(!is_loopback(abierta), "0.0.0.0 nunca");
    }
}
