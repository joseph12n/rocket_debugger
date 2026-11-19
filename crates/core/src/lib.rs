//! Motor de deteccion y suministro de contexto para flujos de Rocketbot.
//!
//! Convierte un `robot.db` en un context pack que un agente lee de una vez, con
//! cada error atribuido al snapshot de codigo vivo en su timestamp.
//!
//! Invariante: `id` del nodo == `cmd_id` del log. `line` no es unica (Try y Catch
//! comparten numeracion), asi que solo sirve como etiqueta de display.

pub mod analysis;
pub mod attribution;
pub mod db;
pub mod diff;
pub mod error;
pub mod exec;
pub mod graph;
pub mod logs;
pub mod pack;
pub mod paths;
pub mod pathscan;
pub mod redact;
pub mod registry;
pub mod render;
pub mod review;
pub mod rules;
pub mod signature;
pub mod tree;
pub mod versions;

pub use analysis::{analyse, Analysis};
pub use error::{Error, Result};
pub use pack::{
    build_from_conn, build_from_conn_with_logs, build_from_conn_with_logs_and_paths, Pack, VarInfo,
};
pub use render::{estimate_tokens, render_changes, Sizing};
pub use review::{review_conn, review_workspace, Review, ReviewRow};
pub use versions::{VersionIndex, VersionInfo};
