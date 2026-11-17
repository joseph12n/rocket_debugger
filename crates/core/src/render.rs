//! Render del context pack y contabilidad de tokens.
//!
//! El entregable para el agente es markdown, no JSON: es ~4x mas compacto que el
//! JSON equivalente (las claves se repiten una vez por nodo) y un modelo lo lee
//! igual de bien. Cada seccion del render es una card de la UI futura.

use std::fmt::Write as _;

use crate::analysis::ErrorItem;
use crate::graph::Contract;
use crate::pack::Pack;
use crate::rules::{Finding, Severity};

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

/// Errores vivos que se listan uno a uno.
///
/// Medido en la entrega BBRE2: `reporteMercadolibre` deja 49 firmas historicas y
/// `ML_06_Legacy` 5. Con seis filas el agente ve las que mas duelen; el resto es
/// cola larga y se cuenta, no se lista.
pub const ERRORS_LIVE_MAX: usize = 6;
/// Errores con parametros cambiados que se listan: son los candidatos a "el fix
/// estaba a medias" y con tres basta para decidir si hay que mirar el diff.
pub const ERRORS_PAYLOAD_MAX: usize = 3;
/// Nombres que se listan en las lineas agregadas (historicos y plataforma).
pub const ERRORS_GROUP_MAX: usize = 3;

/// Mensaje de error en el render.
///
/// El analisis ya lo recorta a 160; aqui se baja a 120 porque va multiplicado por
/// cada fila y la seccion entera tiene que caber en el presupuesto del pack.
const ERROR_MESSAGE_SHOW: usize = 120;

/// Hallazgos que se listan por seccion (accionables e higiene por separado).
///
/// El analisis ya no recorta: trae todos y aqui se decide cuanto se pinta. Cinco
/// por seccion es lo que se lee de verdad; el resto se cuenta en la cabecera y en
/// una linea final, para que el lector sepa que existe y no lo confunda con el total.
pub const MAX_PER_SECTION: usize = 5;

/// Un mensaje que cita una ruta larga no puede comerse el presupuesto de contexto.
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

/// Pinta un cubo de hallazgos con tope y aviso de lo omitido.
///
/// El recuento por severidad es de lo **mostrado**, y si se recorta se dice
/// explicitamente ("5 de 23") porque el silencio hacia creer que ese era el total.
fn render_bucket(o: &mut String, title: &str, findings: &[Finding]) {
    if findings.is_empty() {
        return;
    }
    let shown = findings.len().min(MAX_PER_SECTION);
    let hidden = findings.len() - shown;
    let count = |s: Severity| {
        findings
            .iter()
            .take(shown)
            .filter(|f| f.severity == s)
            .count()
    };
    let total = if hidden > 0 {
        format!("{shown} de {}", findings.len())
    } else {
        shown.to_string()
    };
    let _ = writeln!(
        o,
        "## {title} ({total}: {} alta / {} media / {} baja)",
        count(Severity::Alta),
        count(Severity::Media),
        count(Severity::Baja)
    );
    for f in findings.iter().take(shown) {
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
    if hidden > 0 {
        let _ = writeln!(o, "- … y {hidden} mas de la misma seccion");
    }
    let _ = writeln!(o);
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
        self.render_errors(&mut o);

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

    /// Hallazgos del motor de reglas, en dos secciones: accionables e higiene.
    ///
    /// Se separan porque compiten por el mismo cupo y no valen lo mismo: una
    /// colision de linea es un defecto concreto y ``SIN_TRY`` es una carencia
    /// general del robot. Medido sobre 3 dbs reales: las reglas genericas eran el
    /// 36% de lo mostrado y hundian la precision util al 64%.
    ///
    /// Cada seccion se omite si esta vacia: un "## Hallazgos (0)" ensena al agente
    /// a saltarsela, y el dia que tenga contenido la saltara igual.
    fn render_findings(&self, o: &mut String) {
        render_bucket(o, "Hallazgos", &self.analysis.findings);
        render_bucket(o, "Higiene", &self.analysis.higiene);
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

    /// Errores del log ya atribuidos al snapshot que estaba vivo.
    ///
    /// Va detras del contrato y delante de las variables: es la unica seccion que
    /// dice que se rompio **de verdad**, y el orden importa porque el pack se lee
    /// de arriba abajo y se deja de leer muy pronto.
    ///
    /// Se acota por cubos y por filas (6 vivos + 3 con payload cambiado + 3
    /// agregadas). El caso real de `reporteMercadolibre` son 51 firmas: listarlas
    /// seria la mitad del presupuesto de 2,5k tokens en cola larga, justo lo que
    /// hace inutil el panel de errores. Los historicos y el ruido se resumen en una
    /// linea: ni se arreglan hoy ni se arreglan desde el robot.
    fn render_errors(&self, o: &mut String) {
        let errors = &self.analysis.errors;
        if errors.is_empty() {
            // Un "## Errores (0)" ensena a saltarse la seccion: se omite entera,
            // igual que los hallazgos y el contrato.
            return;
        }
        let _ = writeln!(
            o,
            "## Errores (vivos {} · payload cambiado {} · historicos {} · modulo {} · ruido {})",
            errors.live.len(),
            errors.payload_changed.len(),
            errors.historical.len(),
            errors.platform.len(),
            errors.noise.len()
        );

        for item in errors.live.iter().take(ERRORS_LIVE_MAX) {
            let _ = writeln!(o, "{}", error_line(item));
        }
        for item in errors.payload_changed.iter().take(ERRORS_PAYLOAD_MAX) {
            let _ = writeln!(o, "{}", error_line(item));
        }

        let omitidos = errors.live.len().saturating_sub(ERRORS_LIVE_MAX)
            + errors
                .payload_changed
                .len()
                .saturating_sub(ERRORS_PAYLOAD_MAX);
        if omitidos > 0 {
            let _ = writeln!(o, "- ... y {omitidos} mas (los de menos ocurrencias)");
        }

        if !errors.historical.is_empty() {
            let _ = writeln!(
                o,
                "- historicos de versiones anteriores: {}",
                tally_preview(&errors.historical, |item| item.command.as_str())
            );
        }
        if !errors.platform.is_empty() {
            let _ = writeln!(
                o,
                "- defecto de modulo (no del robot): {}",
                tally_preview(&errors.platform, platform_source)
            );
        }
        if !errors.noise.is_empty() {
            let n = errors.noise.len();
            let palabra = if n == 1 { "firma" } else { "firmas" };
            let _ = writeln!(o, "- ruido del motor (ignorado): {n} {palabra}");
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

/// Una fila de error: `- x3 L2.10 evaluateIf: NameError: name 'contador'...`.
///
/// La linea vacia se rotula `L-`: es un error que no se pudo ubicar en el
/// snapshot, y dejarla en blanco se leeria como un fallo de formato.
fn error_line(item: &ErrorItem) -> String {
    let line = if item.line.is_empty() {
        "L-".to_string()
    } else {
        format!("L{}", item.line)
    };
    let command = if item.command.is_empty() {
        "?"
    } else {
        item.command.as_str()
    };
    format!(
        "- x{} {} {}: {}",
        item.occurrences,
        line,
        command,
        field(&item.message, ERROR_MESSAGE_SHOW)
    )
}

/// Modulo de plataforma tal como lo nombra el runner: `EXCEPTION IN (<string> X, ...)`.
///
/// Se toma del mensaje porque el cubo ya viene decidido por `signature::classify`;
/// aqui solo se necesita el nombre para mostrar y no merece otra clasificacion.
fn module_of(message: &str) -> Option<&str> {
    const MARK: &str = "<string>";
    let at = message.find(MARK)?;
    let rest = message.get(at + MARK.len()..)?.trim_start();
    let end = rest.find([',', ')']).unwrap_or(rest.len());
    let module = rest[..end].trim();
    if module.is_empty() {
        None
    } else {
        Some(module)
    }
}

/// Con que se nombra un error de plataforma: el modulo si el mensaje lo delata.
fn platform_source(item: &ErrorItem) -> &str {
    match module_of(&item.message) {
        Some(module) => module,
        // Sin marcador de modulo queda el comando del robot, que al menos dice
        // por donde pasaba el flujo cuando fallo la plataforma.
        None if item.command.is_empty() => "?",
        None => item.command.as_str(),
    }
}

/// `setVar(x3), raise_exception(x3), module(x3) …` con el recorte declarado.
///
/// Cuenta ocurrencias, no firmas: el agente necesita saber que lo que mas se
/// repitio fue `setVar`, no cuantas variantes de mensaje tuvo.
fn tally_preview(items: &[ErrorItem], clave: impl Fn(&ErrorItem) -> &str) -> String {
    let mut cuenta: Vec<(&str, usize)> = Vec::new();
    for item in items {
        let nombre = clave(item);
        match cuenta.iter_mut().find(|(n, _)| *n == nombre) {
            Some((_, n)) => *n += item.occurrences,
            None => cuenta.push((nombre, item.occurrences)),
        }
    }
    if cuenta.is_empty() {
        return "(nada)".to_string();
    }
    cuenta.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

    let mut out = cuenta
        .iter()
        .take(ERRORS_GROUP_MAX)
        .map(|(nombre, n)| format!("{nombre}(x{n})"))
        .collect::<Vec<_>>()
        .join(", ");
    if cuenta.len() > ERRORS_GROUP_MAX {
        // Un solo caracter: la linea ya esta en el limite del presupuesto.
        out.push_str(" …");
    }
    out
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
