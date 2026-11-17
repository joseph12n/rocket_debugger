//! Cableado del grafo y las reglas: lo que convierte cuatro modulos en producto.
//!
//! [`crate::graph`] sabe de llamadas entre robots y [`crate::rules`] sabe de
//! defectos dentro de un flujo, pero ninguno de los dos sirve solo: el grafo, por
//! separado, no dice nada al agente, y las reglas sin grafo marcan como muertas
//! las variables que en realidad son la interfaz de los subrobots (medido: 86
//! falsos positivos de 89 en un orquestador de 8 hijos).
//!
//! Este modulo hace el recorrido completo y devuelve ya resuelto lo que el render
//! necesita: los hallazgos del robot pedido, sus contratos de entrada y salida y
//! las variables que siguen sin uso incluso contando lo que consumen sus hijos.
//!
//! # Por que necesita la db entera
//!
//! El grafo solo existe si se tienen los hechos de **todos** los robots de la db:
//! para saber que provee el padre y que lee el hijo hay que mirar los dos lados.
//! Por eso se aplanan las ultimas versiones de toda la db, no solo la del robot
//! pedido.
//!
//! # Limite conocido
//!
//! El analisis describe el **ultimo** snapshot de cada robot, mientras que el pack
//! puede construirse sobre uno anterior (`--snapshot`). En ese caso el flujo
//! listado y los hallazgos hablan de versiones distintas del mismo robot; se
//! acepta porque el caso normal es diagnosticar el estado actual y el historico
//! se usa para atribuir errores, no para listar hallazgos.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::db;
use crate::error::{Error, Result};
use crate::graph::{self, Contract, Edge, RobotFacts};
use crate::rules::{self, Finding};
use crate::tree;

/// Resultado del analisis cruzado de un robot dentro de su db.
#[derive(Debug, Clone)]
pub struct Analysis {
    /// Hallazgos ya ordenados por severidad y truncados por [`rules::evaluate`].
    pub findings: Vec<Finding>,
    /// Contratos en los que este robot es el padre: lo que ofrece a sus hijos.
    pub contracts_out: Vec<Contract>,
    /// Contratos en los que este robot es el hijo: lo que espera de sus padres.
    pub contracts_in: Vec<Contract>,
    /// Declaradas que siguen sin uso despues de descontar lo que leen los hijos.
    pub unused_declared: Vec<String>,
    /// Llamadas cuyo hijo no se puede resolver con certeza, como `"padre -> hijo"`.
    ///
    /// [`analyse`] construye los hechos con una sola db, asi que hoy esta lista
    /// sale vacia: [`graph::build_edges`] solo marca [`Edge::ambiguous`] cuando el
    /// nombre existe **solo** en otra db, y para saberlo hacen falta los hechos de
    /// esa otra db. El campo existe para que el consumidor (UI, daemon multi-db)
    /// no cambie de forma cuando el grafo se alimente con varias.
    pub ambiguous_calls: Vec<String>,
}

/// Analiza un robot usando el resto de la db como contexto.
///
/// Devuelve [`Error::RobotNotFound`] si el robot no esta en la db y deja pasar el
/// error de lectura si es **el robot pedido** el que no se puede decodificar: un
/// pack del que no se pueden leer los nodos no es un pack degradado, es una
/// mentira. Un robot distinto e ilegible solo se omite, porque como mucho hara
/// que su contrato no aparezca.
pub fn analyse(conn: &rusqlite::Connection, db_label: &str, robot: &str) -> Result<Analysis> {
    let facts = load_facts(conn, db_label, robot)?;
    let edges = graph::build_edges(&facts);
    let all_contracts = graph::contracts(&facts, &edges);

    let this = facts
        .iter()
        .find(|fact| fact.name == robot)
        .ok_or_else(|| Error::RobotNotFound(robot.to_string()))?;

    // `unused_declared` devuelve un mapa por robot y omite los que no tienen
    // variables muertas: hay que sacar la entrada de este robot y asumir vacio.
    let mut dead_by_robot = graph::unused_declared(&facts, &all_contracts);
    let unused_declared = dead_by_robot.remove(robot).unwrap_or_default();

    let collisions = tree::collisions(&this.nodes);
    let findings = rules::evaluate(
        &this.nodes,
        &collisions,
        &unused_declared,
        this.declared.len(),
    );

    let mut contracts_out = Vec::new();
    let mut contracts_in = Vec::new();
    for contract in &all_contracts {
        // Un robot puede llamarse a si mismo (bucle de reintento): entonces el
        // contrato es de ida y vuelta y aparece en las dos listas.
        if contract.parent == robot {
            contracts_out.push(contract.clone());
        }
        if contract.child == robot {
            contracts_in.push(contract.clone());
        }
    }

    Ok(Analysis {
        findings,
        contracts_out,
        contracts_in,
        unused_declared,
        ambiguous_calls: ambiguous_calls(&edges, robot),
    })
}

/// Extrae los hechos (nodos + declaradas) del **ultimo** snapshot de cada robot.
///
/// `load_snapshots` decodifica todas las versiones porque no hay consulta que
/// pida solo la ultima; con las dbs reales (decenas de versiones, cientos de KB)
/// es barato y evita una segunda forma de leer `bots` que se desincronice cuando
/// cambie el formato. Cuando llegue el daemon, esto se cachea por (db, robot).
fn load_facts(conn: &rusqlite::Connection, db_label: &str, robot: &str) -> Result<Vec<RobotFacts>> {
    let mut facts = Vec::new();

    for summary in db::list_robots(conn)? {
        let snaps = match db::load_snapshots(conn, &summary.name) {
            Ok(snaps) => snaps,
            // Ver doc de `analyse`: omite el resto, propaga el del robot pedido.
            Err(_) if summary.name != robot => continue,
            Err(e) => return Err(e),
        };
        let Some(snap) = snaps.last() else {
            continue;
        };
        let root = db::project_root(&snap.project);
        facts.push(RobotFacts {
            db: db_label.to_string(),
            name: summary.name.clone(),
            nodes: tree::flatten(commands_of(root)),
            declared: declared_of(root),
        });
    }

    if !facts.iter().any(|fact| fact.name == robot) {
        return Err(Error::RobotNotFound(robot.to_string()));
    }
    Ok(facts)
}

/// Aristas ambiguas del robot, ya formateadas para el consumidor.
///
/// Se deduplican porque un hijo homologo en dos dbs ajenas genera una arista por
/// db y el agente solo necesita saber que la llamada no esta resuelta aqui.
fn ambiguous_calls(edges: &[Edge], robot: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for edge in edges {
        if !edge.ambiguous || edge.parent != robot {
            continue;
        }
        let call = format!("{} -> {}", edge.parent, edge.child);
        if !out.contains(&call) {
            out.push(call);
        }
    }
    out
}

fn commands_of(root: &Value) -> &[Value] {
    match root.get("commands").and_then(Value::as_array) {
        Some(v) => v.as_slice(),
        None => &[],
    }
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
            ambiguous_calls(&edges, "orq"),
            vec!["orq -> fantasma".to_string()],
            "sin duplicados y sin las aristas de otros padres"
        );
        assert!(ambiguous_calls(&edges, "nadie").is_empty());
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
    fn un_root_sin_commands_no_rompe_el_aplanado() {
        assert!(commands_of(&serde_json::json!({"vars": []})).is_empty());
    }
}
