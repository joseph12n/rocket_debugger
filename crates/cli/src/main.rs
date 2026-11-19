use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};
use rocketdebug_core::analysis::{analyse_with_workspace_at_and_paths, load_workspace};
use rocketdebug_core::paths::LocalPaths;
use rocketdebug_core::{build_from_conn_with_logs_and_paths, db, logs};

mod review;
mod watch;

#[derive(Parser, Debug)]
#[command(
    name = "rocketdebug",
    version,
    about = "Depurador de flujos Rocketbot: context pack y atribucion de errores"
)]
struct Cli {
    #[arg(
        help = "Ruta a robot.db. Por defecto la del Studio local, o ROCKETBOT_DB.",
        long,
        global = true,
        env = "ROCKETBOT_DB",
        value_name = "RUTA"
    )]
    db: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    #[command(about = "Lista los robots de la base de datos con sus versiones.")]
    List {
        #[arg(
            help = "Directorio del workspace: lista todas las dbs de robots bajo la raiz.",
            long,
            value_name = "DIR"
        )]
        workspace: Option<PathBuf>,
    },

    #[command(about = "Emite el context pack de un robot.")]
    Pack {
        robot: String,
        #[arg(help = "Snapshot concreto (por defecto, el ultimo).", long)]
        snapshot: Option<i64>,
        #[arg(help = "Imprime la contabilidad de tokens en stderr.", long)]
        stats: bool,
        #[arg(
            help = "Analiza el grafo contra todas las dbs de robots bajo este directorio.",
            long,
            value_name = "DIR"
        )]
        workspace: Option<PathBuf>,
    },

    #[command(about = "Busca bases de datos de robots bajo una raiz.")]
    Dbs {
        #[arg(long, default_value = r"C:\rocketbot_bots")]
        root: PathBuf,
    },

    #[command(
        about = "Vista de bandeja de entrada: que robots estan mal y cuanto, en una pantalla."
    )]
    Review {
        #[arg(
            help = "Recorre todas las dbs de robots bajo este directorio.",
            long,
            value_name = "DIR"
        )]
        workspace: Option<PathBuf>,
        #[arg(help = "Maximo de robots en la tabla; el resto se resume en una linea.", long, default_value_t = review::DEFAULT_LIMIT, value_name = "N")]
        limit: usize,
        #[arg(help = "No lee `C:\\Rocketbot\\logs`: mas rapido y hermetico.", long)]
        no_logs: bool,
    },

    #[command(about = "Vigila la db y alimenta el registro de problemas sin intervencion.")]
    Watch {
        #[arg(
            help = "Recorre todas las dbs de robots bajo este directorio; sin el flag, solo la db de `--db`.",
            long,
            value_name = "DIR"
        )]
        workspace: Option<PathBuf>,
        #[arg(
            help = "Segundos entre pasadas.",
            long,
            default_value_t = watch::DEFAULT_INTERVAL_SECS,
            value_name = "S",
            allow_negative_numbers = true
        )]
        interval: i64,
        #[arg(help = "Una sola pasada y sale.", long)]
        once: bool,
        #[arg(
            help = "Fichero del registro; por defecto `ROCKETDEBUG_REGISTRY` o el del motor.",
            long,
            value_name = "RUTA"
        )]
        registry: Option<PathBuf>,
        #[arg(help = "No lee `C:\\Rocketbot\\logs`.", long)]
        no_logs: bool,
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
            let labels = db::labels_for(&found);
            for (p, label) in found.into_iter().zip(labels) {
                println!("  {label}  ({})", p.display());
            }
            Ok(())
        }
        Cmd::List { workspace } => match workspace {
            None => {
                let conn = db::open_readonly(&db_path)
                    .with_context(|| format!("abriendo {}", db_path.display()))?;
                print_robots(&db_label(&db_path), &db::list_robots(&conn)?);
                Ok(())
            }
            Some(dir) => list_workspace(&dir),
        },
        Cmd::Pack {
            robot,
            snapshot,
            stats,
            workspace,
        } => {
            let conn = db::open_readonly(&db_path)
                .with_context(|| format!("abriendo {}", db_path.display()))?;
            let logs_dir = Path::new(logs::DEFAULT_LOGS_DIR);
            let log_source = logs_dir
                .is_dir()
                .then_some((logs_dir, rocketdebug_core::pack::DEFAULT_LOG_FILES));
            let path_probe = LocalPaths::from_env(&db_path);

            let pack = match &workspace {
                None => {
                    let label = db::label(&db_path);
                    build_from_conn_with_logs_and_paths(
                        &conn,
                        &robot,
                        snapshot,
                        &label,
                        log_source,
                        &path_probe,
                    )?
                }
                Some(dir) => {
                    let discovered = db::discover_databases(dir)
                        .with_context(|| format!("descubriendo bases en {}", dir.display()))?;
                    let mut paths = vec![db_path.clone()];
                    paths.extend(
                        discovered
                            .into_iter()
                            .filter(|path| !same_file(path, &db_path)),
                    );
                    let labels = db::labels_for(&paths);
                    let label = labels[0].clone();

                    let mut opened = Vec::new();
                    for (path, ws_label) in paths.iter().zip(labels.iter()).skip(1) {
                        match db::open_readonly(path) {
                            Ok(ws_conn) => opened.push((ws_conn, ws_label.clone())),
                            Err(e) => eprintln!("aviso: {e}"),
                        }
                    }

                    let mut refs = Vec::with_capacity(opened.len() + 1);
                    refs.push((&conn, label.as_str()));
                    for (ws_conn, ws_label) in &opened {
                        refs.push((ws_conn, ws_label.as_str()));
                    }

                    let analysis = analyse_with_workspace_at_and_paths(
                        &refs,
                        &label,
                        &robot,
                        snapshot,
                        log_source,
                        &path_probe,
                    )?;
                    eprintln!(
                        "workspace: {} dbs | {} llamadas ambiguas | {} llamadas sin destino",
                        refs.len(),
                        analysis.ambiguous_calls.len(),
                        analysis.missing_calls.len()
                    );
                    for call in &analysis.ambiguous_calls {
                        eprintln!("  ambigua: {call}");
                    }
                    for call in &analysis.missing_calls {
                        eprintln!("  sin destino: {call}");
                    }

                    let mut pack = build_from_conn_with_logs_and_paths(
                        &conn,
                        &robot,
                        snapshot,
                        &label,
                        None,
                        &path_probe,
                    )?;
                    pack.analysis = analysis;
                    pack
                }
            };

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
        Cmd::Review {
            workspace,
            limit,
            no_logs,
        } => {
            let log_source = if no_logs {
                None
            } else {
                let logs_dir = Path::new(logs::DEFAULT_LOGS_DIR);
                logs_dir
                    .is_dir()
                    .then_some((logs_dir, rocketdebug_core::pack::DEFAULT_LOG_FILES))
            };
            review::run(&db_path, workspace.as_deref(), limit, log_source)
        }
        Cmd::Watch {
            workspace,
            interval,
            once,
            registry,
            no_logs,
        } => {
            let interval = watch::interval_from_secs(interval)?;
            let log_source = if no_logs {
                None
            } else {
                let logs_dir = Path::new(logs::DEFAULT_LOGS_DIR);
                logs_dir.is_dir().then(|| {
                    (
                        logs_dir.to_path_buf(),
                        rocketdebug_core::pack::DEFAULT_LOG_FILES,
                    )
                })
            };
            let options = watch::WatchOptions {
                db: db_path,
                workspace,
                interval,
                once,
                registry: watch::resolve_registry(registry),
                log_source,
            };
            let mut stdout = std::io::stdout();
            watch::run(&options, &mut stdout)
        }
    }
}

fn list_workspace(dir: &Path) -> anyhow::Result<()> {
    let found = db::discover_databases(dir)
        .with_context(|| format!("descubriendo bases en {}", dir.display()))?;
    if found.is_empty() {
        println!("0 bases con tabla bots en {}", dir.display());
        return Ok(());
    }

    let mut opened = Vec::new();
    let mut rows = 0usize;
    let labels = db::labels_for(&found);
    for (path, label) in found.into_iter().zip(labels) {
        let conn = match db::open_readonly(&path) {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("aviso: {e}");
                continue;
            }
        };
        let robots = db::list_robots(&conn)?;
        rows += robots.len();
        print_robots(&label, &robots);
        opened.push((conn, label));
    }

    let mut refs = Vec::new();
    for (conn, label) in &opened {
        refs.push((conn, label.as_str()));
    }
    let hechos = load_workspace(&refs)?.len();
    println!();
    println!(
        "workspace: {} dbs | {} filas (una por robot y db) | {} hechos distintos \
         (dedupe por contenido)",
        refs.len(),
        rows,
        hechos
    );
    Ok(())
}

fn print_robots(label: &str, robots: &[db::RobotSummary]) {
    println!("{label}  |  {} robots", robots.len());
    for r in robots {
        println!(
            "  {:<28} {:>3} versiones   ultimo id {:<5}",
            r.name, r.versions, r.latest_id
        );
    }
}

pub(crate) fn db_label(path: &Path) -> String {
    db::label(path)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
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
        assert_eq!(
            db_label(Path::new("/home/josep/proyecto/entrega/robot_run.db")),
            "entrega\\robot_run.db"
        );
        assert_eq!(db_label(Path::new("robot.db")), "robot.db");
    }

    #[test]
    fn la_misma_db_por_dos_rutas_no_se_duplica() {
        assert!(same_file(Path::new("."), Path::new("./")));
        assert!(!same_file(
            Path::new("."),
            Path::new("no-existe-en-este-directorio.db")
        ));
    }
}
