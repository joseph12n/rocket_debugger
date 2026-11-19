use std::path::Path;
use std::time::Instant;

use anyhow::Context;
use rocketdebug_core::db;
use rocketdebug_core::review::{review_conn, review_workspace, Review, ReviewRow};

pub const DEFAULT_LIMIT: usize = 20;

const NAME_MIN_WIDTH: usize = 24;
const NODES_MIN_WIDTH: usize = 8;
const ACTIONABLE_MIN_WIDTH: usize = 5;
const HYGIENE_MIN_WIDTH: usize = 5;
const LIVE_MIN_WIDTH: usize = 7;

pub fn run(
    db_path: &Path,
    workspace: Option<&Path>,
    limit: usize,
    log_source: Option<(&Path, usize)>,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut review = collect(db_path, workspace, log_source)?;
    if workspace.is_some() {
        report_dbs(&review);
    }
    let source = display_source(&review, workspace.is_some());
    print!("{}", format_review(&source, &mut review, limit));
    eprintln!(
        "review: {} en {:.1} s",
        robots_label(review.total_robots()),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn collect(
    db_path: &Path,
    workspace: Option<&Path>,
    log_source: Option<(&Path, usize)>,
) -> anyhow::Result<Review> {
    match workspace {
        None => {
            let label = crate::db_label(db_path);
            let conn = db::open_readonly(db_path)
                .with_context(|| format!("abriendo {}", db_path.display()))?;
            Ok(review_conn(&conn, &label, log_source)?)
        }
        Some(dir) => {
            let paths = db::discover_databases(dir)
                .with_context(|| format!("descubriendo bases en {}", dir.display()))?;
            let labels = db::labels_for(&paths);
            let mut opened = Vec::with_capacity(paths.len());
            for (path, label) in paths.iter().zip(labels) {
                match db::open_readonly(path) {
                    Ok(conn) => opened.push((conn, label)),
                    Err(e) => eprintln!("aviso: {e}"),
                }
            }
            let refs: Vec<_> = opened
                .iter()
                .map(|(conn, label)| (conn, label.as_str()))
                .collect();
            Ok(review_workspace(&refs, log_source)?)
        }
    }
}

fn display_source(review: &Review, workspace: bool) -> String {
    if workspace {
        format!("workspace: {}", dbs_label(review.dbs))
    } else {
        format!("db: {}", review.source)
    }
}

fn report_dbs(review: &Review) {
    let mut actual: Option<&str> = None;
    let mut count = 0usize;
    for row in &review.rows {
        if actual != Some(row.db.as_str()) {
            if let Some(label) = actual {
                eprintln!("review: {label}: {}", robots_label(count));
            }
            actual = Some(row.db.as_str());
            count = 0;
        }
        count += 1;
    }
    if let Some(label) = actual {
        eprintln!("review: {label}: {}", robots_label(count));
    }
}

fn dbs_label(dbs: usize) -> String {
    if dbs == 1 {
        "1 db".to_string()
    } else {
        format!("{dbs} dbs")
    }
}

struct ColumnWidths {
    name: usize,
    nodes: usize,
    actionable: usize,
    hygiene: usize,
    live: usize,
}

impl ColumnWidths {
    fn for_rows(rows: &[ReviewRow]) -> Self {
        Self {
            name: rows
                .iter()
                .map(|row| row.robot.chars().count())
                .chain(std::iter::once("robot".len()))
                .max()
                .unwrap_or(0)
                .max(NAME_MIN_WIDTH),
            nodes: numeric_width(NODES_MIN_WIDTH, rows.iter().map(|row| row.nodes)),
            actionable: numeric_width(ACTIONABLE_MIN_WIDTH, rows.iter().map(|row| row.accionables)),
            hygiene: numeric_width(HYGIENE_MIN_WIDTH, rows.iter().map(|row| row.higiene)),
            live: numeric_width(LIVE_MIN_WIDTH, rows.iter().map(|row| row.vivos)),
        }
    }

    fn line(
        &self,
        robot: &str,
        nodes: &str,
        actionable: &str,
        hygiene: &str,
        live: &str,
    ) -> String {
        format!(
            "  {robot:<name$}{nodes:>nodes_w$}{actionable:>actionable_w$}\
             {hygiene:>hygiene_w$}{live:>live_w$}",
            name = self.name,
            nodes_w = self.nodes,
            actionable_w = self.actionable,
            hygiene_w = self.hygiene,
            live_w = self.live,
        )
    }
}

fn robots_label(count: usize) -> String {
    if count == 1 {
        "1 robot".to_string()
    } else {
        format!("{count} robots")
    }
}

fn numeric_width(minimum: usize, values: impl Iterator<Item = usize>) -> usize {
    match values.max() {
        Some(maximum) => {
            let digits = if maximum == 0 {
                1
            } else {
                maximum.ilog10() as usize + 1
            };
            digits.max(minimum)
        }
        None => minimum,
    }
}

pub fn format_review(source: &str, review: &mut Review, limit: usize) -> String {
    review.sorted();

    let actionable = review.total_accionables();
    let hygiene = review.total_higiene();
    let live = review.total_vivos();
    let total = review.total_robots();
    let with_findings = review.rows.iter().filter(|row| row.accionables > 0).count();

    let widths = ColumnWidths::for_rows(&review.rows);
    let mut lines = Vec::with_capacity(review.rows.len() + 5);
    lines.push(format!(
        "{source} | {} | {with_findings} con hallazgos accionables",
        robots_label(total)
    ));
    lines.push(String::new());
    lines.push(widths.line("robot", "nodos", "acc", "hig", "vivos"));
    for row in review.rows.iter().take(limit) {
        lines.push(widths.line(
            &row.robot,
            &row.nodes.to_string(),
            &row.accionables.to_string(),
            &row.higiene.to_string(),
            &row.vivos.to_string(),
        ));
    }
    if total > limit {
        lines.push(format!("  ... y {} mas", robots_label(total - limit)));
    }
    lines.push(String::new());
    lines.push(format!(
        "resultado: {actionable} accionables, {hygiene} higiene, {live} vivos en {}",
        robots_label(total)
    ));

    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        robot: &str,
        nodes: usize,
        accionables: usize,
        higiene: usize,
        vivos: usize,
    ) -> ReviewRow {
        ReviewRow {
            robot: robot.to_string(),
            db: "x.db".to_string(),
            nodes,
            accionables,
            higiene,
            vivos,
        }
    }

    fn review_of(rows: Vec<ReviewRow>) -> Review {
        Review {
            source: "x.db".to_string(),
            dbs: 1,
            rows,
        }
    }

    fn table_names(out: &str) -> Vec<&str> {
        out.lines()
            .skip(3)
            .take_while(|line| line.starts_with("  ") && !line.trim_start().starts_with("..."))
            .filter_map(|line| line.split_whitespace().next())
            .collect()
    }

    #[test]
    fn ordena_por_accionables_despues_vivos_y_despues_nombre() {
        let mut review = review_of(vec![
            row("gamma", 10, 0, 5, 3),
            row("delta", 10, 2, 1, 0),
            row("beta", 10, 2, 0, 1),
            row("alfa", 10, 2, 0, 0),
        ]);
        let out = format_review("db: x.db", &mut review, DEFAULT_LIMIT);
        assert_eq!(table_names(&out), ["beta", "alfa", "delta", "gamma"]);
    }

    #[test]
    fn la_tabla_sale_alineada_como_el_ejemplo_de_referencia() {
        let mut review = review_of(vec![
            row("reporteMercadolibre", 17, 3, 2, 0),
            row("ML_06_Legacy", 36, 5, 3, 0),
            row("ML_07_Gmail", 20, 0, 7, 0),
        ]);
        let out = format_review(r"db: BBRE2\robot_run.db", &mut review, DEFAULT_LIMIT);
        let expected = concat!(
            "db: BBRE2\\robot_run.db | 3 robots | 2 con hallazgos accionables\n",
            "\n",
            "  robot                      nodos  acc  hig  vivos\n",
            "  ML_06_Legacy                  36    5    3      0\n",
            "  reporteMercadolibre           17    3    2      0\n",
            "  ML_07_Gmail                   20    0    7      0\n",
            "\n",
            "resultado: 8 accionables, 12 higiene, 0 vivos en 3 robots\n",
        );
        assert_eq!(out, expected);
        let table: Vec<usize> = out
            .lines()
            .filter(|line| line.starts_with("  "))
            .map(|line| line.chars().count())
            .collect();
        assert!(
            table.iter().all(|len| *len == table[0]),
            "la tabla no queda alineada: {table:?}"
        );
    }

    #[test]
    fn el_limite_corta_y_avisa_de_lo_que_queda_fuera() {
        let mut review = review_of(vec![
            row("ML_06_Legacy", 36, 5, 3, 0),
            row("reporteMercadolibre", 17, 3, 2, 0),
            row("ML_07_Gmail", 20, 0, 7, 0),
        ]);
        let out = format_review(r"db: BBRE2\robot_run.db", &mut review, 2);
        assert!(out.contains("  ... y 1 robot mas\n"), "{out}");
        assert_eq!(table_names(&out), ["ML_06_Legacy", "reporteMercadolibre"]);
        assert!(
            out.ends_with("resultado: 8 accionables, 12 higiene, 0 vivos en 3 robots\n"),
            "{out}"
        );
        assert!(
            out.contains("| 3 robots | 2 con hallazgos accionables"),
            "{out}"
        );
    }

    #[test]
    fn un_nombre_largo_ensancha_la_columna_y_no_desalinea() {
        let mut review = review_of(vec![
            row(
                "robot_con_un_nombre_bastante_mas_largo_que_el_minimo",
                1,
                1,
                0,
                0,
            ),
            row("corto", 2, 0, 1, 0),
        ]);
        let out = format_review("db: x.db", &mut review, DEFAULT_LIMIT);
        let table: Vec<usize> = out
            .lines()
            .filter(|line| line.starts_with("  "))
            .map(|line| line.chars().count())
            .collect();
        assert!(
            table.iter().all(|len| *len == table[0]),
            "la tabla no queda alineada: {table:?}"
        );
        assert!(
            table[0] > 51,
            "la columna de nombre no se ensancho: {}",
            table[0]
        );
    }

    #[test]
    fn un_solo_robot_no_se_pluraliza() {
        let mut review = review_of(vec![row("solo", 3, 1, 0, 0)]);
        let out = format_review("db: una.db", &mut review, DEFAULT_LIMIT);
        assert!(out.starts_with("db: una.db | 1 robot | "), "{out}");
        assert!(
            out.ends_with("resultado: 1 accionables, 0 higiene, 0 vivos en 1 robot\n"),
            "{out}"
        );
    }

    #[test]
    fn una_db_sin_robots_no_inventa_filas() {
        let mut review = review_of(Vec::new());
        let out = format_review("db: vacia.db", &mut review, DEFAULT_LIMIT);
        let expected = concat!(
            "db: vacia.db | 0 robots | 0 con hallazgos accionables\n",
            "\n",
            "  robot                      nodos  acc  hig  vivos\n",
            "\n",
            "resultado: 0 accionables, 0 higiene, 0 vivos en 0 robots\n",
        );
        assert_eq!(out, expected);
    }
}
