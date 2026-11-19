//! Cableado del grafo, las reglas y los errores: lo que convierte cuatro modulos
//! en producto.
//!
//! Ninguno sirve solo: el grafo no dice nada al agente; las reglas sin grafo
//! marcan como muertas las variables que son la interfaz de los subrobots
//! (medido: 86 falsos positivos de 89 en un orquestador de 8 hijos); y los
//! errores sin atribucion temporal mezclan los de hoy con los de una version que
//! ya no corre (medido: de 51 firmas de `reporteMercadolibre` en BBRE2, 49 son de
//! versiones anteriores y ninguna esta viva).
//!
//! # Decisiones
//!
//! - Se cargan los hechos de **toda** la db y de todas las dbs del workspace: el
//!   grafo necesita los dos lados de cada llamada. [`Analysis::ambiguous_calls`] y
//!   [`Analysis::missing_calls`] separan "no existe" de "no lo veo" (issue #12),
//!   que es lo que fallaria en runtime.
//! - El workspace deduplica por `(nombre, hash)`: la misma entrega esta copiada en
//!   varias dbs y, medido, 31 filas son 20 robots distintos.
//! - El snapshot (`--snapshot`, issue #18) fija solo el robot analizado; los demas
//!   aportan su ultimo guardado, que es el que el runner resuelve hoy. Fijarlos
//!   tambien contestaria otra pregunta (un historico) y un hijo puede no tener
//!   snapshot de esa fecha.
//! - El pin no toca los errores: [`errors_for`] los atribuye al snapshot vivo en su
//!   timestamp, que es lo que distingue este motor de un vistazo al log.
//!
//! Este modulo solo decide **que** se analiza y con que extras (logs, probe de
//! disco, snapshot); el como vive en `workspace` (hechos, dedupe y analisis por
//! robot) y `errors` (informe de errores).

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
    /// render (issue #13: 18 variables muertas y solo 2 visibles, sin aviso).
    pub findings: Vec<Finding>,
    /// Hallazgos de **higiene**: afectan al robot entero (sin Try/Catch, IF sin
    /// else, anidamiento, variables muertas).
    ///
    /// En seccion aparte no compiten por el cupo con lo accionable: medido, eran
    /// el 36% de lo mostrado y son justo los que un lector ignora.
    pub higiene: Vec<Finding>,
    /// Contratos en los que este robot es el padre: lo que ofrece a sus hijos.
    pub contracts_out: Vec<Contract>,
    /// Contratos en los que este robot es el hijo: lo que espera de sus padres.
    pub contracts_in: Vec<Contract>,
    /// Declaradas que siguen sin uso despues de descontar lo que leen los hijos.
    pub unused_declared: Vec<String>,
    /// Llamadas cuyo hijo solo existe fuera de la db del padre, como
    /// `"padre -> hijo"`.
    ///
    /// Son la mejor conjetura y no una certeza: el runner busca el nombre en su
    /// propia db ([`crate::graph::Edge::ambiguous`]). Con una sola db la lista
    /// queda vacia; [`analyse_with_workspace_at_and_paths`] es quien la puede llenar.
    pub ambiguous_calls: Vec<String>,
    /// Llamadas cuyo destino no existe en **ninguna** db, como
    /// `"padre L2.3 -> hijo"`.
    ///
    /// [`crate::graph::build_edges`] las omite (sin hechos del hijo no hay
    /// contrato que calcular) y por eso una llamada inexistente se perdia en
    /// silencio; en runtime el padre falla. La linea del nodo es lo unico que
    /// permite hallarla.
    pub missing_calls: Vec<String>,
    /// Errores del log del robot, clasificados y agrupados por firma.
    ///
    /// Vacio si no se pidieron logs: el pack sin ellos es valido.
    pub errors: ErrorReport,
}

/// Analiza un robot usando el resto de la db como contexto, **sin** logs.
///
/// Es la firma historica; el informe de errores sale vacio y el render lo omite.
pub fn analyse(conn: &rusqlite::Connection, db_label: &str, robot: &str) -> Result<Analysis> {
    analyse_with_logs(conn, db_label, robot, None)
}

/// Analiza un robot usando el resto de la db como contexto y, si se pide, los
/// logs de ejecucion: `(directorio, maximo de ficheros)`.
///
/// `logs` es `Option` a proposito: el motor no decide leer disco. Quien sabe si
/// hay logs es `pack::build_from_conn`, y asi un test no depende de `C:\Rocketbot`.
///
/// El robot pedido ilegible es error ([`Error::RobotNotFound`] o el de lectura):
/// un pack sin nodos no es degradado, es una mentira. Un robot distinto e
/// ilegible solo se omite. Delega con `None` en [`analyse_with_logs_at`].
pub fn analyse_with_logs(
    conn: &rusqlite::Connection,
    db_label: &str,
    robot: &str,
    logs: Option<(&Path, usize)>,
) -> Result<Analysis> {
    analyse_with_logs_at(conn, db_label, robot, None, logs)
}

/// Como [`analyse_with_logs`], pero fija el snapshot (`bots.id`) del robot
/// analizado: nodos, hallazgos, contratos y llamadas salen de esa version, que es
/// lo que hace que un pack de la v26 hable de la v26 (issue #18).
///
/// El pin es solo para este robot; los demas aportan su ultimo snapshot (ver doc
/// del modulo). Una version inexistente es [`Error::SnapshotNotFound`] y no una
/// caida al ultimo: describir otra version en silencio es el defecto que corrige.
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

/// Como [`analyse_with_logs_at`], comprobando ademas las rutas contra el
/// dispositivo mediante `path_probe`.
///
/// Es la que usa el pack real: `RUTA_MIGRADA` necesita saber si la ruta existe
/// aqui y el motor no lo decide por su cuenta.
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
/// `conns` son las conexiones de solo lectura con su etiqueta; la db pedida va
/// primera para que el hecho del robot conserve su etiqueta y sus copias queden
/// en [`crate::graph::RobotFacts::also_in`] (asi se resuelven las aristas que el
/// runner resolveria en la db del padre).
///
/// `snapshot` fija la version del robot pedido (lo necesita el pack con
/// `--snapshot`); el pin no toca a los demas. Con [`paths::NoPaths`] la regla
/// `RUTA_MIGRADA` no se evalua y el resultado es el historico; el pack real pasa
/// [`paths::LocalPaths`].
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
    // La db pedida primero: el dedupe conserva la primera aparicion, asi que el
    // hecho del robot analizado lleva la etiqueta de la db que el consumidor pidio.
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

    // El snapshot pedido se fija solo en este robot; los demas hechos del
    // workspace aportan su ultimo guardado, que es lo que el runner resolveria.
    let facts = workspace::load_workspace_of(
        &ordered,
        Some(workspace::Target {
            db_label,
            robot,
            snapshot,
        }),
    )?;
    let workspace = Workspace::from_facts(facts);

    // La db pedida esta en `conns` por construccion: el robot se busco en ella.
    let analysed = conns
        .iter()
        .find(|(_, label)| *label == db_label)
        .map(|(conn, _)| *conn)
        .ok_or_else(|| Error::RobotNotFound(robot.to_string()))?;

    workspace.analyse(analysed, db_label, robot, logs, path_probe)
}
