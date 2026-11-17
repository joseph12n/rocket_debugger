//! CLI de rocket_debugger.
//!
//! Nunca escribe en `robot.db`: la herramienta detecta, el agente edita.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};
use rocketdebug_core::{build_from_conn_with_logs, db, logs};

#[derive(Parser, Debug)]
#[command(
    name = "rocketdebug",
    version,
    about = "Depurador de flujos Rocketbot: context pack y atribucion de errores"
)]
struct Cli {
    /// Ruta a robot.db. Por defecto la del Studio local, o ROCKETBOT_DB.
    #[arg(long, global = true, env = "ROCKETBOT_DB", value_name = "RUTA")]
    db: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Lista los robots de la base de datos con sus versiones.
    List,

    /// Emite el context pack de un robot.
    Pack {
        robot: String,
        /// Snapshot concreto (por defecto, el ultimo).
        #[arg(long)]
        snapshot: Option<i64>,
        /// Imprime la contabilidad de tokens en stderr.
        #[arg(long)]
        stats: bool,
    },

    /// Busca bases de datos de robots bajo una raiz.
    Dbs {
        #[arg(long, default_value = r"C:\rocketbot_bots")]
        root: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let db_path = cli.db.unwrap_or_else(|| PathBuf::from(db::DEFAULT_DB));

    match cli.cmd {
        Cmd::Dbs { root } => {
            let found = db::discover_databases(&root)
                .with_context(|| format!("descubriendo bases en {}", root.display()))?;
            println!("{} bases con tabla bots:", found.len());
            for p in found {
                println!("  {}  ({})", db_label(&p), p.display());
            }
            Ok(())
        }
        Cmd::List => {
            let conn = db::open_readonly(&db_path)
                .with_context(|| format!("abriendo {}", db_path.display()))?;
            let robots = db::list_robots(&conn)?;
            println!("{}  |  {} robots", db_label(&db_path), robots.len());
            for r in robots {
                println!(
                    "  {:<28} {:>3} versiones   ultimo id {:<5}",
                    r.name, r.versions, r.latest_id
                );
            }
            Ok(())
        }
        Cmd::Pack {
            robot,
            snapshot,
            stats,
        } => {
            let conn = db::open_readonly(&db_path)
                .with_context(|| format!("abriendo {}", db_path.display()))?;
            let label = db_label(&db_path);
            // Los logs son responsabilidad del CLI, no del motor: asi la suite de
            // tests del crate core queda hermética y este binario sigue leyendo el
            // entorno real cuando existe.
            let logs_dir = std::path::Path::new(logs::DEFAULT_LOGS_DIR);
            let log_source = logs_dir
                .is_dir()
                .then_some((logs_dir, rocketdebug_core::pack::DEFAULT_LOG_FILES));
            let pack = build_from_conn_with_logs(&conn, &robot, snapshot, &label, log_source)?;
            let md = pack.render_markdown();
            print!("{md}");

            if stats {
                let s = pack.sizing(&md);
                let ahorro = 100.0 - (s.render_tokens as f64 / s.raw_tokens.max(1) as f64 * 100.0);
                eprintln!(
                    "crudo {} tok ({} car.) | render {} tok ({} car.) | {:.1}x | -{:.0}%",
                    s.raw_tokens,
                    s.raw_chars,
                    s.render_tokens,
                    s.render_chars,
                    s.ratio(),
                    ahorro
                );
            }
            Ok(())
        }
    }
}

/// Etiqueta de la db sin ruta absoluta: `BBRE2\robot_run.db`.
///
/// Se normalizan las dos formas de separador a proposito: `Path` solo reconoce
/// `\` en Windows, y con la implementacion ingenua la funcion devolvia la ruta
/// absoluta completa cuando se compilaba en Linux (lo detecto CI).
fn db_label(path: &Path) -> String {
    let normalizado = path.to_string_lossy().replace('\\', "/");
    let mut partes: Vec<&str> = normalizado.split('/').filter(|p| !p.is_empty()).collect();
    let Some(file) = partes.pop() else {
        return "robot.db".to_string();
    };
    match partes.pop() {
        Some(dir) => format!("{dir}\\{file}"),
        None => file.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_etiqueta_no_filtra_la_ruta_absoluta() {
        assert_eq!(
            db_label(Path::new(r"C:\Users\josep\proyecto\entrega\robot_run.db")),
            "entrega\\robot_run.db"
        );
        // La misma ruta en forma posix: el resultado no puede depender del host.
        assert_eq!(
            db_label(Path::new("/home/josep/proyecto/entrega/robot_run.db")),
            "entrega\\robot_run.db"
        );
        assert_eq!(db_label(Path::new("robot.db")), "robot.db");
    }
}
