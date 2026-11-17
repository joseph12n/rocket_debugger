//! Motor de deteccion y suministro de contexto para flujos de Rocketbot.
//!
//! El producto no es un IDE: es el motor que convierte un `robot.db` en un
//! *context pack* que un agente puede leer de una vez (1-3k tokens) en lugar del
//! JSON crudo (10-250k tokens), y que atribuye cada error al snapshot de codigo
//! que estaba vivo cuando ocurrio.
//!
//! # Capas
//!
//! - [`db`]      lectura de `robot.db` (solo lectura, tolerante al formato)
//! - [`tree`]    arbol de comandos -> lista plana con linea, rama y profundidad
//! - [`versions`] historial de snapshots + atribucion temporal
//! - [`pack`]    ensamblado del context pack
//! - [`render`]  markdown + contabilidad de tokens
//!
//! # Decisiones de diseno
//!
//! * El formato de `bots.data` no esta documentado y puede cambiar entre versiones
//!   de Rocketbot, asi que se parsea con [`serde_json::Value`] en vez de structs
//!   rigidos. Un campo nuevo se ignora; un campo que desaparece no rompe nada.
//! * `id` del nodo == `cmd_id` del log. Es la unica union fiable entre ejecucion y
//!   codigo. `line` **no** es unico: hay robots donde el `try` y el `catch` comparten
//!   numeracion (`2.1`, `2.2`, `2.3`), asi que solo sirve como etiqueta de display.

pub mod db;
pub mod graph;
pub mod logs;
pub mod pack;
pub mod redact;
pub mod render;
pub mod rules;
pub mod signature;
pub mod tree;
pub mod versions;

pub use error::{Error, Result};
pub use pack::{build_from_conn, Pack, VarInfo};
pub use render::{estimate_tokens, Sizing};
pub use versions::{VersionIndex, VersionInfo};