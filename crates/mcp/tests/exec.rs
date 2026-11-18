//! Tests de `run_robot` y `run_status` por el protocolo MCP.
//!
//! El runner es un doble que escribe logs sinteticos: no se lanza Rocketbot. La
//! ejecucion real contra BBRE2 vive al final, marcada `#[ignore]`.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rocketdebug_core::exec::{Broker, RunOutcome, RunRequest, Runner};
use rocketdebug_mcp::{handle_line_with, ExecSettings, Settings};
use serde_json::{json, Value};

use common::{call_request, demo_project, is_error, make_db, parse_response, tool_text, TempDir};

const ROBOT: &str = "demo";
const SETVAR_ID: &str = "22222222-2222-2222-2222-222222222222";

fn call(settings: &Settings, id: i64, tool: &str, args: Value) -> Value {
    let linea = call_request(id, tool, args);
    let respuesta = handle_line_with(&linea, settings)
        .unwrap_or_else(|| panic!("`{tool}` tiene que responder"));
    parse_response(&respuesta)
}

/// Runner falso: cada ejecucion crea su directorio de logs y escribe (o no) una
/// peticion del robot y, opcionalmente, un error.
struct FixturesRunner {
    base: PathBuf,
    studio: Vec<u32>,
    write_events: bool,
    write_error: bool,
    runs: AtomicUsize,
}

impl FixturesRunner {
    fn new(base: PathBuf) -> Self {
        Self {
            base,
            studio: Vec::new(),
            write_events: true,
            write_error: true,
            runs: AtomicUsize::new(0),
        }
    }

    fn studio(mut self, pids: Vec<u32>) -> Self {
        self.studio = pids;
        self
    }

    fn without_events(mut self) -> Self {
        self.write_events = false;
        self.write_error = false;
        self
    }
}

impl Runner for FixturesRunner {
    fn studio_open(&self) -> Vec<u32> {
        self.studio.clone()
    }

    fn run(&self, _request: &RunRequest) -> RunOutcome {
        let n = self.runs.fetch_add(1, Ordering::SeqCst);
        let log_dir = self.base.join("rocketdebug-runs").join(format!("run-{n}"));
        std::fs::create_dir_all(&log_dir).expect("directorio de logs");
        if self.write_events {
            let mut text = format!(
                "2099-01-01 00:00:00.000000 - rocketbot - INFO - request: {ROBOT} : line 2 - rpasystem : setVar  id: {SETVAR_ID} -  - \"hola\"\n"
            );
            if self.write_error {
                text.push_str(
                    "2099-01-01 00:00:01.000000 - rocketbot - ERROR - EXCEPTION IN (<string> system, L_253 \"\"): NameError: name 'contador' is not defined\n",
                );
            }
            std::fs::write(log_dir.join("logfile.log"), text).expect("escribo el log");
        }
        RunOutcome {
            exit_code: Some(0),
            timed_out: false,
            duration: Duration::from_millis(250),
            started_at: chrono::NaiveDateTime::default(),
            log_dir,
            log_fallback: Some(self.base.clone()),
            reported_log: None,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            spawn_error: None,
            command: "rocketbot.exe -start=demo".to_string(),
        }
    }
}

fn settings_with_runner(tmp: &TempDir, runner: FixturesRunner) -> Settings {
    let base = tmp.path().to_path_buf();
    let broker = Broker::new(Arc::new(runner));
    let mut settings = Settings::hermetic();
    settings.logs = Some((base.clone(), 20));
    settings.exec = Some(ExecSettings {
        broker: Arc::new(broker),
        logs_dir: base,
        default_timeout: Duration::from_secs(30),
    });
    settings
}

fn db(tmp: &TempDir) -> PathBuf {
    make_db(
        tmp.path(),
        ROBOT,
        &[(1, "2026-11-17 10:00:00", demo_project().as_str())],
    )
}

#[test]
fn run_robot_verifica_eventos_y_atribuye_errores() {
    let tmp = TempDir::new("run-ok");
    let settings = settings_with_runner(&tmp, FixturesRunner::new(tmp.path().to_path_buf()));
    let db = db(&tmp);

    let respuesta = call(&settings, 1, "run_robot", json!({"robot": ROBOT, "db": db}));
    assert!(!is_error(&respuesta), "{respuesta}");
    let texto = tool_text(&respuesta);
    assert!(texto.contains("ok: true"), "{texto}");
    assert!(texto.contains("eventos del robot: 2"), "{texto}");
    assert!(texto.contains("snapshot vivo: 1"), "{texto}");
    assert!(texto.contains("## Errores (1 distintos)"), "{texto}");
    assert!(texto.contains("- L2 setVar [vivo v1]"), "{texto}");
    assert!(texto.contains("NameError"), "{texto}");

    // El pack lee los mismos logs: los errores nuevos aparecen atribuidos al
    // snapshot vivo, sin volver a ejecutar nada.
    let pack = call(&settings, 2, "flow_pack", json!({"robot": ROBOT, "db": db}));
    assert!(!is_error(&pack), "{pack}");
    let pack_text = tool_text(&pack);
    assert!(pack_text.contains("## Errores"), "{pack_text}");
    assert!(pack_text.contains("vivos 1"), "{pack_text}");
    assert!(pack_text.contains("L2"), "{pack_text}");
}

#[test]
fn sin_eventos_la_tool_es_error_y_no_finge_exito() {
    let tmp = TempDir::new("run-empty");
    let settings = settings_with_runner(
        &tmp,
        FixturesRunner::new(tmp.path().to_path_buf()).without_events(),
    );
    let db = db(&tmp);

    let respuesta = call(&settings, 1, "run_robot", json!({"robot": ROBOT, "db": db}));
    assert!(is_error(&respuesta), "sin eventos no es exito: {respuesta}");
    let texto = tool_text(&respuesta);
    assert!(texto.contains("ok: false"), "{texto}");
    assert!(texto.contains("0 eventos del robot"), "{texto}");
}

#[test]
fn studio_abierto_se_rechaza_con_la_alternativa_documentada() {
    let tmp = TempDir::new("run-studio");
    let settings = settings_with_runner(
        &tmp,
        FixturesRunner::new(tmp.path().to_path_buf()).studio(vec![10876]),
    );
    let db = db(&tmp);

    let respuesta = call(&settings, 1, "run_robot", json!({"robot": ROBOT, "db": db}));
    assert!(is_error(&respuesta), "{respuesta}");
    let texto = tool_text(&respuesta);
    assert!(texto.contains("Studio abierto"), "{texto}");
    assert!(texto.contains("10876"), "{texto}");
    assert!(texto.contains("modo API"), "{texto}");
}

#[test]
fn run_robot_valida_el_robot_antes_de_ocupar_la_cola() {
    let tmp = TempDir::new("run-404");
    let settings = settings_with_runner(&tmp, FixturesRunner::new(tmp.path().to_path_buf()));
    let db = db(&tmp);

    let respuesta = call(
        &settings,
        1,
        "run_robot",
        json!({"robot": "no_existe", "db": db}),
    );
    assert!(is_error(&respuesta), "{respuesta}");
    assert!(tool_text(&respuesta).contains("no_existe"), "{respuesta}");
}

#[test]
fn run_status_muestra_el_broker_libre_y_la_ultima_ejecucion() {
    let tmp = TempDir::new("run-status");
    let settings = settings_with_runner(&tmp, FixturesRunner::new(tmp.path().to_path_buf()));

    let libre = call(&settings, 1, "run_status", json!({}));
    assert!(!is_error(&libre), "{libre}");
    assert!(tool_text(&libre).contains("broker: libre"), "{libre}");

    let db = db(&tmp);
    let _run = call(&settings, 2, "run_robot", json!({"robot": ROBOT, "db": db}));
    let despues = call(&settings, 3, "run_status", json!({}));
    let texto = tool_text(&despues);
    assert!(texto.contains("ultima: demo"), "{texto}");
    assert!(texto.contains("ok"), "{texto}");
}

/// Ejecucion real contra BBRE2. Requiere:
///
/// - `ROCKETBOT_EXE` apuntando a `rocketbot.exe` (o la ruta estandar),
/// - **Studio cerrado** (el broker se niega con Studio abierto),
/// - `C:\rocketbot_bots\BBRE2\robot_run.db` (o `ROCKETDEBUG_REAL_DB`).
///
/// `ML_00_Preparacion` es el robot mas barato de BBRE2 (12 nodos, sin navegador)
/// y basta para demostrar que el broker ejecuta, ve eventos y deja el log que el
/// pack lee. No se afirma que el robot salga limpio: de hecho sus rutas
/// migradas son el caso de #26.
#[test]
#[ignore = "requiere rocketbot.exe, Studio cerrado y BBRE2 real"]
fn ejecucion_real_contra_bbre2() {
    let exe = std::env::var("ROCKETBOT_EXE")
        .unwrap_or_else(|_| r"C:\Rocketbot\rocketbot.exe".to_string());
    assert!(
        Path::new(&exe).is_file(),
        "no encuentro {exe}: define ROCKETBOT_EXE"
    );
    let db_path = std::env::var("ROCKETDEBUG_REAL_DB")
        .unwrap_or_else(|_| r"C:\rocketbot_bots\BBRE2\robot_run.db".to_string());
    assert!(
        Path::new(&db_path).is_file(),
        "no encuentro {db_path}: define ROCKETDEBUG_REAL_DB"
    );

    // `Settings::from_env` resuelve el broker una sola vez; se le da el ejecutable
    // por entorno para no depender de que este en `C:\Rocketbot`.
    std::env::set_var("ROCKETBOT_EXE", &exe);
    let settings = Settings::from_env();
    assert!(
        settings.exec.is_some(),
        "el entorno no configuro el broker; revisa ROCKETBOT_EXE"
    );

    let respuesta = call(
        &settings,
        1,
        "run_robot",
        json!({"robot": "ML_00_Preparacion", "db": db_path, "timeout": 300}),
    );
    let texto = tool_text(&respuesta);
    let eventos = texto
        .lines()
        .find_map(|line| line.strip_prefix("eventos del robot: "))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or_else(|| panic!("la respuesta no trae el contador de eventos:\n{texto}"));
    assert!(
        eventos > 0,
        "la ejecucion no dejo ni un evento del robot:\n{texto}"
    );
    assert!(
        texto.contains("snapshot vivo:"),
        "falta la atribucion al snapshot vivo:\n{texto}"
    );

    let pack = call(
        &settings,
        2,
        "flow_pack",
        json!({"robot": "ML_00_Preparacion", "db": db_path}),
    );
    assert!(!is_error(&pack), "{pack}");
    let pack_text = tool_text(&pack);
    if texto.contains("## Errores") {
        // El caso util para #26/#27: la ejecucion fallo y el pack tiene que ver
        // esos errores nuevos atribuidos al snapshot vivo.
        assert!(
            pack_text.contains("## Errores"),
            "el pack no ve los errores de la ejecucion:\n{pack_text}"
        );
    } else {
        assert!(
            !pack_text.contains("## Errores"),
            "el pack inventa errores que la ejecucion no dejo:\n{pack_text}"
        );
    }
}
