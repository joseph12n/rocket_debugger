//! Servidor MCP de `rocket_debugger` por stdio.
//!
//! `main` solo hace E/S: lee una linea, la despacha y escribe la respuesta.
//! Toda la logica (y los tests) vive en la libreria, para poder probar el
//! protocolo sin arrancar un proceso.
//!
//! stdout es **solo protocolo**: cualquier diagnostico va a stderr, porque una
//! linea suelta en stdout rompe la sesion del cliente.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use rocketdebug_mcp::{handle_line_with, Settings};

fn main() -> ExitCode {
    // El entorno se resuelve una vez: la db de la sesion no cambia a mitad de
    // conversacion aunque alguien toque ROCKETBOT_DB por debajo.
    let settings = Settings::from_env();
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
        let Some(response) = handle_line_with(&line, &settings) else {
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
