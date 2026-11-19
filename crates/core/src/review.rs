//! Agregacion de la vista de bandeja: una fila por robot con sus contadores.
//!
//! La vista es el pack contado, no un resumen paralelo: sale del mismo
//! `analysis::Workspace`. `vivos` cuenta firmas, no ocurrencias; los totales
//! suman todas las filas de la foto y un robot en dos entregas suma dos.
//! Un robot o una db ilegible se omite: una fila menos, nunca una fila a cero.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::Connection;

use crate::paths::NoPaths;
use crate::{analysis, db, Result};

/// Una fila del panorama: un robot de una db y sus contadores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRow {
    pub robot: String,
    /// Etiqueta relativa de la db; nunca una ruta absoluta.
    pub db: String,
    /// Nodos del ultimo snapshot, que es el que analiza el pack.
    pub nodes: usize,
    /// Hallazgo concreto sobre una linea.
    pub accionables: usize,
    /// Hallazgo que afecta al robot entero, no a una linea.
    pub higiene: usize,
    /// Firmas de error vivas en la version actual, con los parametros intactos.
    pub vivos: usize,
}

/// Foto completa de una db o de un workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    /// Etiqueta a mostrar, sin ruta absoluta.
    pub source: String,
    /// Dbs recorridas: solo las que se pudieron barrer.
    pub dbs: usize,
    pub rows: Vec<ReviewRow>,
}

impl Review {
    pub fn total_robots(&self) -> usize {
        self.rows.len()
    }

    pub fn total_accionables(&self) -> usize {
        self.rows.iter().map(|row| row.accionables).sum()
    }

    pub fn total_higiene(&self) -> usize {
        self.rows.iter().map(|row| row.higiene).sum()
    }

    pub fn total_vivos(&self) -> usize {
        self.rows.iter().map(|row| row.vivos).sum()
    }

    /// Orden de la bandeja: accionables desc, vivos desc y nombre. Vive aqui para
    /// que UI y CLI no ordenen distinto.
    pub fn sorted(&mut self) {
        self.rows.sort_by(|a, b| {
            b.accionables
                .cmp(&a.accionables)
                .then_with(|| b.vivos.cmp(&a.vivos))
                .then_with(|| a.robot.cmp(&b.robot))
        });
    }
}

/// Panorama de una sola db ya abierta. `label` es la etiqueta relativa que se
/// copia a `source` y a cada fila; `logs` es `(directorio, maximo)` o `None`.
pub fn review_conn(conn: &Connection, label: &str, logs: Option<(&Path, usize)>) -> Result<Review> {
    Ok(Review {
        source: label.to_string(),
        dbs: 1,
        rows: rows_of(conn, label, logs)?,
    })
}

/// Panorama de varias dbs ya abiertas y etiquetadas por quien llama; una que
/// falle al listarse se omite para no tumbar el resto.
pub fn review_workspace(
    conns: &[(&Connection, &str)],
    logs: Option<(&Path, usize)>,
) -> Result<Review> {
    let mut rows = Vec::new();
    let mut dbs = 0usize;
    for (conn, label) in conns {
        match rows_of(conn, label, logs) {
            Ok(db_rows) => {
                dbs += 1;
                rows.extend(db_rows);
            }
            Err(_) => continue,
        }
    }
    Ok(Review {
        source: format!("workspace ({})", dbs_label(dbs)),
        dbs,
        rows,
    })
}

fn rows_of(conn: &Connection, label: &str, logs: Option<(&Path, usize)>) -> Result<Vec<ReviewRow>> {
    let summaries = db::list_robots(conn)?;

    // Una sola carga de hechos para toda la db (issue #18).
    let workspace = analysis::Workspace::load(&[(conn, label)])?;
    let nodes: BTreeMap<String, usize> = workspace
        .facts
        .iter()
        .map(|fact| (fact.name.clone(), fact.nodes.len()))
        .collect();

    let mut rows = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let analysis = match workspace.analyse(conn, label, &summary.name, logs, &NoPaths) {
            Ok(analysis) => analysis,
            Err(_) => continue,
        };
        rows.push(ReviewRow {
            nodes: nodes.get(&summary.name).copied().unwrap_or_default(),
            accionables: analysis.findings.len(),
            higiene: analysis.higiene.len(),
            vivos: analysis.errors.live.len(),
            robot: summary.name,
            db: label.to_string(),
        });
    }
    Ok(rows)
}

fn dbs_label(dbs: usize) -> String {
    if dbs == 1 {
        "1 db".to_string()
    } else {
        format!("{dbs} dbs")
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use base64::Engine as _;
    use serde_json::{json, Value};

    use super::*;

    /// Robot minimo de un nodo; `command` decide si dispara una regla.
    fn robot(name: &str, command: &str) -> Value {
        json!({
            "project": {
                "commands": [{
                    "father": "log_message",
                    "line": "1",
                    "command": command,
                    "children": [],
                    "else": [],
                }],
                "vars": [],
                "profile": {"name": name},
            }
        })
    }

    /// Robot con una ruta absoluta en los parametros: dispara `RUTA_ABSOLUTA`, que
    /// es accionable. Un robot sin trycatch solo tiene higiene (`SIN_TRY`), asi que
    /// hace falta una regla de linea para tener un caso con hallazgos accionables.
    fn robot_con_ruta(name: &str) -> Value {
        robot(name, r"C:\temp\salida.txt")
    }

    fn make_db(dir: &Path, robots: &[(&str, Value)]) -> PathBuf {
        std::fs::create_dir_all(dir).expect("creo la carpeta de la db");
        let path = dir.join("robot.db");
        let conn = rusqlite::Connection::open(&path).expect("creo la db temporal");
        conn.execute_batch(
            "CREATE TABLE bots (
                id INTEGER PRIMARY KEY,
                name TEXT,
                created_at TEXT,
                data TEXT
            );",
        )
        .expect("creo la tabla bots");
        for (index, (name, project)) in robots.iter().enumerate() {
            let raw = project.to_string();
            let b64 = base64::engine::general_purpose::STANDARD.encode(raw.as_bytes());
            conn.execute(
                "INSERT INTO bots (id, name, created_at, data) \
                 VALUES (?1, ?2, '2026-11-17 10:00:00', ?3)",
                rusqlite::params![index as i64 + 1, name, b64],
            )
            .expect("inserto el snapshot");
        }
        path
    }

    /// Inserta una fila que no se puede decodificar: el robot existe para
    /// `list_robots` pero `analyse` falla con el.
    fn insertar_robot_roto(path: &Path, name: &str) {
        let conn = rusqlite::Connection::open(path).expect("abro la db temporal");
        conn.execute(
            "INSERT INTO bots (id, name, created_at, data) \
             VALUES (99, ?1, '2026-11-17 10:00:00', 'esto-no-es-base64')",
            rusqlite::params![name],
        )
        .expect("inserto la fila rota");
    }

    fn row(robot: &str, db: &str, accionables: usize, vivos: usize) -> ReviewRow {
        ReviewRow {
            robot: robot.to_string(),
            db: db.to_string(),
            nodes: 1,
            accionables,
            higiene: 0,
            vivos,
        }
    }

    #[test]
    fn los_totales_suman_todas_las_filas() {
        let review = Review {
            source: "workspace (2 dbs)".to_string(),
            dbs: 2,
            rows: vec![
                row("alfa", "a\\robot.db", 3, 1),
                row("beta", "b\\robot.db", 2, 4),
            ],
        };
        assert_eq!(review.total_robots(), 2);
        assert_eq!(review.total_accionables(), 5);
        assert_eq!(review.total_higiene(), 0);
        assert_eq!(review.total_vivos(), 5);
    }

    #[test]
    fn una_lista_vacia_no_inventa_totales() {
        let review = Review {
            source: "db: vacia.db".to_string(),
            dbs: 1,
            rows: Vec::new(),
        };
        assert_eq!(review.total_robots(), 0);
        assert_eq!(review.total_accionables(), 0);
        assert_eq!(review.total_higiene(), 0);
        assert_eq!(review.total_vivos(), 0);
    }

    #[test]
    fn sorted_ordena_por_accionables_despues_vivos_y_despues_nombre() {
        let mut review = Review {
            source: "db: x.db".to_string(),
            dbs: 1,
            rows: vec![
                row("gamma", "x.db", 0, 3),
                row("delta", "x.db", 2, 0),
                row("beta", "x.db", 2, 1),
                row("alfa", "x.db", 2, 0),
            ],
        };
        review.sorted();
        let nombres: Vec<&str> = review.rows.iter().map(|row| row.robot.as_str()).collect();
        assert_eq!(nombres, ["beta", "alfa", "delta", "gamma"]);
    }

    #[test]
    fn la_revision_de_una_db_copia_la_etiqueta_relativa_y_los_numeros_del_pack() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = make_db(
            tmp.path(),
            &[
                ("limpio", robot("limpio", "hola")),
                ("ruta", robot_con_ruta("ruta")),
            ],
        );
        let conn = db::open_readonly(&path).expect("abro la db");

        let label = r"entrega\robot.db";
        let review = review_conn(&conn, label, None).expect("review");

        assert_eq!(review.source, label);
        assert_eq!(review.dbs, 1);
        assert_eq!(review.total_robots(), 2);
        assert!(
            review.rows.iter().all(|row| row.db == label),
            "una fila con otra etiqueta: {:?}",
            review.rows
        );

        let con_ruta = review
            .rows
            .iter()
            .find(|row| row.robot == "ruta")
            .expect("fila del robot con ruta");
        assert_eq!(con_ruta.nodes, 1);
        assert!(
            con_ruta.accionables > 0,
            "la ruta absoluta no genero hallazgo"
        );
        assert!(
            review
                .rows
                .iter()
                .find(|row| row.robot == "limpio")
                .is_some_and(|row| row.accionables == 0),
            "el robot limpio no deberia tener accionables"
        );

        // La definicion no se reimplementa: es exactamente la del analisis.
        let analysis = analysis::analyse(&conn, label, "ruta").expect("analisis directo");
        assert_eq!(con_ruta.accionables, analysis.findings.len());
        assert_eq!(con_ruta.higiene, analysis.higiene.len());
        assert_eq!(con_ruta.vivos, analysis.errors.live.len());

        // Los totales de la vista son la suma de sus filas.
        assert_eq!(
            review.total_accionables(),
            review.rows.iter().map(|r| r.accionables).sum::<usize>()
        );
        assert_eq!(
            review.total_higiene(),
            review.rows.iter().map(|r| r.higiene).sum::<usize>()
        );
        assert_eq!(
            review.total_vivos(),
            review.rows.iter().map(|r| r.vivos).sum::<usize>()
        );
    }

    #[test]
    fn un_robot_ilegible_se_omite_sin_abortar_el_barrido() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = make_db(tmp.path(), &[("bueno", robot("bueno", "hola"))]);
        insertar_robot_roto(&path, "roto");
        let conn = db::open_readonly(&path).expect("abro la db");

        let review = review_conn(&conn, "x.db", None).expect("el barrido no aborta");
        assert_eq!(review.total_robots(), 1);
        assert_eq!(review.rows[0].robot, "bueno");
        assert!(
            review.rows.iter().all(|row| row.robot != "roto"),
            "el robot ilegible no puede pintarse como una fila a cero"
        );
        assert_eq!(review.dbs, 1);
    }

    #[test]
    fn el_workspace_suma_las_dbs_y_conserva_la_etiqueta_de_cada_una() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let a = make_db(&tmp.path().join("a"), &[("alpha", robot("alpha", "hola"))]);
        let b = make_db(&tmp.path().join("b"), &[("beta", robot_con_ruta("beta"))]);
        let conn_a = db::open_readonly(&a).expect("abro a");
        let conn_b = db::open_readonly(&b).expect("abro b");

        let review = review_workspace(&[(&conn_a, "a\\robot.db"), (&conn_b, "b\\robot.db")], None)
            .expect("review del workspace");

        assert_eq!(review.dbs, 2);
        assert_eq!(review.source, "workspace (2 dbs)");
        assert_eq!(review.total_robots(), 2);
        let alpha = review.rows.iter().find(|row| row.robot == "alpha").unwrap();
        let beta = review.rows.iter().find(|row| row.robot == "beta").unwrap();
        assert_eq!(alpha.db, "a\\robot.db");
        assert_eq!(beta.db, "b\\robot.db");
        assert_eq!(alpha.accionables, 0);
        assert!(beta.accionables > 0);
    }

    #[test]
    fn un_workspace_vacio_no_inventa_dbs() {
        let review = review_workspace(&[], None).expect("review vacio");
        assert_eq!(review.dbs, 0);
        assert_eq!(review.source, "workspace (0 dbs)");
        assert_eq!(review.total_robots(), 0);
    }
}
