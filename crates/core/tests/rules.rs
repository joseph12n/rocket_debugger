//! Tests del motor de reglas sobre fixtures y JSON sintetico.
//!
//! Ningun test toca `C:\Rocketbot`: el flujo se construye con `tree::flatten` a
//! partir del fixture del repo o de JSON inline.

use std::collections::BTreeMap;

use rocketdebug_core::rules::{
    self, evaluate, evaluate_split, evaluate_with_limit, Finding, Kind, Severity,
};
use rocketdebug_core::tree::{self, FlatNode};
use serde_json::{json, Value};

const LINE_COLLISION: &str = include_str!("../../../fixtures/robots/line_collision.json");

fn node(father: &str, line: &str, command: &str) -> Value {
    json!({
        "father": father,
        "line": line,
        "command": command,
        "children": [],
        "else": [],
    })
}

fn flat(commands: Value) -> Vec<FlatNode> {
    tree::flatten(commands.as_array().expect("lista de comandos"))
}

fn fixture_nodes() -> Vec<FlatNode> {
    let root: Value = serde_json::from_str(LINE_COLLISION).expect("fixture JSON");
    let commands = root["project"]["commands"].as_array().expect("commands");
    tree::flatten(commands)
}

/// Evaluacion con el tope por defecto, sin colisiones ni variables.
fn default_findings(nodes: &[FlatNode]) -> Vec<Finding> {
    evaluate(nodes, &BTreeMap::new(), &[], 0)
}

/// Evaluacion con tope amplio: sirve para comprobar que una regla calla.
fn all_findings(nodes: &[FlatNode]) -> Vec<Finding> {
    evaluate_with_limit(nodes, &BTreeMap::new(), &[], 0, 50)
}

fn rule_ids(findings: &[Finding]) -> Vec<&str> {
    findings.iter().map(|f| f.rule_id.as_str()).collect()
}

#[test]
fn el_fixture_de_colision_produce_dos_linea_colision_sin_duplicados() {
    let nodes = fixture_nodes();
    let collisions = tree::collisions(&nodes);
    assert_eq!(collisions.len(), 2, "el fixture tiene dos lineas repetidas");

    let out = evaluate(&nodes, &collisions, &[], 0);
    let colisiones: Vec<&Finding> = out
        .iter()
        .filter(|f| f.rule_id == "LINEA_COLISION")
        .collect();

    assert_eq!(colisiones.len(), 2, "una sola vez por linea: {out:?}");
    assert_eq!(
        colisiones
            .iter()
            .map(|f| f.line.as_str())
            .collect::<Vec<_>>(),
        ["2.1", "2.2"]
    );
    assert!(colisiones.iter().all(|f| f.severity == Severity::Alta));
    assert!(
        colisiones.iter().all(|f| f.message.contains("2 veces")),
        "el mensaje debe llevar el numero de repeticiones: {out:?}"
    );

    let mut claves: Vec<(&str, &str)> = colisiones
        .iter()
        .map(|f| (f.rule_id.as_str(), f.line.as_str()))
        .collect();
    let total = claves.len();
    claves.sort_unstable();
    claves.dedup();
    assert_eq!(claves.len(), total, "sin duplicados: {out:?}");

    assert!(out.len() <= rules::MAX_FINDINGS);
    assert!(
        out.windows(2).all(|w| w[0].severity >= w[1].severity),
        "orden por severidad descendente: {out:?}"
    );
}

#[test]
fn un_robot_sin_try_produce_sin_try() {
    let nodes = flat(json!([
        node("setVar", "1", "\"x\""),
        node("log_message", "2", "{x}")
    ]));
    let out = default_findings(&nodes);
    let finding = out
        .iter()
        .find(|f| f.rule_id == "SIN_TRY")
        .unwrap_or_else(|| panic!("falta SIN_TRY: {out:?}"));

    assert_eq!(finding.severity, Severity::Media);
    assert!(finding.line.is_empty(), "el hallazgo es del robot entero");
    assert!(
        !finding.fix.is_empty(),
        "el arreglo sale del JSON de reglas"
    );
}

#[test]
fn un_trycatch_con_catch_no_produce_sin_try_ni_catch_vacio() {
    let nodes = flat(json!([{
        "father": "trycatch",
        "line": 1,
        "command": "",
        "children": [node("log_message", "1.1", "ok")],
        "else": [node("log_message", "1.1", "fallo")]
    }]));
    let out = all_findings(&nodes);
    let ids = rule_ids(&out);
    assert!(!ids.contains(&"SIN_TRY"), "ids: {ids:?}");
    assert!(!ids.contains(&"CATCH_VACIO"), "ids: {ids:?}");
}

#[test]
fn un_trycatch_con_cuerpo_y_else_vacio_produce_catch_vacio() {
    let nodes = flat(json!([{
        "father": "trycatch",
        "line": 2,
        "command": "",
        "children": [node("setVar", "2.1", "\"x\"")],
        "else": []
    }]));
    let out = all_findings(&nodes);
    let finding = out
        .iter()
        .find(|f| f.rule_id == "CATCH_VACIO")
        .unwrap_or_else(|| panic!("falta CATCH_VACIO: {out:?}"));

    assert_eq!(finding.severity, Severity::Alta);
    assert_eq!(finding.line, "2");
    assert!(!finding.fix.is_empty());
}

#[test]
fn if_sin_else_dispara_y_con_else_calla() {
    let sin_else = flat(json!([{
        "father": "evaluateIf",
        "line": 3,
        "command": "1 == 1",
        "children": [node("log_message", "3.1", "si")],
        "else": []
    }]));
    let out = all_findings(&sin_else);
    let finding = out
        .iter()
        .find(|f| f.rule_id == "IF_SIN_ELSE")
        .unwrap_or_else(|| panic!("falta IF_SIN_ELSE: {out:?}"));
    assert_eq!(finding.severity, Severity::Baja);
    assert_eq!(finding.line, "3");

    let con_else = flat(json!([{
        "father": "evaluateIf",
        "line": 3,
        "command": "1 == 1",
        "children": [node("log_message", "3.1", "si")],
        "else": [node("log_message", "3.1", "no")]
    }]));
    assert!(!rule_ids(&all_findings(&con_else)).contains(&"IF_SIN_ELSE"));
}

#[test]
fn ruta_absoluta_dispara_en_params_pero_no_con_una_url() {
    let nodes = flat(json!([
        node("openBrowser", "1", "C:/Users/x"),
        node("openBrowser", "2", "https://x"),
        node("setVar", "3", r#"{"ruta":"D:\\datos"}"#)
    ]));
    let out = all_findings(&nodes);
    let rutas: Vec<&Finding> = out
        .iter()
        .filter(|f| f.rule_id == "RUTA_ABSOLUTA")
        .collect();

    assert_eq!(rutas.len(), 2, "solo las dos rutas reales: {out:?}");
    assert_eq!(rutas[0].line, "1");
    assert!(rutas[0].message.contains("C:/Users/x"));
    assert_eq!(rutas[0].severity, Severity::Media);
    assert_eq!(rutas[1].line, "3");
    assert!(
        !out.iter()
            .any(|f| f.rule_id == "RUTA_ABSOLUTA" && f.line == "2"),
        "https://x no es una ruta de unidad: {out:?}"
    );
}

/// Cadena de `levels` niveles: cada nivel es un `evaluateIf` con else, para que
/// el unico hallazgo estructural esperable sea el de anidamiento.
fn nested(levels: usize) -> Value {
    let mut inner = node("log_message", "fin", "ok");
    for level in (0..levels.saturating_sub(1)).rev() {
        inner = json!({
            "father": "evaluateIf",
            "line": level.to_string(),
            "command": "True",
            "children": [inner],
            "else": [node("log_message", &format!("alt{level}"), "no")]
        });
    }
    inner
}

#[test]
fn anidamiento_dispara_desde_cuatro_niveles() {
    let hondo = tree::flatten(&[nested(rules::DEPTH_LIMIT)]);
    let out = all_findings(&hondo);
    let finding = out
        .iter()
        .find(|f| f.rule_id == "ANIDAMIENTO")
        .unwrap_or_else(|| panic!("falta ANIDAMIENTO: {out:?}"));
    assert_eq!(finding.severity, Severity::Media);
    assert!(finding.line.is_empty());
    assert!(finding.message.contains('4'), "{finding:?}");

    let plano = tree::flatten(&[nested(rules::DEPTH_LIMIT - 1)]);
    assert!(!rule_ids(&all_findings(&plano)).contains(&"ANIDAMIENTO"));
}

#[test]
fn variables_sin_uso_se_reportan_una_vez_y_con_contexto() {
    let nodes = flat(json!([node("log_message", "1", "ok")]));
    let unused = vec![
        "ruta".to_string(),
        "correo".to_string(),
        "ruta".to_string(),
        "  ".to_string(),
    ];
    let out = evaluate_with_limit(&nodes, &BTreeMap::new(), &unused, 4, 50);
    let vars: Vec<&Finding> = out.iter().filter(|f| f.rule_id == "VAR_NO_USADA").collect();

    assert_eq!(vars.len(), 2, "una por variable, sin duplicados: {out:?}");
    assert!(vars.iter().all(|f| f.severity == Severity::Baja));
    assert!(vars.iter().all(|f| f.line.is_empty()));
    assert!(
        vars.iter().all(|f| f.message.contains("2 de 4")),
        "el aviso se dimensiona con vars_total: {vars:?}"
    );
}

#[test]
fn un_recuento_con_lineas_unicas_no_genera_hallazgos() {
    let nodes = flat(json!([node("log_message", "1", "ok")]));
    let mut counts = BTreeMap::new();
    counts.insert("1".to_string(), 1);

    let out = evaluate_with_limit(&nodes, &counts, &[], 0, 50);
    assert!(
        !rule_ids(&out).contains(&"LINEA_COLISION"),
        "una linea unica no es una colision: {out:?}"
    );
}

#[test]
fn el_tope_de_cinco_se_respeta_y_conserva_lo_mas_grave() {
    let mut commands = Vec::new();
    for line in 1..=3 {
        commands.push(node("setVar", &line.to_string(), "\"a\""));
        commands.push(node("setVar", &line.to_string(), "\"b\""));
    }
    for line in 10..=13 {
        commands.push(node("evaluateIf", &line.to_string(), "True"));
    }
    let nodes = tree::flatten(&commands);
    let collisions = tree::collisions(&nodes);

    let out = evaluate(&nodes, &collisions, &[], 0);
    assert_eq!(out.len(), rules::MAX_FINDINGS, "{out:?}");
    assert_eq!(
        out.iter().filter(|f| f.severity == Severity::Alta).count(),
        3,
        "las tres colisiones entran antes que el ruido: {out:?}"
    );
    assert!(out.iter().any(|f| f.rule_id == "SIN_TRY"));
    assert!(
        out.windows(2).all(|w| w[0].severity >= w[1].severity),
        "orden por severidad descendente: {out:?}"
    );

    let dos = evaluate_with_limit(&nodes, &collisions, &[], 0, 2);
    assert_eq!(dos.len(), 2);
    assert!(dos.iter().all(|f| f.severity == Severity::Alta));
}

/// `(rule_id, line)` ordenados: compara conjuntos de hallazgos sin depender del
/// orden en que los cubos se concatenan.
fn claves_ordenadas(findings: &[Finding]) -> Vec<(String, String)> {
    let mut claves: Vec<(String, String)> = findings
        .iter()
        .map(|f| (f.rule_id.clone(), f.line.clone()))
        .collect();
    claves.sort();
    claves
}

#[test]
fn builtin_rules_declara_las_ocho_con_su_cubo() {
    let rules = rules::builtin_rules();
    assert_eq!(rules.len(), 8, "ocho reglas declaradas");
    assert_eq!(
        rules.iter().filter(|r| r.kind == Kind::Accionable).count(),
        4,
        "accionables: LINEA_COLISION, CATCH_VACIO, RUTA_ABSOLUTA y RUTA_MIGRADA"
    );
    assert_eq!(
        rules.iter().filter(|r| r.kind == Kind::Higiene).count(),
        4,
        "higiene: SIN_TRY, IF_SIN_ELSE, ANIDAMIENTO y VAR_NO_USADA"
    );
    assert!(
        rules
            .iter()
            .all(|r| matches!(r.kind.label(), "accionable" | "higiene")),
        "las dos etiquetas del render estan cubiertas"
    );
}

#[test]
fn el_fixture_reparte_colisiones_en_accionables_y_higiene_en_su_cubo() {
    let nodes = fixture_nodes();
    let collisions = tree::collisions(&nodes);

    let (accionables, higiene) = evaluate_split(&nodes, &collisions, &[], 0, 5);

    assert_eq!(
        rule_ids(&accionables),
        ["LINEA_COLISION", "LINEA_COLISION"],
        "las colisiones son el unico defecto con linea concreta del fixture"
    );
    assert_eq!(
        accionables
            .iter()
            .map(|f| f.line.as_str())
            .collect::<Vec<_>>(),
        ["2.1", "2.2"]
    );
    assert!(accionables.iter().all(|f| f.kind == Kind::Accionable));

    let ids_higiene = rule_ids(&higiene);
    assert!(
        ids_higiene.contains(&"IF_SIN_ELSE"),
        "el evaluateIf sin else del fixture es higiene: {higiene:?}"
    );
    assert!(
        !ids_higiene.contains(&"SIN_TRY"),
        "el fixture si tiene trycatch: {higiene:?}"
    );
    assert!(higiene.iter().all(|f| f.kind == Kind::Higiene));

    assert!(
        accionables
            .windows(2)
            .all(|w| w[0].severity >= w[1].severity),
        "orden por severidad descendente en accionables: {accionables:?}"
    );
    assert!(
        higiene.windows(2).all(|w| w[0].severity >= w[1].severity),
        "orden por severidad descendente en higiene: {higiene:?}"
    );
}

#[test]
fn un_robot_sin_try_manda_sin_try_a_higiene_y_deja_vacio_lo_accionable() {
    // El defecto es del robot entero y no apunta a ninguna linea: es exactamente
    // el ruido que hundia la precision util, asi que no puede caer en accionables.
    let nodes = flat(json!([node("log_message", "1", "ok")]));
    let (accionables, higiene) = evaluate_split(&nodes, &BTreeMap::new(), &[], 0, 5);

    assert!(accionables.is_empty(), "{accionables:?}");
    assert_eq!(rule_ids(&higiene), ["SIN_TRY"]);
    assert!(higiene[0].line.is_empty());
    assert_eq!(higiene[0].kind, Kind::Higiene);
}

#[test]
fn las_tres_accionables_senalan_una_linea_concreta() {
    // Un robot donde disparan las tres reglas accionables a la vez: colision,
    // catch vacio y ruta absoluta. Es el contrato de `Kind::Accionable`: si
    // dispara, senala una linea; sin linea no habria nada que accionar.
    let nodes = flat(json!([
        {
            "father": "trycatch",
            "line": 1,
            "command": "",
            "children": [node("openBrowser", "1.1", "C:/Users/x")],
            "else": []
        },
        node("setVar", "2", "\"a\""),
        node("setVar", "2", "\"b\"")
    ]));
    let collisions = tree::collisions(&nodes);

    let (accionables, higiene) = evaluate_split(&nodes, &collisions, &[], 0, 50);

    let mut ids = rule_ids(&accionables);
    ids.sort_unstable();
    assert_eq!(
        ids,
        ["CATCH_VACIO", "LINEA_COLISION", "RUTA_ABSOLUTA"],
        "las tres accionables disparan: {accionables:?}"
    );
    assert!(
        accionables.iter().all(|f| !f.line.trim().is_empty()),
        "una accionable sin linea no se puede accionar: {accionables:?}"
    );
    assert_eq!(
        accionables
            .iter()
            .filter(|f| f.rule_id == "CATCH_VACIO")
            .map(|f| f.line.as_str())
            .collect::<Vec<_>>(),
        ["1"]
    );
    assert_eq!(
        accionables
            .iter()
            .filter(|f| f.rule_id == "RUTA_ABSOLUTA")
            .map(|f| f.line.as_str())
            .collect::<Vec<_>>(),
        ["1.1"]
    );
    assert!(higiene.is_empty(), "aqui no hay higiene: {higiene:?}");
}

#[test]
fn cada_cubo_se_trunca_por_separado_a_su_tope() {
    // Tres colisiones (accionable, alta) y cuatro evaluateIf sin else (higiene,
    // baja) mas el SIN_TRY que comparte el cubo de higiene.
    let mut commands = Vec::new();
    for line in 1..=3 {
        commands.push(node("setVar", &line.to_string(), "\"a\""));
        commands.push(node("setVar", &line.to_string(), "\"b\""));
    }
    for line in 10..=13 {
        commands.push(node("evaluateIf", &line.to_string(), "True"));
    }
    let nodes = tree::flatten(&commands);
    let collisions = tree::collisions(&nodes);
    assert_eq!(collisions.len(), 3, "tres lineas repetidas");

    let (accionables, higiene) = evaluate_split(&nodes, &collisions, &[], 0, 2);
    assert_eq!(
        accionables.len(),
        2,
        "el cubo accionable tiene su propio tope: {accionables:?}"
    );
    assert!(
        accionables.iter().all(|f| f.kind == Kind::Accionable),
        "{accionables:?}"
    );
    assert_eq!(
        accionables
            .iter()
            .map(|f| f.line.as_str())
            .collect::<Vec<_>>(),
        ["1", "2"],
        "se queda con las mas graves y, a igualdad, las primeras: {accionables:?}"
    );

    assert_eq!(
        higiene.len(),
        2,
        "el cubo de higiene se trunca aparte, no contra las accionables: {higiene:?}"
    );
    assert!(
        higiene.iter().all(|f| f.kind == Kind::Higiene),
        "{higiene:?}"
    );
    assert_eq!(
        higiene[0].rule_id, "SIN_TRY",
        "el tope conserva lo mas grave del cubo: {higiene:?}"
    );
    assert_eq!(higiene[1].rule_id, "IF_SIN_ELSE");
    assert_eq!(higiene[1].line, "10", "orden determinista dentro del cubo");

    // Con tope amplio se ven los ocho: repartir no descarta ni duplica hallazgos.
    let (todos_accionables, todos_higiene) = evaluate_split(&nodes, &collisions, &[], 0, 50);
    assert_eq!(todos_accionables.len(), 3);
    assert_eq!(
        todos_higiene.len(),
        5,
        "cuatro evaluateIf sin else + un SIN_TRY: {todos_higiene:?}"
    );

    let mut repartidos = todos_accionables;
    repartidos.extend(todos_higiene);
    let completa = evaluate_with_limit(&nodes, &collisions, &[], 0, 50);
    assert_eq!(
        claves_ordenadas(&repartidos),
        claves_ordenadas(&completa),
        "el reparto es una particion de la misma evaluacion"
    );
}

#[test]
fn cada_hallazgo_hereda_el_kind_declarado_por_su_regla() {
    // El `Vec` de reglas vive hasta el final del test: las claves del mapa son
    // `&str` a sus ids.
    let specs = rules::builtin_rules();
    let por_id: BTreeMap<&str, Kind> = specs.iter().map(|r| (r.id.as_str(), r.kind)).collect();

    let nodes = fixture_nodes();
    let collisions = tree::collisions(&nodes);
    let unused = vec!["sobra".to_string()];
    let (accionables, higiene) = evaluate_split(&nodes, &collisions, &unused, 1, 50);
    let sin_try = flat(json!([node("log_message", "1", "ok")]));
    let (sin_try_accionables, sin_try_higiene) =
        evaluate_split(&sin_try, &BTreeMap::new(), &[], 0, 50);

    let todos: Vec<&Finding> = accionables
        .iter()
        .chain(higiene.iter())
        .chain(sin_try_accionables.iter())
        .chain(sin_try_higiene.iter())
        .collect();
    assert_eq!(
        todos.len(),
        accionables.len() + higiene.len() + sin_try_accionables.len() + sin_try_higiene.len(),
        "no se pierde ningun hallazgo al repartir"
    );
    assert!(!todos.is_empty(), "las evaluaciones producen hallazgos");
    for finding in todos {
        let esperado = por_id
            .get(finding.rule_id.as_str())
            .unwrap_or_else(|| panic!("regla desconocida: {}", finding.rule_id));
        assert_eq!(
            *esperado, finding.kind,
            "{} acabo en el cubo {:?}",
            finding.rule_id, finding.kind
        );
    }
}
