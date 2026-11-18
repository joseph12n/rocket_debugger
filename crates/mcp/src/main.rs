//! Servidor MCP de `rocket_debugger`.
//!
//! Dos transportes, el mismo despacho:
//!
//! - **sin argumentos, stdio**: lee una linea, la despacha y escribe la
//!   respuesta. Es lo que usa DSH: el cliente arranca el proceso.
//! - **`--http <PUERTO>`** (o `ROCKETDEBUG_MCP_HTTP`): la app es el servidor de
//!   una sola instancia y el agente se conecta a `127.0.0.1:<PUERTO>/mcp`.
//!
//! `main` solo hace E/S; toda la logica (y los tests) vive en la libreria.
//!
//! stdout es **solo protocolo**: cualquier diagnostico va a stderr, porque una
//! linea suelta en stdout rompe la sesion del cliente por stdio.

use std::io::{self, BufRead, Write};
use std::net::{SocketAddr, TcpListener};
use std::process::ExitCode;

use rocketdebug_mcp::{handle_line_with, serve, Settings};

/// Variable de entorno equivalente a `--http`.
const HTTP_ENV: &str = "ROCKETDEBUG_MCP_HTTP";

/// Ayuda. Va a stderr porque stdout es del protocolo.
const USAGE: &str = "\
uso: rocketdebug-mcp [--http <PUERTO>]

  (sin --http)        habla MCP por stdin/stdout
  --http <PUERTO>     escucha HTTP en 127.0.0.1:<PUERTO> (ruta /mcp)
  -h, --help          esta ayuda

ROCKETDEBUG_MCP_HTTP=<PUERTO> equivale a --http cuando no se pasa el flag.";

/// Como se ha pedido arrancar el servidor.
enum Mode {
    Stdio,
    Http(u16),
    Help,
}

fn main() -> ExitCode {
    // El entorno se resuelve una vez: la db de la sesion no cambia a mitad de
    // conversacion aunque alguien toque ROCKETBOT_DB por debajo.
    let settings = Settings::from_env();

    match parse_mode() {
        Ok(Mode::Http(port)) => serve_http(port, settings),
        Ok(Mode::Stdio) => serve_stdio(&settings),
        Ok(Mode::Help) => {
            eprintln!("{USAGE}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            // Codigo 2 = uso invalido (el 1 queda para fallos del servidor).
            ExitCode::from(2)
        }
    }
}

/// Que transporte pide la linea de comandos, si pide alguno.
///
/// El flag manda sobre la variable de entorno: lo explicito de la invocacion no
/// puede quedar pisado por una variable que quedo puesta de una prueba anterior.
fn parse_mode() -> Result<Mode, String> {
    let mut requested: Option<String> = None;
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        if arg == "--http" {
            let Some(value) = args.next() else {
                return Err("`--http` necesita un puerto (0-65535)".to_string());
            };
            requested = Some(value);
        } else if let Some(value) = arg.strip_prefix("--http=") {
            requested = Some(value.to_string());
        } else if arg == "-h" || arg == "--help" {
            return Ok(Mode::Help);
        } else {
            return Err(format!("argumento desconocido: `{arg}`"));
        }
    }

    if let Some(value) = requested {
        return parse_port(&value).map(Mode::Http);
    }

    match std::env::var(HTTP_ENV) {
        Ok(value) if !value.trim().is_empty() => parse_port(value.trim()).map(Mode::Http),
        _ => Ok(Mode::Stdio),
    }
}

/// Puerto de escucha, con un mensaje que diga que se esperaba.
fn parse_port(value: &str) -> Result<u16, String> {
    value.trim().parse::<u16>().map_err(|_| {
        format!("`{value}` no es un puerto valido: tiene que ser un numero entre 0 y 65535")
    })
}

/// Levanta el transporte HTTP local.
///
/// El bind se hace **aqui** y solo a `127.0.0.1`: el motor lee `robot.db` sin
/// autenticacion, asi que escuchar en `0.0.0.0` seria repartir el disco del
/// usuario por la red local.
fn serve_http(port: u16, settings: Settings) -> ExitCode {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = match TcpListener::bind(address) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("rocket_debugger MCP: no pude escuchar en {address}: {error}");
            if error.kind() == io::ErrorKind::AddrInUse {
                eprintln!("el puerto {port} ya esta ocupado; arranca con `--http <otro-puerto>`");
            }
            return ExitCode::FAILURE;
        }
    };

    // Se anuncia la direccion real, no la pedida: con `--http 0` el SO elige el
    // puerto y, si no se dice cual, nadie sabria donde conectar.
    let local = listener.local_addr().unwrap_or(address);
    eprintln!(
        "rocket_debugger MCP v{} | http://{local}/mcp | db {} (solo lectura)",
        env!("CARGO_PKG_VERSION"),
        settings.default_db().display()
    );

    match serve(listener, settings) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rocket_debugger MCP: el servidor HTTP paro: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Transporte stdio: una linea, una respuesta, hasta EOF.
fn serve_stdio(settings: &Settings) -> ExitCode {
    let logs = match &settings.logs {
        Some((dir, _)) => dir.display().to_string(),
        None => "sin logs".to_string(),
    };
    eprintln!(
        "rocket_debugger MCP v{} | db {} | logs {}",
        env!("CARGO_PKG_VERSION"),
        settings.default_db().display(),
        logs
    );

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    // Sin EOF no hay otra senal de parada: el cliente cierra stdin y el bucle
    // termina. Un `write` roto (cliente ido) tambien corta en vez de girar en
    // vacio.
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Some(response) = handle_line_with(&line, settings) else {
            continue;
        };
        if writeln!(out, "{response}")
            .and_then(|()| out.flush())
            .is_err()
        {
            break;
        }
    }

    ExitCode::SUCCESS
}
