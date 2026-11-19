//! Fachada MCP de `rocket_debugger`: tools de alto nivel sobre `robot.db` en
//! solo lectura, por stdio o HTTP local.
//!
//! Invariante: toda apertura pasa por `db::open_readonly`; esta fachada no
//! escribe nunca en `robot.db`. El unico fichero que escribe es el registro de
//! problemas, si esta configurado.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rocketdebug_core::db;
use rocketdebug_core::exec::{self, Broker, ExecConfig, ProcessRunner, StudioProbe};
use rocketdebug_core::logs;
use rocketdebug_core::pack::DEFAULT_LOG_FILES;

mod http;
mod protocol;
mod tools;

pub use http::{serve, MAX_BODY_BYTES};

/// La ejecucion configurada en este servidor; vive en `Settings` para inyectar un broker falso en tests.
#[derive(Clone)]
pub struct ExecSettings {
    /// Broker compartido por todas las conexiones HTTP del proceso.
    pub broker: Arc<Broker>,
    /// Directorio donde el runner crea los logs de cada ejecucion; el pack lee el mismo.
    pub logs_dir: PathBuf,
    /// Timeout cuando la peticion no trae uno.
    pub default_timeout: Duration,
}

impl std::fmt::Debug for ExecSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecSettings")
            .field("logs_dir", &self.logs_dir)
            .field("default_timeout", &self.default_timeout)
            .finish_non_exhaustive()
    }
}

/// Entorno que el servidor resuelve una vez al arrancar, no en cada peticion.
/// La db y los logs son del entorno, no del mensaje.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Valor de `ROCKETBOT_DB`, si esta definido y no vacio.
    pub env_db: Option<PathBuf>,
    /// Directorio de logs y cuantos ficheros leer; `None` deja la seccion de
    /// errores vacia y las tools lo dicen en vez de fingir un robot limpio.
    pub logs: Option<(PathBuf, usize)>,
    /// Fichero del registro de problemas: el unico sitio donde esta herramienta
    /// escribe algo, y nunca es `robot.db`. `None` desactiva las tools `problems_*`.
    pub registry: Option<PathBuf>,
    /// Raiz donde descubrir varias dbs para la vista de bandeja; `None` = solo `default_db()`.
    pub workspace: Option<PathBuf>,
    /// Broker de ejecucion; `None` = `run_robot` no esta configurado y lo dice.
    pub exec: Option<ExecSettings>,
}

fn workspace_from_env() -> Option<PathBuf> {
    std::env::var_os("ROCKETDEBUG_WORKSPACE")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty() && p.is_dir())
}

fn registry_from_env() -> Option<PathBuf> {
    std::env::var_os("ROCKETDEBUG_REGISTRY")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| Some(rocketdebug_core::registry::default_path()))
}

fn logs_from_env() -> Option<PathBuf> {
    std::env::var_os("ROCKETDEBUG_LOGS")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// Configura el broker si hay un `rocketbot.exe` al que apuntar.
/// `ROCKETBOT_EXE` definido activa la tool aunque la ruta no exista: el fallo se reporta al ejecutar.
fn exec_from_env(logs_env: Option<PathBuf>) -> Option<ExecSettings> {
    let exe_env = std::env::var_os("ROCKETBOT_EXE")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty());
    let exe = exe_env
        .clone()
        .unwrap_or_else(|| PathBuf::from(exec::DEFAULT_EXE));
    if exe_env.is_none() && !exe.is_file() {
        return None;
    }

    // Los logs de ejecucion van bajo el mismo directorio que lee el pack.
    let logs_dir = logs_env
        .or_else(|| exe.parent().map(|dir| dir.join("logs")))
        .unwrap_or_else(|| PathBuf::from(logs::DEFAULT_LOGS_DIR));
    let _ = std::fs::create_dir_all(&logs_dir);

    let config = ExecConfig {
        exe,
        logs_dir: logs_dir.clone(),
        probe: StudioProbe::System,
    };
    Some(ExecSettings {
        broker: Arc::new(Broker::new(Arc::new(ProcessRunner::new(config)))),
        logs_dir,
        default_timeout: Duration::from_secs(exec::DEFAULT_TIMEOUT_SECS),
    })
}

impl Settings {
    /// Entorno vacio (ni db, ni logs, ni registro): lo usan los tests.
    pub fn hermetic() -> Self {
        Self {
            env_db: None,
            logs: None,
            registry: None,
            workspace: None,
            exec: None,
        }
    }

    /// Lee el entorno del proceso; los logs se leen solo si el directorio existe.
    pub fn from_env() -> Self {
        let env_db = std::env::var_os("ROCKETBOT_DB")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty());

        let logs_env = logs_from_env();
        let exec = exec_from_env(logs_env.clone());

        // Si hay broker, su directorio de logs manda: el pack tiene que ver la ejecucion.
        let logs = if let Some(exec) = &exec {
            Some((exec.logs_dir.clone(), DEFAULT_LOG_FILES))
        } else if let Some(dir) = logs_env {
            dir.is_dir().then_some((dir, DEFAULT_LOG_FILES))
        } else {
            let dir = Path::new(logs::DEFAULT_LOGS_DIR);
            dir.is_dir().then(|| (dir.to_path_buf(), DEFAULT_LOG_FILES))
        };

        Self {
            env_db,
            logs,
            registry: registry_from_env(),
            workspace: workspace_from_env(),
            exec,
        }
    }

    /// Db que se usa cuando la peticion no trae `db`: `ROCKETBOT_DB` o la del Studio local.
    pub fn default_db(&self) -> PathBuf {
        self.env_db
            .clone()
            .unwrap_or_else(|| PathBuf::from(db::DEFAULT_DB))
    }
}

/// Despacha una linea con el entorno del proceso.
/// `main` usa `handle_line_with` con un `Settings` resuelto al arrancar.
pub fn handle_line(line: &str) -> Option<String> {
    handle_line_with(line, &Settings::from_env())
}

/// Despacha una linea contra un entorno concreto, sin tocar stdin ni stdout.
/// Devuelve `None` cuando no toca responder: linea vacia y notificaciones.
pub fn handle_line_with(line: &str, settings: &Settings) -> Option<String> {
    protocol::handle(line, settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_db_por_defecto_es_la_del_entorno_o_la_del_studio() {
        let settings = Settings {
            env_db: Some(PathBuf::from(r"D:\proyecto\robot.db")),
            logs: None,
            registry: None,
            workspace: None,
            exec: None,
        };
        assert_eq!(
            settings.default_db(),
            PathBuf::from(r"D:\proyecto\robot.db")
        );

        assert_eq!(
            Settings::hermetic().default_db(),
            PathBuf::from(db::DEFAULT_DB),
            "sin ROCKETBOT_DB se usa la db del Studio local"
        );
    }
}
