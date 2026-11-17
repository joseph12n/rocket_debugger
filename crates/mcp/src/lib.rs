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
//! # Transporte
//!
//! stdio, JSON-RPC 2.0 y **un documento JSON por linea** (una peticion por
//! linea, una respuesta por linea). No se usa el framing `Content-Length` de
//! LSP: el cliente escribe lineas y un prefijo de longitud solo anade una forma
//! de desincronizarse.
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

use rocketdebug_core::db;
use rocketdebug_core::logs;
use rocketdebug_core::pack::DEFAULT_LOG_FILES;

mod protocol;
mod tools;

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
}

impl Settings {
    /// Entorno vacio: ni `ROCKETBOT_DB` ni logs.
    ///
    /// Es lo que usan los tests para no depender de `C:\Rocketbot`, y lo que
    /// usaria cualquier consumidor embebido que quiera un pack puro (solo db).
    pub fn hermetic() -> Self {
        Self {
            env_db: None,
            logs: None,
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

        let logs_dir = Path::new(logs::DEFAULT_LOGS_DIR);
        let logs = if logs_dir.is_dir() {
            Some((logs_dir.to_path_buf(), DEFAULT_LOG_FILES))
        } else {
            None
        };

        Self { env_db, logs }
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
