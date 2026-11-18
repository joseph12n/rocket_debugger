//! Vista de bandeja de entrada: que robots estan mal y cuanto, en una pantalla.
//!
//! El proyecto nace para responder esa pregunta, pero hasta ahora habia que pedir
//! un context pack por robot: para decidir cual atender primero habia que recorrer
//! la lista a mano. Este modulo hace el barrido y deja una fila por robot con los
//! contadores que resumiria su pack: nodos, hallazgos accionables, hallazgos de
//! higiene y errores vivos.
//!
//! # Los numeros son los del pack
//!
//! Cada fila sale del mismo [`analysis::analyse_with_logs`] que alimenta el pack,
//! sobre la misma db y con el mismo origen de logs: la vista no es un resumen
//! paralelo que pueda desincronizarse del pack, es el pack contado. Por eso no hay
//! un camino rapido con numeros propios.
//!
//! # Coste conocido (issue #18)
//!
//! `analyse_with_logs` carga los hechos de **todos** los robots de la db en cada
//! llamada, asi que recorrer N robots decodifica la misma db N veces: O(N^2).
//! Medido con el binario de depuracion sobre el workspace real: la db de BBRE2
//! (10 robots) tarda 0,3 s sin logs y 2,4 s con logs, las dos entregas de BBRE2
//! (20 instancias) 0,4 s y 4,6 s, y el workspace entero (4 dbs, 32 instancias) 1,1 s
//! y 7,6 s; con logs, casi todo el coste es volver a parsear los ~20 ficheros de
//! `C:\Rocketbot\logs` una vez por robot. El cacheo por (db, robot, ultimo_id) esta
//! en #18; reutilizar aqui los hechos ya cargados exigiria una firma nueva en
//! `crates/core`, que no es de este issue. Se acepta el coste porque la vista es una
//! foto de estado y no un bucle de vigilancia, y `--no-logs` deja el barrido en el
//! orden del segundo.
//!
//! # Una db o un workspace
//!
//! Sin `--workspace` se analiza una sola db y la cabecera lleva su etiqueta
//! relativa (`db: BBRE2\robot_run.db`), igual que el pack, para no filtrar el arbol
//! de directorios. Con `--workspace` se recorren todas las dbs descubiertas y cada
//! robot se analiza contra **su** db, como si se pidiera un pack por db; el mismo
//! robot copiado en dos entregas aparece dos veces porque son dos despliegues y el
//! total suma instancias, no nombres distintos (es la cifra que se viene midiendo
//! en los barridos del proyecto).
//!
//! # Hermeticidad
//!
//! El motor no decide leer disco: el CLI resuelve el origen de logs y lo pasa. Con
//! `--no-logs` llega `None` y el barrido solo abre las dbs indicadas, sin tocar
//! `C:\Rocketbot\logs`.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use anyhow::Context;
use rocketdebug_core::{analysis, db};

/// Filas que se listan sin `--limit`.
///
/// Veinte deja el panorama de una entrega entera en pantalla (la mayor db real
/// medida tiene 10 robots y el workspace 32 instancias) y, si se recorta, la ultima
/// fila de la tabla dice cuantas quedaron fuera.
pub const DEFAULT_LIMIT: usize = 20;

/// Anchos minimos de cada columna, tomados del ejemplo de referencia del issue.
///
/// Son minimos y no fijos: si un nombre o un contador no cabe, la columna se
/// ensancha para todos y la alineacion se mantiene. Un numero grande no puede
/// desalinear el resto de la tabla.
const NAME_MIN_WIDTH: usize = 24;
const NODES_MIN_WIDTH: usize = 8;
const ACTIONABLE_MIN_WIDTH: usize = 5;
const HYGIENE_MIN_WIDTH: usize = 5;
const LIVE_MIN_WIDTH: usize = 7;

/// Una fila del panorama: un robot de una db y sus tres contadores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRow {
    /// Nombre del robot tal como lo guarda Studio.
    pub robot: String,
    /// Nodos del **ultimo** snapshot, que es el que analiza [`analysis`].
    pub nodes: usize,
    /// Hallazgos accionables: defecto concreto sobre una linea.
    pub actionable: usize,
    /// Hallazgos de higiene: afectan al robot entero, no a una linea.
    pub hygiene: usize,
    /// Firmas de error vivas en la version actual y con los parametros intactos.
    pub live: usize,
}

/// Ejecuta `rocketdebug review` y escribe la vista en stdout.
///
/// `log_source` es `(directorio, maximo de ficheros)` ya resuelto por el CLI, o
/// `None` con `--no-logs`. El tiempo se informa por stderr: stdout es el contrato
/// de formato de la vista y la medida del coste no puede ensuciarlo.
pub fn run(
    db_path: &Path,
    workspace: Option<&Path>,
    limit: usize,
    log_source: Option<(&Path, usize)>,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let (source, rows) = collect(db_path, workspace, log_source)?;
    print!("{}", format_review(&source, &rows, limit));
    eprintln!(
        "review: {} en {:.1} s",
        robots_label(rows.len()),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Recorre la db pedida (o el workspace) y devuelve la cabecera y las filas.
fn collect(
    db_path: &Path,
    workspace: Option<&Path>,
    log_source: Option<(&Path, usize)>,
) -> anyhow::Result<(String, Vec<ReviewRow>)> {
    match workspace {
        None => {
            let label = crate::db_label(db_path);
            let rows = rows_of(db_path, &label, log_source)?;
            Ok((format!("db: {label}"), rows))
        }
        Some(dir) => {
            let paths = db::discover_databases(dir)
                .with_context(|| format!("descubriendo bases en {}", dir.display()))?;
            let mut rows = Vec::new();
            let mut dbs = 0usize;
            for path in paths {
                let label = crate::db_label(&path);
                match rows_of(&path, &label, log_source) {
                    Ok(db_rows) => {
                        dbs += 1;
                        eprintln!("review: {label}: {}", robots_label(db_rows.len()));
                        rows.extend(db_rows);
                    }
                    // Una db ilegible no puede tumbar el panorama de las demas: se
                    // avisa y se sigue. En el modo de una sola db el error si sube,
                    // porque ahi no hay nada mas que mirar.
                    Err(e) => eprintln!("aviso: {e}"),
                }
            }
            // Singular/plural: una salida que empieza por "1 dbs" se lee mal.
            let dbs_label = if dbs == 1 {
                "1 db".to_string()
            } else {
                format!("{dbs} dbs")
            };
            Ok((format!("workspace: {dbs_label}"), rows))
        }
    }
}

/// Filas de una db: un robot por nombre, con sus contadores.
///
/// El tipo de la conexion no se nombra a proposito: `rusqlite` no es dependencia
/// del CLI (la db la abre el motor) y este comando no debe saber de SQL.
fn rows_of(
    db_path: &Path,
    label: &str,
    log_source: Option<(&Path, usize)>,
) -> anyhow::Result<Vec<ReviewRow>> {
    let conn =
        db::open_readonly(db_path).with_context(|| format!("abriendo {}", db_path.display()))?;
    let summaries = db::list_robots(&conn)?;

    // El analisis no devuelve el numero de nodos, y contar por robot con
    // `load_snapshots` decodificaria **todas** sus versiones: `load_workspace` ya
    // decodifica una sola vez el ultimo snapshot de cada robot de la db, que es
    // justo el que analiza `analyse`. Es una carga mas sobre las N del barrido: el
    // coste sigue siendo O(N^2), no O(N^3).
    let nodes: BTreeMap<String, usize> = analysis::load_workspace(&[(&conn, label)])?
        .into_iter()
        .map(|facts| (facts.name, facts.nodes.len()))
        .collect();

    let mut rows = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let result = match log_source {
            Some(source) => analysis::analyse_with_logs(&conn, label, &summary.name, Some(source)),
            None => analysis::analyse(&conn, label, &summary.name),
        };
        let analysis = match result {
            Ok(analysis) => analysis,
            // Un robot ilegible no puede ocultar el panorama de los demas: se
            // avisa y se sigue. No se pinta una fila a cero porque "no se pudo
            // leer" no es "esta limpio", y la cabecera solo cuenta lo analizado.
            Err(e) => {
                eprintln!("aviso: {}: {e}", summary.name);
                continue;
            }
        };
        rows.push(ReviewRow {
            // Si el analisis del robot salio bien, su ultimo snapshot se decodifico
            // al cargar los hechos, asi que el contador esta siempre presente.
            nodes: nodes.get(&summary.name).copied().unwrap_or_default(),
            actionable: analysis.findings.len(),
            hygiene: analysis.higiene.len(),
            live: analysis.errors.live.len(),
            robot: summary.name,
        });
    }
    Ok(rows)
}

/// Anchos de las cinco columnas, calculados sobre las filas que se van a pintar.
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
            actionable: numeric_width(ACTIONABLE_MIN_WIDTH, rows.iter().map(|row| row.actionable)),
            hygiene: numeric_width(HYGIENE_MIN_WIDTH, rows.iter().map(|row| row.hygiene)),
            live: numeric_width(LIVE_MIN_WIDTH, rows.iter().map(|row| row.live)),
        }
    }

    /// Una linea de la tabla: nombre a la izquierda y contadores a la derecha.
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

/// `1 robot` / `N robots`: la salida la lee una persona, no un parser.
fn robots_label(count: usize) -> String {
    if count == 1 {
        "1 robot".to_string()
    } else {
        format!("{count} robots")
    }
}

/// Ancho de una columna numerica: el minimo del formato o mas si el valor no cabe.
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

/// Pinta el panorama: cabecera, tabla ordenada y totales.
///
/// El orden es el de la bandeja: primero lo que tiene mas defectos accionables,
/// despues lo que tiene mas errores vivos y, a igualdad de los dos, el nombre. Los
/// totales suman **todas** las filas, no solo las que entran por `limit`: cortar la
/// tabla no puede cambiar la foto.
pub fn format_review(source: &str, rows: &[ReviewRow], limit: usize) -> String {
    let mut ordered: Vec<&ReviewRow> = rows.iter().collect();
    ordered.sort_by(|a, b| {
        b.actionable
            .cmp(&a.actionable)
            .then_with(|| b.live.cmp(&a.live))
            .then_with(|| a.robot.cmp(&b.robot))
    });

    let actionable: usize = rows.iter().map(|row| row.actionable).sum();
    let hygiene: usize = rows.iter().map(|row| row.hygiene).sum();
    let live: usize = rows.iter().map(|row| row.live).sum();
    let with_findings = rows.iter().filter(|row| row.actionable > 0).count();

    let widths = ColumnWidths::for_rows(rows);
    let mut lines = Vec::with_capacity(rows.len() + 5);
    lines.push(format!(
        "{source} | {} | {with_findings} con hallazgos accionables",
        robots_label(rows.len())
    ));
    lines.push(String::new());
    lines.push(widths.line("robot", "nodos", "acc", "hig", "vivos"));
    for row in ordered.iter().take(limit) {
        lines.push(widths.line(
            &row.robot,
            &row.nodes.to_string(),
            &row.actionable.to_string(),
            &row.hygiene.to_string(),
            &row.live.to_string(),
        ));
    }
    // Nunca se oculta en silencio lo que el limite deja fuera.
    if rows.len() > limit {
        lines.push(format!("  ... y {} mas", robots_label(rows.len() - limit)));
    }
    lines.push(String::new());
    lines.push(format!(
        "resultado: {actionable} accionables, {hygiene} higiene, {live} vivos en {}",
        robots_label(rows.len())
    ));

    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(robot: &str, nodes: usize, actionable: usize, hygiene: usize, live: usize) -> ReviewRow {
        ReviewRow {
            robot: robot.to_string(),
            nodes,
            actionable,
            hygiene,
            live,
        }
    }

    /// Nombres de las filas de la tabla, en el orden en que salen.
    fn table_names(out: &str) -> Vec<&str> {
        out.lines()
            .skip(3)
            .take_while(|line| line.starts_with("  ") && !line.trim_start().starts_with("..."))
            .filter_map(|line| line.split_whitespace().next())
            .collect()
    }

    #[test]
    fn ordena_por_accionables_despues_vivos_y_despues_nombre() {
        let rows = [
            row("gamma", 10, 0, 5, 3),
            row("delta", 10, 2, 1, 0),
            row("beta", 10, 2, 0, 1),
            row("alfa", 10, 2, 0, 0),
        ];
        let out = format_review("db: x.db", &rows, DEFAULT_LIMIT);
        assert_eq!(table_names(&out), ["beta", "alfa", "delta", "gamma"]);
    }

    #[test]
    fn la_tabla_sale_alineada_como_el_ejemplo_de_referencia() {
        let rows = [
            row("reporteMercadolibre", 17, 3, 2, 0),
            row("ML_06_Legacy", 36, 5, 3, 0),
            row("ML_07_Gmail", 20, 0, 7, 0),
        ];
        let out = format_review(r"db: BBRE2\robot_run.db", &rows, DEFAULT_LIMIT);
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
        // Alineacion = todas las lineas de la tabla miden lo mismo.
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
        let rows = [
            row("ML_06_Legacy", 36, 5, 3, 0),
            row("reporteMercadolibre", 17, 3, 2, 0),
            row("ML_07_Gmail", 20, 0, 7, 0),
        ];
        let out = format_review(r"db: BBRE2\robot_run.db", &rows, 2);
        assert!(out.contains("  ... y 1 robot mas\n"), "{out}");
        assert_eq!(table_names(&out), ["ML_06_Legacy", "reporteMercadolibre"]);
        // Los totales cuentan las tres filas aunque solo se pinten dos.
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
        let rows = [
            row(
                "robot_con_un_nombre_bastante_mas_largo_que_el_minimo",
                1,
                1,
                0,
                0,
            ),
            row("corto", 2, 0, 1, 0),
        ];
        let out = format_review("db: x.db", &rows, DEFAULT_LIMIT);
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
        let rows = [row("solo", 3, 1, 0, 0)];
        let out = format_review("db: una.db", &rows, DEFAULT_LIMIT);
        assert!(out.starts_with("db: una.db | 1 robot | "), "{out}");
        assert!(
            out.ends_with("resultado: 1 accionables, 0 higiene, 0 vivos en 1 robot\n"),
            "{out}"
        );
    }

    #[test]
    fn una_db_sin_robots_no_inventa_filas() {
        let out = format_review("db: vacia.db", &[], DEFAULT_LIMIT);
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
