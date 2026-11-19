use std::io::{self, BufRead, Write};
use std::net::{SocketAddr, TcpListener};
use std::process::ExitCode;

use rocketdebug_mcp::{handle_line_with, serve, Settings};

const HTTP_ENV: &str = "ROCKETDEBUG_MCP_HTTP";

const USAGE: &str = "\
uso: rocketdebug-mcp [--http <PUERTO>]

  (sin --http)        habla MCP por stdin/stdout
  --http <PUERTO>     escucha HTTP en 127.0.0.1:<PUERTO> (ruta /mcp)
  -h, --help          esta ayuda

ROCKETDEBUG_MCP_HTTP=<PUERTO> equivale a --http cuando no se pasa el flag.";

enum Mode {
    Stdio,
    Http(u16),
    Help,
}

fn main() -> ExitCode {
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
            ExitCode::from(2)
        }
    }
}

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

fn parse_port(value: &str) -> Result<u16, String> {
    value.trim().parse::<u16>().map_err(|_| {
        format!("`{value}` no es un puerto valido: tiene que ser un numero entre 0 y 65535")
    })
}

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
