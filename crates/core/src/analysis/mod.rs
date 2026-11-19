//! Cableado del grafo, las reglas y los errores de un robot: decide *que* se
//! analiza (logs, probe de disco, snapshot); el *como* vive en `workspace` y
//! `errors`.
//!
//! Invariante: el snapshot pedido fija solo el robot analizado; los errores se
//! atribuyen siempre al snapshot vivo en su timestamp, no al ultimo.

use std::path::Path;

mod errors;
mod workspace;

pub(crate) use errors::snapshot_views;
pub use errors::{errors_for, ErrorItem, ErrorReport, ERROR_MESSAGE_MAX};
pub use workspace::{load_workspace, Workspace};

use crate::error::{Error, Result};
use crate::graph::Contract;
use crate::paths::{self, PathProbe};
use crate::rules::Finding;

/// Resultado del analisis cruzado de un robot dentro de su db.
#[derive(Debug, Clone)]
pub struct Analysis {
    /// Hallazgos **accionables**: senalan una linea con un defecto concreto.
    ///
    /// Van **sin recortar**: el tope y el aviso de lo que queda fuera son del
    /// render (issue #13).
    pub findings: Vec<Finding>,
    /// Hallazgos de **higiene**: afectan al robot entero (sin Try/Catch, IF sin
    /// else, anidamiento, variables muertas).
    ///
    /// En seccion aparte no compiten por el cupo con lo accionable.
    pub higiene: Vec<Finding>,
    pub contracts_out: Vec<Contract>,
    pub contracts_in: Vec<Contract>,
    /// Declaradas que siguen sin uso despues de descontar lo que leen los hijos.
    pub unused_declared: Vec<String>,
    /// Llamadas cuyo hijo solo existe fuera de la db del padre, como
    /// `"padre -> hijo"`: son la mejor conjetura, no una certeza.
    pub ambiguous_calls: Vec<String>,
    /// Llamadas cuyo destino no existe en **ninguna** db, como
    /// `"padre L2.3 -> hijo"`: en runtime el padre falla.
    pub missing_calls: Vec<String>,
    /// Errores del log del robot, vacio si no se pidieron logs.
    pub errors: ErrorReport,
}

/// Analiza un robot usando el resto de la db como contexto, **sin** logs.
pub fn analyse(conn: &rusqlite::Connection, db_label: &str, robot: &str) -> Result<Analysis> {
    analyse_with_logs(conn, db_label, robot, None)
}

/// Como [`analyse`], con logs de ejecucion: `(directorio, maximo de ficheros)`.
pub fn analyse_with_logs(
    conn: &rusqlite::Connection,
    db_label: &str,
    robot: &str,
    logs: Option<(&Path, usize)>,
) -> Result<Analysis> {
    analyse_with_logs_at(conn, db_label, robot, None, logs)
}

/// Como [`analyse_with_logs`], fijando el snapshot (`bots.id`) del robot
/// analizado (issue #18). El pin no toca a los demas robots ni a los errores.
pub fn analyse_with_logs_at(
    conn: &rusqlite::Connection,
    db_label: &str,
    robot: &str,
    snapshot: Option<i64>,
    logs: Option<(&Path, usize)>,
) -> Result<Analysis> {
    analyse_with_workspace_at_and_paths(
        &[(conn, db_label)],
        db_label,
        robot,
        snapshot,
        logs,
        &paths::NoPaths,
    )
}

/// Como [`analyse_with_logs_at`], comprobando las rutas contra el dispositivo
/// con `path_probe` (lo necesita la regla `RUTA_MIGRADA`).
pub fn analyse_with_logs_at_paths(
    conn: &rusqlite::Connection,
    db_label: &str,
    robot: &str,
    snapshot: Option<i64>,
    logs: Option<(&Path, usize)>,
    path_probe: &dyn PathProbe,
) -> Result<Analysis> {
    analyse_with_workspace_at_and_paths(
        &[(conn, db_label)],
        db_label,
        robot,
        snapshot,
        logs,
        path_probe,
    )
}

/// Analiza un robot con los hechos de **todas** las dbs del workspace.
///
/// La db pedida va primera: el dedupe conserva la primera aparicion y el hecho
/// del robot conserva su etiqueta. Con [`paths::NoPaths`] no se evalua
/// `RUTA_MIGRADA` y el resultado es el historico.
///
/// [`Error::RobotNotFound`] si el robot no esta en `db_label`, aunque exista en
/// otra db, y [`Error::SnapshotNotFound`] si la version pedida no existe.
pub fn analyse_with_workspace_at_and_paths(
    conns: &[(&rusqlite::Connection, &str)],
    db_label: &str,
    robot: &str,
    snapshot: Option<i64>,
    logs: Option<(&Path, usize)>,
    path_probe: &dyn PathProbe,
) -> Result<Analysis> {
    let mut ordered: Vec<(&rusqlite::Connection, &str)> = Vec::with_capacity(conns.len());
    ordered.extend(
        conns
            .iter()
            .copied()
            .filter(|(_, label)| *label == db_label),
    );
    ordered.extend(
        conns
            .iter()
            .copied()
            .filter(|(_, label)| *label != db_label),
    );

    let facts = workspace::load_workspace_of(
        &ordered,
        Some(workspace::Target {
            db_label,
            robot,
            snapshot,
        }),
    )?;
    let workspace = Workspace::from_facts(facts);

    let analysed = conns
        .iter()
        .find(|(_, label)| *label == db_label)
        .map(|(conn, _)| *conn)
        .ok_or_else(|| Error::RobotNotFound(robot.to_string()))?;

    workspace.analyse(analysed, db_label, robot, logs, path_probe)
}
