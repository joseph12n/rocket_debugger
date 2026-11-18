//! Tests del broker de ejecucion con fixtures sinteticos.
//!
//! Ningun test lanza `rocketbot.exe`: los runners son dobles que escriben logs
//! en directorios temporales. La ejecucion real contra BBRE2 vive en
//! `crates/mcp/tests/exec.rs`, marcada `#[ignore]`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use chrono::NaiveDateTime;
use rocketdebug_core::attribution::Verdict;
use rocketdebug_core::db;
use rocketdebug_core::exec::{
    Broker, BrokerStatus, RunFailure, RunOutcome, RunReport, RunRequest, Runner,
};
use serde_json::json;

const ROBOT_ID: &str = "22222222-2222-2222-2222-222222222222";

fn snapshot_at() -> NaiveDateTime {
    db::parse_date("2026-11-17 10:00:00")
}

/// Proyecto minimo con un `setVar` que el log puede direccionar por `cmd_id`.
fn project() -> String {
    json!({
        "project": {
            "commands": [{
                "father": "setVar",
                "command": "\"hola\"",
                "group": "system",
                "id": ROBOT_ID,
                "line": 2,
                "var": "saludo",
                "children": [],
                "else": [],
            }],
            "vars": [{"name": "saludo", "data": null}],
            "profile": {"name": "demo", "description": "fixture"},
            "ifs": [],
            "modules": [],
            "expose": {},
        }
    })
    .to_string()
}

fn make_db(dir: &Path) -> PathBuf {
    make_db_with_versions(dir, &[(1, "2026-11-17 10:00:00")])
}

fn make_db_with_versions(dir: &Path, rows: &[(i64, &str)]) -> PathBuf {
    let path = dir.join("robot.db");
    let conn = rusqlite::Connection::open(&path).expect("creo la db");
    conn.execute_batch(
        "CREATE TABLE bots (
            id INTEGER PRIMARY KEY,
            name TEXT,
            created_at TEXT,
            data TEXT
        );",
    )
    .expect("creo la tabla");
    let b64 = base64::engine::general_purpose::STANDARD.encode(project().as_bytes());
    for (id, created_at) in rows {
        conn.execute(
            "INSERT INTO bots (id, name, created_at, data) VALUES (?1, 'demo', ?2, ?3)",
            rusqlite::params![id, created_at, b64],
        )
        .expect("inserto el snapshot");
    }
    drop(conn);
    path
}

/// Escribe un log con una peticion y un error posterior. La hora es muy futura
/// para que quede dentro de la ventana de cualquier ejecucion del test.
fn write_log(dir: &Path, name: &str, robot: &str) {
    std::fs::create_dir_all(dir).expect("creo el directorio de logs");
    let text = format!(
        "2099-01-01 00:00:00.000000 - rocketbot - INFO - request: {robot} : line 2 - rpasystem : setVar  id: {ROBOT_ID} -  - \"hola\"\n\
         2099-01-01 00:00:01.000000 - rocketbot - ERROR - EXCEPTION IN (<string> system, L_253 \"\"): NameError: name 'contador' is not defined\n"
    );
    std::fs::write(dir.join(name), text).expect("escribo el log");
}

fn outcome(log_dir: PathBuf, exit_code: Option<i32>) -> RunOutcome {
    RunOutcome {
        exit_code,
        timed_out: false,
        duration: Duration::from_millis(120),
        started_at: snapshot_at(),
        log_dir,
        log_fallback: None,
        reported_log: None,
        stdout_tail: String::new(),
        stderr_tail: String::new(),
        spawn_error: None,
        command: "rocketbot.exe -start=demo".to_string(),
    }
}

struct StaticRunner {
    studio: Vec<u32>,
    outcome: RunOutcome,
}

impl Runner for StaticRunner {
    fn studio_open(&self) -> Vec<u32> {
        self.studio.clone()
    }

    fn run(&self, _request: &RunRequest) -> RunOutcome {
        self.outcome.clone()
    }
}

fn broker_with(studio: Vec<u32>, outcome: RunOutcome) -> Broker {
    Broker::new(Arc::new(StaticRunner { studio, outcome }))
}

fn request(db: &Path) -> RunRequest {
    RunRequest::new("demo", db.to_path_buf())
}

#[test]
fn sin_eventos_del_robot_es_fallo_aunque_el_exit_code_sea_cero() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let log_dir = tmp.path().join("run");
    std::fs::create_dir_all(&log_dir).expect("run dir");
    let broker = broker_with(Vec::new(), outcome(log_dir, Some(0)));

    let report = broker.execute(request(&tmp.path().join("robot.db")));

    assert!(!report.ok);
    assert_eq!(report.failure, Some(RunFailure::NoEvents));
    assert_eq!(report.events, 0);
    assert_eq!(report.exit_code, Some(0));
}

#[test]
fn con_eventos_y_exit_cero_la_ejecucion_es_valida() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = make_db(tmp.path());
    let log_dir = tmp.path().join("run");
    write_log(&log_dir, "logfile.log", "demo");
    let broker = broker_with(Vec::new(), outcome(log_dir, Some(0)));

    let report = broker.execute(request(&db_path));

    assert!(report.ok, "{:?}", report.failure);
    assert_eq!(report.events, 2, "la peticion y el error son eventos");
    assert_eq!(report.errors.len(), 1);
    let error = &report.errors[0];
    assert_eq!(error.line, "2", "la linea sale del snapshot, no del log");
    assert_eq!(error.version, Some(1));
    assert_eq!(error.verdict, Verdict::Vivo);
    assert_eq!(report.snapshot_id, Some(1));
}

#[test]
fn los_errores_se_atribuyen_al_snapshot_vivo_no_al_ultimo_guardado() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // v1 estaba vivo cuando ocurrio el error; v2 se guardo despues. El informe
    // debe hablar de v1 aunque el snapshot actual del robot sea v2.
    let db_path = make_db_with_versions(
        tmp.path(),
        &[(1, "2026-11-17 10:00:00"), (2, "2026-11-17 10:00:05")],
    );
    let log_dir = tmp.path().join("run");
    std::fs::create_dir_all(&log_dir).expect("run dir");
    std::fs::write(
        log_dir.join("logfile.log"),
        format!(
            "2026-11-17 10:00:03.000000 - rocketbot - INFO - request: demo : line 2 - rpasystem : setVar  id: {ROBOT_ID} -  - \"hola\"\n\
             2026-11-17 10:00:03.500000 - rocketbot - ERROR - EXCEPTION IN (<string> system, L_253 \"\"): boom\n"
        ),
    )
    .expect("log");
    let mut raw = outcome(log_dir, Some(0));
    raw.started_at = db::parse_date("2026-11-17 10:00:00");
    let broker = broker_with(Vec::new(), raw);

    let report = broker.execute(request(&db_path));
    assert!(report.ok, "{:?}", report.failure);
    assert_eq!(report.snapshot_id, Some(2), "el actual al terminar es v2");
    assert_eq!(report.errors.len(), 1);
    assert_eq!(
        report.errors[0].version,
        Some(1),
        "el error ocurrio cuando estaba vivo v1"
    );
    assert_eq!(report.errors[0].verdict, Verdict::Historico);
}

#[test]
fn studio_abierto_se_detecta_antes_de_lanzar_y_se_explica() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let broker = broker_with(vec![10876, 42], outcome(tmp.path().join("run"), Some(0)));

    let report = broker.execute(request(&tmp.path().join("robot.db")));

    assert!(!report.ok);
    assert_eq!(
        report.failure,
        Some(RunFailure::StudioOpen(vec![10876, 42]))
    );
    assert_eq!(report.events, 0);
    let text = report.failure.as_ref().expect("fallo").explain();
    assert!(text.contains("10876"), "{text}");
    assert!(text.contains("modo API"), "{text}");
    assert!(text.contains("No se lanzo nada"), "{text}");
}

#[test]
fn un_exit_code_distinto_de_cero_es_fallo_aunque_haya_eventos() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = make_db(tmp.path());
    let log_dir = tmp.path().join("run");
    write_log(&log_dir, "logfile.log", "demo");
    let broker = broker_with(Vec::new(), outcome(log_dir, Some(7)));

    let report = broker.execute(request(&db_path));

    assert!(!report.ok);
    assert_eq!(report.failure, Some(RunFailure::Exit(Some(7))));
    assert_eq!(report.events, 2, "los eventos se conservan como evidencia");
}

#[test]
fn un_timeout_es_fallo_y_conserva_la_duracion() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = make_db(tmp.path());
    let mut raw = outcome(tmp.path().join("run"), None);
    raw.timed_out = true;
    raw.duration = Duration::from_secs(180);
    let broker = broker_with(Vec::new(), raw);

    let report = broker.execute(request(&db_path));

    assert!(!report.ok);
    assert_eq!(report.failure, Some(RunFailure::Timeout));
    assert!(report.timed_out);
    assert_eq!(report.duration_ms, 180_000);
}

#[test]
fn un_error_al_lanzar_no_se_confunde_con_un_fallo_del_robot() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut raw = outcome(tmp.path().join("run"), None);
    raw.spawn_error = Some("no such file".to_string());
    let broker = broker_with(Vec::new(), raw);

    let report = broker.execute(request(&tmp.path().join("robot.db")));

    assert!(!report.ok);
    assert!(matches!(report.failure, Some(RunFailure::Spawn(_))));
}

struct GatedRunner {
    entered: Mutex<Option<mpsc::Sender<()>>>,
    gate: Arc<(Mutex<bool>, Condvar)>,
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
    outcome: RunOutcome,
}

impl Runner for GatedRunner {
    fn run(&self, _request: &RunRequest) -> RunOutcome {
        let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(now, Ordering::SeqCst);
        if let Some(tx) = self.entered.lock().expect("entered").take() {
            let _ = tx.send(());
        }
        let (lock, cv) = &*self.gate;
        let mut go = lock.lock().expect("gate");
        while !*go {
            go = cv.wait(go).expect("wait");
        }
        drop(go);
        self.active.fetch_sub(1, Ordering::SeqCst);
        self.outcome.clone()
    }
}

fn wait_until(mut condition: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timeout esperando: {what}");
}

#[test]
fn el_broker_serializa_las_ejecuciones_y_la_cola_es_visible() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = make_db(tmp.path());
    let log_dir = tmp.path().join("run");
    write_log(&log_dir, "logfile.log", "demo");

    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (tx, rx) = mpsc::channel();
    let runner = Arc::new(GatedRunner {
        entered: Mutex::new(Some(tx)),
        gate: gate.clone(),
        active: Arc::new(AtomicUsize::new(0)),
        max_active: Arc::new(AtomicUsize::new(0)),
        outcome: outcome(log_dir, Some(0)),
    });
    let broker = Arc::new(Broker::new(runner.clone()));

    let first = {
        let broker = broker.clone();
        let db_path = db_path.clone();
        std::thread::spawn(move || broker.execute(request(&db_path)))
    };
    rx.recv_timeout(Duration::from_secs(5))
        .expect("la primera ejecucion arranca");

    let status = broker.status();
    assert!(status.running.is_some(), "la primera esta corriendo");
    assert!(status.queue.is_empty(), "todavia no hay nadie esperando");

    let second = {
        let broker = broker.clone();
        let db_path = db_path.clone();
        std::thread::spawn(move || broker.execute(request(&db_path)))
    };
    wait_until(
        || broker.status().queue.len() == 1,
        "la segunda ejecucion aparece en la cola",
    );

    let status = broker.status();
    assert_eq!(status.queue.len(), 1);
    assert_eq!(status.queue[0].robot, "demo");
    assert_eq!(status.queue[0].db_label, db::label(&db_path));

    {
        let (lock, cv) = &*gate;
        *lock.lock().expect("gate") = true;
        cv.notify_all();
    }

    let first = first.join().expect("primera");
    let second = second.join().expect("segunda");
    assert!(first.ok, "{:?}", first.failure);
    assert!(second.ok, "{:?}", second.failure);
    assert_eq!(
        runner.max_active.load(Ordering::SeqCst),
        1,
        "nunca hay dos ejecuciones a la vez"
    );
    assert!(
        broker.status().running.is_none() && broker.status().queue.is_empty(),
        "el broker queda libre"
    );
}

struct PanicOnce {
    calls: AtomicUsize,
    outcome: RunOutcome,
}

impl Runner for PanicOnce {
    fn run(&self, _request: &RunRequest) -> RunOutcome {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("fallo simulado del runner");
        }
        self.outcome.clone()
    }
}

#[test]
fn un_panico_del_runner_libera_el_turno() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = make_db(tmp.path());
    let log_dir = tmp.path().join("run");
    write_log(&log_dir, "logfile.log", "demo");
    let broker = Broker::new(Arc::new(PanicOnce {
        calls: AtomicUsize::new(0),
        outcome: outcome(log_dir, Some(0)),
    }));

    let first = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        broker.execute(request(&db_path))
    }));
    assert!(first.is_err(), "el runner entro en panico");

    let status: BrokerStatus = broker.status();
    assert!(
        status.running.is_none(),
        "el turno se libera aunque el runner reviente"
    );

    let second = broker.execute(request(&db_path));
    assert!(second.ok, "{:?}", second.failure);
}

#[test]
fn el_informe_no_filtra_la_ruta_absoluta_de_la_db() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let log_dir = tmp.path().join("run");
    std::fs::create_dir_all(&log_dir).expect("run dir");
    let db_path = tmp.path().join("entrega").join("robot.db");
    std::fs::create_dir_all(db_path.parent().expect("parent")).expect("dir");
    let broker = broker_with(Vec::new(), outcome(log_dir, Some(0)));

    let report: RunReport = broker.execute(request(&db_path));
    assert_eq!(report.db_label, "entrega\\robot.db");
    assert!(
        !report
            .db_label
            .contains(&tmp.path().to_string_lossy().to_string()),
        "la etiqueta no lleva la ruta del usuario"
    );
}
