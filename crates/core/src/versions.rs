//! Historial de snapshots y atribucion temporal.
//!
//! ```text
//! snapshot_vivo(t) = argmax { created_at <= t }
//! ```

use chrono::NaiveDateTime;

#[derive(Debug, Clone)]
pub struct VersionInfo {
    pub id: i64,
    pub created_at: NaiveDateTime,
    /// Tamano del base64 en la base de datos (no del JSON decodificado).
    pub bytes: i64,
    pub nodes: usize,
    /// Hash corto del contenido: mismo hash = mismo robot copiado en otra db.
    pub hash: String,
}

#[derive(Debug, Clone, Default)]
pub struct VersionIndex {
    pub versions: Vec<VersionInfo>,
}

impl VersionIndex {
    pub fn len(&self) -> usize {
        self.versions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.versions.is_empty()
    }

    /// Version que estaba viva en el instante `t`.
    pub fn live_at(&self, t: NaiveDateTime) -> Option<&VersionInfo> {
        self.versions
            .iter()
            .filter(|v| v.created_at <= t)
            .max_by_key(|v| v.created_at)
    }

    pub fn latest(&self) -> Option<&VersionInfo> {
        self.versions.last()
    }

    /// Version inmediatamente anterior a `id`, si existe.
    pub fn previous(&self, id: i64) -> Option<&VersionInfo> {
        let idx = self.versions.iter().position(|v| v.id == id)?;
        if idx == 0 {
            None
        } else {
            self.versions.get(idx - 1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(id: i64, fecha: &str, hash: &str) -> VersionInfo {
        VersionInfo {
            id,
            created_at: crate::db::parse_date(fecha),
            bytes: 100,
            nodes: 1,
            hash: hash.to_string(),
        }
    }

    fn indice() -> VersionIndex {
        VersionIndex {
            versions: vec![
                v(25, "2026-11-17 12:47:10", "aaa"),
                v(26, "2026-11-17 12:50:24", "bbb"),
                v(27, "2026-11-17 12:52:29", "ccc"),
                v(29, "2026-11-17 12:59:37", "ddd"),
            ],
        }
    }

    #[test]
    fn atribuye_al_snapshot_vivo_no_al_ultimo() {
        // Caso real: el error de `contador` ocurrio a las 12:49:22.
        let t = crate::db::parse_date("2026-11-17 12:49:22");
        let idx = indice();
        let vivo = idx.live_at(t).expect("hay version viva");
        assert_eq!(vivo.id, 25, "a las 12:49:22 estaba vivo v25, no v29");
    }

    #[test]
    fn justo_despues_de_un_guardado_gana_el_nuevo() {
        let t = crate::db::parse_date("2026-11-17 12:50:28");
        assert_eq!(indice().live_at(t).map(|v| v.id), Some(26));
    }

    #[test]
    fn antes_de_la_primera_version_no_hay_nada() {
        let t = crate::db::parse_date("2026-11-17 12:00:00");
        assert!(indice().live_at(t).is_none());
    }

    #[test]
    fn navega_al_snapshot_anterior() {
        let i = indice();
        assert_eq!(i.previous(27).map(|v| v.id), Some(26));
        assert_eq!(i.latest().map(|v| v.id), Some(29));
        assert!(i.previous(25).is_none());
    }
}
