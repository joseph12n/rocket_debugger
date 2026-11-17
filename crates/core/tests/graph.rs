//! Test de aceptacion del issue #3 sobre el caso medido: un orquestador cuyas
//! variables son la superficie de API de sus subrobots.
//!
//! Ningun test toca `C:\Rocketbot`: los robots se construyen en memoria.

use std::collections::BTreeSet;

use rocketdebug_core::graph::{build_edges, contracts, unused_declared, RobotFacts};
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
        name: name.to_string(),
        nodes,
        declared: declared.clone(),
    }
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
