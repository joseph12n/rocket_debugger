//! Motor de reglas: hallazgos accionables sobre un flujo ya aplanado.
//!
//! Las reglas son **datos** (`rules/builtin.json`), no codigo: id, severidad,
//! alcance, condicion y arreglo. El evaluador interpreta el id y rellena el
//! mensaje con lo que ve en el robot; el `fix` sale del JSON, de modo que afinar
//! una recomendacion no obliga a tocar Rust.
//!
//! El producto son "unos pocos hallazgos", no un informe. Por eso todo se ordena
//! por severidad y se trunca: un motor que enumera cada detalle que ve se deja de
//! leer a la semana.
//!
//! Un hallazgo no vale lo mismo que otro segun donde apunte: los **accionables**
//! senalan una linea con un defecto concreto (una colision, un catch vacio, una
//! ruta absoluta) y se arreglan leyendo esa linea; los de **higiene** describen
//! un habito del robot entero ("sin trycatch", "sin else", "anidado", "variables
//! muertas") y son ciertos pero genericos: en las dbs reales son la mayoria y un
//! lector humano los ignora. Separarlos en dos cubos ([`evaluate_split`]) deja que
//! el render les de secciones y topes distintos sin tocar la severidad, que sigue
//! midiendo el dano y no la accionabilidad.

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BTreeSet};

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer};

use crate::pathscan;
use crate::tree::{self, Branch, FlatNode};

/// Hallazgos que devuelve [`evaluate`] como maximo. Es el presupuesto de
/// atencion: cinco cosas que arreglar valen mas que noventa que ignorar.
pub const MAX_FINDINGS: usize = 5;

/// Niveles de anidamiento (depth 0 cuenta como 1) a partir de los cuales el
/// flujo cuesta seguirlo.
pub const DEPTH_LIMIT: usize = 4;

/// JSON embebido: el binario no depende de rutas de instalacion.
const BUILTIN_RULES: &str = include_str!("../rules/builtin.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Alta,
    Media,
    Baja,
}

impl Severity {
    /// No se puede derivar `Ord`: el orden de las variantes lo fija el contrato
    /// (`Alta, Media, Baja`) y aqui Alta tiene que ser la mayor. El rango
    /// explicito evita que un reordenamiento del enum cambie la prioridad.
    fn rank(self) -> u8 {
        match self {
            Severity::Baja => 0,
            Severity::Media => 1,
            Severity::Alta => 2,
        }
    }

    /// Etiqueta para el render, en espanol y en minusculas (`alta`, `media`, `baja`).
    ///
    /// El `Debug` del enum es ingles por convencion de codigo, pero el pack lo lee
    /// un agente junto al resto del markdown, que esta en espanol; derivarlo del
    /// enum en el render multiplicaria el `match` en cada modulo que lo muestre.
    pub fn severity_label(&self) -> &'static str {
        match self {
            Severity::Alta => "alta",
            Severity::Media => "media",
            Severity::Baja => "baja",
        }
    }
}

impl Ord for Severity {
    fn cmp(&self, other: &Self) -> Ordering {
        self.rank().cmp(&other.rank())
    }
}

impl PartialOrd for Severity {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<'de> Deserialize<'de> for Severity {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        match raw.trim().to_ascii_lowercase().as_str() {
            "alta" => Ok(Severity::Alta),
            "media" => Ok(Severity::Media),
            "baja" => Ok(Severity::Baja),
            otro => Err(DeError::custom(format!(
                "severidad desconocida `{otro}`: usa alta, media o baja"
            ))),
        }
    }
}

/// Eje de la separacion util: si el defecto se arregla leyendo una linea.
///
/// El criterio objetivo es "tiene linea concreta", pero se declara por **regla**
/// en `rules/builtin.json` y no se deduce del `line` de cada hallazgo: el render
/// necesita saber el cubo para elegir seccion y tope antes de que exista la
/// evaluacion, y una regla no debe cambiar de cubo porque hoy no dispare. Los
/// accionables siempre llevan linea (hay test que lo fija); `IF_SIN_ELSE` es
/// higiene aunque apunte a una linea, porque el defecto es el habito repetido y
/// no ese `if` en concreto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Senala una linea con un defecto concreto: se arregla en el sitio.
    Accionable,
    /// Afecta al robot entero: cierto, pero generico y masivo.
    Higiene,
}

impl Kind {
    /// Etiqueta para el render, en espanol y en minusculas (`accionable`, `higiene`).
    ///
    /// Misma razon que [`Severity::severity_label`]: derivarla del `Debug` en cada
    /// modulo que muestre hallazgos multiplicaria el `match`.
    pub fn label(&self) -> &'static str {
        match self {
            Kind::Accionable => "accionable",
            Kind::Higiene => "higiene",
        }
    }
}

impl<'de> Deserialize<'de> for Kind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Misma tolerancia que `Severity`: el JSON lo edita una persona y un
        // espacio o una mayuscula de mas no deben tumbar el motor.
        let raw = String::deserialize(deserializer)?;
        match raw.trim().to_ascii_lowercase().as_str() {
            "accionable" => Ok(Kind::Accionable),
            "higiene" => Ok(Kind::Higiene),
            otro => Err(DeError::custom(format!(
                "kind desconocido `{otro}`: usa accionable o higiene"
            ))),
        }
    }
}

/// Una regla declarada en `rules/builtin.json`.
///
/// `scope` y `when` son metadata para la UI/CLI: el evaluador decide con `id` y
/// redacta el mensaje con los datos del robot. `fix` es el texto que se copia en
/// cada hallazgo.
#[derive(Debug, Clone, Deserialize)]
pub struct RuleSpec {
    pub id: String,
    pub severity: Severity,
    /// Cubo al que pertenece la regla. Obligatorio: un `kind` ausente es un fallo
    /// ruidoso al parsear (y en el primer test), no una regla mal clasificada.
    pub kind: Kind,
    pub scope: String,
    pub when: String,
    pub fix: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub rule_id: String,
    pub severity: Severity,
    /// Cubo heredado de la regla: el render agrupa y trunca por aqui.
    pub kind: Kind,
    /// Linea de Studio del nodo implicado. Vacia cuando el hallazgo afecta al
    /// robot entero (`SIN_TRY`, `ANIDAMIENTO`, `VAR_NO_USADA`).
    ///
    /// No es el eje de [`Kind`]: las accionables siempre traen linea, pero
    /// `IF_SIN_ELSE` trae una y aun asi es higiene.
    pub line: String,
    pub message: String,
    pub fix: String,
}

/// Carga las reglas embebidas.
///
/// Son siete objetos y ~2 KB: se parsea en cada evaluacion en vez de mantener
/// una cache global. Si el JSON se rompe, el `expect` lo dice en el primer test
/// en vez de degradar en silencio a un motor sin reglas.
pub fn builtin_rules() -> Vec<RuleSpec> {
    serde_json::from_str(BUILTIN_RULES)
        .expect("rules/builtin.json embebido no parsea: revisa ids y severidades")
}

/// Una regla concreta por id.
///
/// Lo usan las evaluaciones que no viven en este modulo (por ejemplo
/// `paths::RUTA_MIGRADA`, que necesita comprobar el disco y no puede ser pura).
pub fn builtin_rule(id: &str) -> Option<RuleSpec> {
    builtin_rules().into_iter().find(|rule| rule.id == id)
}

/// Evalua todas las reglas y devuelve hasta [`MAX_FINDINGS`] hallazgos,
/// ordenados de severidad mayor a menor.
///
/// Los dos cubos compiten aqui por el mismo presupuesto: para darle a cada uno su
/// tope y su seccion, usa [`evaluate_split`].
///
/// - `collisions`: salida de [`crate::tree::collisions`].
/// - `declared_unused`: variables sin uso ya filtradas por el grafo de llamadas;
///   aqui no se decide que esta sin usar.
/// - `vars_total`: variables declaradas en el robot, para dimensionar el aviso.
pub fn evaluate(
    nodes: &[FlatNode],
    collisions: &BTreeMap<String, usize>,
    declared_unused: &[String],
    vars_total: usize,
) -> Vec<Finding> {
    evaluate_with_limit(nodes, collisions, declared_unused, vars_total, MAX_FINDINGS)
}

/// Igual que [`evaluate`] con un tope explicito (tests, UI con presupuesto).
pub fn evaluate_with_limit(
    nodes: &[FlatNode],
    collisions: &BTreeMap<String, usize>,
    declared_unused: &[String],
    vars_total: usize,
    max_findings: usize,
) -> Vec<Finding> {
    let mut out = Vec::new();
    for rule in builtin_rules() {
        match rule.id.as_str() {
            "LINEA_COLISION" => line_collisions(&rule, collisions, &mut out),
            "CATCH_VACIO" => empty_catches(&rule, nodes, &mut out),
            "SIN_TRY" => require_try(&rule, nodes, &mut out),
            "IF_SIN_ELSE" => ifs_without_else(&rule, nodes, &mut out),
            "RUTA_ABSOLUTA" => absolute_paths(&rule, nodes, &mut out),
            "ANIDAMIENTO" => nesting(&rule, nodes, &mut out),
            "VAR_NO_USADA" => unused_vars(&rule, declared_unused, vars_total, &mut out),
            // Una regla nueva en el JSON sin interprete aqui no inventa ruido:
            // se ignora hasta que exista su evaluador.
            _ => {}
        }
    }
    // Estable: dentro de la misma severidad se conserva el orden de generacion,
    // que ya es deterministico (BTreeMap, pre-orden, BTreeSet).
    out.sort_by_key(|f| Reverse(f.severity));
    out.truncate(max_findings);
    out
}

/// Devuelve (accionables, higiene), cada cubo ordenado por severidad descendente
/// y truncado a `max_por_cubo`.
///
/// Es la misma evaluacion que [`evaluate_with_limit`] sin tope, repartida por
/// [`Kind`]: filtrar despues de ordenar conserva el orden relativo dentro de cada
/// cubo y no duplica las siete reglas, que es donde vive el coste de mantener dos
/// motores. El tope de [`MAX_FINDINGS`] sigue existiendo para quien quiera una
/// sola lista; aqui cada cubo tiene su presupuesto porque un hallazgo de higiene
/// no compite con uno accionable: con un tope comun, los de higiene (mas
/// numerosos) se comen las lineas que un humano si miraria (44 de los 122
/// hallazgos reales eran `SIN_TRY` e `IF_SIN_ELSE`, y de ahi el 64% de precision
/// util).
pub fn evaluate_split(
    nodes: &[FlatNode],
    collisions: &BTreeMap<String, usize>,
    declared_unused: &[String],
    vars_total: usize,
    max_por_cubo: usize,
) -> (Vec<Finding>, Vec<Finding>) {
    let mut accionables = Vec::new();
    let mut higiene = Vec::new();
    // `usize::MAX` desactiva el tope interno (`truncate` con un limite mayor que
    // la lista no hace nada): el recorte se aplica luego, una vez por cubo.
    for finding in evaluate_with_limit(nodes, collisions, declared_unused, vars_total, usize::MAX) {
        match finding.kind {
            Kind::Accionable => accionables.push(finding),
            Kind::Higiene => higiene.push(finding),
        }
    }
    accionables.truncate(max_por_cubo);
    higiene.truncate(max_por_cubo);
    (accionables, higiene)
}

fn line_collisions(rule: &RuleSpec, collisions: &BTreeMap<String, usize>, out: &mut Vec<Finding>) {
    for (line, count) in collisions {
        // El mapa deberia venir ya filtrado, pero un llamador puede pasar el
        // recuento completo: no convertimos una linea unica en un hallazgo.
        if *count < 2 {
            continue;
        }
        push_finding(
            rule,
            line,
            format!("la linea {line} aparece {count} veces: direccionar por linea puede editar el nodo equivocado"),
            out,
        );
    }
}

fn empty_catches(rule: &RuleSpec, nodes: &[FlatNode], out: &mut Vec<Finding>) {
    for (i, node) in nodes.iter().enumerate() {
        if node.cmd == "trycatch" && !has_else_branch(nodes, i) {
            push_finding(
                rule,
                &node.line,
                "trycatch sin catch: la rama else esta vacia, asi que el error no se registra ni se recupera"
                    .to_string(),
                out,
            );
        }
    }
}

fn require_try(rule: &RuleSpec, nodes: &[FlatNode], out: &mut Vec<Finding>) {
    if nodes.iter().any(|n| n.cmd == "trycatch") {
        return;
    }
    push_finding(
        rule,
        "",
        "el robot no tiene ningun trycatch: un error corta el flujo sin registro ni recuperacion"
            .to_string(),
        out,
    );
}

fn ifs_without_else(rule: &RuleSpec, nodes: &[FlatNode], out: &mut Vec<Finding>) {
    for (i, node) in nodes.iter().enumerate() {
        if node.cmd == "evaluateIf" && !has_else_branch(nodes, i) {
            push_finding(
                rule,
                &node.line,
                "evaluateIf sin rama else: cuando la condicion es falsa no hay camino alternativo"
                    .to_string(),
                out,
            );
        }
    }
}

fn absolute_paths(rule: &RuleSpec, nodes: &[FlatNode], out: &mut Vec<Finding>) {
    for node in nodes {
        if let Some(path) = absolute_path(&node.params) {
            push_finding(
                rule,
                &node.line,
                format!("ruta absoluta en los parametros: {path}; no es portable a otra maquina"),
                out,
            );
        }
    }
}

fn nesting(rule: &RuleSpec, nodes: &[FlatNode], out: &mut Vec<Finding>) {
    let depth = tree::max_depth(nodes);
    if depth >= DEPTH_LIMIT {
        push_finding(
            rule,
            "",
            format!("el flujo llega a {depth} niveles de anidamiento (umbral {DEPTH_LIMIT})"),
            out,
        );
    }
}

fn unused_vars(
    rule: &RuleSpec,
    declared_unused: &[String],
    vars_total: usize,
    out: &mut Vec<Finding>,
) {
    // El mismo nombre puede llegar repetido si el llamador une varias fuentes:
    // un hallazgo por variable, no por repeticion.
    let unused: BTreeSet<&str> = declared_unused
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let total = vars_total.max(unused.len());
    let count = unused.len();
    for name in unused {
        push_finding(
            rule,
            "",
            format!("variable declarada sin uso: {name} ({count} de {total} declaradas)"),
            out,
        );
    }
}

/// ¿El nodo de `index` tiene algun hijo directo en su rama `else`?
///
/// El aplanado es pre-orden: el subarbol de un nodo acaba en el primer nodo con
/// profundidad menor o igual. Dentro de ese tramo, un hijo directo de la rama
/// `else` es el unico que cumple `depth == d + 1` y `branch == Else`: los hijos
/// de `children` llevan `Then` y los nietos van a `d + 2`.
///
/// Mirar solo el nodo siguiente confundiria un try con cuerpo y sin catch (su
/// primer hijo es `Then`) con un try bien cerrado.
fn has_else_branch(nodes: &[FlatNode], index: usize) -> bool {
    let depth = nodes[index].depth;
    nodes[index + 1..]
        .iter()
        .take_while(|n| n.depth > depth)
        .any(|n| n.depth == depth + 1 && n.branch == Branch::Else)
}

fn push_finding(rule: &RuleSpec, line: &str, message: String, out: &mut Vec<Finding>) {
    out.push(Finding {
        rule_id: rule.id.clone(),
        severity: rule.severity,
        // El cubo lo decide la regla en el JSON, no la linea del hallazgo: asi
        // una regla no cambia de seccion segun lo que encuentre hoy.
        kind: rule.kind,
        line: line.to_string(),
        message,
        fix: rule.fix.clone(),
    });
}

/// Primera ruta con letra de unidad (`C:\`, `C:/`) que no viva dentro de una URL.
///
/// La trampa es `https://`: la `s` va seguida de `:` y `/`, asi que el patron
/// `X:/` la acepta. Se exige que la letra no continue un esquema y que el token
/// no lleve `://` antes de la ruta.
fn absolute_path(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    (0..bytes.len().saturating_sub(2)).find_map(|i| {
        let is_drive = bytes[i].is_ascii_alphabetic()
            && bytes[i + 1] == b':'
            && matches!(bytes[i + 2], b'\\' | b'/');
        if !is_drive || pathscan::continues_scheme(text, i) || pathscan::inside_url_token(text, i) {
            return None;
        }
        Some(path_snippet(&text[i..]))
    })
}

/// Recorta la ruta para el mensaje: un parametro puede ser un JSON entero.
///
/// El tope es aqui y no en `pathscan` porque es formato del hallazgo, no deteccion:
/// el pack de 60 corta tambien en la coma (un parametro puede ser un CSV) y el del
/// diagnostico aguanta 260 porque la ruta entera se va a comprobar en disco.
fn path_snippet(rest: &str) -> String {
    rest.chars()
        .take(PATH_SNIPPET_MAX)
        .take_while(|c| !matches!(c, '"' | '\'' | '\n' | '\r' | '}' | ']' | ';' | ',' | '|'))
        .collect()
}

const PATH_SNIPPET_MAX: usize = 60;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn alta_es_mayor_que_media_y_media_mayor_que_baja() {
        assert!(Severity::Alta > Severity::Media);
        assert!(Severity::Media > Severity::Baja);
        assert!(Severity::Alta > Severity::Baja);

        let mut ordenadas = vec![Severity::Baja, Severity::Alta, Severity::Media];
        ordenadas.sort();
        assert_eq!(
            ordenadas,
            vec![Severity::Baja, Severity::Media, Severity::Alta]
        );
    }

    #[test]
    fn las_severidades_tienen_etiqueta_en_espanol_para_el_render() {
        assert_eq!(Severity::Alta.severity_label(), "alta");
        assert_eq!(Severity::Media.severity_label(), "media");
        assert_eq!(Severity::Baja.severity_label(), "baja");
    }

    #[test]
    fn las_reglas_embebidas_parsean_y_estan_completas() {
        let rules = builtin_rules();
        let mut ids: Vec<&str> = rules.iter().map(|r| r.id.as_str()).collect();
        ids.sort_unstable();
        let mut esperados = vec![
            "ANIDAMIENTO",
            "CATCH_VACIO",
            "IF_SIN_ELSE",
            "LINEA_COLISION",
            "RUTA_ABSOLUTA",
            "RUTA_MIGRADA",
            "SIN_TRY",
            "VAR_NO_USADA",
        ];
        esperados.sort_unstable();
        assert_eq!(
            ids, esperados,
            "el JSON y el evaluador deben cubrir lo mismo"
        );
        assert!(rules.iter().all(|r| !r.when.trim().is_empty()));
        assert!(rules.iter().all(|r| !r.scope.trim().is_empty()));
        assert!(rules.iter().all(|r| !r.fix.trim().is_empty()));
        assert_eq!(
            rules
                .iter()
                .filter(|r| r.severity == Severity::Alta)
                .count(),
            3
        );
    }

    #[test]
    fn las_reglas_embebidas_reparten_las_ocho_en_dos_cubos() {
        let rules = builtin_rules();
        assert_eq!(rules.len(), 8, "ocho reglas declaradas");

        let mut accionables: Vec<&str> = rules
            .iter()
            .filter(|r| r.kind == Kind::Accionable)
            .map(|r| r.id.as_str())
            .collect();
        accionables.sort_unstable();
        assert_eq!(
            accionables,
            vec![
                "CATCH_VACIO",
                "LINEA_COLISION",
                "RUTA_ABSOLUTA",
                "RUTA_MIGRADA"
            ],
            "las accionables son las que senalan un defecto en una linea concreta"
        );
        assert_eq!(
            rules.iter().filter(|r| r.kind == Kind::Higiene).count(),
            4,
            "el resto describe higiene del robot entero: {rules:?}"
        );
    }

    #[test]
    fn las_kinds_tienen_etiqueta_en_espanol_para_el_render() {
        assert_eq!(Kind::Accionable.label(), "accionable");
        assert_eq!(Kind::Higiene.label(), "higiene");
    }

    #[test]
    fn la_kind_acepta_texto_con_espacios_y_mayusculas() {
        let rule: RuleSpec = serde_json::from_str(
            r#"{"id":"X","severity":"alta","kind":"  HIGIENE ","scope":"robot","when":"w","fix":"f"}"#,
        )
        .expect("kind valida");
        assert_eq!(rule.kind, Kind::Higiene);
    }

    #[test]
    fn una_kind_inventada_no_parsea_en_silencio() {
        let err = serde_json::from_str::<RuleSpec>(
            r#"{"id":"X","severity":"alta","kind":"urgente","scope":"robot","when":"w","fix":"f"}"#,
        )
        .expect_err("`urgente` no es un cubo");
        assert!(err.to_string().contains("urgente"), "error: {err}");
    }

    #[test]
    fn una_regla_sin_kind_no_parsea_en_silencio() {
        // Con un `default` una regla nueva caeria en un cubo sin que nadie lo
        // decidiera; el error de parseo es la unica senal de que falta el dato.
        let err = serde_json::from_str::<RuleSpec>(
            r#"{"id":"X","severity":"alta","scope":"robot","when":"w","fix":"f"}"#,
        )
        .expect_err("falta kind");
        assert!(err.to_string().contains("kind"), "error: {err}");
    }

    #[test]
    fn la_severidad_acepta_texto_con_espacios_y_mayusculas() {
        // `kind` es obligatorio desde que los hallazgos se separan en cubos; esta
        // prueba sigue midiendo solo la tolerancia de `severity`.
        let rule: RuleSpec = serde_json::from_str(
            r#"{"id":"X","severity":" ALTA ","kind":"accionable","scope":"robot","when":"w","fix":"f"}"#,
        )
        .expect("severidad valida");
        assert_eq!(rule.severity, Severity::Alta);
    }

    #[test]
    fn una_severidad_inventada_no_parsea_en_silencio() {
        let err = serde_json::from_str::<RuleSpec>(
            r#"{"id":"X","severity":"urgente","kind":"accionable","scope":"robot","when":"w","fix":"f"}"#,
        )
        .expect_err("`urgente` no es una severidad");
        assert!(err.to_string().contains("urgente"), "error: {err}");
    }

    #[test]
    fn distingue_rutas_de_unidad_de_urls() {
        assert_eq!(absolute_path("C:/Users/x").as_deref(), Some("C:/Users/x"));
        assert_eq!(absolute_path(r"D:\datos\x").as_deref(), Some(r"D:\datos\x"));
        assert_eq!(
            absolute_path(r#"{"ruta":"C:\\Users\\x"}"#).as_deref(),
            Some(r"C:\\Users\\x")
        );
        assert_eq!(
            absolute_path(r#"abre "E:/tmp" ya"#).as_deref(),
            Some("E:/tmp")
        );
        assert!(absolute_path("https://x").is_none());
        assert!(absolute_path("https://x/C:/y").is_none());
        assert!(absolute_path("file:///C:/x").is_none());
        assert!(absolute_path("/usr/local").is_none());
        assert!(absolute_path("%base_path%/datos").is_none());
        assert!(absolute_path("{ruta}/datos").is_none());
    }

    #[test]
    fn el_else_directo_no_se_confunde_con_el_de_un_hijo() {
        let arbol = json!([{
            "father": "trycatch",
            "line": 1,
            "command": "",
            "children": [{
                "father": "evaluateIf",
                "line": "1.1",
                "command": "True",
                "children": [{
                    "father": "log_message",
                    "line": "1.1.1",
                    "command": "ok",
                    "children": [],
                    "else": []
                }],
                "else": [{
                    "father": "log_message",
                    "line": "1.1.1",
                    "command": "alt",
                    "children": [],
                    "else": []
                }]
            }],
            "else": []
        }]);
        let flat = tree::flatten(arbol.as_array().expect("lista"));

        assert!(
            !has_else_branch(&flat, 0),
            "el else del if interno no es el catch del try"
        );
        assert!(has_else_branch(&flat, 1), "el if interno si tiene else");
    }
}
