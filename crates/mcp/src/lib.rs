//! Fachada MCP de `rocket_debugger`: la interfaz por la que un agente opera el
//! motor sin leer el JSON crudo de `robot.db`.
//!
//! # MCP para intenciones, no para datos
//!
//! La fachada expone **pocas tools de alto nivel** y ninguna devuelve un volcado:
//! `list_robots` dice que hay, `flow_pack` entrega el context pack (~1-3k tokens)
//! y `robot_errors` solo la seccion de errores de ese pack. El agente pide "el
//! robot X" o "sus errores", no "dame la tabla `bots`": si pidiera datos crudos
//! gastaria el contexto que este producto existe para ahorrar. Por eso no hay
//! tools de logs sueltos, ni de versiones, ni de comandos: cada una seria una
//! puerta abierta a un volcado de 10.000 lineas.
//!
//! # Transportes
//!
//! Dos transportes, un solo despacho:
//!
//! - **stdio**: JSON-RPC 2.0 y **un documento JSON por linea** (una peticion por
//!   linea, una respuesta por linea). No se usa el framing `Content-Length` de
//!   LSP: el cliente escribe lineas y un prefijo de longitud solo anade una forma
//!   de desincronizarse.
//! - **HTTP/1.1 local** ([`serve`]): la app sera el servidor de una sola
//!   instancia y el agente el cliente que se conecta a `127.0.0.1`. HTTP minimo
//!   sobre `std::net`, sin `keep-alive` ni dependencias nuevas.
//!
//! Los dos acaban en [`handle_line_with`], asi que la misma linea produce la
//! misma respuesta por cualquiera de ellos.
//!
//! # Despacho separado de la E/S
//!
//! [`handle_line`] y [`handle_line_with`] reciben una linea y devuelven la
//! respuesta, o `None` cuando no toca responder (notificaciones). No tocan
//! stdin ni stdout, asi que el protocolo entero se prueba sin arrancar un
//! proceso; `main` solo lee lineas, llama y escribe.
//!
//! # Solo lectura
//!
//! Toda apertura pasa por [`rocketdebug_core::db::open_readonly`]: esta fachada
//! no escribe nunca en `robot.db`. El agente edita con sus herramientas; aqui
//! solo se diagnostica.

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

/// La ejecucion configurada en este servidor.
///
/// Vive en [`Settings`] y no en un `static` para que los tests inyecten un
/// broker con un runner falso: la misma tool se prueba sin `rocketbot.exe` y
/// sin depender de que Studio este cerrado en la maquina que corre la suite.
#[derive(Clone)]
pub struct ExecSettings {
    /// Broker compartido por todas las conexiones HTTP del proceso.
    pub broker: Arc<Broker>,
    /// Directorio de logs bajo el que se crean los de cada ejecucion. Es el
    /// mismo que lee el pack, para que `flow_pack` vea los errores nuevos.
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

/// Entorno que el servidor resuelve **una vez** al arrancar, no en cada peticion.
///
/// La db y los logs son del entorno, no del mensaje. Resolverlos aqui (en vez de
/// leer variables de entorno dentro de cada tool) tiene dos consecuencias
/// buscadas: los tests construyen un entorno vacio con [`Settings::hermetic`] sin
/// tocar `C:\Rocketbot`, y un cambio de `ROCKETBOT_DB` a mitad de sesion no hace
/// que dos peticiones seguidas hablen de bases distintas.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Valor de `ROCKETBOT_DB`, si esta definido y no vacio.
    pub env_db: Option<PathBuf>,
    /// Directorio de logs y cuantos ficheros leer.
    ///
    /// `None` deja la seccion de errores vacia: el motor no lee logs por su
    /// cuenta, y la fachada solo lo hace cuando el entorno los tiene. Sin logs,
    /// las tools lo dicen en vez de fingir que el robot esta limpio.
    pub logs: Option<(PathBuf, usize)>,
    /// Fichero del registro de problemas, si hay uno configurado.
    ///
    /// El registro es el **unico** sitio donde esta herramienta escribe algo, y es
    /// su propio fichero: nunca `robot.db`. `None` desactiva las tools `problems_*`
    /// con un mensaje claro en vez de fallar por la espalda.
    pub registry: Option<PathBuf>,
    /// Raiz donde descubrir varias dbs para la vista de bandeja de la UI.
    ///
    /// `None` = la UI mira solo `default_db()`. Se resuelve una vez al arrancar,
    /// como el resto del entorno, para que dos peticiones seguidas no hablen de
    /// workspaces distintos.
    pub workspace: Option<PathBuf>,
    /// Broker de ejecucion. `None` = `run_robot` no esta configurado (no hay
    /// `rocketbot.exe`) y lo dice en vez de fallar por la espalda.
    pub exec: Option<ExecSettings>,
}

/// Ruta del registro de problemas.
///
/// `ROCKETDEBUG_REGISTRY` manda (util para tests y para separar registros por
/// proyecto); si no esta, se usa el camino por defecto del motor, que respeta
/// `ROCKETDEBUG_HOME`.
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

/// Directorio de logs pedido explicitamente, si lo hay.
fn logs_from_env() -> Option<PathBuf> {
    std::env::var_os("ROCKETDEBUG_LOGS")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// Configura el broker si hay un `rocketbot.exe` al que apuntar.
///
/// Basta con que `ROCKETBOT_EXE` este definido aunque la ruta no exista: el
/// fallo se reporta al ejecutar, con la ruta concreta, en vez de desactivar la
/// tool en silencio. Sin variable, solo se activa si existe la ruta estandar.
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

    // Los logs de ejecucion van bajo el mismo directorio que lee el pack: si
    // `--log` funciona, quedan en un subdirectorio y `recent_files` los
    // encuentra; si la version instalada lo ignora, el runner escribe en el
    // directorio por defecto de Rocketbot y el respaldo del broker los lee.
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
    /// Entorno vacio: ni `ROCKETBOT_DB`, ni logs, ni registro.
    ///
    /// Es lo que usan los tests para no depender de `C:\Rocketbot`, y lo que
    /// usaria cualquier consumidor embebido que quiera un pack puro (solo db).
    pub fn hermetic() -> Self {
        Self {
            env_db: None,
            logs: None,
            registry: None,
            workspace: None,
            exec: None,
        }
    }

    /// Lee el entorno del proceso.
    ///
    /// Los logs se leen **si el directorio existe**: `robot_errors` sin logs solo
    /// podria decir "no lo se", y quien arranca el servidor en esta maquina ya
    /// sabe que el entorno es este.
    pub fn from_env() -> Self {
        let env_db = std::env::var_os("ROCKETBOT_DB")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty());

        let logs_env = logs_from_env();
        let exec = exec_from_env(logs_env.clone());

        // Con broker configurado, el directorio de logs es el suyo (se acaba de
        // crear si no existia): el pack tiene que ver la ejecucion.
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

    /// Db que se usa cuando la peticion no trae `db`: `ROCKETBOT_DB` o la del
    /// Studio local.
    pub fn default_db(&self) -> PathBuf {
        self.env_db
            .clone()
            .unwrap_or_else(|| PathBuf::from(db::DEFAULT_DB))
    }
}

/// Despacha una linea con el entorno del proceso.
///
/// Es la entrada de una linea del protocolo. `main` prefiere [`handle_line_with`]
/// con un [`Settings`] resuelto al arrancar; esta version existe para que un
/// consumidor (o un test) pueda usarla sin montar el servidor.
pub fn handle_line(line: &str) -> Option<String> {
    handle_line_with(line, &Settings::from_env())
}

/// Despacha una linea contra un entorno concreto, sin tocar stdin ni stdout.
///
/// Devuelve `None` cuando no hay que responder: linea vacia y notificaciones.
/// Cualquier otra cosa produce exactamente una linea de respuesta JSON.
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
