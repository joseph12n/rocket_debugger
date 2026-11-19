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

#[derive(Clone)]
pub struct ExecSettings {
    pub broker: Arc<Broker>,
    pub logs_dir: PathBuf,
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

#[derive(Debug, Clone)]
pub struct Settings {
    pub env_db: Option<PathBuf>,
    pub logs: Option<(PathBuf, usize)>,
    pub registry: Option<PathBuf>,
    pub workspace: Option<PathBuf>,
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
    pub fn hermetic() -> Self {
        Self {
            env_db: None,
            logs: None,
            registry: None,
            workspace: None,
            exec: None,
        }
    }

    pub fn from_env() -> Self {
        let env_db = std::env::var_os("ROCKETBOT_DB")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty());

        let logs_env = logs_from_env();
        let exec = exec_from_env(logs_env.clone());

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

    pub fn default_db(&self) -> PathBuf {
        self.env_db
            .clone()
            .unwrap_or_else(|| PathBuf::from(db::DEFAULT_DB))
    }
}

pub fn handle_line(line: &str) -> Option<String> {
    handle_line_with(line, &Settings::from_env())
}

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
