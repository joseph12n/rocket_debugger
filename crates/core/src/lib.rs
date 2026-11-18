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
//! - [`versions`] historial de snapshots + indice temporal
//! - [`logs`]    parser de los logs de ejecucion del runner
//! - [`paths`]   rutas horneadas de otro dispositivo y propuesta de remapeo
//! - [`exec`]    broker de ejecucion: cola, deteccion de Studio y verificacion de eventos
//! - [`signature`] normalizacion de firmas de error y clasificacion del ruido
//! - [`attribution`] atribucion de cada error al snapshot vivo (veredicto y linea)
//! - [`diff`]     diff estructural entre dos snapshots del mismo robot
//! - [`graph`]    grafo de llamadas `execRocketBotDB` y contrato de variables
//! - [`rules`]    motor de reglas declarativo (hallazgos)
//! - [`analysis`] grafo + reglas cableados sobre los hechos de la db
//! - [`redact`]   redaccion de valores cifrados y nombres sensibles
//! - [`pack`]    ensamblado del context pack
//! - [`render`]  markdown + contabilidad de tokens
//! - [`review`]  agregacion de la vista de bandeja (una fila por robot)
//!
//! # Decisiones de diseno
//!
//! * El formato de `bots.data` no esta documentado y puede cambiar entre versiones
//!   de Rocketbot, asi que se parsea con [`serde_json::Value`] en vez de structs
//!   rigidos. Un campo nuevo se ignora; un campo que desaparece no rompe nada.
//! * `id` del nodo == `cmd_id` del log. Es la unica union fiable entre ejecucion y
//!   codigo. `line` **no** es unico: hay robots donde el `try` y el `catch` comparten
//!   numeracion (`2.1`, `2.2`, `2.3`), asi que solo sirve como etiqueta de display.
//! * Un error pertenece al snapshot que estaba vivo en su timestamp, no al ultimo:
//!   un bug se arregla a veces anadiendo un nodo antes, sin tocar el nodo que falla.

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
