//! Render del context pack y contabilidad de tokens.
//!
//! El entregable para el agente es markdown, no JSON: es ~4x mas compacto que el
//! JSON equivalente (las claves se repiten una vez por nodo) y un modelo lo lee
//! igual de bien. Cada seccion del render es una card de la UI futura.

use std::fmt::Write as _;

use crate::graph::Contract;
use crate::pack::Pack;
use crate::rules::Severity;

/// Se cortan los parametros largos: un `execJs` de 2 KB aporta el inicio, no el final.
pub const PARAM_MAX: usize = 100;
/// Se cortan los valores de variables por la misma razon.
pub const VAR_MAX: usize = 60;

/// Llamadas de contrato que se listan por seccion.
///
/// Un orquestador con 8 hijos ya ocupa ~30 lineas de contrato; el resto del detalle
/// se consulta en el pack del subrobot, que es donde se puede actuar.
pub const CONTRACT_MAX: usize = 8;

/// Variables por linea de contrato.
///
/// El orquestador real declara 86 variables y sus hijos leen 83: listarlas todas
/// convertiria el contrato en la seccion mas grande del pack sin decir nada nuevo
/// (lo que importa es *que* se provee, no el inventario completo).
pub const CONTRACT_VARS_MAX: usize = 6;

/// Los hallazgos ya vienen truncados a 5, pero un mensaje que cita una ruta larga
/// no puede comerse el presupuesto de contexto.
const FINDING_MESSAGE_MAX: usize = 140;
const FINDING_FIX_MAX: usize = 120;

#[derive(Debug, Clone, Copy)]
pub struct Sizing {
    pub raw_chars: usize,
    pub raw_tokens: usize,
    pub render_chars: usize,
    pub render_tokens: usize,
}

impl Sizing {
    pub fn ratio(&self) -> f64 {
        if self.render_tokens == 0 {
            0.0
        } else {
            self.raw_tokens as f64 / self.render_tokens as f64
        }
    }
}

/// Estimacion de tokens. ~3,7 caracteres por token para JSON/codigo; el error
/// tipico es +-15%, suficiente para tomar decisiones de context budget.
pub fn estimate_tokens(s: &str) -> usize {
    (s.chars().count() as f64 / 3.7).ceil() as usize
}

/// Colapsa saltos de linea para que un nodo ocupe una sola linea en el render.
fn one_line(s: &str) -> String {
    let joined = s.replace("\r\n", " ⏎ ").replace('\n', " ⏎ ");
    joined.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push_str("...");
    }
    out
}

fn field(s: &str, max: usize) -> String {
    clip(&one_line(s), max)
}

/// Lista de variables acotada: `a, b, c (+3 mas)`.
///
/// `(nada)` para la lista vacia: en el contrato, "provee: " y nada detras se lee
/// como un error de formato, no como "no provee ninguna".
fn vars_preview(vars: &[String]) -> String {
    if vars.is_empty() {
        return "(nada)".to_string();
    }
    let mut out = vars
        .iter()
        .take(CONTRACT_VARS_MAX)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if vars.len() > CONTRACT_VARS_MAX {
        let _ = write!(out, " (+{} mas)", vars.len() - CONTRACT_VARS_MAX);
    }
    out
}

impl Pack {
    pub fn render_markdown(&self) -> String {
        let mut o = String::with_capacity(4096);

        let _ = writeln!(
            o,
            "# {}  (snapshot {}, {} versiones)",
            self.robot,
            self.snapshot_id,
            self.versions.len()
        );
        if let Some(d) = &self.description {
            let _ = writeln!(o, "> {}", one_line(d));
        }
        let _ = writeln!(
            o,
            "db: {} | nodos {} | vars {} | prof {}",
            self.db_label,
            self.node_count(),
            self.vars.len(),
            self.depth
        );
        if !self.collisions.is_empty() {
            let _ = writeln!(
                o,
                "!! direccionamiento por linea NO FIABLE: {} lineas duplicadas",
                self.collisions.len()
            );
            let detalle: Vec<String> = self
                .collisions
                .iter()
                .take(5)
                .map(|(line, n)| format!("{line} x{n}"))
                .collect();
            let _ = writeln!(o, "   {}", detalle.join(", "));
        }
        let _ = writeln!(o);

        self.render_findings(&mut o);
        self.render_contracts(&mut o);

        if !self.vars.is_empty() {
            let _ = writeln!(o, "## Variables ({})", self.vars.len());
            for v in &self.vars {
                match &v.value {
                    Some(val) => {
                        let _ = writeln!(o, "- {} = {}", v.name, field(val, VAR_MAX));
                    }
                    None => {
                        let _ = writeln!(o, "- {} = (sin valor inicial)", v.name);
                    }
                }
            }
            let _ = writeln!(o);
        }

        let _ = writeln!(o, "## Flujo");
        for n in &self.flow {
            let indent = "  ".repeat(n.depth);
            let mut linea = format!("{}{} {}", indent, n.line, n.cmd);
            if let Some(v) = &n.write_var {
                let _ = write!(linea, " → {v}");
            }
            if n.branch.is_else() {
                linea.push_str(" [else]");
            }
            if !n.params.is_empty() {
                let _ = write!(linea, "  {}", field(&n.params, PARAM_MAX));
            }
            let _ = writeln!(o, "{}", linea.trim_end());
        }

        o
    }

    /// Hallazgos del motor de reglas: pocos, ordenados y con el arreglo al lado.
    ///
    /// Se omite la seccion entera si no hay nada: un "## Hallazgos (0)" ensena al
    /// agente a saltarse la seccion, y el dia que tenga contenido la saltara igual.
    fn render_findings(&self, o: &mut String) {
        let findings = &self.analysis.findings;
        if findings.is_empty() {
            return;
        }
        let count = |severity: Severity| findings.iter().filter(|f| f.severity == severity).count();
        let _ = writeln!(
            o,
            "## Hallazgos ({}: {} alta / {} media / {} baja)",
            findings.len(),
            count(Severity::Alta),
            count(Severity::Media),
            count(Severity::Baja)
        );
        for f in findings {
            // Linea vacia = el hallazgo afecta al robot entero, no a un nodo.
            let line = if f.line.is_empty() {
                "L-".to_string()
            } else {
                format!("L{}", f.line)
            };
            let _ = writeln!(
                o,
                "- [{}] {} {} — {} → {}",
                f.severity.severity_label(),
                line,
                f.rule_id,
                field(&f.message, FINDING_MESSAGE_MAX),
                field(&f.fix, FINDING_FIX_MAX)
            );
        }
        let _ = writeln!(o);
    }

    /// Contrato implicito de las llamadas `execRocketBotDB`.
    ///
    /// El hijo corre dentro del interprete del padre y ve su panel de variables; eso
    /// no esta escrito en ningun sitio y es la causa de los `NameError` que el log
    /// solo muestra cuando ya fallo. Aqui se ve antes de ejecutar: que provee el
    /// padre (`contracts_out`) y que espera de quien lo llama (`contracts_in`).
    fn render_contracts(&self, o: &mut String) {
        let analysis = &self.analysis;
        if analysis.contracts_out.is_empty() && analysis.contracts_in.is_empty() {
            return;
        }
        let _ = writeln!(o, "## Contrato implicito (subrobots)");

        for contract in analysis.contracts_out.iter().take(CONTRACT_MAX) {
            let _ = writeln!(
                o,
                "- llama a {} en L{} → provee: {}",
                contract.child,
                contract.line,
                vars_preview(&contract.provided)
            );
            missing_line(o, contract);
        }
        for contract in analysis.contracts_in.iter().take(CONTRACT_MAX) {
            let _ = writeln!(
                o,
                "- lo llama {} en L{} → recibe: {}",
                contract.parent,
                contract.line,
                vars_preview(&contract.provided)
            );
            missing_line(o, contract);
        }

        let shown = analysis.contracts_out.len().min(CONTRACT_MAX)
            + analysis.contracts_in.len().min(CONTRACT_MAX);
        let omitted = analysis.contracts_out.len() + analysis.contracts_in.len() - shown;
        if omitted > 0 {
            let word = if omitted == 1 { "llamada" } else { "llamadas" };
            let _ = writeln!(
                o,
                "- ... y {omitted} {word} mas (el detalle, en el pack del subrobot)"
            );
        }
        let _ = writeln!(o);
    }

    /// Resumen de una linea para el listado de robots.
    pub fn summary_line(&self) -> String {
        format!(
            "{:<26} v{:<3} nodos {:>3}  vars {:>3}  prof {}",
            self.robot,
            self.snapshot_id,
            self.node_count(),
            self.vars.len(),
            self.depth
        )
    }
}

/// Lo que el hijo lee y nadie le provee: es el `NameError` de la proxima ejecucion.
fn missing_line(o: &mut String, contract: &Contract) {
    if contract.missing.is_empty() {
        return;
    }
    let _ = writeln!(
        o,
        "  !! el hijo espera y nadie provee: {}",
        vars_preview(&contract.missing)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimacion_de_tokens_es_monotona() {
        assert!(estimate_tokens("") <= 1);
        assert!(estimate_tokens(&"a".repeat(370)) >= 100);
        assert!(estimate_tokens(&"a".repeat(1000)) > estimate_tokens(&"a".repeat(100)));
    }

    #[test]
    fn una_linea_colapsa_saltos_y_espacios() {
        assert_eq!(one_line("a\n  b\r\nc"), "a ⏎ b ⏎ c");
    }

    #[test]
    fn clip_respeta_fronteras_de_caracter_unicode() {
        assert_eq!(clip("código", 3), "cód...");
    }

    #[test]
    fn la_vista_previa_de_variables_es_acotada_y_legible() {
        assert_eq!(vars_preview(&[]), "(nada)");

        let pocas = vec!["a".to_string(), "b".to_string()];
        assert_eq!(vars_preview(&pocas), "a, b");

        let muchas: Vec<String> = (0..CONTRACT_VARS_MAX + 3)
            .map(|i| format!("v{i}"))
            .collect();
        let preview = vars_preview(&muchas);
        assert!(
            preview.ends_with("(+3 mas)"),
            "el recorte se declara: {preview}"
        );
        assert!(
            !preview.contains("v9"),
            "no se listan todas las del orquestador: {preview}"
        );
    }
}
