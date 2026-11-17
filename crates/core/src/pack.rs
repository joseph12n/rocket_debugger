//! Ensamblado del context pack.
//!
//! El pack es la unica estructura que consume el resto del sistema: el render
//! markdown (para el agente), el JSON (para la UI) y las metricas (para los
//! tests de ratio) salen todos de aqui.

use std::collections::BTreeMap;

use chrono::NaiveDateTime;
use serde_json::Value;

use crate::analysis::{self, Analysis};
use crate::db::{self, Snapshot};
use crate::error::Result;
use crate::redact;
use crate::render::{estimate_tokens, Sizing};
use crate::tree::{self, FlatNode};
use crate::versions::{VersionIndex, VersionInfo};

#[derive(Debug, Clone)]
pub struct VarInfo {
    pub name: String,
    pub value: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Pack {
    pub robot: String,
    /// Etiqueta de la db sin ruta absoluta (`BBRE2\robot_run.db`), para no filtrar
    /// el arbol de directorios del usuario al compartir un pack.
    pub db_label: String,
    pub snapshot_id: i64,
    pub snapshot_at: NaiveDateTime,
    pub versions: VersionIndex,
    pub description: Option<String>,
    pub depth: usize,
    pub vars: Vec<VarInfo>,
    /// Lineas repetidas: si hay alguna, `line` no sirve para direccionar nodos.
    pub collisions: BTreeMap<String, usize>,
    pub flow: Vec<FlatNode>,
    /// Hallazgos, contratos implicitos y variables muertas del robot.
    ///
    /// Se calcula contra la db entera (grafo de llamadas) y describe el **ultimo**
    /// snapshot de cada robot, tambien cuando el pack se pide sobre uno anterior.
    pub analysis: Analysis,
    /// JSON completo del snapshot, para medir el ahorro real.
    pub raw_pretty: String,
}

impl Pack {
    pub fn node_count(&self) -> usize {
        self.flow.len()
    }

    /// Contabilidad del ahorro: JSON crudo vs render entregado.
    pub fn sizing(&self, rendered: &str) -> Sizing {
        let raw_chars = self.raw_pretty.chars().count();
        let render_chars = rendered.chars().count();
        Sizing {
            raw_chars,
            raw_tokens: estimate_tokens(&self.raw_pretty),
            render_chars,
            render_tokens: estimate_tokens(rendered),
        }
    }
}

/// Construye el pack del snapshot indicado (por defecto, el ultimo).
pub fn build_from_conn(
    conn: &rusqlite::Connection,
    robot: &str,
    snapshot: Option<i64>,
    db_label: &str,
) -> Result<Pack> {
    let snaps = db::load_snapshots(conn, robot)?;
    let target = match snapshot {
        Some(id) => snaps
            .iter()
            .find(|s| s.id == id)
            .ok_or(crate::error::Error::SnapshotNotFound(id))?,
        None => snaps.last().expect("load_snapshots nunca devuelve vacio"),
    };
    // El analisis cruzado necesita la db entera (grafo de llamadas), no solo este
    // robot; por eso se resuelve aqui y se pasa ya hecho a `build`, que trabaja
    // unicamente con el snapshot pedido y no vuelve a tocar la conexion.
    let analysis = analysis::analyse(conn, db_label, robot)?;
    Ok(build(robot, db_label, &snaps, target, analysis))
}

fn build(
    robot: &str,
    db_label: &str,
    snaps: &[Snapshot],
    target: &Snapshot,
    analysis: Analysis,
) -> Pack {
    // El indice recorre todas las versiones. Cuando llegue el daemon esto se
    // cachea por (db, robot, ultimo_id); hoy es barato y mantiene el codigo simple.
    let mut versions = VersionIndex::default();
    for s in snaps {
        let root = db::project_root(&s.project);
        versions.versions.push(VersionInfo {
            id: s.id,
            created_at: s.created_at,
            bytes: s.raw_b64.len() as i64,
            nodes: tree::flatten(commands_of(root)).len(),
            hash: hash_of(&s.raw_b64),
        });
    }

    let root = db::project_root(&target.project);
    let flow = tree::flatten(commands_of(root));
    let collisions = tree::collisions(&flow);
    let depth = tree::max_depth(&flow);
    let vars = vars_of(root);
    let description = description_of(root);
    let raw_pretty = serde_json::to_string_pretty(root).unwrap_or_default();

    Pack {
        robot: robot.to_string(),
        db_label: db_label.to_string(),
        snapshot_id: target.id,
        snapshot_at: target.created_at,
        versions,
        description,
        depth,
        vars,
        collisions,
        flow,
        analysis,
        raw_pretty,
    }
}

fn commands_of(root: &Value) -> &[Value] {
    match root.get("commands").and_then(Value::as_array) {
        Some(v) => v.as_slice(),
        None => &[],
    }
}

/// Las variables salen ya redactadas: el pack se comparte (agente, incidencia,
/// UI) y `project.vars` puede llevar credenciales cifradas por Rocketbot.
fn vars_of(root: &Value) -> Vec<VarInfo> {
    let Some(arr) = root.get("vars").and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| {
            let name = v.get("name")?.as_str()?.trim().to_string();
            if name.is_empty() {
                return None;
            }
            let value = match v.get("data") {
                Some(Value::Null) | None => None,
                Some(Value::String(s)) => redact::redacted_value(&name, s),
                Some(other) => redact::redacted_value(&name, &other.to_string()),
            };
            Some(VarInfo { name, value })
        })
        .collect()
}

fn description_of(root: &Value) -> Option<String> {
    let d = root.get("profile")?.get("description")?.as_str()?.trim();
    if d.is_empty() {
        None
    } else {
        Some(d.to_string())
    }
}

fn hash_of(s: &str) -> String {
    let h = blake3::hash(s.as_bytes());
    h.to_hex().chars().take(16).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extrae_variables_y_su_valor() {
        let root = serde_json::json!({
            "vars": [
                {"name": "url", "data": "https://x"},
                {"name": "vacia", "data": null},
                {"name": "  ", "data": "x"}
            ]
        });
        let vars = vars_of(&root);
        assert_eq!(vars.len(), 2);
        assert_eq!(vars[0].name, "url");
        assert_eq!(vars[0].value.as_deref(), Some("https://x"));
        assert!(vars[1].value.is_none());
    }

    #[test]
    fn redacta_las_variables_sensibles_del_robot() {
        let root = serde_json::json!({
            "vars": [
                {"name": "destinatario", "data": "gAAAAABq7LyRAghP1xNXRUIBkRfDi6ioMMqj_-oPnZ5xvkmSw_Jb1B41VjB_"},
                {"name": "db_password", "data": "hunter2"},
                {"name": "url", "data": "https://x"}
            ]
        });
        let vars = vars_of(&root);
        assert_eq!(vars[0].value.as_deref(), Some("(valor cifrado)"));
        assert_eq!(vars[1].value.as_deref(), Some("(oculto)"));
        assert_eq!(
            vars[2].value.as_deref(),
            Some("https://x"),
            "un valor normal no se toca"
        );
    }

    #[test]
    fn hash_es_estable_y_corto() {
        assert_eq!(hash_of("abc"), hash_of("abc"));
        assert_ne!(hash_of("abc"), hash_of("abd"));
        assert_eq!(hash_of("abc").len(), 16);
    }
}
