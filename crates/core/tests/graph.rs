//! Test de aceptacion del issue #3 sobre el caso medido: un orquestador cuyas
//! variables son la superficie de API de sus subrobots.
//!
//! Ningun test toca `C:\Rocketbot`: los robots se construyen en memoria.

use std::collections::BTreeSet;

use rocketdebug_core::graph::{build_edges, contracts, missing_calls, unused_declared, RobotFacts};
use rocketdebug_core::tree::{Branch, FlatNode};

fn nodo(line: &str, cmd: &str, params: &str, write: Option<&str>) -> FlatNode {
    FlatNode {
        line: line.to_string(),
        id: format!("id-{line}-{cmd}"),
        cmd: cmd.to_string(),
        group: None,
        option: None,
        desc: None,
        params: params.to_string(),
        write_var: write.map(str::to_string),
        branch: Branch::Main,
        depth: 0,
    }
}

fn hechos(db: &str, name: &str, declared: &BTreeSet<String>, nodes: Vec<FlatNode>) -> RobotFacts {
    RobotFacts {
        db: db.to_string(),
        also_in: Vec::new(),
        name: name.to_string(),
        nodes,
        declared: declared.clone(),
    }
}

/// Nodo de llamada a un subrobot: el nombre del hijo va en `command`.
fn llamada(line: &str, child: &str) -> FlatNode {
    nodo(line, "execRocketBotDB", &format!("\"{child}\""), None)
}

/// Reproduce la forma del orquestador real: 86 variables declaradas, 8 hijos que
/// leen 83 de ellas y solo 3 que no usa nadie. Sin grafo, las 86 parecen muertas;
/// con grafo quedan las 3 reales.
#[test]
fn el_grafo_quita_los_falsos_positivos_del_orquestador() {
    const DECLARADAS: usize = 86;
    const HIJOS: usize = 8;
    // 83 variables repartidas entre los 8 hijos; las 3 ultimas no las lee nadie.
    const USADAS: usize = 83;

    let declaradas: BTreeSet<String> = (0..DECLARADAS).map(|i| format!("v{i}")).collect();

    let mut calls = Vec::new();
    for hijo in 0..HIJOS {
        calls.push(nodo(
            &(hijo + 1).to_string(),
            "execRocketBotDB",
            &format!("\"hijo_{hijo}\""),
            None,
        ));
    }
    let orquestador = hechos(r"entrega\robot_run.db", "orq", &declaradas, calls);

    let mut facts = vec![orquestador];
    let mut leidas_por_hijos = 0usize;
    for hijo in 0..HIJOS {
        let lecturas: Vec<String> = (0..USADAS)
            .filter(|i| i % HIJOS == hijo)
            .map(|i| format!("{{v{i}}}"))
            .collect();
        leidas_por_hijos += lecturas.len();
        facts.push(hechos(
            r"entrega\robot_run.db",
            &format!("hijo_{hijo}"),
            &BTreeSet::new(),
            vec![nodo("1", "log_message", &lecturas.join(" "), None)],
        ));
    }
    assert_eq!(leidas_por_hijos, USADAS);

    let edges = build_edges(&facts);
    assert_eq!(edges.len(), HIJOS);
    assert!(edges.iter().all(|e| e.same_db && !e.ambiguous));

    let contratos = contracts(&facts, &edges);
    assert_eq!(contratos.len(), HIJOS);
    assert!(
        contratos.iter().all(|c| c.missing.is_empty()),
        "el padre declara todo lo que leen los hijos"
    );

    let muertas = unused_declared(&facts, &contratos);
    assert_eq!(
        muertas.len(),
        1,
        "solo el orquestador tiene variables muertas"
    );
    assert_eq!(
        muertas["orq"],
        vec!["v83".to_string(), "v84".to_string(), "v85".to_string()],
        "las 83 que consumen los hijos no son hallazgos: son su API"
    );
}

/// Issue #12: una llamada a un robot que no esta en ninguna db se perdia en
/// silencio. En runtime el runner no resuelve el nombre y el padre falla, asi que
/// la llamada tiene que salir con su linea aunque no haya arista.
#[test]
fn una_llamada_sin_destino_no_genera_arista_pero_se_reporta() {
    let orq = hechos(
        r"entrega\robot_run.db",
        "orq",
        &BTreeSet::new(),
        vec![llamada("2", "hijo_web"), llamada("4", "no_existe")],
    );
    let hijo = hechos(
        r"entrega\robot_run.db",
        "hijo_web",
        &BTreeSet::new(),
        vec![nodo("2.1", "log_message", "Buscando", None)],
    );
    let facts = vec![orq, hijo];

    let edges = build_edges(&facts);
    assert_eq!(
        edges.len(),
        1,
        "sin hechos del hijo no hay contrato que calcular"
    );
    assert!(edges.iter().all(|e| e.child == "hijo_web"));

    let faltan = missing_calls(&facts);
    assert_eq!(faltan.len(), 1);
    assert_eq!(faltan[0].parent, "orq");
    assert_eq!(faltan[0].parent_db, r"entrega\robot_run.db");
    assert_eq!(
        faltan[0].line, "4",
        "la linea es lo unico que localiza la llamada"
    );
    assert_eq!(faltan[0].child, "no_existe");
}

/// El dedupe por contenido deja un solo hecho, pero el runner busca el nombre en
/// la db del padre: una copia identica alli no puede salir como ambigua.
#[test]
fn una_copia_identica_en_otra_db_resuelve_la_arista() {
    let orq = hechos(
        r"entrega\robot_run.db",
        "orq",
        &BTreeSet::new(),
        vec![llamada("2", "hijo_web")],
    );
    let mut hijo = hechos(r"BBRE2\robot_run.db", "hijo_web", &BTreeSet::new(), vec![]);
    hijo.also_in = vec![r"entrega\robot_run.db".to_string()];

    let facts = vec![orq, hijo];
    let edges = build_edges(&facts);
    assert_eq!(edges.len(), 1);
    assert!(
        edges[0].same_db && !edges[0].ambiguous,
        "la copia vive en la db del padre: {:?}",
        edges[0]
    );
    assert!(
        missing_calls(&facts).is_empty(),
        "el nombre existe en el workspace"
    );
}
