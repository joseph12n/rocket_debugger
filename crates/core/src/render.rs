//! Render del context pack y contabilidad de tokens.
//!
//! El entregable para el agente es markdown, no JSON: es ~4x mas compacto que el
//! JSON equivalente (las claves se repiten una vez por nodo) y un modelo lo lee
//! igual de bien. Cada seccion del render es una card de la UI futura.

use std::fmt::Write as _;

use crate::pack::Pack;

/// Se cortan los parametros largos: un `execJs` de 2 KB aporta el inicio, no el final.
pub const PARAM_MAX: usize = 100;
/// Se cortan los valores de variables por la misma razon.
pub const VAR_MAX: usize = 60;

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
}
