use std::collections::BTreeMap;

use chrono::NaiveDateTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRef {
    pub line: String,
    pub params: String,
}

#[derive(Debug, Clone)]
pub struct SnapshotView {
    pub id: i64,
    pub created_at: NaiveDateTime,
    pub nodes: BTreeMap<String, NodeRef>,
}

#[derive(Debug, Clone)]
pub struct RawError {
    pub timestamp: NaiveDateTime,
    pub cmd_id: String,
    pub command: String,
    pub payload: String,
    pub message: String,
    pub occurrences: usize,
    pub signature: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Vivo,
    PayloadCambiado,
    Historico,
    Desconocido,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribution {
    pub verdict: Verdict,
    pub version: Option<i64>,
    pub en_version_actual: bool,
    pub line: Option<String>,
    pub signature: String,
}

pub fn attribute(errors: &[RawError], snapshots: &[SnapshotView]) -> Vec<Attribution> {
    let latest = latest_of(snapshots);
    errors
        .iter()
        .map(|error| attribute_one(error, snapshots, latest))
        .collect()
}

fn attribute_one(
    error: &RawError,
    snapshots: &[SnapshotView],
    latest: Option<&SnapshotView>,
) -> Attribution {
    let Some(viva) = live_at(snapshots, error.timestamp) else {
        return desconocido(error);
    };

    let Some(nodo) = viva.nodes.get(&error.cmd_id) else {
        return if existe_en_alguna(snapshots, &error.cmd_id) {
            Attribution {
                verdict: Verdict::Historico,
                version: Some(viva.id),
                en_version_actual: false,
                line: None,
                signature: error.signature.clone(),
            }
        } else {
            desconocido(error)
        };
    };

    let en_version_actual = latest.is_some_and(|l| l.id == viva.id);

    let verdict = if !en_version_actual {
        Verdict::Historico
    } else if mismos_params(&error.payload, &nodo.params) {
        Verdict::Vivo
    } else {
        Verdict::PayloadCambiado
    };

    Attribution {
        verdict,
        version: Some(viva.id),
        en_version_actual,
        line: Some(nodo.line.clone()),
        signature: error.signature.clone(),
    }
}

fn desconocido(error: &RawError) -> Attribution {
    Attribution {
        verdict: Verdict::Desconocido,
        version: None,
        en_version_actual: false,
        line: None,
        signature: error.signature.clone(),
    }
}

fn live_at(snapshots: &[SnapshotView], t: NaiveDateTime) -> Option<&SnapshotView> {
    snapshots
        .iter()
        .filter(|s| s.created_at <= t)
        .max_by_key(|s| (s.created_at, s.id))
}

fn latest_of(snapshots: &[SnapshotView]) -> Option<&SnapshotView> {
    snapshots.iter().max_by_key(|s| (s.created_at, s.id))
}

fn existe_en_alguna(snapshots: &[SnapshotView], cmd_id: &str) -> bool {
    snapshots.iter().any(|s| s.nodes.contains_key(cmd_id))
}

fn mismos_params(payload: &str, params: &str) -> bool {
    let payload = collapse_ws(payload);
    let params = collapse_ws(params);
    payload.is_empty() || params.is_empty() || payload == params
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::parse_date;

    const LINEA_CONTADOR: &str = "2.10";
    const PARAMS_CONTADOR: &str = "{contador} < 10";

    const FIRMA_CONTADOR: &str = "NameError:name:{var}:not_defined";

    fn snapshot(id: i64, fecha: &str, nodos: &[(&str, &str, &str)]) -> SnapshotView {
        SnapshotView {
            id,
            created_at: parse_date(fecha),
            nodes: nodos
                .iter()
                .map(|(cmd_id, line, params)| {
                    (
                        (*cmd_id).to_string(),
                        NodeRef {
                            line: (*line).to_string(),
                            params: (*params).to_string(),
                        },
                    )
                })
                .collect(),
        }
    }

    fn historial() -> Vec<SnapshotView> {
        let log = ("id-log", "2.1", "Paso Legacy...");
        let init = ("id-init", "2.6", "contador = 0");
        let guarda = ("id-guarda", "2.9", "guardar_filas()");
        let contador = ("id-contador", LINEA_CONTADOR, PARAMS_CONTADOR);
        let cierre = ("id-cierre", "2.11", "cerrar()");
        let reporte = ("id-reporte", "2.12", "reporte()");
        let finanzas = ("id-finanzas", "2.13", "{finanzas} < 1");
        vec![
            snapshot(25, "2026-11-17 12:47:10", &[log, init, contador]),
            snapshot(26, "2026-11-17 12:50:24", &[log, init, guarda, contador]),
            snapshot(
                27,
                "2026-11-17 12:52:29",
                &[log, init, guarda, contador, cierre],
            ),
            snapshot(
                28,
                "2026-11-17 12:55:20",
                &[log, init, guarda, contador, cierre, reporte],
            ),
            snapshot(
                29,
                "2026-11-17 12:59:37",
                &[log, init, guarda, contador, cierre, reporte, finanzas],
            ),
        ]
    }

    fn error_contador(fecha: &str) -> RawError {
        RawError {
            timestamp: parse_date(fecha),
            cmd_id: "id-contador".to_string(),
            command: "evaluateIf".to_string(),
            payload: PARAMS_CONTADOR.to_string(),
            message: "NameError: name 'contador' is not defined".to_string(),
            occurrences: 3,
            signature: FIRMA_CONTADOR.to_string(),
        }
    }

    #[test]
    fn contador_a_las_1249_es_historico_en_la_v25() {
        let out = attribute(&[error_contador("2026-11-17 12:49:22")], &historial());
        assert_eq!(out.len(), 1);
        let a = &out[0];
        assert_eq!(a.verdict, Verdict::Historico);
        assert_eq!(
            a.version,
            Some(25),
            "la version viva era v25, no la actual v29"
        );
        assert!(
            !a.en_version_actual,
            "v25 no es la ultima version del robot"
        );
        assert_eq!(a.line.as_deref(), Some(LINEA_CONTADOR));
        assert_eq!(a.signature, FIRMA_CONTADOR, "la firma se copia sin tocar");
    }

    #[test]
    fn el_mismo_error_a_las_1300_sigue_vivo_en_la_v29() {
        let out = attribute(&[error_contador("2026-11-17 13:00:00")], &historial());
        let a = &out[0];
        assert_eq!(a.verdict, Verdict::Vivo);
        assert_eq!(a.version, Some(29));
        assert!(a.en_version_actual);
        assert_eq!(a.line.as_deref(), Some(LINEA_CONTADOR));
    }

    #[test]
    fn parametros_distintos_en_la_version_actual_son_payload_cambiado() {
        let mut e = error_contador("2026-11-17 13:00:00");
        e.payload = "{contador} < 3".to_string();
        let out = attribute(&[e], &historial());
        assert_eq!(out[0].verdict, Verdict::PayloadCambiado);
        assert_eq!(out[0].version, Some(29));
        assert!(out[0].en_version_actual);
        assert_eq!(out[0].line.as_deref(), Some(LINEA_CONTADOR));
    }

    #[test]
    fn un_cmd_id_que_no_existe_en_ningun_snapshot_es_desconocido() {
        let mut e = error_contador("2026-11-17 13:00:00");
        e.cmd_id = "id-que-nunca-existio".to_string();
        let out = attribute(&[e], &historial());
        assert_eq!(out[0].verdict, Verdict::Desconocido);
        assert_eq!(
            out[0].version, None,
            "sin nodo no hay version a la que atribuirlo"
        );
        assert_eq!(out[0].line, None);
        assert!(!out[0].en_version_actual);
    }

    #[test]
    fn un_error_anterior_al_primer_snapshot_es_desconocido() {
        let out = attribute(&[error_contador("2026-11-17 12:40:00")], &historial());
        assert_eq!(out[0].verdict, Verdict::Desconocido);
        assert_eq!(
            out[0].version, None,
            "las 12:40:00 son antes del primer guardado (12:47:10)"
        );
        assert_eq!(out[0].line, None);
    }

    #[test]
    fn la_misma_firma_se_atribuye_por_separado() {
        let historico = error_contador("2026-11-17 12:49:22");
        let mut actual = error_contador("2026-11-17 13:00:00");
        actual.cmd_id = "id-finanzas".to_string();
        actual.payload = "{finanzas} < 1".to_string();
        actual.message = "NameError: name 'finanzas' is not defined".to_string();

        let out = attribute(&[historico, actual], &historial());
        assert_eq!(out.len(), 2, "una atribucion por error, no por firma");
        assert_eq!(out[0].signature, out[1].signature, "misma firma");
        assert_eq!(out[0].verdict, Verdict::Historico);
        assert_eq!(out[0].version, Some(25));
        assert_eq!(out[1].verdict, Verdict::Vivo);
        assert_eq!(out[1].version, Some(29));
        assert_eq!(out[1].line.as_deref(), Some("2.13"));
    }

    #[test]
    fn en_una_version_pasada_el_payload_no_cambia_el_veredicto() {
        let mut e = error_contador("2026-11-17 12:49:22");
        e.payload = "{contador} < 3".to_string();
        assert_eq!(attribute(&[e], &historial())[0].verdict, Verdict::Historico);
    }

    #[test]
    fn espacios_de_mas_no_cuentan_como_cambio() {
        let mut e = error_contador("2026-11-17 13:00:00");
        e.payload = "  {contador}     <\n10  ".to_string();
        assert_eq!(attribute(&[e], &historial())[0].verdict, Verdict::Vivo);
    }

    #[test]
    fn un_lado_vacio_no_acusa_cambio() {
        let mut sin_payload = error_contador("2026-11-17 13:00:00");
        sin_payload.payload = String::new();
        assert_eq!(
            attribute(&[sin_payload], &historial())[0].verdict,
            Verdict::Vivo,
            "sin payload no se puede afirmar que los parametros cambiaron"
        );

        let desnudo = snapshot(40, "2026-11-17 13:10:00", &[("id-sin-params", "1", "")]);
        let mut e = error_contador("2026-11-17 13:20:00");
        e.cmd_id = "id-sin-params".to_string();
        e.payload = "lo que el runner escriba".to_string();
        assert_eq!(attribute(&[e], &[desnudo])[0].verdict, Verdict::Vivo);
    }

    #[test]
    fn el_instante_del_guardado_ya_es_la_version_nueva() {
        let out = attribute(&[error_contador("2026-11-17 12:50:24")], &historial());
        assert_eq!(out[0].version, Some(26));
        assert_eq!(out[0].verdict, Verdict::Historico);
        assert_eq!(out[0].line.as_deref(), Some(LINEA_CONTADOR));
    }

    #[test]
    fn el_orden_de_los_snapshots_no_altera_la_atribucion() {
        let mut al_reves = historial();
        al_reves.reverse();
        let out = attribute(
            &[
                error_contador("2026-11-17 12:49:22"),
                error_contador("2026-11-17 13:00:00"),
            ],
            &al_reves,
        );
        assert_eq!(out[0].version, Some(25));
        assert_eq!(out[1].version, Some(29));
        assert_eq!(out[1].verdict, Verdict::Vivo, "v29 sigue siendo la ultima");
    }

    #[test]
    fn con_dos_guardados_en_el_mismo_segundo_gana_el_id_mayor() {
        let t = "2026-11-17 12:51:00";
        let viejo = snapshot(30, t, &[("id-otro", "1", "x")]);
        let nuevo = snapshot(31, t, &[("id-contador", LINEA_CONTADOR, PARAMS_CONTADOR)]);
        let e = error_contador(t);

        for out in [
            attribute(std::slice::from_ref(&e), &[viejo.clone(), nuevo.clone()]),
            attribute(std::slice::from_ref(&e), &[nuevo, viejo]),
        ] {
            assert_eq!(
                out[0].version,
                Some(31),
                "el id mayor es el guardado posterior"
            );
            assert_eq!(out[0].verdict, Verdict::Vivo);
        }
    }

    #[test]
    fn un_nodo_retirado_de_la_version_actual_es_historico() {
        let snaps = [
            snapshot(
                25,
                "2026-11-17 12:47:10",
                &[("id-retirado", "3.2", "borrar_todo()")],
            ),
            snapshot(
                29,
                "2026-11-17 12:59:37",
                &[("id-contador", LINEA_CONTADOR, PARAMS_CONTADOR)],
            ),
        ];
        let mut e = error_contador("2026-11-17 13:00:00");
        e.cmd_id = "id-retirado".to_string();
        let out = attribute(&[e], &snaps);
        assert_eq!(out[0].verdict, Verdict::Historico);
        assert_eq!(out[0].version, Some(29));
        assert_eq!(out[0].line, None, "el nodo no existe en v29");
        assert!(!out[0].en_version_actual);
    }

    #[test]
    fn sin_snapshots_no_hay_atribucion_posible() {
        let out = attribute(&[error_contador("2026-11-17 13:00:00")], &[]);
        assert_eq!(out[0].verdict, Verdict::Desconocido);
        assert_eq!(out[0].version, None);
        assert_eq!(out[0].line, None);
        assert!(attribute(&[], &[]).is_empty());
    }

    #[test]
    fn devuelve_una_atribucion_por_error_en_su_orden() {
        let errores = [
            error_contador("2026-11-17 13:00:00"),
            error_contador("2026-11-17 12:49:22"),
            error_contador("2026-11-17 12:00:00"),
        ];
        let out = attribute(&errores, &historial());
        assert_eq!(out.len(), errores.len());
        assert_eq!(out[0].verdict, Verdict::Vivo);
        assert_eq!(out[1].verdict, Verdict::Historico);
        assert_eq!(out[2].verdict, Verdict::Desconocido);
        assert!(out.iter().all(|a| a.signature == FIRMA_CONTADOR));
    }
}
