//! Seccion `## Errores` del context pack.
//!
//! Los errores se prueban aqui contra `fixtures/logs/sample.log` y una db
//! sintetica: ningun test lee `C:\Rocketbot` ni depende de que el entorno tenga
//! logs. El caso reproducido es el medido en `ML_06_Legacy`: el `NameError` de
//! `contador` ocurrio cuando estaba viva una version anterior, asi que es
//! historico aunque el robot de hoy siga teniendo ese nodo con esos parametros.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use rocketdebug_core::analysis::{self, ErrorItem, ErrorReport};
use rocketdebug_core::build_from_conn;
use rocketdebug_core::db;
use rocketdebug_core::pack::Pack;
use serde_json::{json, Value};

const DB: &str = r"test\robot.db";

/// `cmd_id` del `evaluateIf` que consultaba `contador` (`sample.log`, L2.10).
const CMD_CONTADOR: &str = "3c4d5e6f-7a8b-4c9d-8e0f-1a2b3c4d5e6f";
/// `cmd_id` del `stop` que estaba vivo cuando fallo `similitud`.
const CMD_STOP: &str = "c08176a8-97c4-4ecb-8433-e52920439e0e";

fn logs_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/logs")
}

fn node(father: &str, line: &str, id: &str, command: &str) -> Value {
    json!({
        "father": father,
        "line": line,
        "id": id,
        "command": command,
        "children": [],
        "else": [],
    })
}

/// Crea un `robot.db` con los snapshots indicados `(id, created_at, comandos)`.
fn make_db(dir: &Path, robot: &str, rows: &[(i64, &str, Vec<Value>)]) -> PathBuf {
    std::fs::create_dir_all(dir).expect("creo el directorio de la db");
    let path = dir.join("robot.db");
    let conn = rusqlite::Connection::open(&path).expect("abro db temporal");
    conn.execute_batch(
        "CREATE TABLE bots (
            id INTEGER PRIMARY KEY,
            name TEXT,
            created_at TEXT,
            data TEXT
        );",
    )
    .expect("creo tabla");
    for (id, created, commands) in rows {
        let project = json!({
            "project": {
                "commands": commands,
                "vars": [],
                "profile": {"name": robot},
            }
        });
        let b64 = base64::engine::general_purpose::STANDARD.encode(project.to_string().as_bytes());
        conn.execute(
            "INSERT INTO bots (id, name, created_at, data) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, robot, created, b64],
        )
        .expect("inserto snapshot");
    }
    path
}

/// El robot de `sample.log` en dos guardados: el de las 12:47:10, cuando corria el
/// nodo del `contador`, y el de las 12:50:24, que ya es la version actual.
fn db_ml_legacy(dir: &Path) -> PathBuf {
    make_db(
        dir,
        "ML_06_Legacy",
        &[
            (
                25,
                "2026-11-17 12:47:10",
                vec![node(
                    "evaluateIf",
                    "2.10",
                    CMD_CONTADOR,
                    "int({contador}) < 10",
                )],
            ),
            (
                26,
                "2026-11-17 12:50:24",
                vec![node("stop", "4", CMD_STOP, "")],
            ),
        ],
    )
}

fn item<'a>(items: &'a [ErrorItem], needle: &str) -> &'a ErrorItem {
    items
        .iter()
        .find(|i| i.message.contains(needle))
        .unwrap_or_else(|| panic!("no hay ningun error con {needle:?} en {items:#?}"))
}

/// Pack de un robot minimo en un temporal, con el informe de errores sustituido.
///
/// Se construye con `build_from_conn` en vez de armar el `Pack` a mano para que el
/// render se pruebe sobre la estructura real y no sobre una copia que se
/// desincronice cuando el pack gane secciones.
fn pack_de_prueba(dir: &Path, errors: ErrorReport) -> Pack {
    let path = make_db(
        dir,
        "demo",
        &[(
            1,
            "2026-11-17 12:00:00",
            vec![node("stop", "1", "id-x", "")],
        )],
    );
    let conn = db::open_readonly(&path).expect("abro la db");
    let mut pack = build_from_conn(&conn, "demo", None, DB).expect("pack");
    pack.analysis.errors = errors;
    pack
}

fn errors_de_sample(dir: &Path) -> ErrorReport {
    let path = db_ml_legacy(dir);
    let conn = db::open_readonly(&path).expect("abro la db");
    analysis::errors_for(&conn, "ML_06_Legacy", &logs_fixture(), 10).expect("informe de errores")
}

#[test]
fn el_name_error_de_contador_es_historico_y_no_vivo() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let report = errors_de_sample(&tmp.path().join("db"));

    // El log lo escribio el runner a las 12:49:22, cuando vivia v25; el robot es
    // v26 desde las 12:50:24. El nodo sigue existiendo con los mismos parametros,
    // asi que compararlo con la version actual daria "vivo" y seria una mentira.
    let contador = item(&report.historical, "contador");
    assert_eq!(contador.version, Some(25), "{report:#?}");
    assert_eq!(contador.line, "2.10");
    assert_eq!(contador.command, "evaluateIf");
    assert_eq!(contador.occurrences, 1, "el error se repitio una sola vez");
    assert!(
        !report.live.iter().any(|i| i.message.contains("contador")),
        "un error de una version anterior no puede contarse como vivo: {:#?}",
        report.live
    );

    // El otro NameError del fixture ocurrio a las 12:50:29, con v26 viva y siendo
    // v26 la ultima: misma firma, veredicto distinto. Es justo la distincion que
    // no se puede hacer mirando el log.
    let vivo = item(&report.live, "similitud");
    assert_eq!(vivo.version, Some(26));
    assert_eq!(vivo.line, "4");
    assert_eq!(vivo.command, "stop");
    assert_eq!(
        contador.signature, vivo.signature,
        "la firma agrupa el defecto, no la version"
    );
}

#[test]
fn el_ruido_del_motor_y_los_defectos_de_modulo_no_van_al_robot() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let report = errors_de_sample(&tmp.path().join("db"));

    // zdev (base64) y getbot (subrobot inexistente) son fallos de Rocketbot: el
    // robot no los puede arreglar y contarlos como suyos deja el panel en rojo.
    let zdev = report
        .noise
        .iter()
        .filter(|i| i.message.contains("zdev"))
        .count();
    assert_eq!(zdev, 3, "las tres variantes de zdev: {:#?}", report.noise);
    assert!(
        report
            .noise
            .iter()
            .any(|i| i.message.contains("getbot") && i.message.contains("IndexError")),
        "getbot tambien es ruido de motor: {:#?}",
        report.noise
    );
    assert!(
        !report.live.iter().any(|i| i.message.contains("zdev")),
        "el ruido no puede aparecer como error vivo"
    );

    // datatransform es defecto de la plataforma: se muestra aparte, con su fuente.
    assert_eq!(report.platform.len(), 1, "{:#?}", report.platform);
    assert!(report.platform[0].message.contains("datatransform"));
    assert!(
        !report
            .live
            .iter()
            .any(|i| i.message.contains("datatransform")),
        "un defecto de modulo no es un error del robot"
    );
}

#[test]
fn el_render_lista_los_vivos_y_resume_lo_demas() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let pack = pack_de_prueba(
        &tmp.path().join("pack"),
        errors_de_sample(&tmp.path().join("db")),
    );
    let md = pack.render_markdown();

    assert!(
        md.contains(
            "## Errores (vivos 1 · payload cambiado 0 · historicos 3 · modulo 1 · ruido 4)"
        ),
        "cabecera con el recuento por cubo:\n{md}"
    );
    assert!(
        md.contains("- x1 L4 stop: "),
        "el error vivo va con linea y comando:\n{md}"
    );
    assert!(
        md.contains("- historicos de versiones anteriores: "),
        "los historicos van agregados en una linea, no listados:\n{md}"
    );
    assert!(
        md.contains("- defecto de modulo (no del robot): datatransform(x1)"),
        "el defecto de plataforma cita su modulo:\n{md}"
    );
    assert!(
        md.contains("- ruido del motor (ignorado): 4 firmas"),
        "el ruido se cuenta, no se lista:\n{md}"
    );

    // La seccion va detras del contrato y delante del flujo: es lo primero
    // accionable del pack.
    let pos = |needle: &str| md.find(needle).unwrap_or_else(|| panic!("falta {needle}"));
    assert!(pos("## Errores") < pos("## Flujo"));
}

#[test]
fn la_seccion_se_omite_si_no_hay_errores() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let md = pack_de_prueba(&tmp.path().join("pack"), ErrorReport::default()).render_markdown();
    assert!(
        !md.contains("## Errores"),
        "sin logs no se pinta una seccion vacia:\n{md}"
    );
    assert!(ErrorReport::default().is_empty());
    assert_eq!(ErrorReport::default().total(), 0);
}

#[test]
fn sin_logs_ni_directorio_el_informe_sale_vacio() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = db_ml_legacy(&tmp.path().join("db"));
    let conn = db::open_readonly(&path).expect("abro la db");

    // Sin logs pedidos, el analisis del codigo funciona igual y no hay errores.
    let analysis =
        analysis::analyse_with_logs(&conn, DB, "ML_06_Legacy", None).expect("analisis del codigo");
    assert!(analysis.errors.is_empty());
    assert_eq!(analysis.errors.total(), 0);

    // Un directorio que no existe no es un error: el pack degrada, no miente.
    let vacio = analysis::errors_for(&conn, "ML_06_Legacy", &tmp.path().join("no_existe"), 10)
        .expect("informe");
    assert!(vacio.is_empty());

    // Y un directorio de logs sin ficheros tampoco.
    let sin_ficheros = tmp.path().join("logs_vacios");
    std::fs::create_dir_all(&sin_ficheros).expect("creo el directorio");
    let vacio = analysis::errors_for(&conn, "ML_06_Legacy", &sin_ficheros, 10).expect("informe");
    assert!(vacio.is_empty());
}

#[test]
fn las_repeticiones_se_agrupan_por_firma_y_el_payload_cambiado_se_separa() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Un unico guardado (el actual) con el nodo que consulta `contador`.
    let path = make_db(
        &tmp.path().join("db"),
        "demo",
        &[(
            7,
            "2026-11-17 12:00:00",
            vec![node(
                "evaluateIf",
                "2.10",
                "id-contador",
                "int({contador}) < 10",
            )],
        )],
    );
    let logs_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&logs_dir).expect("creo logs");
    // Dos errores con los parametros de hoy y uno con los parametros cambiados: la
    // misma firma, dos veredictos (es el caso "el fix estaba a medias").
    std::fs::write(
        logs_dir.join("logfile.log"),
        "2026-11-17 12:10:00.000000 - rocketbot - INFO - request: demo : line 2.10 - rpalogic : evaluateIf  id: id-contador -  - int({contador}) < 10\n\
         2026-11-17 12:10:01.000000 - Rocketbot - ERROR - EXCEPTION IN (<string> Rocketbot, L_91 \"\"): NameError: name 'uno' is not defined\n\
         2026-11-17 12:10:02.000000 - Rocketbot - ERROR - EXCEPTION IN (<string> Rocketbot, L_92 \"\"): NameError: name 'dos' is not defined\n\
         2026-11-17 12:10:03.000000 - rocketbot - INFO - request: demo : line 2.10 - rpalogic : evaluateIf  id: id-contador -  - int({contador}) < 3\n\
         2026-11-17 12:10:04.000000 - Rocketbot - ERROR - EXCEPTION IN (<string> Rocketbot, L_93 \"\"): NameError: name 'tres' is not defined\n",
    )
    .expect("escribo el log");

    let conn = db::open_readonly(&path).expect("abro la db");
    let report = analysis::errors_for(&conn, "demo", &logs_dir, 10).expect("informe");

    assert_eq!(
        report.live.len(),
        1,
        "dos repeticiones son una firma: {report:#?}"
    );
    let vivo = &report.live[0];
    assert_eq!(vivo.occurrences, 2, "las repeticiones se suman");
    assert_eq!(vivo.line, "2.10");
    assert_eq!(vivo.command, "evaluateIf");
    assert_eq!(vivo.version, Some(7));
    assert!(
        vivo.message.contains("uno"),
        "mensaje de la primera ocurrencia"
    );

    assert_eq!(report.payload_changed.len(), 1, "{report:#?}");
    let cambiado = &report.payload_changed[0];
    assert_eq!(cambiado.occurrences, 1);
    assert!(cambiado.message.contains("tres"));
    assert_eq!(
        vivo.signature, cambiado.signature,
        "es el mismo defecto con los parametros cambiados"
    );

    let pack = pack_de_prueba(&tmp.path().join("pack"), report);
    let md = pack.render_markdown();
    assert!(
        md.contains(
            "## Errores (vivos 1 · payload cambiado 1 · historicos 0 · modulo 0 · ruido 0)"
        ),
        "{md}"
    );
    assert!(md.contains("- x2 L2.10 evaluateIf: "), "{md}");
    assert!(md.contains("- x1 L2.10 evaluateIf: "), "{md}");
}
