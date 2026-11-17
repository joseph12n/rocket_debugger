//! CLI de rocket_debugger.
//!
//! Nunca escribe en `robot.db`: la herramienta detecta, el agente edita.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};
use rocketdebug_core::{build_from_conn, db};

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
            let pack = build_from_conn(&conn, &robot, snapshot, &label)?;
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
fn db_label(path: &Path) -> String {
    let file = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "robot.db".to_string());
    match path.parent().and_then(|p| p.file_name()) {
        Some(dir) => format!("{}\\{}", dir.to_string_lossy(), file),
        None => file,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_etiqueta_no_filtra_la_ruta_absoluta() {
        let p = Path::new(r"C:\Users\josep\proyecto\entrega\robot_run.db");
        assert_eq!(db_label(p), "entrega\\robot_run.db");
    }
}
