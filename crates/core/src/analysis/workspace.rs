//! Carga de los hechos del workspace: que robots hay, con que contenido y donde.
//!
//! El grafo necesita los hechos de **todas** las dbs a la vez, y la misma entrega
//! esta copiada en varias. Aqui se leen las filas necesarias de cada robot y se
//! deduplica por contenido; el resto del analisis (`mod.rs`) trabaja sobre el
//! resultado ya limpio.

use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::db;
use crate::error::{Error, Result};
use crate::graph::RobotFacts;
use crate::tree;

use super::declared_of;

/// Carga los hechos del ultimo snapshot de cada robot de cada db, deduplicando
/// por contenido: la misma entrega esta copiada en varias dbs y, medido, 31 filas
/// son 20 robots distintos (sin dedupe, un workspace de 28 se cuenta como 50).
///
/// La clave es `(nombre, hash del raw_b64)`: mismo nombre y contenido es una
/// copia, se queda un hecho y las demas etiquetas van a [`RobotFacts::also_in`]
/// (una copia descartada no puede convertir una arista en ambigua). Contenido
/// distinto o nombre distinto son dos hechos: un renombrado sigue siendo dos
/// nombres en el grafo, y colapsarlo haria que sus llamadas parecieran
/// inexistentes.
///
/// El hash es el `blake3` del `raw_b64` de [`crate::versions::VersionInfo::hash`]:
/// aqui solo se compara, no se muestra.
pub fn load_workspace(dbs: &[(&Connection, &str)]) -> Result<Vec<RobotFacts>> {
    load_workspace_of(dbs, None)
}

/// [`load_workspace`] con una db y un robot que el consumidor **exige**.
///
/// `target` es el robot pedido y, opcionalmente, el snapshot del que deben salir
/// sus hechos. De esa db el robot no puede faltar ([`Error::RobotNotFound`]) ni
/// fallar al decodificar (el error se propaga): es la diferencia entre un pack
/// degradado y un pack que miente. En las demas dbs un robot ilegible solo se
/// omite, porque como mucho hara que falte su contrato.
pub(super) fn load_workspace_of(
    dbs: &[(&Connection, &str)],
    target: Option<Target>,
) -> Result<Vec<RobotFacts>> {
    let mut facts: Vec<RobotFacts> = Vec::new();
    let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();

    for (conn, label) in dbs {
        // El pin viaja solo a la db del robot pedido. En las demas se sigue
        // tomando el ultimo snapshot: es el que el runner resuelve hoy.
        let pinned = target.filter(|t| t.db_label == *label);
        for (fact, hash) in load_db_facts(conn, label, pinned)? {
            let key = (fact.name.clone(), hash);
            match seen.get(&key).copied() {
                Some(idx) => {
                    // Copia identica en otra db: el hecho ya existe y solo se
                    // anota donde mas vive.
                    let kept = &mut facts[idx];
                    if !kept.lives_in(label) {
                        kept.also_in.push((*label).to_string());
                    }
                }
                None => {
                    seen.insert(key, facts.len());
                    facts.push(fact);
                }
            }
        }
    }

    // Sin conexion con la etiqueta pedida no se puede haber encontrado el robot.
    if let Some(t) = target {
        if !facts
            .iter()
            .any(|fact| fact.name == t.robot && fact.lives_in(t.db_label))
        {
            return Err(Error::RobotNotFound(t.robot.to_string()));
        }
    }

    Ok(facts)
}

/// Robot del workspace cuyos hechos el consumidor **exige** y, si se indica, el
/// snapshot exacto del que deben salir.
///
/// Existe en vez de un `Option<(&str, &str, Option<i64>)>` para que el pin no se
/// pueda pasar sin nombrar al robot: el snapshot es una propiedad del robot
/// analizado, no de la db ni del workspace.
#[derive(Debug, Clone, Copy)]
pub(super) struct Target<'a> {
    pub(super) db_label: &'a str,
    pub(super) robot: &'a str,
    /// `Some(id)` fija el snapshot; `None` acepta el ultimo.
    pub(super) snapshot: Option<i64>,
}

/// Hechos de una db, con el hash del contenido que usa el dedupe.
///
/// El snapshot es el **ultimo** de cada robot, salvo para el robot exigido: si
/// [`Target::snapshot`] trae un id, sus hechos salen de esa fila. Se lee solo la
/// fila necesaria ([`db::load_latest_snapshot`] o [`db::load_snapshot`]), no el
/// historial entero: el analisis no necesita las versiones anteriores y una
/// version vieja corrupta no debe bloquear la ultima (issue #18).
fn load_db_facts(
    conn: &Connection,
    db_label: &str,
    target: Option<Target>,
) -> Result<Vec<(RobotFacts, String)>> {
    let mut out = Vec::new();

    for summary in db::list_robots(conn)? {
        // El robot exigido es el unico que puede venir fijado a un snapshot; en
        // los demas `pinned` queda en `None` y se toma el ultimo, como siempre.
        let required = target.is_some_and(|t| t.robot == summary.name.as_str());
        let pinned = target.filter(|_| required).and_then(|t| t.snapshot);
        // Solo se decodifica la version que el analisis necesita: la fijada o la
        // ultima. Cargar todo el historial por robot era O(versiones) por pasada
        // (issue #18) y ademas una version vieja corrupta bloqueaba la ultima.
        let snap = match pinned {
            Some(id) => match db::load_snapshot(conn, &summary.name, id) {
                Ok(snap) => snap,
                Err(_) if !required => continue,
                Err(error) => return Err(error),
            },
            None => match db::load_latest_snapshot(conn, &summary.name) {
                Ok(Some(snap)) => snap,
                Ok(None) => continue,
                // Un robot distinto e ilegible solo se omite: como mucho hara que
                // su contrato no aparezca.
                Err(_) if !required => continue,
                Err(error) => return Err(error),
            },
        };
        let root = db::project_root(&snap.project);
        out.push((
            RobotFacts {
                db: db_label.to_string(),
                also_in: Vec::new(),
                name: summary.name.clone(),
                nodes: tree::flatten(db::commands_of(root)),
                declared: declared_of(root),
                vars: crate::paths::vars_of(root),
            },
            content_hash(&snap.raw_b64),
        ));
    }

    if let Some(t) = target {
        if !out.iter().any(|(fact, _)| fact.name == t.robot) {
            return Err(Error::RobotNotFound(t.robot.to_string()));
        }
    }
    Ok(out)
}

/// Hash del contenido de un snapshot: `blake3` del base64 tal cual esta en `bots`.
///
/// Se usa el base64 y no el JSON decodificado por la misma razon que
/// [`crate::versions::VersionInfo::hash`]: es lo que se copia entre dbs de entrega
/// y no depende de como se re-serialice el JSON.
fn content_hash(raw_b64: &str) -> String {
    blake3::hash(raw_b64.as_bytes()).to_hex().to_string()
}
