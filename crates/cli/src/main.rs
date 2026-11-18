//! CLI de rocket_debugger.
//!
//! Nunca escribe en `robot.db`: la herramienta detecta, el agente edita.

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
    /// Ruta a robot.db. Por defecto la del Studio local, o ROCKETBOT_DB.
    #[arg(long, global = true, env = "ROCKETBOT_DB", value_name = "RUTA")]
    db: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Lista los robots de la base de datos con sus versiones.
    List {
        /// Directorio del workspace: lista todas las dbs de robots bajo la raiz.
        ///
        /// Sin el flag se lista solo la db de `--db` (comportamiento historico).
        #[arg(long, value_name = "DIR")]
        workspace: Option<PathBuf>,
    },

    /// Emite el context pack de un robot.
    Pack {
        robot: String,
        /// Snapshot concreto (por defecto, el ultimo).
        #[arg(long)]
        snapshot: Option<i64>,
        /// Imprime la contabilidad de tokens en stderr.
        #[arg(long)]
        stats: bool,
        /// Analiza el grafo contra todas las dbs de robots bajo este directorio.
        ///
        /// La db de `--db` es la del robot analizado y va siempre en el workspace;
        /// las demas se descubren con `db::discover_databases`, que ya excluye
        /// `backups/`. Sin el flag el grafo se alimenta con una sola db, y entonces
        /// una llamada a un robot que vive en otra db no se puede distinguir de una
        /// llamada a un robot inexistente.
        #[arg(long, value_name = "DIR")]
        workspace: Option<PathBuf>,
    },

    /// Busca bases de datos de robots bajo una raiz.
    Dbs {
        #[arg(long, default_value = r"C:\rocketbot_bots")]
        root: PathBuf,
    },

    /// Vista de bandeja de entrada: que robots estan mal y cuanto, en una pantalla.
    ///
    /// Analiza cada robot de la db con el mismo motor que el context pack y resume
    /// sus hallazgos accionables, su higiene y sus errores vivos, ordenado por lo
    /// que hay que atender primero. Evita pedir un pack por robot para decidir por
    /// donde empezar.
    Review {
        /// Recorre todas las dbs de robots bajo este directorio.
        ///
        /// Mismos dbs que ve `pack --workspace` (`db::discover_databases`, que ya
        /// excluye `backups/`). Sin el flag solo se analiza la db de `--db`.
        #[arg(long, value_name = "DIR")]
        workspace: Option<PathBuf>,
        /// Maximo de robots en la tabla; el resto se resume en una linea.
        #[arg(long, default_value_t = review::DEFAULT_LIMIT, value_name = "N")]
        limit: usize,
        /// No lee `C:\Rocketbot\logs`: mas rapido y hermetico.
        ///
        /// Los errores vivos salen a cero, igual que en un pack sin logs: el
        /// analisis del codigo no depende de que este entorno tenga logs.
        #[arg(long)]
        no_logs: bool,
    },

    /// Vigila la db y alimenta el registro de problemas sin intervencion.
    ///
    /// Es el eslabon que convierte el registro en memoria pasiva: `problems_record`
    /// solo acumula historia cuando un agente se acuerda de llamarlo, y en la
    /// practica se olvida. Aqui se mira la db cada `--interval` segundos y se
    /// registran los errores de los robots que cambiaron (comparando el ultimo id de
    /// `bots` con el de la pasada anterior). Silencio = no cambio nada.
    ///
    /// Un vigilante con la db en produccion debe correr en una sola instancia: dos no
    /// corrompen el registro (usa transacciones y bloqueo), pero duplican trabajo y
    /// pueden contar dos veces el mismo defecto.
    ///
    /// `--once` hace una sola pasada y sale: es lo que se pone en una tarea
    /// programada y lo que se puede probar sin esperar.
    Watch {
        /// Recorre todas las dbs de robots bajo este directorio, como
        /// `review --workspace`; sin el flag se vigila solo la db de `--db`.
        #[arg(long, value_name = "DIR")]
        workspace: Option<PathBuf>,
        /// Segundos entre pasadas.
        #[arg(
            long,
            default_value_t = watch::DEFAULT_INTERVAL_SECS,
            value_name = "S",
            allow_negative_numbers = true
        )]
        interval: i64,
        /// Una sola pasada y sale.
        #[arg(long)]
        once: bool,
        /// Fichero del registro; por defecto `ROCKETDEBUG_REGISTRY` o el del motor.
        #[arg(long, value_name = "RUTA")]
        registry: Option<PathBuf>,
        /// No lee `C:\Rocketbot\logs`.
        ///
        /// Sin logs no hay errores de ejecucion: el registro no recibe observaciones
        /// y la salida lo dice en vez de fingir que el robot esta limpio.
        #[arg(long)]
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
            for p in found {
                println!("  {}  ({})", db_label(&p), p.display());
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
            let label = db_label(&db_path);
            // Los logs son responsabilidad del CLI, no del motor: asi la suite de
            // tests del crate core queda hermética y este binario sigue leyendo el
            // entorno real cuando existe.
            let logs_dir = Path::new(logs::DEFAULT_LOGS_DIR);
            let log_source = logs_dir
                .is_dir()
                .then_some((logs_dir, rocketdebug_core::pack::DEFAULT_LOG_FILES));
            // El CLI si comprueba el dispositivo: `RUTA_MIGRADA` necesita saber
            // si la ruta existe aqui. El motor puro no lo hace por su cuenta.
            let path_probe = LocalPaths::from_env(&db_path);

            let pack = match &workspace {
                None => build_from_conn_with_logs_and_paths(
                    &conn,
                    &robot,
                    snapshot,
                    &label,
                    log_source,
                    &path_probe,
                )?,
                Some(dir) => {
                    // La db pedida va primera y no se repite: es la etiqueta con la
                    // que viaja el hecho del robot analizado (ver
                    // `analyse_with_workspace`).
                    let mut opened = Vec::new();
                    for path in db::discover_databases(dir)
                        .with_context(|| format!("descubriendo bases en {}", dir.display()))?
                    {
                        if same_file(&path, &db_path) {
                            continue;
                        }
                        match db::open_readonly(&path) {
                            Ok(ws_conn) => opened.push((ws_conn, db_label(&path))),
                            // Una db ilegible no puede tumbar el pack: se avisa y el
                            // analisis sigue con las demas.
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

                    // `pack` cablea su propio analisis de una sola db y no acepta uno
                    // externo sin cambiar su firma, que no es de este issue: se
                    // construye sin logs y se sustituye por el del workspace, que si
                    // los trae. Asi los logs se leen una sola vez.
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
            // El motor no lee logs por su cuenta: este CLI decide si este entorno
            // los tiene. `--no-logs` corta antes de tocar el disco, que es lo que
            // hace el barrido hermetico (y rapido).
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
            // Se valida antes de tocar nada: un intervalo de cero es un bucle en
            // vacio, no un vigilante mas rapido.
            let interval = watch::interval_from_secs(interval)?;
            // Igual que `review`: el motor no decide leer logs, lo decide el CLI.
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

/// Lista las dbs de un workspace y resume cuanto se deduplica.
///
/// Las conexiones viven hasta el final porque el dedupe necesita los hechos de
/// todas las dbs a la vez. Cuenta hechos, no nombres: dos homonimos con contenido
/// distinto son dos.
fn list_workspace(dir: &Path) -> anyhow::Result<()> {
    let found = db::discover_databases(dir)
        .with_context(|| format!("descubriendo bases en {}", dir.display()))?;
    if found.is_empty() {
        println!("0 bases con tabla bots en {}", dir.display());
        return Ok(());
    }

    let mut opened = Vec::new();
    let mut rows = 0usize;
    for path in found {
        let conn = match db::open_readonly(&path) {
            Ok(conn) => conn,
            // Una db ilegible no puede tumbar el listado del resto.
            Err(e) => {
                eprintln!("aviso: {e}");
                continue;
            }
        };
        let robots = db::list_robots(&conn)?;
        rows += robots.len();
        print_robots(&db_label(&path), &robots);
        opened.push((conn, db_label(&path)));
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

/// Etiqueta de la db sin ruta absoluta: `BBRE2\robot_run.db`.
///
/// Se normalizan las dos formas de separador a proposito: `Path` solo reconoce
/// `\` en Windows, y con la implementacion ingenua la funcion devolvia la ruta
/// absoluta completa cuando se compilaba en Linux (lo detecto CI).
///
/// La comparte `review`: la cabecera del panorama lleva la misma etiqueta que el
/// pack para no filtrar el arbol de directorios del usuario.
pub(crate) fn db_label(path: &Path) -> String {
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

/// `true` si las dos rutas son el mismo fichero.
///
/// Se comparan rutas canonicas porque el descubrimiento devuelve rutas absolutas y
/// `--db` puede venir relativa: comparando texto, la db analizada entraria dos
/// veces en el workspace y sus hechos se duplicarian.
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        // Si alguna no existe, la comparacion literal sigue siendo la mejor
        // respuesta: no se puede afirmar que sean el mismo fichero.
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
        // La misma ruta en forma posix: el resultado no puede depender del host.
        assert_eq!(
            db_label(Path::new("/home/josep/proyecto/entrega/robot_run.db")),
            "entrega\\robot_run.db"
        );
        assert_eq!(db_label(Path::new("robot.db")), "robot.db");
    }

    #[test]
    fn la_misma_db_por_dos_rutas_no_se_duplica() {
        // El descubrimiento y `--db` no tienen por que escribir la ruta igual.
        assert!(same_file(Path::new("."), Path::new("./")));
        assert!(!same_file(
            Path::new("."),
            Path::new("no-existe-en-este-directorio.db")
        ));
    }
}
