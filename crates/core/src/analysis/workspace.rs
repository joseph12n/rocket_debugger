//! Carga de los hechos del workspace y analisis por robot sobre esa foto.
//!
//! El grafo necesita los hechos de **todas** las dbs a la vez, y la misma entrega
//! esta copiada en varias. Aqui se leen las filas necesarias de cada robot, se
//! deduplica por contenido y se calculan los derivados comunes (aristas,
//! contratos, variables muertas) **una sola vez**; el orquestador (`mod.rs`) solo
//! decide que robot se analiza y con que extras (logs, probe de disco).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rusqlite::Connection;
use serde_json::Value;

use crate::db;
use crate::error::{Error, Result};
use crate::graph::{self, Edge, RobotFacts};
use crate::paths::{self, PathProbe};
use crate::rules;
use crate::tree;

use super::{errors_for, Analysis, ErrorReport};

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
pub fn load_workspace(dbs: &[(&Connection, &str)]) -> Result<Vec<RobotFacts>> {
    load_workspace_of(dbs, None)
}

/// [`load_workspace`] con una db y un robot que el consumidor **exige**.
///
/// `target` es el robot pedido y, opcionalmente, el snapshot del que deben salir
/// sus hechos. De esa db el robot no puede faltar ([`Error::RobotNotFound`]) ni
/// fallar al decodificar (el error se propaga): es la diferencia entre un pack
/// degradado y un pack que miente. En las demas dbs un robot ilegible solo se
/// omite, porque como mucho hara que falte su contrato.
pub(super) fn load_workspace_of(
    dbs: &[(&Connection, &str)],
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
pub(super) struct Target<'a> {
    pub(super) db_label: &'a str,
    pub(super) robot: &'a str,
    /// `Some(id)` fija el snapshot; `None` acepta el ultimo.
    pub(super) snapshot: Option<i64>,
}

/// Hechos de una db, con el hash del contenido que usa el dedupe.
///
/// El snapshot es el **ultimo** de cada robot, salvo para el robot exigido: si
/// [`Target::snapshot`] trae un id, sus hechos salen de esa fila. Se lee solo la
/// fila necesaria ([`db::load_latest_snapshot`] o [`db::load_snapshot`]), no el
/// historial entero: el analisis no necesita las versiones anteriores y una
/// version vieja corrupta no debe bloquear la ultima (issue #18).
fn load_db_facts(
    conn: &Connection,
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

/// Hash del contenido de un snapshot: `blake3` del base64 tal cual esta en `bots`.
///
/// Se usa el base64 y no el JSON decodificado por la misma razon que
/// [`crate::versions::VersionInfo::hash`]: es lo que se copia entre dbs de entrega
/// y no depende de como se re-serialice el JSON.
fn content_hash(raw_b64: &str) -> String {
    blake3::hash(raw_b64.as_bytes()).to_hex().to_string()
}

/// Hechos del workspace ya cargados, con los derivados comunes calculados una
/// sola vez.
///
/// La bandeja recorre N robots de la misma db; antes cada uno llamaba a
/// `analyse_with_workspace_at_and_paths` y volvia a decodificar todo: O(N^2)
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
    pub fn load(conns: &[(&Connection, &str)]) -> Result<Self> {
        let facts = load_workspace_of(conns, None)?;
        Ok(Self::from_facts(facts))
    }

    /// Derivados comunes del conjunto de hechos: aristas, contratos y muertas.
    pub(super) fn from_facts(facts: Vec<RobotFacts>) -> Self {
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
        conn: &Connection,
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
    conn: &Connection,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
