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

#[derive(Debug, Clone)]
pub struct Analysis {
    pub findings: Vec<Finding>,
    pub higiene: Vec<Finding>,
    pub contracts_out: Vec<Contract>,
    pub contracts_in: Vec<Contract>,
    pub unused_declared: Vec<String>,
    pub ambiguous_calls: Vec<String>,
    pub missing_calls: Vec<String>,
    pub errors: ErrorReport,
}

pub fn analyse(conn: &rusqlite::Connection, db_label: &str, robot: &str) -> Result<Analysis> {
    analyse_with_logs(conn, db_label, robot, None)
}

pub fn analyse_with_logs(
    conn: &rusqlite::Connection,
    db_label: &str,
    robot: &str,
    logs: Option<(&Path, usize)>,
) -> Result<Analysis> {
    analyse_with_logs_at(conn, db_label, robot, None, logs)
}

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
