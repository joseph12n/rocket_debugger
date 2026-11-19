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

pub fn load_workspace(dbs: &[(&Connection, &str)]) -> Result<Vec<RobotFacts>> {
    load_workspace_of(dbs, None)
}

pub(super) fn load_workspace_of(
    dbs: &[(&Connection, &str)],
    target: Option<Target>,
) -> Result<Vec<RobotFacts>> {
    let mut facts: Vec<RobotFacts> = Vec::new();
    let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();

    for (conn, label) in dbs {
        let pinned = target.filter(|t| t.db_label == *label);
        for (fact, hash) in load_db_facts(conn, label, pinned)? {
            let key = (fact.name.clone(), hash);
            match seen.get(&key).copied() {
                Some(idx) => {
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

#[derive(Debug, Clone, Copy)]
pub(super) struct Target<'a> {
    pub(super) db_label: &'a str,
    pub(super) robot: &'a str,
    pub(super) snapshot: Option<i64>,
}

fn load_db_facts(
    conn: &Connection,
    db_label: &str,
    target: Option<Target>,
) -> Result<Vec<(RobotFacts, String)>> {
    let mut out = Vec::new();

    for summary in db::list_robots(conn)? {
        let required = target.is_some_and(|t| t.robot == summary.name.as_str());
        let pinned = target.filter(|_| required).and_then(|t| t.snapshot);
        let snap = match pinned {
            Some(id) => match db::load_snapshot(conn, &summary.name, id) {
                Ok(snap) => snap,
                Err(_) if !required => continue,
                Err(error) => return Err(error),
            },
            None => match db::load_latest_snapshot(conn, &summary.name) {
                Ok(Some(snap)) => snap,
                Ok(None) => continue,
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

fn content_hash(raw_b64: &str) -> String {
    blake3::hash(raw_b64.as_bytes()).to_hex().to_string()
}

pub struct Workspace {
    pub facts: Vec<RobotFacts>,
    edges: Vec<Edge>,
    unused: BTreeMap<String, Vec<String>>,
    missing: Vec<graph::MissingCall>,
}

impl Workspace {
    pub fn load(conns: &[(&Connection, &str)]) -> Result<Self> {
        let facts = load_workspace_of(conns, None)?;
        Ok(Self::from_facts(facts))
    }

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

fn analyse_loaded(
    workspace: &Workspace,
    conn: &Connection,
    db_label: &str,
    robot: &str,
    logs: Option<(&Path, usize)>,
    path_probe: &dyn PathProbe,
) -> Result<Analysis> {
    let this = workspace
        .facts
        .iter()
        .find(|fact| fact.name == robot && fact.lives_in(db_label))
        .ok_or_else(|| Error::RobotNotFound(robot.to_string()))?;

    let unused_declared = workspace.unused.get(robot).cloned().unwrap_or_default();

    let collisions = tree::collisions(&this.nodes);
    let (mut findings, higiene) = rules::evaluate_split(
        &this.nodes,
        &collisions,
        &unused_declared,
        this.declared.len(),
        usize::MAX,
    );

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
