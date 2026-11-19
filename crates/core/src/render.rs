use std::fmt::Write as _;

use crate::analysis::ErrorItem;
use crate::diff::{self, ChangeKind, NodeChange};
use crate::graph::Contract;
use crate::pack::Pack;
use crate::redact;
use crate::rules::{Finding, Severity};

pub const PARAM_MAX: usize = 100;
pub const VAR_MAX: usize = 60;

const DESCRIPTION_MAX: usize = 200;

pub const CONTRACT_MAX: usize = 8;

pub const CONTRACT_VARS_MAX: usize = 6;

pub const ERRORS_LIVE_MAX: usize = 6;
pub const ERRORS_PAYLOAD_MAX: usize = 3;
pub const ERRORS_GROUP_MAX: usize = 3;

const ERROR_MESSAGE_SHOW: usize = 120;

pub const MAX_PER_SECTION: usize = 5;

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

pub fn estimate_tokens(s: &str) -> usize {
    (s.chars().count() as f64 / 3.7).ceil() as usize
}

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
    let masked = redact::mask_user_paths(&one_line(s));
    clip(&redact::mask_emails(&masked), max)
}

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
            let _ = writeln!(o, "> {}", field(d, DESCRIPTION_MAX));
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

    fn render_findings(&self, o: &mut String) {
        render_bucket(o, "Hallazgos", &self.analysis.findings);
        render_bucket(o, "Higiene", &self.analysis.higiene);
    }

    fn render_contracts(&self, o: &mut String) {
        let analysis = &self.analysis;
        let sin_resolver =
            !analysis.ambiguous_calls.is_empty() || !analysis.missing_calls.is_empty();
        if analysis.contracts_out.is_empty() && analysis.contracts_in.is_empty() && !sin_resolver {
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

        for call in &analysis.missing_calls {
            let _ = writeln!(
                o,
                "- llamada sin destino: {call} — no existe en ninguna db del workspace"
            );
        }
        for call in &analysis.ambiguous_calls {
            let _ = writeln!(
                o,
                "- llamada ambigua: {call} — el hijo solo existe fuera de esta db"
            );
        }
        let _ = writeln!(o);
    }

    fn render_errors(&self, o: &mut String) {
        let errors = &self.analysis.errors;
        if errors.is_empty() {
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

    pub fn render_errors_section(&self) -> Option<String> {
        let mut out = String::new();
        self.render_errors(&mut out);
        let text = out.trim_end();
        (!text.is_empty()).then(|| text.to_string())
    }

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

pub fn render_changes(changes: &[NodeChange]) -> String {
    let mut o = String::with_capacity(1024);
    let _ = writeln!(o, "{}", diff::summarize(changes));
    if changes.is_empty() {
        return o;
    }
    let _ = writeln!(o);
    for c in changes {
        let signo = match c.kind {
            ChangeKind::Added => "+",
            ChangeKind::Removed => "-",
            ChangeKind::Changed => "~",
        };
        let mut linea = format!("{signo} L{} {}", c.line, c.command);
        match c.kind {
            ChangeKind::Added | ChangeKind::Removed => {
                let texto = c.after.as_deref().or(c.before.as_deref()).unwrap_or("");
                let _ = write!(linea, "  {}", field(texto, PARAM_MAX));
            }
            ChangeKind::Changed => {
                let antes = field(c.before.as_deref().unwrap_or(""), PARAM_MAX / 2);
                let despues = field(c.after.as_deref().unwrap_or(""), PARAM_MAX / 2);
                let _ = write!(linea, "  {antes} → {despues}");
            }
        }
        let _ = writeln!(o, "{}", linea.trim_end());
    }
    o
}

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

fn platform_source(item: &ErrorItem) -> &str {
    match module_of(&item.message) {
        Some(module) => module,
        None if item.command.is_empty() => "?",
        None => item.command.as_str(),
    }
}

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
    fn el_tope_avisa_de_los_hallazgos_que_quedan_fuera() {
        let hallazgo = |i: usize| Finding {
            rule_id: format!("REGLA_{i}"),
            severity: Severity::Alta,
            kind: crate::rules::Kind::Accionable,
            line: (i + 1).to_string(),
            message: "mensaje".to_string(),
            fix: "arreglo".to_string(),
        };

        let muchos: Vec<Finding> = (0..MAX_PER_SECTION + 2).map(hallazgo).collect();
        let mut o = String::new();
        render_bucket(&mut o, "Hallazgos", &muchos);
        assert!(
            o.contains(&format!(
                "## Hallazgos ({} de {}:",
                MAX_PER_SECTION,
                muchos.len()
            )),
            "la cabecera debe declarar el total real:\n{o}"
        );
        assert!(
            o.contains(&format!("… y {} mas", muchos.len() - MAX_PER_SECTION)),
            "falta la linea de cierre:\n{o}"
        );

        let pocos: Vec<Finding> = (0..2).map(hallazgo).collect();
        let mut o2 = String::new();
        render_bucket(&mut o2, "Hallazgos", &pocos);
        assert!(o2.contains("## Hallazgos (2:"), "{o2}");
        assert!(!o2.contains("mas"), "sin recorte no hay aviso:\n{o2}");

        let mut o3 = String::new();
        render_bucket(&mut o3, "Higiene", &[]);
        assert!(o3.is_empty(), "{o3}");
    }

    #[test]
    fn el_render_enmascara_un_correo() {
        assert_eq!(
            field("avisa a juan@ejemplo.com", 100),
            "avisa a j***@ejemplo.com"
        );
        assert_eq!(
            field("juan@ejemplo.com", 12),
            "j***@ejemplo...",
            "primero se enmascara y luego se recorta"
        );
    }

    #[test]
    fn el_diff_enmascara_la_ruta_y_resume() {
        let cambios = vec![
            crate::diff::NodeChange {
                kind: crate::diff::ChangeKind::Changed,
                id: "n1".to_string(),
                line: "2".to_string(),
                command: "setVar".to_string(),
                before: Some("C:/Users/josep/a.txt".to_string()),
                after: Some("C:/Users/josep/b.txt".to_string()),
            },
            crate::diff::NodeChange {
                kind: crate::diff::ChangeKind::Added,
                id: "n2".to_string(),
                line: "3".to_string(),
                command: "log_message".to_string(),
                before: None,
                after: Some("nuevo".to_string()),
            },
        ];
        let texto = render_changes(&cambios);
        assert!(texto.contains("1 anadido"), "resumen:\n{texto}");
        assert!(texto.contains("1 cambiado"), "resumen:\n{texto}");
        assert!(texto.contains("~ L2 setVar"), "{texto}");
        assert!(texto.contains("+ L3 log_message"), "{texto}");
        assert!(
            !texto.contains("josep"),
            "el diff no puede publicar el usuario:\n{texto}"
        );
        assert!(texto.contains("%USERPROFILE%"), "{texto}");

        assert!(render_changes(&[]).contains("sin cambios"));
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

    fn pack_con(
        flow: Vec<crate::tree::FlatNode>,
        findings: Vec<Finding>,
        errors: Vec<ErrorItem>,
    ) -> Pack {
        Pack {
            robot: "rutas".to_string(),
            db_label: "test\\robot.db".to_string(),
            snapshot_id: 1,
            snapshot_at: crate::db::parse_date("2026-11-17 10:00:00"),
            versions: crate::versions::VersionIndex::default(),
            description: None,
            depth: 0,
            vars: Vec::new(),
            collisions: std::collections::BTreeMap::new(),
            flow,
            analysis: crate::analysis::Analysis {
                findings,
                higiene: Vec::new(),
                contracts_out: Vec::new(),
                contracts_in: Vec::new(),
                unused_declared: Vec::new(),
                ambiguous_calls: Vec::new(),
                missing_calls: Vec::new(),
                errors: crate::analysis::ErrorReport {
                    live: errors,
                    ..Default::default()
                },
            },
            raw_pretty: String::new(),
        }
    }

    #[test]
    fn la_seccion_de_errores_se_renderiza_sola() {
        let error = ErrorItem {
            line: "1".to_string(),
            command: "log_message".to_string(),
            message: "NameError: name 'contador' is not defined".to_string(),
            occurrences: 2,
            signature: "NameError:name:{var}:not_defined".to_string(),
            version: Some(1),
        };
        let mut pack = pack_con(Vec::new(), Vec::new(), vec![error]);

        let section = pack
            .render_errors_section()
            .expect("con errores hay seccion");
        assert!(section.starts_with("## Errores"), "{section}");
        assert!(section.contains("NameError"), "{section}");
        assert!(
            !section.contains("## Variables") && !section.contains("## Flujo"),
            "la seccion no arrastra el resto del pack:\n{section}"
        );

        pack.analysis.errors.live.clear();
        assert!(
            pack.render_errors_section().is_none(),
            "sin errores no se inventa una seccion"
        );
    }

    #[test]
    fn las_llamadas_sin_resolver_no_se_callan() {
        let mut pack = pack_con(Vec::new(), Vec::new(), Vec::new());
        assert!(
            !pack.render_markdown().contains("## Contrato"),
            "sin contratos ni llamadas no hay seccion"
        );

        pack.analysis.missing_calls = vec!["orq L4 -> no_existe".to_string()];
        let md = pack.render_markdown();
        assert!(
            md.contains("## Contrato implicito"),
            "una llamada sin destino ya justifica la seccion:\n{md}"
        );
        assert!(
            md.contains(
                "- llamada sin destino: orq L4 -> no_existe — no existe en ninguna db del workspace"
            ),
            "falta la linea de llamada sin destino:\n{md}"
        );

        pack.analysis.missing_calls.clear();
        pack.analysis.ambiguous_calls = vec!["orq -> hijo_web".to_string()];
        let md = pack.render_markdown();
        assert!(
            md.contains(
                "- llamada ambigua: orq -> hijo_web — el hijo solo existe fuera de esta db"
            ),
            "falta la linea de llamada ambigua:\n{md}"
        );
    }

    #[test]
    fn el_render_enmascara_el_usuario_y_conserva_la_ruta_util() {
        let flow = vec![crate::tree::FlatNode {
            line: "1".to_string(),
            id: "id-1".to_string(),
            cmd: "setVar".to_string(),
            group: Some("system".to_string()),
            option: None,
            desc: None,
            params: r#""C:/Users/josep/OneDrive/Documentos/mercadolibre_" + datetime.now()"#
                .to_string(),
            write_var: Some("ruta_salida".to_string()),
            branch: crate::tree::Branch::Main,
            depth: 0,
        }];
        let findings = vec![Finding {
            rule_id: "RUTA_ABSOLUTA".to_string(),
            severity: Severity::Media,
            kind: crate::rules::Kind::Accionable,
            line: "1".to_string(),
            message: r"ruta absoluta en los parametros: C:\Users\josep\OneDrive\Documentos; no es portable".to_string(),
            fix: "usa una ruta relativa o %base_path%".to_string(),
        }];
        let errors = vec![ErrorItem {
            line: "1".to_string(),
            command: "setVar".to_string(),
            message: r"PermissionError: C:\Users\josep\OneDrive\Documentos\reporte.xlsx"
                .to_string(),
            occurrences: 2,
            signature: "permissionerror".to_string(),
            version: Some(1),
        }];

        let pack = pack_con(flow, findings, errors);
        let md = pack.render_markdown();

        assert!(!md.contains("josep"), "el usuario se escapo:\n{md}");
        assert!(
            !md.contains("C:/Users"),
            "ruta Windows sin enmascarar:\n{md}"
        );
        assert!(
            !md.contains(r"C:\Users"),
            "ruta Windows sin enmascarar:\n{md}"
        );

        assert!(
            md.contains("%USERPROFILE%/OneDrive/Documentos/mercadolibre_"),
            "se perdio la cola del parametro:\n{md}"
        );
        assert!(
            md.contains(r"%USERPROFILE%\OneDrive\Documentos;"),
            "se perdio la cola del hallazgo:\n{md}"
        );
        assert!(
            md.contains(r"%USERPROFILE%\OneDrive\Documentos\reporte.xlsx"),
            "se perdio la cola del error:\n{md}"
        );
    }

    #[test]
    fn el_recorte_no_deja_medio_nombre_de_usuario() {
        let recortado = field("C:/Users/josep/OneDrive/Documentos", 20);
        assert!(!recortado.contains("josep"), "{recortado}");
        assert!(recortado.starts_with("%USERPROFILE%"), "{recortado}");
    }
}
