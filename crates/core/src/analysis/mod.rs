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

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

mod errors;
pub use errors::{errors_for, ErrorItem, ErrorReport, ERROR_MESSAGE_MAX};

use crate::attribution::{NodeRef, SnapshotView};
use crate::db;
use crate::error::{Error, Result};
use crate::graph::{self, Contract, Edge, RobotFacts};
use crate::paths::{self, PathProbe};
use crate::rules::{self, Finding};
use crate::tree;

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
    /// propia db ([`Edge::ambiguous`]). Con una sola db la lista queda vacia;
    /// [`analyse_with_workspace_at_and_paths`] es quien la puede llenar.
    pub ambiguous_calls: Vec<String>,
    /// Llamadas cuyo destino no existe en **ninguna** db, como
    /// `"padre L2.3 -> hijo"`.
    ///
    /// [`graph::build_edges`] las omite (sin hechos del hijo no hay contrato que
    /// calcular) y por eso una llamada inexistente se perdia en silencio; en
    /// runtime el padre falla. La linea del nodo es lo unico que permite hallarla.
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

/// Hechos del workspace ya cargados, con los derivados comunes calculados una
/// sola vez.
///
/// La bandeja recorre N robots de la misma db; antes cada uno llamaba a
/// [`analyse_with_workspace_at_and_paths`] y volvia a decodificar todo: O(N^2)
/// (issue #18).
///
/// `facts` es publico porque la bandeja cuenta nodos sin volver a decodificar.
pub struct Workspace {
    pub facts: Vec<RobotFacts>,
    edges: Vec<Edge>,
    unused: BTreeMap<String, Vec<String>>,
    missing: Vec<graph::MissingCall>,
}

impl Workspace {
    /// Carga y deduplica los hechos de todas las dbs, sin analizar ningun robot.
    pub fn load(conns: &[(&rusqlite::Connection, &str)]) -> Result<Self> {
        let facts = load_workspace_of(conns, None)?;
        Ok(Self::from_facts(facts))
    }

    fn from_facts(facts: Vec<RobotFacts>) -> Self {
        let edges = graph::build_edges(&facts);
        let contracts = graph::contracts(&facts, &edges);
        let unused = graph::unused_declared(&facts, &contracts);
        let missing = graph::missing_calls(&facts);
        Self {
            facts,
            edges,
            unused,
            missing,
        }
    }

    /// Analiza un robot sobre esta foto.
    ///
    /// `conn` es la conexion de la db del robot: solo se usa para leer **sus**
    /// logs. No se vuelve a decodificar ni a cargar el workspace.
    pub fn analyse(
        &self,
        conn: &rusqlite::Connection,
        db_label: &str,
        robot: &str,
        logs: Option<(&Path, usize)>,
        path_probe: &dyn PathProbe,
    ) -> Result<Analysis> {
        analyse_loaded(self, conn, db_label, robot, logs, path_probe)
    }
}

/// Analiza un robot reutilizando los hechos y derivados de [`Workspace`].
fn analyse_loaded(
    workspace: &Workspace,
    conn: &rusqlite::Connection,
    db_label: &str,
    robot: &str,
    logs: Option<(&Path, usize)>,
    path_probe: &dyn PathProbe,
) -> Result<Analysis> {
    // Un homonimo de otra db es otro hecho: se busca el que vive en la db pedida.
    let this = workspace
        .facts
        .iter()
        .find(|fact| fact.name == robot && fact.lives_in(db_label))
        .ok_or_else(|| Error::RobotNotFound(robot.to_string()))?;

    // `unused_declared` se calculo una vez para todo el workspace; aqui solo se
    // saca la entrada de este robot (vacia si no tiene variables muertas).
    let unused_declared = workspace.unused.get(robot).cloned().unwrap_or_default();

    let collisions = tree::collisions(&this.nodes);
    // `usize::MAX`: no se recorta aqui. El render decide el tope por seccion y dice
    // cuantos hallazgos quedaron fuera.
    let (mut findings, higiene) = rules::evaluate_split(
        &this.nodes,
        &collisions,
        &unused_declared,
        this.declared.len(),
        usize::MAX,
    );

    // `RUTA_MIGRADA` no vive en `evaluate_split`: necesita comprobar el disco y
    // solo se evalua cuando el consumidor pasa un probe. El orden por severidad
    // se restablece despues de anadirla para que una alta no quede al final.
    if path_probe.enabled() {
        let line_hints = variable_lines(this, &workspace.facts, &workspace.edges);
        findings.extend(paths::findings(
            &this.nodes,
            &this.vars,
            path_probe,
            &line_hints,
        ));
        findings.sort_by_key(|finding| std::cmp::Reverse(finding.severity));
    }

    // Los contratos se recalculan sobre las aristas de **este** hecho: las de un
    // homologo con contenido distinto no son suyas y presentarlas juntas haria que
    // el pack de un robot hablara de las llamadas de otro.
    let own_edges: Vec<Edge> = workspace
        .edges
        .iter()
        .filter(|edge| edge.parent == this.name && this.lives_in(&edge.parent_db))
        .cloned()
        .collect();
    let contracts_out = graph::contracts(&workspace.facts, &own_edges);
    let inbound_edges: Vec<Edge> = workspace
        .edges
        .iter()
        .filter(|edge| edge.child == this.name && this.lives_in(&edge.child_db))
        .cloned()
        .collect();
    let contracts_in = graph::contracts(&workspace.facts, &inbound_edges);

    // Los logs se leen despues de resolver el robot: si no existe, el error tiene
    // que ser el de siempre y no el de un directorio de logs ausente.
    let errors = match logs {
        Some((dir, max_files)) => errors_for(conn, robot, dir, max_files)?,
        None => ErrorReport::default(),
    };

    Ok(Analysis {
        findings,
        higiene,
        contracts_out,
        contracts_in,
        unused_declared,
        ambiguous_calls: ambiguous_calls(&workspace.edges, this),
        missing_calls: missing_call_labels(&workspace.missing, this),
        errors,
    })
}

/// Analiza un robot con los hechos de **todas** las dbs del workspace.
///
/// `conns` son las conexiones de solo lectura con su etiqueta; la db pedida va
/// primera para que el hecho del robot conserve su etiqueta y sus copias queden
/// en [`RobotFacts::also_in`] (asi se resuelven las aristas que el runner
/// resolveria en la db del padre).
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
    let facts = load_workspace_of(
        &ordered,
        Some(Target {
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

/// Snapshots del robot reducidos a `cmd_id -> nodo`, para la atribucion.
///
/// `pub(crate)` porque el broker de ejecucion atribuye los errores de **una**
/// ejecucion con la misma vista: duplicar el aplanado aqui haria que el pack y el
/// informe de ejecucion pudieran dar lineas distintas para el mismo error.
pub(crate) fn snapshot_views(
    conn: &rusqlite::Connection,
    robot: &str,
) -> Result<Vec<SnapshotView>> {
    let mut out = Vec::new();
    for snap in db::load_snapshots(conn, robot)? {
        let root = db::project_root(&snap.project);
        // El aplanado es el de `tree`: reimplementarlo aqui acabaria en dos
        // definiciones de "linea" y "parametros" que se desincronizan.
        let nodes = tree::flatten(db::commands_of(root))
            .into_iter()
            // Sin `id` el nodo no es direccionable y no puede cruzar con el log.
            .filter(|n| !n.id.is_empty())
            .map(|n| {
                (
                    n.id,
                    NodeRef {
                        line: n.line,
                        params: n.params,
                    },
                )
            })
            .collect();
        out.push(SnapshotView {
            id: snap.id,
            created_at: snap.created_at,
            nodes,
        });
    }
    Ok(out)
}

/// Carga los hechos del ultimo snapshot de cada robot de cada db, deduplicando
/// por contenido: la misma entrega esta copiada en varias dbs y, medido, 31 filas
/// son 20 robots distintos (sin dedupe, un workspace de 28 se cuenta como 50).
///
/// La clave es `(nombre, hash del raw_b64)`: mismo nombre y contenido es una
/// copia, se queda un hecho y las demas etiquetas van a [`RobotFacts::also_in`]
/// (una copia descartada no puede convertir una arista en ambigua). Contenido
/// distinto o nombre distinto son dos hechos: un renombrado sigue siendo dos
/// nombres en el grafo, y colapsarlo haria que sus llamadas parecieran
/// inexistentes.
///
/// El hash es el `blake3` del `raw_b64` de [`crate::versions::VersionInfo::hash`]:
/// aqui solo se compara, no se muestra.
pub fn load_workspace(dbs: &[(&rusqlite::Connection, &str)]) -> Result<Vec<RobotFacts>> {
    load_workspace_of(dbs, None)
}

/// [`load_workspace`] con una db y un robot que el consumidor **exige**.
///
/// `target` es el robot pedido y, opcionalmente, el snapshot del que deben salir
/// sus hechos. De esa db el robot no puede faltar ([`Error::RobotNotFound`]) ni
/// fallar al decodificar (el error se propaga): es la diferencia entre un pack
/// degradado y un pack que miente. En las demas dbs un robot ilegible solo se
/// omite, porque como mucho hara que falte su contrato.
fn load_workspace_of(
    dbs: &[(&rusqlite::Connection, &str)],
    target: Option<Target>,
) -> Result<Vec<RobotFacts>> {
    let mut facts: Vec<RobotFacts> = Vec::new();
    let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();

    for (conn, label) in dbs {
        // El pin viaja solo a la db del robot pedido. En las demas se sigue
        // tomando el ultimo snapshot: es el que el runner resuelve hoy.
        let pinned = target.filter(|t| t.db_label == *label);
        for (fact, hash) in load_db_facts(conn, label, pinned)? {
            let key = (fact.name.clone(), hash);
            match seen.get(&key).copied() {
                Some(idx) => {
                    // Copia identica en otra db: el hecho ya existe y solo se
                    // anota donde mas vive.
                    let kept = &mut facts[idx];
                    if !kept.lives_in(label) {
                        kept.also_in.push((*label).to_string());
                    }
                }
                None => {
                    seen.insert(key, facts.len());
                    facts.push(fact);
                }
            }
        }
    }

    // Sin conexion con la etiqueta pedida no se puede haber encontrado el robot.
    if let Some(t) = target {
        if !facts
            .iter()
            .any(|fact| fact.name == t.robot && fact.lives_in(t.db_label))
        {
            return Err(Error::RobotNotFound(t.robot.to_string()));
        }
    }

    Ok(facts)
}

/// Robot del workspace cuyos hechos el consumidor **exige** y, si se indica, el
/// snapshot exacto del que deben salir.
///
/// Existe en vez de un `Option<(&str, &str, Option<i64>)>` para que el pin no se
/// pueda pasar sin nombrar al robot: el snapshot es una propiedad del robot
/// analizado, no de la db ni del workspace.
#[derive(Debug, Clone, Copy)]
struct Target<'a> {
    db_label: &'a str,
    robot: &'a str,
    /// `Some(id)` fija el snapshot; `None` acepta el ultimo.
    snapshot: Option<i64>,
}

/// Hechos de una db, con el hash del contenido que usa el dedupe.
///
/// El snapshot es el **ultimo** de cada robot, salvo para el robot exigido: si
/// [`Target::snapshot`] trae un id, sus hechos salen de esa fila. Se lee solo la
/// fila necesaria ([`db::load_latest_snapshot`] o [`db::load_snapshot`]), no el
/// historial entero: el analisis no necesita las versiones anteriores y una
/// version vieja corrupta no debe bloquear la ultima (issue #18).
fn load_db_facts(
    conn: &rusqlite::Connection,
    db_label: &str,
    target: Option<Target>,
) -> Result<Vec<(RobotFacts, String)>> {
    let mut out = Vec::new();

    for summary in db::list_robots(conn)? {
        // El robot exigido es el unico que puede venir fijado a un snapshot; en
        // los demas `pinned` queda en `None` y se toma el ultimo, como siempre.
        let required = target.is_some_and(|t| t.robot == summary.name.as_str());
        let pinned = target.filter(|_| required).and_then(|t| t.snapshot);
        // Solo se decodifica la version que el analisis necesita: la fijada o la
        // ultima. Cargar todo el historial por robot era O(versiones) por pasada
        // (issue #18) y ademas una version vieja corrupta bloqueaba la ultima.
        let snap = match pinned {
            Some(id) => match db::load_snapshot(conn, &summary.name, id) {
                Ok(snap) => snap,
                Err(_) if !required => continue,
                Err(error) => return Err(error),
            },
            None => match db::load_latest_snapshot(conn, &summary.name) {
                Ok(Some(snap)) => snap,
                Ok(None) => continue,
                // Un robot distinto e ilegible solo se omite: como mucho hara que
                // su contrato no aparezca.
                Err(_) if !required => continue,
                Err(error) => return Err(error),
            },
        };
        let root = db::project_root(&snap.project);
        out.push((
            RobotFacts {
                db: db_label.to_string(),
                also_in: Vec::new(),
                name: summary.name.clone(),
                nodes: tree::flatten(db::commands_of(root)),
                declared: declared_of(root),
                vars: crate::paths::vars_of(root),
            },
            content_hash(&snap.raw_b64),
        ));
    }

    if let Some(t) = target {
        if !out.iter().any(|(fact, _)| fact.name == t.robot) {
            return Err(Error::RobotNotFound(t.robot.to_string()));
        }
    }
    Ok(out)
}

/// Hash del contenido de un snapshot: `blake3` del base64 tal cual esta en `bots`.
///
/// Se usa el base64 y no el JSON decodificado por la misma razon que
/// [`crate::versions::VersionInfo::hash`]: es lo que se copia entre dbs de entrega
/// y no depende de como se re-serialice el JSON.
fn content_hash(raw_b64: &str) -> String {
    blake3::hash(raw_b64.as_bytes()).to_hex().to_string()
}

/// Primera linea donde se usa cada variable, mirando tambien los subrobots.
///
/// Una variable del padre puede no aparecer en ningun nodo suyo y ser consumida
/// por un hijo. En ese caso la linea util para el agente es la de la llamada
/// `execRocketBotDB` que lleva al hijo, no `L-`: es donde se decide que ese
/// contrato se ejecuta.
///
/// La resolucion del hijo usa la misma regla que el grafo (`lives_in`), asi que
/// un homologo de otra db no aporta una linea que no es de esta llamada.
fn variable_lines(
    this: &RobotFacts,
    facts: &[RobotFacts],
    edges: &[Edge],
) -> BTreeMap<String, String> {
    let names: Vec<&str> = this.vars.iter().map(|var| var.name.as_str()).collect();
    let mut out = BTreeMap::new();

    for node in &this.nodes {
        for name in &names {
            if out.contains_key(*name) {
                continue;
            }
            if node.params.contains(&format!("{{{name}}}")) {
                out.insert((*name).to_string(), node.line.clone());
            }
        }
    }

    // Las aristas vienen en el orden en que `build_edges` recorre los nodos: la
    // primera llamada que use la variable es la que se queda.
    for edge in edges
        .iter()
        .filter(|edge| edge.parent == this.name && this.lives_in(&edge.parent_db))
    {
        let Some(child) = facts
            .iter()
            .find(|fact| fact.name == edge.child && fact.lives_in(&edge.child_db))
        else {
            continue;
        };
        for name in &names {
            if out.contains_key(*name) {
                continue;
            }
            if child
                .nodes
                .iter()
                .any(|node| node.params.contains(&format!("{{{name}}}")))
            {
                out.insert((*name).to_string(), edge.line.clone());
            }
        }
    }
    out
}

/// Aristas ambiguas del robot, ya formateadas para el consumidor.
///
/// Se deduplican porque un hijo homologo en dos dbs ajenas genera una arista por
/// db y el agente solo necesita saber que la llamada no esta resuelta aqui. El
/// filtro es por hecho (`db` o copia), no solo por nombre: las llamadas de un
/// homonimo de otra db no son las de este robot.
fn ambiguous_calls(edges: &[Edge], this: &RobotFacts) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for edge in edges {
        if !edge.ambiguous || edge.parent != this.name || !this.lives_in(&edge.parent_db) {
            continue;
        }
        let call = format!("{} -> {}", edge.parent, edge.child);
        if !out.contains(&call) {
            out.push(call);
        }
    }
    out
}

/// Llamadas sin destino del robot, con la linea para que el aviso sea accionable.
///
/// Mismo criterio que [`ambiguous_calls`]: solo las de este hecho, sin repetir.
fn missing_call_labels(calls: &[graph::MissingCall], this: &RobotFacts) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for call in calls {
        if call.parent != this.name || !this.lives_in(&call.parent_db) {
            continue;
        }
        let label = format!("{} L{} -> {}", call.parent, call.line, call.child);
        if !out.contains(&label) {
            out.push(label);
        }
    }
    out
}

/// Variables del panel del robot (`project.vars`).
///
/// Es la misma fuente que usa el contrato: el panel del padre es exactamente lo
/// que hereda el hijo al ejecutarse dentro de su interprete. Se ignoran nombres
/// vacios y repetidos para que el recuento no se infle.
fn declared_of(root: &Value) -> BTreeSet<String> {
    let Some(arr) = root.get("vars").and_then(Value::as_array) else {
        return BTreeSet::new();
    };
    arr.iter()
        .filter_map(|v| v.get("name")?.as_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attribution::Verdict;
    use crate::logs;

    use super::errors::{bucket_of, max_version, short_message, Bucket};

    fn edge(parent: &str, child: &str, db: &str, ambiguous: bool) -> Edge {
        Edge {
            parent: parent.to_string(),
            child: child.to_string(),
            line: "1".to_string(),
            parent_db: "A".to_string(),
            child_db: db.to_string(),
            same_db: !ambiguous,
            ambiguous,
        }
    }

    fn hecho(db: &str, name: &str) -> RobotFacts {
        RobotFacts {
            db: db.to_string(),
            also_in: Vec::new(),
            name: name.to_string(),
            nodes: Vec::new(),
            declared: BTreeSet::new(),
            vars: Vec::new(),
        }
    }

    fn nodo(line: &str, params: &str) -> tree::FlatNode {
        tree::FlatNode {
            line: line.to_string(),
            id: format!("id-{line}"),
            cmd: "setVar".to_string(),
            group: None,
            option: None,
            desc: None,
            params: params.to_string(),
            write_var: None,
            branch: tree::Branch::Main,
            depth: 0,
        }
    }

    #[test]
    fn la_linea_de_una_var_que_usa_un_hijo_es_la_de_la_llamada() {
        let mut padre = hecho("A", "orq");
        padre.nodes = vec![nodo("2.2", "\"hijo\"")];
        padre.vars = vec![crate::paths::VarValue {
            name: "carpeta_salida".to_string(),
            value: Some("C:/Users/x/ENTREGA_BBRE2".to_string()),
        }];
        let mut hijo = hecho("A", "hijo");
        hijo.nodes = vec![nodo("1", "{carpeta_salida}")];
        let llamada = Edge {
            parent: "orq".to_string(),
            child: "hijo".to_string(),
            line: "2.2".to_string(),
            parent_db: "A".to_string(),
            child_db: "A".to_string(),
            same_db: true,
            ambiguous: false,
        };

        let hints = variable_lines(&padre, &[padre.clone(), hijo], &[llamada]);
        assert_eq!(hints.get("carpeta_salida"), Some(&"2.2".to_string()));
    }

    #[test]
    fn sin_uso_en_el_hijo_no_se_inventa_una_linea() {
        let mut padre = hecho("A", "orq");
        padre.nodes = vec![nodo("2.2", "\"hijo\"")];
        padre.vars = vec![crate::paths::VarValue {
            name: "carpeta_salida".to_string(),
            value: None,
        }];
        let mut hijo = hecho("A", "hijo");
        hijo.nodes = vec![nodo("1", "otra_cosa")];
        let llamada = Edge {
            parent: "orq".to_string(),
            child: "hijo".to_string(),
            line: "2.2".to_string(),
            parent_db: "A".to_string(),
            child_db: "A".to_string(),
            same_db: true,
            ambiguous: false,
        };

        let hints = variable_lines(&padre, &[padre.clone(), hijo], &[llamada]);
        assert!(hints.is_empty());
    }

    #[test]
    fn solo_las_aristas_ambiguas_del_padre_pedido() {
        let edges = vec![
            edge("orq", "comun", "A", false),
            edge("orq", "fantasma", "B", true),
            // Mismo hijo homologo en dos dbs: una sola mencion para el agente.
            edge("orq", "fantasma", "C", true),
            edge("otro", "fantasma", "B", true),
        ];

        assert_eq!(
            ambiguous_calls(&edges, &hecho("A", "orq")),
            vec!["orq -> fantasma".to_string()],
            "sin duplicados y sin las aristas de otros padres"
        );
        assert!(ambiguous_calls(&edges, &hecho("A", "nadie")).is_empty());
    }

    #[test]
    fn las_aristas_de_un_homonimo_de_otra_db_no_son_de_este_robot() {
        // `orq` existe en A y en B con contenido distinto: son dos hechos. Las
        // llamadas del de B no pueden aparecer en el analisis del de A.
        let mut edges = vec![edge("orq", "fantasma", "B", true)];
        edges[0].parent_db = "B".to_string();
        edges.push(edge("orq", "comun", "C", true));

        assert_eq!(
            ambiguous_calls(&edges, &hecho("A", "orq")),
            vec!["orq -> comun".to_string()],
            "solo la arista cuyo padre es el hecho de A"
        );

        let mut copia = hecho("A", "orq");
        copia.also_in = vec!["B".to_string()];
        assert_eq!(
            ambiguous_calls(&edges, &copia),
            vec!["orq -> fantasma".to_string(), "orq -> comun".to_string()],
            "una copia identica si es el mismo hecho en las dos dbs"
        );
    }

    #[test]
    fn las_llamadas_sin_destino_llevan_linea_y_no_se_repiten() {
        let calls = vec![
            graph::MissingCall {
                parent: "orq".to_string(),
                parent_db: "A".to_string(),
                line: "4".to_string(),
                child: "no_existe".to_string(),
            },
            // Misma linea repetida (Try/Catch comparten numeracion): un aviso.
            graph::MissingCall {
                parent: "orq".to_string(),
                parent_db: "A".to_string(),
                line: "4".to_string(),
                child: "no_existe".to_string(),
            },
            graph::MissingCall {
                parent: "otro".to_string(),
                parent_db: "A".to_string(),
                line: "2.1".to_string(),
                child: "tampoco".to_string(),
            },
        ];

        assert_eq!(
            missing_call_labels(&calls, &hecho("A", "orq")),
            vec!["orq L4 -> no_existe".to_string()],
            "con linea, sin duplicados y sin las llamadas de otros padres"
        );
        assert!(missing_call_labels(&calls, &hecho("A", "nadie")).is_empty());
    }

    #[test]
    fn las_declaradas_salen_del_panel_y_se_deduplican() {
        let root = serde_json::json!({
            "vars": [
                {"name": "url", "data": null},
                {"name": "  url  ", "data": "repetida"},
                {"name": "   ", "data": null},
                {"sin_nombre": true}
            ]
        });
        let declared = declared_of(&root);
        assert_eq!(declared.len(), 1);
        assert!(declared.contains("url"));

        let sin_panel = serde_json::json!({"commands": []});
        assert!(declared_of(&sin_panel).is_empty());
    }

    #[test]
    fn el_mensaje_del_error_se_queda_en_la_primera_linea() {
        assert_eq!(
            short_message("NameError: name 'x' is not defined\nTraceback (most recent call last):"),
            "NameError: name 'x' is not defined",
            "la traza de Python no es parte del defecto"
        );
        assert_eq!(short_message("  con espacios  "), "con espacios");
        assert_eq!(short_message(""), "");
    }

    #[test]
    fn el_mensaje_largo_se_recorta_sin_romper_unicode() {
        let largo = "a".repeat(ERROR_MESSAGE_MAX + 10);
        let recortado = short_message(&largo);
        assert_eq!(recortado.chars().count(), ERROR_MESSAGE_MAX + 1);
        assert!(recortado.ends_with('…'));

        // Recortar por bytes partiria un caracter de dos y dejaria el pack con
        // texto invalido.
        let unicode = "ñ".repeat(ERROR_MESSAGE_MAX + 1);
        let recortado = short_message(&unicode);
        assert_eq!(recortado.chars().count(), ERROR_MESSAGE_MAX + 1);
        assert!(recortado.starts_with('ñ'));
    }

    #[test]
    fn el_veredicto_decide_el_cubo_y_lo_desconocido_no_parece_vivo() {
        assert_eq!(Bucket::from_verdict(Verdict::Vivo), Bucket::Live);
        assert_eq!(
            Bucket::from_verdict(Verdict::PayloadCambiado),
            Bucket::PayloadChanged
        );
        assert_eq!(Bucket::from_verdict(Verdict::Historico), Bucket::Historical);
        assert_eq!(
            Bucket::from_verdict(Verdict::Desconocido),
            Bucket::Historical,
            "sin version no es accionable contra el codigo de hoy"
        );
    }

    #[test]
    fn el_cubo_por_mensaje_separa_ruido_y_plataforma() {
        // Ruido de motor: no se arregla desde el robot.
        assert_eq!(
            bucket_of(r#"EXCEPTION IN (<string> zdev, L_1375 ""): Error: Incorrect padding"#),
            Some(Bucket::Noise)
        );
        // Defecto de un modulo de la plataforma: no es ruido transitorio.
        assert_eq!(
            bucket_of(r#"EXCEPTION IN (<string> datatransform, L_212 ""): AttributeError: x"#),
            Some(Bucket::Platform)
        );
        // Error del robot: hay que atribuirlo antes de colocarlo.
        assert_eq!(
            bucket_of(r#"EXCEPTION IN (<string> Rocketbot, L_91 ""): NameError: name 'x'"#),
            None
        );
    }

    #[test]
    fn la_version_del_grupo_es_la_mas_alta() {
        assert_eq!(max_version(None, None), None);
        assert_eq!(max_version(Some(25), None), Some(25));
        assert_eq!(max_version(None, Some(29)), Some(29));
        assert_eq!(max_version(Some(25), Some(29)), Some(29));
        assert_eq!(max_version(Some(29), Some(25)), Some(29));
    }

    /// Db minima con un snapshot por robot: `errors_for` atribuye cada error al
    /// snapshot vivo, y sin snapshots no habria a que atribuirlo.
    fn db_con_robots(dir: &Path, robots: &[&str]) -> rusqlite::Connection {
        use base64::Engine as _;

        let conn = rusqlite::Connection::open(dir.join("robot.db")).expect("creo la db");
        conn.execute_batch(
            "CREATE TABLE bots (
                id INTEGER PRIMARY KEY,
                name TEXT,
                created_at TEXT,
                data TEXT
            );",
        )
        .expect("creo la tabla bots");
        for (index, robot) in robots.iter().enumerate() {
            // El `id` del nodo tiene que coincidir con el `cmd_id` del log: es la
            // unica union fiable entre ejecucion y codigo.
            let project = serde_json::json!({
                "project": {
                    "commands": [{
                        "father": "log_message",
                        "line": "1",
                        "id": format!("{robot}-1"),
                        "command": "hola",
                        "children": [],
                        "else": [],
                    }],
                    "vars": [],
                }
            });
            let b64 =
                base64::engine::general_purpose::STANDARD.encode(project.to_string().as_bytes());
            conn.execute(
                "INSERT INTO bots (id, name, created_at, data) \
                 VALUES (?1, ?2, '2026-11-17 10:00:00', ?3)",
                rusqlite::params![index as i64 + 1, robot, b64],
            )
            .expect("inserto el snapshot");
        }
        conn
    }

    /// Log con una peticion del robot y un error que la hereda, como los escribe el
    /// runner: la linea de error no dice de que nodo es.
    fn log_de(robot: &str) -> String {
        format!(
            "2026-11-17 12:43:47.586814 - rocketbot - INFO - request: {robot} : line 1 - rpasystem \
             : log_message  id: {robot}-1 - INFO - hola\n\
             2026-11-17 12:43:48.000000 - rocketbot - ERROR - EXCEPTION IN (<string> Rocketbot, \
             L_91 \"\"): NameError: name 'x' is not defined\n"
        )
    }

    /// El cache no puede cambiar el informe: dos robots sobre el mismo directorio
    /// dan lo mismo que daban sin cache, y el segundo reaprovecha el parseo del
    /// primero.
    #[test]
    fn dos_robots_sobre_los_mismos_logs_conservan_el_informe_y_aprovechan_el_cache() {
        let _turno = logs::cache_test_guard();
        logs::clear_cache();

        let tmp = tempfile::tempdir().expect("tempdir");
        let conn = db_con_robots(tmp.path(), &["alfa", "beta"]);
        let logs_dir = tmp.path().join("logs");
        std::fs::create_dir_all(&logs_dir).expect("creo el directorio de logs");
        for robot in ["alfa", "beta"] {
            std::fs::write(logs_dir.join(format!("logfile_{robot}.log")), log_de(robot))
                .expect("escribo el log");
        }

        // Primera pasada: dos ficheros parseados, ningun acierto.
        let alfa = errors_for(&conn, "alfa", &logs_dir, 20).expect("informe de alfa");
        assert_eq!(
            alfa.live.len(),
            1,
            "el error hereda la peticion viva de alfa: {alfa:?}"
        );
        assert_eq!(
            logs::cache_stats(),
            (2, 0),
            "un fichero cacheado por fichero leido"
        );

        // Segundo robot, mismos ficheros: ni una lectura nueva.
        let beta = errors_for(&conn, "beta", &logs_dir, 20).expect("informe de beta");
        assert_eq!(
            logs::cache_stats(),
            (2, 2),
            "el segundo robot acierta en los dos ficheros, sin parsearlos otra vez"
        );
        assert_eq!(beta.live.len(), 1, "beta tiene su propio error: {beta:?}");
        assert_eq!(
            beta.live[0].occurrences, 1,
            "el error de alfa no puede contarse en el informe de beta"
        );

        // Y alfa, otra vez, da exactamente el mismo informe que la primera vez.
        let otra_vez = errors_for(&conn, "alfa", &logs_dir, 20).expect("informe de alfa otra vez");
        assert_eq!(otra_vez, alfa, "el cache no cambia ni un cubo del informe");
        assert_eq!(
            logs::cache_stats(),
            (2, 4),
            "dos aciertos mas, ningun parseo"
        );
    }

    /// Un directorio sin ficheros de log no entra al cache ni inventa un informe.
    #[test]
    fn sin_logs_el_informe_sale_vacio_y_el_cache_intacto() {
        let _turno = logs::cache_test_guard();
        logs::clear_cache();

        let tmp = tempfile::tempdir().expect("tempdir");
        let conn = db_con_robots(tmp.path(), &["alfa"]);
        let vacio = tmp.path().join("sin_logs");

        let informe = errors_for(&conn, "alfa", &vacio, 20).expect("informe vacio");
        assert!(informe.is_empty(), "sin logs no hay errores: {informe:?}");
        assert_eq!(logs::cache_stats(), (0, 0), "no hay ficheros que cachear");
    }
}
