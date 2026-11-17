use std::path::PathBuf;

/// Errores del motor. Todos llevan contexto suficiente para que el mensaje
/// sea accionable sin abrir el codigo.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no pude abrir la base de datos {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("error de sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("robot no encontrado: {0}")]
    RobotNotFound(String),

    #[error("el snapshot {0} no existe para ese robot")]
    SnapshotNotFound(i64),

    #[error("no pude decodificar bots.data del snapshot {id}: {reason}")]
    Decode { id: i64, reason: String },

    #[error("json invalido en bots.data del snapshot {id}: {source}")]
    Json {
        id: i64,
        #[source]
        source: serde_json::Error,
    },
}

pub type Result<T> = std::result::Result<T, Error>;
