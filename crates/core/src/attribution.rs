//! Atribucion temporal de cada error al snapshot de codigo que estaba vivo.
//!
//! Rocketbot no escribe en el log que version del robot se ejecuto. Pero `bots`
//! guarda una fila por cada guardado con su `created_at`, asi que el intervalo de
//! vida de cada version es reconstruible:
//!
//! ```text
//! snapshot_vivo(t) = argmax { created_at <= t }
//! ```
//!
//! # Por que la dimension temporal es obligatoria
//!
//! Caso real medido en `ML_06_Legacy`: `NameError: name 'contador' is not defined`
//! entre las 12:49:22 y las 12:50:28 del 2026-11-17. `contador` estaba
//! inicializado en las siete versiones guardadas y el nodo que reventaba
//! conservaba `id`, linea y parametros en todas: el fix consistio en anadir un
//! nodo **antes**, sin tocar el nodo que fallaba. Comparar el nodo del error con
//! el snapshot actual da "coincide" y concluye que el error sigue vivo, cuando en
//! esos minutos lo que corria era v25/v26 y el robot actual ya era v29. Solo la
//! atribucion temporal separa "el codigo de hoy tiene ese nodo" de "ese nodo se
//! ejecuto cuando fallo".
//!
//! # Veredictos
//!
//! | veredicto | significado |
//! |---|---|
//! | [`Verdict::Vivo`] | la version viva es la ultima y el nodo sigue con los mismos parametros |
//! | [`Verdict::PayloadCambiado`] | la version viva es la ultima, pero los parametros del nodo cambiaron |
//! | [`Verdict::Historico`] | ocurrio en una version que ya no es la actual, o en un nodo que se retiro |
//! | [`Verdict::Desconocido`] | no habia version viva (error anterior al primer guardado) o el nodo no existe en ningun snapshot |
//!
//! El motor no afirma causas (ver `docs/FINDINGS.md`): aqui solo se decide *a que
//! codigo* pertenece el error y si ese codigo sigue siendo el actual. El *porque*
//! lo cierra el agente leyendo el diff entre dos snapshots.
//!
//! # Modulo puro
//!
//! No toca disco ni `robot.db`: recibe los snapshots ya cargados y los errores ya
//! agregados, y devuelve una atribucion por error. Es lo que permite reproducir el
//! caso real en un test sin depender de `C:\Rocketbot`.

use std::collections::BTreeMap;

use chrono::NaiveDateTime;

/// Nodo del snapshot reducido a lo unico que hace falta para atribuir un error.
///
/// La clave del mapa que lo contiene es el `cmd_id`, no la linea: `line` **no es
/// unica** (Studio reutiliza la numeracion entre las ramas `children` y `else` de
/// un Try/Catch) y direccionar por ella acaba en el nodo equivocado.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRef {
    /// Etiqueta de linea de Studio en esa version (`2.10`).
    pub line: String,
    /// Parametros del comando como texto de una linea (`tree::params_text`).
    pub params: String,
}

/// Un guardado de `bots` visto como mapa `cmd_id -> nodo`.
#[derive(Debug, Clone)]
pub struct SnapshotView {
    /// `bots.id`: identifica el snapshot y rompe empates de fecha.
    pub id: i64,
    /// `bots.created_at`: instante desde el que esta version esta viva.
    pub created_at: NaiveDateTime,
    /// Nodos de esta version indexados por `cmd_id`.
    pub nodes: BTreeMap<String, NodeRef>,
}

/// Un error agregado del log, listo para atribuir.
#[derive(Debug, Clone)]
pub struct RawError {
    /// Instante del error. Es la unica entrada de la dimension temporal.
    pub timestamp: NaiveDateTime,
    /// `cmd_id` que el runner escribio en el bloque `request:` del log.
    pub cmd_id: String,
    /// Comando del nodo (`evaluateIf`, `savexlsx`...). No entra en el veredicto:
    /// viaja con el error para que el render no tenga que volver al log.
    pub command: String,
    /// Parametros que el runner registro al ejecutar el nodo. Se compara con
    /// [`NodeRef::params`] para decidir si el nodo cambio.
    pub payload: String,
    /// Texto del error. No entra en el veredicto: es la evidencia que se muestra.
    pub message: String,
    /// Veces que se repitio. No entra en el veredicto: un error repetido no es
    /// mas ni menos historico que uno que ocurrio una vez.
    pub occurrences: usize,
    /// Firma estable (`signature::normalize`), copiada tal cual a la atribucion.
    /// Agrupar por defecto es cosa de quien llama; atribuir es cosa de este modulo.
    pub signature: String,
}

/// Veredicto de la atribucion: que relacion tiene el error con el codigo de hoy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// La version viva es la ultima y el nodo conserva los mismos parametros.
    Vivo,
    /// La version viva es la ultima, pero los parametros del nodo cambiaron.
    PayloadCambiado,
    /// Ocurrio en una version que ya no es la actual (o en un nodo que se retiro).
    Historico,
    /// No se puede atribuir: no habia version viva o el nodo no existe en el
    /// historial.
    Desconocido,
}

/// Atribucion de un error del log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribution {
    pub verdict: Verdict,
    /// `id` del snapshot vivo en el instante del error. `None` cuando no hay
    /// version a la que atribuirlo (ver [`Verdict::Desconocido`]).
    pub version: Option<i64>,
    /// Si la version viva es el ultimo guardado.
    pub en_version_actual: bool,
    /// Linea del nodo **en su version**. `None` si el nodo no existe en esa
    /// version: no se inventa una linea que no se puede leer.
    pub line: Option<String>,
    /// Firma del error, copiada sin tocar.
    pub signature: String,
}

/// Atribuye cada error al snapshot vivo en su instante y decide si sigue vivo.
///
/// Reglas, en este orden:
///
/// 1. `latest` = snapshot con mayor `created_at` (empate: mayor `id`).
/// 2. Version viva = snapshot con mayor `created_at` menor o igual que el error.
///    Si no hay ninguna, el error es [`Verdict::Desconocido`] sin version.
/// 3. Si el `cmd_id` no esta en la version viva pero si en otra, el error es
///    [`Verdict::Historico`]; si no esta en ninguna, [`Verdict::Desconocido`].
/// 4. Si la version viva no es `latest`, el error es [`Verdict::Historico`].
/// 5. Si la version viva es `latest`, se compara el `payload` con los `params`
///    del nodo colapsando solo espacios: iguales (o alguno vacio) ->
///    [`Verdict::Vivo`]; si no -> [`Verdict::PayloadCambiado`].
/// 6. `line` es la del nodo en su version y `signature` se copia del error.
///
/// Siempre devuelve una atribucion por error, en el mismo orden de entrada.
///
/// ```
/// use std::collections::BTreeMap;
///
/// use rocketdebug_core::attribution::{attribute, NodeRef, RawError, SnapshotView, Verdict};
/// use rocketdebug_core::db::parse_date;
///
/// fn version(id: i64, created_at: &str) -> SnapshotView {
///     let mut nodes = BTreeMap::new();
///     nodes.insert(
///         "uuid-del-nodo".to_string(),
///         NodeRef {
///             line: "2.10".to_string(),
///             params: "{contador} < 10".to_string(),
///         },
///     );
///     SnapshotView {
///         id,
///         created_at: parse_date(created_at),
///         nodes,
///     }
/// }
///
/// let snapshots = [
///     version(25, "2026-11-17 12:47:10"),
///     version(29, "2026-11-17 12:59:37"),
/// ];
/// let error = RawError {
///     timestamp: parse_date("2026-11-17 12:49:22"),
///     cmd_id: "uuid-del-nodo".to_string(),
///     command: "evaluateIf".to_string(),
///     payload: "{contador} < 10".to_string(),
///     message: "NameError: name 'contador' is not defined".to_string(),
///     occurrences: 1,
///     signature: "NameError:name:{var}:not_defined".to_string(),
/// };
///
/// let atribuciones = attribute(&[error], &snapshots);
/// assert_eq!(atribuciones.len(), 1);
/// // A las 12:49:22 corria v25, no la version actual: el error es historico
/// // aunque el nodo siga existiendo con los mismos parametros.
/// assert_eq!(atribuciones[0].verdict, Verdict::Historico);
/// assert_eq!(atribuciones[0].version, Some(25));
/// assert_eq!(atribuciones[0].line.as_deref(), Some("2.10"));
/// assert!(!atribuciones[0].en_version_actual);
/// ```
pub fn attribute(errors: &[RawError], snapshots: &[SnapshotView]) -> Vec<Attribution> {
    // `latest` se calcula una vez, no una por error: el historial es el mismo
    // para toda la lista y recalcularlo invita a que dos errores discrepen.
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
        // Sin guardado anterior no hay codigo al que colgar el error. Decir "v29"
        // porque es la actual seria el falso positivo que este modulo evita.
        return desconocido(error);
    };

    let Some(nodo) = viva.nodes.get(&error.cmd_id) else {
        // El nodo no esta en la version viva. Si aparece en otra version, el error
        // pertenece a codigo que ya no corria en ese intervalo; si no aparece en
        // ninguna, el `cmd_id` del log no se corresponde con ningun nodo.
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

    // Los snapshots son filas de `bots`: `id` es la unica clave que dice "es el
    // mismo guardado". Comparar por fecha no valdria con dos guardados en el
    // mismo segundo.
    let en_version_actual = latest.is_some_and(|l| l.id == viva.id);

    let verdict = if !en_version_actual {
        // Vivo en su momento no es vivo hoy. La comparacion de parametros solo
        // tiene sentido contra la ultima version: en una anterior, que los
        // parametros coincidan no cambia el hecho de que ese codigo ya no corre.
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

/// Snapshot vivo en `t`: el ultimo guardado que ya existia.
///
/// La comparacion es `<=` y no `<`: un guardado a las 12:50:24 ya es el codigo
/// que corre en ese instante, y el error de las 12:50:28 pertenece a esa version
/// (caso real: v25 muere justo en v26).
fn live_at(snapshots: &[SnapshotView], t: NaiveDateTime) -> Option<&SnapshotView> {
    snapshots
        .iter()
        .filter(|s| s.created_at <= t)
        .max_by_key(|s| (s.created_at, s.id))
}

/// Ultimo guardado del historial.
///
/// El empate se rompe por `id` porque `created_at` solo tiene precision de
/// segundo: dos guardados seguidos pueden compartir fecha, y el posterior es el
/// de `id` mayor (la tabla es un historial `AUTOINCREMENT`).
fn latest_of(snapshots: &[SnapshotView]) -> Option<&SnapshotView> {
    snapshots.iter().max_by_key(|s| (s.created_at, s.id))
}

fn existe_en_alguna(snapshots: &[SnapshotView], cmd_id: &str) -> bool {
    snapshots.iter().any(|s| s.nodes.contains_key(cmd_id))
}

/// Compara los parametros que escribio el runner con los del snapshot.
///
/// Solo se colapsan los espacios: el `payload` es texto formateado por el runner
/// (con saltos de linea y separadores variables) y los `params` salen de
/// `tree::params_text`, que ya compacta el JSON. Normalizar mas -minusculas,
/// comillas, expresiones- esconderia justo el cambio que hay que detectar: un
/// `{contador} < 10` reescrito a `{contador} < 3` es el defecto, no ruido.
///
/// Con un lado vacio no hay nada que comparar. Ante la duda se elige `Vivo`:
/// afirmar un cambio sin evidencia es el tipo de falso positivo que hace que el
/// informe se deje de mirar.
fn mismos_params(payload: &str, params: &str) -> bool {
    let payload = collapse_ws(payload);
    let params = collapse_ws(params);
    payload.is_empty() || params.is_empty() || payload == params
}

/// Espacios multiples (incluidos saltos de linea y tabuladores) a uno solo, sin
/// extremos.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::parse_date;

    /// Linea y parametros del nodo que reventaba en `ML_06_Legacy`: el
    /// `evaluateIf` que consultaba `contador` antes de inicializarlo.
    const LINEA_CONTADOR: &str = "2.10";
    const PARAMS_CONTADOR: &str = "{contador} < 10";

    /// Firma del `NameError` del caso real, tal como la produce
    /// `signature::normalize`.
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

    /// Los cinco guardados medidos el 2026-11-17 en `ML_06_Legacy`.
    ///
    /// El nodo que fallaba conserva `id`, linea y parametros en las cinco
    /// versiones: el fix consistio en anadir un nodo antes (de ahi que crezca el
    /// numero de nodos, 31 -> 36 en las mediciones). Esa es exactamente la trampa
    /// que hace que comparar contra el snapshot actual no detecte nada.
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
        // La firma agrupa el defecto, no la version: dos errores con la misma
        // firma pueden estar en versiones distintas y hay que decidirlo uno a uno.
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
        // Regla 4 antes que regla 5: que los parametros difieran no revive un
        // error de una version que ya no corria.
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

        // Y al reves: un nodo sin parametros (`trycatch`, `logic`) tampoco cambia.
        let desnudo = snapshot(40, "2026-11-17 13:10:00", &[("id-sin-params", "1", "")]);
        let mut e = error_contador("2026-11-17 13:20:00");
        e.cmd_id = "id-sin-params".to_string();
        e.payload = "lo que el runner escriba".to_string();
        assert_eq!(attribute(&[e], &[desnudo])[0].verdict, Verdict::Vivo);
    }

    #[test]
    fn el_instante_del_guardado_ya_es_la_version_nueva() {
        // 12:50:24 es el `created_at` de v26: el intervalo de v25 ya cerro.
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
        // El error cae en la version actual, pero el nodo ya no esta en ella: se
        // conserva la version que corria y no se inventa una linea.
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
