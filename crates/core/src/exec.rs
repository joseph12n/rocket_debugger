//! Broker de ejecucion: lanza `rocketbot.exe`, serializa las ejecuciones y no se
//! fia del `exit_code`.
//!
//! # El problema que resuelve
//!
//! Con Studio abierto, `rocketbot.exe -start=... -db=...` **delega en la
//! instancia que ya esta corriendo** y puede devolver `exit_code 0` sin haber
//! ejecutado el robot. La fase 0 lo midio (`docs/CICLO.md`): `eventos_del_robot:
//! 0`. Un agente que se fie del codigo de salida concluye que el robot funciona
//! cuando en realidad no corrio nunca.
//!
//! Por eso este modulo no expone "lanza y devuelve el exit code": expone
//! [`Broker`], que
//!
//! 1. admite **una sola ejecucion a la vez** (cola FIFO con mutex y condvar),
//! 2. detecta Studio abierto **antes de lanzar** y se niega,
//! 3. despues de lanzar exige **al menos un evento del robot** en los logs: sin
//!    eventos el resultado es fallo, aunque el proceso haya salido con 0,
//! 4. atribuye cada error de esa ejecucion al snapshot que estaba vivo.
//!
//! # Que cuenta como evento
//!
//! Una entrada de log con bloque `request: <robot> : line ...`, es decir, un
//! comando que el runner reporto de **ese** robot. Los contadores de bucle
//! (`For count: 1`) y el ruido del motor (`zdev`, `getbot`) no son eventos del
//! robot y no sirven para demostrar que se ejecuto. [`crate::logs::of_robot`] ya
//! aplica exactamente ese filtro; aqui solo se le anade la ventana temporal.
//!
//! # Donde se leen los logs
//!
//! El runner recibe `--log=<dir>` con un directorio nuevo por ejecucion. Si esa
//! opcion fuera ignorada por la version instalada, se intenta el fichero que el
//! propio CLI anuncia en stdout (`Log path: ...`) y, como ultimo recurso, el
//! directorio de logs configurado. Los tres caminos filtran por la hora de
//! arranque, asi que un log viejo no se cuenta como ejecucion de ahora.
//!
//! # Por que no hay dependencias nuevas
//!
//! La cola es `Mutex` + `Condvar` de la biblioteca estandar y el timeout se
//! implementa con `try_wait` + `sleep`: anadir `wait-timeout` o un runtime async
//! a un binario que habla con un agente local no aporta nada. La deteccion de
//! Studio usa `tasklist` en Windows y devuelve vacio en el resto: es la unica
//! forma de enumerar procesos sin `unsafe` adicional.

use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::NaiveDateTime;

use crate::analysis;
use crate::attribution::{self, RawError, Verdict};
use crate::db;
use crate::logs;
use crate::signature;

/// `rocketbot.exe` del Studio instalado en la ruta estandar.
///
/// Es solo un valor por defecto: en una instalacion portatil (la de este equipo,
/// por ejemplo) hay que apuntar `ROCKETBOT_EXE` al ejecutable real.
pub const DEFAULT_EXE: &str = r"C:\Rocketbot\rocketbot.exe";

/// Timeout de ejecucion por defecto, en segundos.
pub const DEFAULT_TIMEOUT_SECS: u64 = 180;

/// Timeout minimo y maximo aceptados por la fachada.
pub const MIN_TIMEOUT_SECS: u64 = 5;
pub const MAX_TIMEOUT_SECS: u64 = 900;

/// Convierte segundos de peticion a un timeout valido, acotando a los limites.
pub fn timeout_from_secs(secs: i64) -> Duration {
    let secs = secs.clamp(MIN_TIMEOUT_SECS as i64, MAX_TIMEOUT_SECS as i64);
    Duration::from_secs(secs as u64)
}

/// Directorios de ejecucion que se conservan. Los logs son evidencia, pero un
/// daemon que vive semanas no puede crecer sin limite.
pub const KEEP_RUN_DIRS: usize = 20;

/// Errores distintos que se guardan en el informe. Un bucle que falla 500 veces
/// es un error con 500 ocurrencias, no 500 lineas.
pub const MAX_REPORT_ERRORS: usize = 50;

/// Bytes de stdout/stderr que se conservan por ejecucion.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Ficheros de log que se miran en el directorio de respaldo.
const FALLBACK_LOG_FILES: usize = 20;

/// Ficheros de log que se miran en el directorio de la ejecucion (es nuevo, con
/// uno basta, pero un runner puede crear un fichero por dia/subcarpeta).
const RUN_LOG_FILES: usize = 64;

/// Margen al comparar la hora del log con la del arranque: los dos relojes son
/// el mismo, pero el redondeo a milisegundos de Windows y los microsegundos del
/// log pueden dejar una entrada del primer comando unos microsegundos "antes".
const CLOCK_SLACK_SECS: i64 = 2;

/// Como se detecta que Studio esta abierto.
///
/// Es un enum y no un trait para que los tests no dependan del `tasklist` de la
/// maquina: `Closed` convierte la comprobacion en una constante.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StudioProbe {
    /// Enumera procesos `rocketbot.exe` del sistema (Windows).
    System,
    /// Simula que no hay ninguno. Solo tests y entornos sin Rocketbot.
    Closed,
}

/// Configuracion del runner de procesos.
#[derive(Debug, Clone)]
pub struct ExecConfig {
    /// Ruta a `rocketbot.exe`.
    pub exe: PathBuf,
    /// Directorio donde el pack busca logs y bajo el que se crean los
    /// subdirectorios de ejecucion.
    pub logs_dir: PathBuf,
    /// Como decidir si Studio esta abierto.
    pub probe: StudioProbe,
}

impl ExecConfig {
    pub fn new(exe: PathBuf, logs_dir: PathBuf) -> Self {
        Self {
            exe,
            logs_dir,
            probe: StudioProbe::System,
        }
    }

    /// Linea de comandos que se le pasa al runner.
    ///
    /// `--log=` va con `=`: la version instalida (2026.10.26) revienta con
    /// `--log <ruta>` separado (`IndexError: list index out of range`), asi que
    /// la forma no es un detalle de estilo.
    ///
    /// No se pasa `--nodebug`: el CLI lo documenta como "disable terminal logs"
    /// y no esta verificado que conserve el log de fichero. El stdout se captura
    /// acotado y solo se muestra si la ejecucion falla.
    pub fn command_spec(&self, db: &Path, robot: &str, log_dir: &Path) -> CommandSpec {
        // Las dos rutas se absolutizan antes de pasarlas: el proceso hijo corre
        // con `cwd` en el directorio de la db, asi que una ruta relativa
        // (`BBRE2\robot.db`) se resolveria contra si misma y el runner abriria
        // `BBRE2\BBRE2\robot.db`.
        let db = absolute(db);
        let log_dir = absolute(log_dir);
        CommandSpec {
            program: self.exe.clone(),
            args: vec![
                format!("-start={robot}"),
                format!("-db={}", db.display()),
                format!("--log={}", log_dir.display()),
            ],
            cwd: db
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
        }
    }
}

/// Programa, argumentos y cwd de una ejecucion. Separado del lanzamiento para
/// poder probar la linea de comandos sin arrancar Rocketbot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

impl CommandSpec {
    /// Representacion de una linea, para el informe. Las rutas se enmascaran al
    /// renderizar, no aqui: el dato original sirve para diagnosticar.
    pub fn display(&self) -> String {
        let mut out = format!("\"{}\"", self.program.display());
        for arg in &self.args {
            out.push(' ');
            out.push_str(arg);
        }
        out
    }
}

/// Una peticion de ejecucion.
#[derive(Debug, Clone)]
pub struct RunRequest {
    pub robot: String,
    pub db: PathBuf,
    /// Tope de tiempo de **ejecucion**, no de espera en cola.
    pub timeout: Duration,
}

impl RunRequest {
    pub fn new(robot: impl Into<String>, db: PathBuf) -> Self {
        Self {
            robot: robot.into(),
            db,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }
}

/// Lo que devuelve un runner. Es el hecho bruto; la verificacion de eventos y la
/// atribucion son del [`Broker`], no del runner, para que un runner falso en un
/// test pase por el mismo control que el real.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration: Duration,
    /// Hora local del arranque. Se compara con las horas del log, que tambien
    /// son locales.
    pub started_at: NaiveDateTime,
    /// Directorio nuevo de logs de esta ejecucion.
    pub log_dir: PathBuf,
    /// Directorio de logs configurado, como respaldo si `--log` se ignora.
    pub log_fallback: Option<PathBuf>,
    /// Fichero de log que el propio CLI anuncio en stdout, si se pudo resolver.
    pub reported_log: Option<PathBuf>,
    pub stdout_tail: String,
    pub stderr_tail: String,
    /// Error al lanzar el proceso (no llego a ejecutarse).
    pub spawn_error: Option<String>,
    /// Linea de comandos efectiva, para el informe.
    pub command: String,
}

impl RunOutcome {
    /// Resultado de un lanzamiento que ni siquiera empezo.
    fn not_launched(started_at: NaiveDateTime, log_dir: PathBuf, error: String) -> Self {
        Self {
            exit_code: None,
            timed_out: false,
            duration: Duration::ZERO,
            started_at,
            log_dir,
            log_fallback: None,
            reported_log: None,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            spawn_error: Some(error),
            command: String::new(),
        }
    }
}

/// Quien sabe ejecutar un robot. El broker solo aporta la serializacion y las
/// garantias; el proceso real lo pone [`ProcessRunner`] y los tests un doble.
pub trait Runner: Send + Sync {
    /// PIDs de `rocketbot.exe` ya en ejecucion. Vacio = se puede lanzar.
    fn studio_open(&self) -> Vec<u32> {
        Vec::new()
    }

    fn run(&self, request: &RunRequest) -> RunOutcome;
}

/// Runner real: `rocketbot.exe` como proceso hijo.
#[derive(Debug, Clone)]
pub struct ProcessRunner {
    config: ExecConfig,
}

impl ProcessRunner {
    pub fn new(config: ExecConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &ExecConfig {
        &self.config
    }
}

impl Runner for ProcessRunner {
    fn studio_open(&self) -> Vec<u32> {
        match self.config.probe {
            StudioProbe::System => rocketbot_pids(),
            StudioProbe::Closed => Vec::new(),
        }
    }

    fn run(&self, request: &RunRequest) -> RunOutcome {
        let started_at = now_local();
        let started = Instant::now();
        let _ = prune_run_dirs(&self.config.logs_dir, KEEP_RUN_DIRS);
        let log_dir = new_run_dir(&self.config.logs_dir, &request.robot);

        if let Err(error) = std::fs::create_dir_all(&log_dir) {
            return RunOutcome::not_launched(
                started_at,
                log_dir,
                format!("no pude crear el directorio de logs: {error}"),
            );
        }

        let spec = self
            .config
            .command_spec(&request.db, &request.robot, &log_dir);
        let mut child = match Command::new(&spec.program)
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                return RunOutcome::not_launched(
                    started_at,
                    log_dir,
                    format!("no pude lanzar {}: {error}", spec.program.display()),
                )
            }
        };

        let stdout_reader = child.stdout.take().map(spawn_reader);
        let stderr_reader = child.stderr.take().map(spawn_reader);

        let deadline = started + request.timeout;
        let mut timed_out = false;
        let exit_code = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.code(),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        timed_out = true;
                        kill_tree(child.id());
                        // `wait` despues de matar no puede quedarse colgado: el
                        // arbol ya recibio la senal. Si falla, el codigo es
                        // desconocido y el informe lo dice.
                        break child.wait().ok().and_then(|status| status.code());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => {
                    timed_out = true;
                    kill_tree(child.id());
                    break child.wait().ok().and_then(|status| status.code());
                }
            }
        };

        let stdout = stdout_reader
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();
        let stderr = stderr_reader
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();
        let duration = started.elapsed();

        RunOutcome {
            exit_code,
            timed_out,
            duration,
            started_at,
            log_dir: log_dir.clone(),
            log_fallback: Some(self.config.logs_dir.clone()),
            reported_log: reported_log_path(&stdout, &spec.cwd, &self.config.exe),
            stdout_tail: stdout,
            stderr_tail: stderr,
            spawn_error: None,
            command: spec.display(),
        }
    }
}

/// Por que una ejecucion no cuenta como exitosa.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunFailure {
    /// Studio (u otra instancia) ya esta corriendo: el CLI delegaria sin ejecutar.
    StudioOpen(Vec<u32>),
    /// No se pudo lanzar el proceso.
    Spawn(String),
    /// Se agoto el timeout y se mato el arbol.
    Timeout,
    /// El proceso termino con un codigo distinto de cero.
    Exit(Option<i32>),
    /// Termino "bien" pero no hay ni un evento del robot: no ejecuto.
    NoEvents,
}

impl RunFailure {
    /// Explicacion para el agente. Vive aqui, junto al dato, para que el CLI y la
    /// fachada MCP no inventen dos redacciones del mismo fallo.
    pub fn explain(&self) -> String {
        match self {
            Self::StudioOpen(pids) => {
                let lista = pids
                    .iter()
                    .map(|pid| pid.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "Studio abierto: rocketbot.exe ya esta corriendo (PID {lista}). Por CLI, \
                     rocketbot.exe delega en la instancia existente y puede devolver 0 sin \
                     ejecutar el robot. Cierra Studio y reintenta, o usa el modo API del \
                     servidor de Rocketbot. No se lanzo nada."
                )
            }
            Self::Spawn(error) => format!("no pude lanzar rocketbot.exe: {error}"),
            Self::Timeout => {
                "timeout: rocketbot.exe no termino a tiempo y se mato el arbol del proceso"
                    .to_string()
            }
            Self::Exit(Some(code)) => format!("el robot termino con exit_code {code}"),
            Self::Exit(None) => {
                "el robot termino sin codigo de salida (senal o proceso matado)".to_string()
            }
            Self::NoEvents => {
                "0 eventos del robot: el proceso termino pero no hay ni una peticion suya en el \
                 log. El exit_code no prueba ejecucion: con Studio abierto, sin licencia de \
                 produccion o con el runner delegando, rocketbot.exe sale con 0 sin correr el \
                 robot."
                    .to_string()
            }
        }
    }
}

/// Un error de la ejecucion, ya atribuido.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunErrorItem {
    /// Linea de Studio en el snapshot atribuido. Vacia si el nodo no existe alli.
    pub line: String,
    pub command: String,
    pub message: String,
    pub signature: String,
    pub verdict: Verdict,
    pub version: Option<i64>,
    pub occurrences: usize,
}

/// Informe completo de una ejecucion. Es el contrato del broker con la fachada.
#[derive(Debug, Clone)]
pub struct RunReport {
    pub robot: String,
    /// Etiqueta relativa de la db (`BBRE2\robot_run.db`), sin ruta absoluta.
    pub db_label: String,
    pub ok: bool,
    pub failure: Option<RunFailure>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    /// Tiempo esperando turno en la cola.
    pub queue_wait_ms: u64,
    /// Entradas de log del robot (peticiones), incluidas las de error.
    pub events: usize,
    pub errors: Vec<RunErrorItem>,
    /// Snapshot vivo al terminar, para que el agente sepa que codigo juzgo.
    pub snapshot_id: Option<i64>,
    pub snapshot_at: Option<NaiveDateTime>,
    pub log_files: Vec<PathBuf>,
    pub log_dir: PathBuf,
    pub command: String,
    pub stdout_tail: String,
    pub stderr_tail: String,
}

impl RunReport {
    fn summary(&self) -> BrokerLast {
        BrokerLast {
            robot: self.robot.clone(),
            ok: self.ok,
            failure: self.failure.clone(),
            at: Instant::now(),
        }
    }
}

/// Una ejecucion vista desde fuera (para `run_status`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerRunInfo {
    pub robot: String,
    pub db_label: String,
    /// Tiempo desde que arranco (si corre) o desde que llego a la cola.
    pub elapsed_ms: u64,
}

/// Ultima ejecucion terminada, resumida.
#[derive(Debug, Clone)]
pub struct BrokerLast {
    pub robot: String,
    pub ok: bool,
    pub failure: Option<RunFailure>,
    pub at: Instant,
}

/// Estado visible del broker.
#[derive(Debug, Clone, Default)]
pub struct BrokerStatus {
    pub running: Option<BrokerRunInfo>,
    pub queue: Vec<BrokerRunInfo>,
    pub last: Option<BrokerLast>,
}

struct QueuedRun {
    ticket: u64,
    robot: String,
    db_label: String,
    queued_at: Instant,
}

struct ActiveRun {
    robot: String,
    db_label: String,
    started_at: Instant,
}

#[derive(Default)]
struct State {
    running: Option<ActiveRun>,
    queue: VecDeque<QueuedRun>,
    last: Option<BrokerLast>,
}

/// Serializa las ejecuciones del workspace.
///
/// El broker es un objeto compartido (`Arc`) por todas las conexiones del
/// servidor: el candado y la cola son de proceso, no de peticion. Como el
/// transporte HTTP atiende cada conexion en su hilo, dos agentes pueden pedir
/// ejecucion a la vez; el segundo espera y su turno es visible en [`status`].
pub struct Broker {
    runner: Arc<dyn Runner>,
    state: Mutex<State>,
    cv: Condvar,
    next_ticket: AtomicU64,
}

impl std::fmt::Debug for Broker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Broker").finish_non_exhaustive()
    }
}

impl Broker {
    pub fn new(runner: Arc<dyn Runner>) -> Self {
        Self {
            runner,
            state: Mutex::new(State::default()),
            cv: Condvar::new(),
            next_ticket: AtomicU64::new(0),
        }
    }

    /// Foto del broker ahora mismo. No bloquea a la ejecucion en curso.
    pub fn status(&self) -> BrokerStatus {
        let state = self.lock();
        let now = Instant::now();
        BrokerStatus {
            running: state.running.as_ref().map(|run| BrokerRunInfo {
                robot: run.robot.clone(),
                db_label: run.db_label.clone(),
                elapsed_ms: elapsed(&now, run.started_at),
            }),
            queue: state
                .queue
                .iter()
                .map(|queued| BrokerRunInfo {
                    robot: queued.robot.clone(),
                    db_label: queued.db_label.clone(),
                    elapsed_ms: elapsed(&now, queued.queued_at),
                })
                .collect(),
            last: state.last.clone(),
        }
    }

    /// Encola y ejecuta. Bloquea hasta que le toca el turno y termina.
    ///
    /// La comprobacion de Studio se hace **dos veces**: antes de encolar (para
    /// responder rapido y no ocupar un turno) y justo antes de lanzar (por si
    /// Studio se abrio mientras esperaba).
    pub fn execute(&self, request: RunRequest) -> RunReport {
        let db_label = db::label(&request.db);
        let queued_at = Instant::now();

        let pids = self.runner.studio_open();
        if !pids.is_empty() {
            return self.failed_without_slot(&request, &db_label, RunFailure::StudioOpen(pids));
        }

        let ticket = self.next_ticket.fetch_add(1, Ordering::SeqCst);
        {
            let mut state = self.lock();
            state.queue.push_back(QueuedRun {
                ticket,
                robot: request.robot.clone(),
                db_label: db_label.clone(),
                queued_at,
            });
            self.cv.notify_all();

            while state.running.is_some()
                || state.queue.front().map(|queued| queued.ticket) != Some(ticket)
            {
                state = self
                    .cv
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            state.queue.pop_front();
            state.running = Some(ActiveRun {
                robot: request.robot.clone(),
                db_label: db_label.clone(),
                started_at: Instant::now(),
            });
        }

        // Si el runner entra en panico, el `Drop` libera el turno: un bug no
        // puede dejar el broker bloqueado para siempre.
        let slot = Slot { broker: self };
        let queue_wait_ms = elapsed(&Instant::now(), queued_at);

        let pids = self.runner.studio_open();
        let report = if !pids.is_empty() {
            self.failed_report(
                &request,
                &db_label,
                RunFailure::StudioOpen(pids),
                queue_wait_ms,
            )
        } else {
            let outcome = self.runner.run(&request);
            self.finish_report(&request, &db_label, outcome, queue_wait_ms)
        };

        drop(slot);
        self.lock().last = Some(report.summary());
        self.cv.notify_all();
        report
    }

    /// Fallo detectado antes de encolar: no se toca la cola.
    fn failed_without_slot(
        &self,
        request: &RunRequest,
        db_label: &str,
        failure: RunFailure,
    ) -> RunReport {
        let mut report = empty_report(request, db_label);
        report.failure = Some(failure);
        report.ok = false;
        report
    }

    fn failed_report(
        &self,
        request: &RunRequest,
        db_label: &str,
        failure: RunFailure,
        queue_wait_ms: u64,
    ) -> RunReport {
        let mut report = empty_report(request, db_label);
        report.failure = Some(failure);
        report.ok = false;
        report.queue_wait_ms = queue_wait_ms;
        report
    }

    fn finish_report(
        &self,
        request: &RunRequest,
        db_label: &str,
        outcome: RunOutcome,
        queue_wait_ms: u64,
    ) -> RunReport {
        let collected = collect_events(&outcome, &request.robot);
        let events = collected.entries.len();

        let (snapshot_id, snapshot_at, errors) = match db::open_readonly(&request.db) {
            Ok(conn) => {
                let snapshot = db::latest_snapshot(&conn, &request.robot).ok().flatten();
                let errors = if events == 0 {
                    Vec::new()
                } else {
                    attribute_run_errors(&conn, &request.robot, &collected.entries)
                };
                match snapshot {
                    Some((id, at)) => (Some(id), Some(at), errors),
                    None => (None, None, errors),
                }
            }
            // La db existia antes de lanzar (la fachada lo comprueba), pero
            // puede desaparecer durante la ejecucion. El hecho bruto de la
            // ejecucion no se pierde: solo falta la atribucion.
            Err(_) => (None, None, Vec::new()),
        };

        let failure = if let Some(error) = &outcome.spawn_error {
            Some(RunFailure::Spawn(error.clone()))
        } else if outcome.timed_out {
            Some(RunFailure::Timeout)
        } else if events == 0 {
            Some(RunFailure::NoEvents)
        } else {
            match outcome.exit_code {
                Some(0) => None,
                other => Some(RunFailure::Exit(other)),
            }
        };

        RunReport {
            robot: request.robot.clone(),
            db_label: db_label.to_string(),
            ok: failure.is_none(),
            failure,
            exit_code: outcome.exit_code,
            timed_out: outcome.timed_out,
            duration_ms: outcome.duration.as_millis() as u64,
            queue_wait_ms,
            events,
            errors,
            snapshot_id,
            snapshot_at,
            log_files: collected.files,
            log_dir: outcome.log_dir,
            command: outcome.command,
            stdout_tail: outcome.stdout_tail,
            stderr_tail: outcome.stderr_tail,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Libera el turno aunque el runner entre en panico.
struct Slot<'a> {
    broker: &'a Broker,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.broker.lock().running = None;
        self.broker.cv.notify_all();
    }
}

fn empty_report(request: &RunRequest, db_label: &str) -> RunReport {
    RunReport {
        robot: request.robot.clone(),
        db_label: db_label.to_string(),
        ok: true,
        failure: None,
        exit_code: None,
        timed_out: false,
        duration_ms: 0,
        queue_wait_ms: 0,
        events: 0,
        errors: Vec::new(),
        snapshot_id: None,
        snapshot_at: None,
        log_files: Vec::new(),
        log_dir: PathBuf::new(),
        command: String::new(),
        stdout_tail: String::new(),
        stderr_tail: String::new(),
    }
}

fn elapsed(now: &Instant, since: Instant) -> u64 {
    now.saturating_duration_since(since).as_millis() as u64
}

/// Logs de la ejecucion ya leidos.
#[derive(Debug, Clone, Default)]
pub struct CollectedEvents {
    pub entries: Vec<logs::LogEntry>,
    pub files: Vec<PathBuf>,
}

/// Lee los eventos del robot de **esta** ejecucion.
///
/// Orden de busqueda: directorio nuevo -> fichero anunciado por el CLI ->
/// directorio configurado. En cuanto un camino aporta eventos no se sigue: los
/// demas son respaldos para versiones del CLI que ignoran `--log`.
pub fn collect_events(outcome: &RunOutcome, robot: &str) -> CollectedEvents {
    let since = outcome.started_at - chrono::Duration::seconds(CLOCK_SLACK_SECS);

    let mut collected = collect_from_dir(&outcome.log_dir, robot, since, RUN_LOG_FILES);
    if !collected.entries.is_empty() {
        return collected;
    }

    if let Some(reported) = &outcome.reported_log {
        if reported.is_file() {
            let from_file = collect_from_file(reported, robot, since);
            if !from_file.entries.is_empty() {
                return from_file;
            }
        }
    }

    if let Some(fallback) = &outcome.log_fallback {
        collected = collect_from_dir(fallback, robot, since, FALLBACK_LOG_FILES);
    }
    collected
}

fn collect_from_dir(
    dir: &Path,
    robot: &str,
    since: NaiveDateTime,
    max_files: usize,
) -> CollectedEvents {
    let mut out = CollectedEvents::default();
    for path in logs::recent_files(dir, max_files) {
        let from_file = collect_from_file(&path, robot, since);
        out.entries.extend(from_file.entries);
        out.files.extend(from_file.files);
    }
    // `recent_files` devuelve del mas nuevo al mas viejo; el informe quiere el
    // orden en que ocurrieron.
    out.entries.sort_by_key(|entry| entry.timestamp);
    out
}

fn collect_from_file(path: &Path, robot: &str, since: NaiveDateTime) -> CollectedEvents {
    let Some(entries) = logs::parse_file_cached(path) else {
        return CollectedEvents::default();
    };
    let mut out = CollectedEvents::default();
    for entry in logs::of_robot(entries.as_ref(), robot) {
        if entry.timestamp >= since {
            out.entries.push(entry.clone());
        }
    }
    if !out.entries.is_empty() {
        out.files.push(path.to_path_buf());
    }
    out
}

/// Atribuye los errores de la ejecucion al snapshot vivo de cada instante.
fn attribute_run_errors(
    conn: &rusqlite::Connection,
    robot: &str,
    entries: &[logs::LogEntry],
) -> Vec<RunErrorItem> {
    // Sin snapshots no hay linea ni version que dar; el error sigue siendo
    // evidencia y se conserva con `version: None`.
    let snapshots = analysis::snapshot_views(conn, robot).unwrap_or_default();

    let mut raws = Vec::new();
    for entry in entries.iter().filter(|entry| entry.is_error()) {
        let Some(request) = entry.request.as_ref() else {
            continue;
        };
        raws.push(RawError {
            timestamp: entry.timestamp,
            cmd_id: request.cmd_id.clone(),
            command: request.command.clone(),
            payload: request.payload.clone(),
            message: first_line(&entry.message),
            occurrences: 1,
            signature: signature::normalize(&entry.message),
        });
    }
    if raws.is_empty() {
        return Vec::new();
    }

    let attributions = attribution::attribute(&raws, &snapshots);
    let mut out: Vec<RunErrorItem> = Vec::new();
    for (raw, attribution) in raws.iter().zip(attributions) {
        let line = attribution.line.clone().unwrap_or_default();
        if let Some(item) = out
            .iter_mut()
            .find(|item| item.signature == raw.signature && item.line == line)
        {
            item.occurrences += 1;
            // La version maxima: si la misma firma se repitio en dos versiones,
            // la ultima es la unica que puede seguir viva.
            if attribution.version > item.version {
                item.version = attribution.version;
                item.verdict = attribution.verdict;
            }
        } else {
            out.push(RunErrorItem {
                line,
                command: raw.command.clone(),
                message: raw.message.clone(),
                signature: raw.signature.clone(),
                verdict: attribution.verdict,
                version: attribution.version,
                occurrences: 1,
            });
        }
    }
    out.truncate(MAX_REPORT_ERRORS);
    out
}

/// Primera linea del mensaje, acotada: la traza de Python no es el defecto.
fn first_line(message: &str) -> String {
    let first = message.lines().next().unwrap_or("").trim();
    let mut out: String = first.chars().take(160).collect();
    if first.chars().count() > 160 {
        out.push('…');
    }
    out
}

/// Ruta de un directorio de ejecucion nuevo bajo `base`.
///
/// El nombre empieza por la fecha (`AAAAMMDD-HHMMSS`) para que ordenar por
/// nombre sea ordenar por antiguedad y [`prune_run_dirs`] no necesite metadata.
pub fn new_run_dir(base: &Path, robot: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let stamp = now_local().format("%Y%m%d-%H%M%S");
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    base.join("rocketdebug-runs").join(format!(
        "{stamp}-{}-{seq}-{}",
        std::process::id(),
        slug(robot)
    ))
}

/// Borra los directorios de ejecucion mas viejos, conservando `keep`.
///
/// Devuelve cuantos borro. Es best-effort: un fichero abierto en Windows no se
/// puede borrar y eso no puede tumbar una ejecucion.
pub fn prune_run_dirs(base: &Path, keep: usize) -> usize {
    let root = base.join("rocketdebug-runs");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return 0;
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect();
    dirs.sort();

    let keep = keep.max(1);
    let mut removed = 0;
    let excess = dirs.len().saturating_sub(keep);
    for dir in dirs.into_iter().take(excess) {
        if std::fs::remove_dir_all(&dir).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Nombre de fichero seguro: solo ASCII alfanumerico, `-` y `_`.
fn slug(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if !out.ends_with('_') {
            out.push('_');
        }
        if out.len() >= 40 {
            break;
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "robot".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `true` si `text` es el nombre del proceso de Rocketbot. Aislado para probar
/// la deteccion sin `tasklist`.
///
/// Solo la usa el codigo de Windows, pero se prueba en todas las plataformas:
/// fuera de Windows se marca para que `dead_code` no la reclame.
#[cfg_attr(not(windows), allow(dead_code))]
fn is_rocketbot_process(name: &str) -> bool {
    name.trim_matches('"')
        .trim()
        .eq_ignore_ascii_case("rocketbot.exe")
}

/// Interpreta la salida CSV de `tasklist /FI "IMAGENAME eq rocketbot.exe"`.
///
/// Cuando no hay coincidencias, `tasklist` imprime una linea localizada
/// ("INFO: No tasks are running...") que no empieza por el nombre del proceso y
/// se ignora sola. Igual que [`is_rocketbot_process`]: probada en todas las
/// plataformas, usada solo en Windows.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_tasklist(output: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    for line in output.lines() {
        let mut fields = line.split("\",\"");
        let name = fields.next().unwrap_or("");
        if !is_rocketbot_process(name) {
            continue;
        }
        let pid = fields
            .next()
            .map(|value| value.trim_matches('"'))
            .and_then(|value| value.parse::<u32>().ok());
        if let Some(pid) = pid {
            pids.push(pid);
        }
    }
    pids
}

#[cfg(windows)]
fn rocketbot_pids() -> Vec<u32> {
    let output = Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq rocketbot.exe", "/FO", "CSV", "/NH"])
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(output) => parse_tasklist(&String::from_utf8_lossy(&output.stdout)),
        // Si `tasklist` no esta o falla, no se inventa una deteccion: la
        // verificacion de eventos sigue siendo la red de seguridad.
        Err(_) => Vec::new(),
    }
}

#[cfg(not(windows))]
fn rocketbot_pids() -> Vec<u32> {
    // Studio solo corre en Windows; en otros hosts no hay nada que detectar.
    Vec::new()
}

#[cfg(windows)]
fn kill_tree(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(windows))]
fn kill_tree(pid: u32) {
    if let Ok(child) = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        let _ = child;
    }
}

/// Convierte una ruta relativa en absoluta usando el cwd del proceso.
///
/// No se usa `canonicalize`: resuelve enlaces y exige que el fichero exista, y
/// aqui solo hace falta que el hijo no dependa de su propio `cwd`.
fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn spawn_reader(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        // Se drena hasta EOF aunque solo se conserven los ultimos 64 KiB: si se
        // dejara de leer, el hijo se bloquearia al llenar el pipe y la ejecucion
        // moriria por timeout sin motivo.
        let mut tail: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    tail.extend_from_slice(&chunk[..read]);
                    if tail.len() > MAX_OUTPUT_BYTES {
                        let excess = tail.len() - MAX_OUTPUT_BYTES;
                        tail.drain(..excess);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&tail).into_owned()
    })
}

/// Resuelve el `Log path: ...` que imprime el CLI.
///
/// La ruta suele ser relativa (`logs\261118\logfile...`). Se prueba contra el cwd
/// del proceso y contra el directorio del ejecutable; si ninguna existe, se
/// devuelve la del cwd y `collect_events` la descartara por no ser fichero.
fn reported_log_path(stdout: &str, cwd: &Path, exe: &Path) -> Option<PathBuf> {
    let line = stdout
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with("Log path:"))?;
    let raw = line.split_once(':')?.1.trim();
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        return Some(path);
    }
    let from_cwd = cwd.join(&path);
    if from_cwd.is_file() {
        return Some(from_cwd);
    }
    let from_exe = exe.parent().unwrap_or_else(|| Path::new(".")).join(&path);
    if from_exe.is_file() {
        return Some(from_exe);
    }
    Some(from_cwd)
}

/// Hora local del sistema.
///
/// `chrono` va sin la feature `clock` en todo el workspace (quien necesita la
/// hora la inyecta), y los logs de Rocketbot estan en hora local. En Windows se
/// pregunta al SO con `GetLocalTime`; en el resto se usa UTC, que es lo que
/// usan los tests sinteticos.
#[cfg(windows)]
fn now_local() -> NaiveDateTime {
    use std::os::raw::c_ushort;

    #[repr(C)]
    struct WinSystemTime {
        year: c_ushort,
        month: c_ushort,
        day_of_week: c_ushort,
        day: c_ushort,
        hour: c_ushort,
        minute: c_ushort,
        second: c_ushort,
        milliseconds: c_ushort,
    }

    extern "system" {
        fn GetLocalTime(system_time: *mut WinSystemTime);
    }

    let mut time = WinSystemTime {
        year: 0,
        month: 0,
        day_of_week: 0,
        day: 0,
        hour: 0,
        minute: 0,
        second: 0,
        milliseconds: 0,
    };
    // SAFETY: `GetLocalTime` solo escribe en el puntero; la estructura es
    // `repr(C)` y tiene exactamente los siete campos `WORD` que espera Win32.
    unsafe { GetLocalTime(&mut time) };
    let text = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        time.year, time.month, time.day, time.hour, time.minute, time.second, time.milliseconds
    );
    NaiveDateTime::parse_from_str(&text, "%Y-%m-%d %H:%M:%S%.3f").unwrap_or_default()
}

#[cfg(not(windows))]
fn now_local() -> NaiveDateTime {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|date| date.naive_utc())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_linea_de_comandos_usa_igual_y_no_nodebug() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = tmp.path().join("BBRE2").join("robot.db");
        std::fs::create_dir_all(db.parent().expect("parent")).expect("dir");
        let log_dir = tmp.path().join("logs").join("run-1");
        let config = ExecConfig::new(PathBuf::from("rocketbot.exe"), tmp.path().join("logs"));
        let spec = config.command_spec(&db, "reporteMercadolibre", &log_dir);

        assert_eq!(
            spec.args,
            vec![
                "-start=reporteMercadolibre".to_string(),
                format!("-db={}", db.display()),
                format!("--log={}", log_dir.display()),
            ]
        );
        assert_eq!(spec.cwd, db.parent().expect("parent"));
        assert!(
            !spec.args.iter().any(|arg| arg == "--nodebug"),
            "no se pasa --nodebug: no esta probado que conserve el log de fichero"
        );
    }

    #[test]
    fn una_db_relativa_se_absolutiza_antes_de_lanzar() {
        // El hijo corre con cwd en el directorio de la db; si la ruta llegara
        // relativa, `-db=BBRE2\robot.db` se resolveria contra `BBRE2` y abriria
        // `BBRE2\BBRE2\robot.db`.
        let cwd = std::env::current_dir().expect("cwd");
        let relative = Path::new("BBRE2").join("robot.db");
        let config = ExecConfig::new(PathBuf::from("rocketbot.exe"), cwd.join("logs"));
        let spec = config.command_spec(&relative, "demo", Path::new("logs"));

        let esperada = cwd.join(&relative);
        assert_eq!(spec.args[1], format!("-db={}", esperada.display()));
        assert_eq!(spec.cwd, cwd.join("BBRE2"));
        assert_eq!(
            spec.args[2],
            format!("--log={}", cwd.join("logs").display())
        );
    }

    #[test]
    fn el_timeout_se_acota_entre_cinco_y_novecientos() {
        assert_eq!(timeout_from_secs(1), Duration::from_secs(MIN_TIMEOUT_SECS));
        assert_eq!(
            timeout_from_secs(10_000),
            Duration::from_secs(MAX_TIMEOUT_SECS)
        );
        assert_eq!(timeout_from_secs(120), Duration::from_secs(120));
    }

    #[test]
    fn el_slug_no_deja_escapes_de_ruta() {
        assert_eq!(slug("reporteMercadolibre"), "reporteMercadolibre");
        assert_eq!(slug(r"..\..\evil name"), "evil_name");
        assert_eq!(slug(" "), "robot");
        assert!(slug(&"x".repeat(100)).len() <= 40);
    }

    #[test]
    fn tasklist_extrae_los_pids_del_nombre_exacto() {
        let csv = concat!(
            "\"rocketbot.exe\",\"10876\",\"Console\",\"1\",\"120.000 K\"\n",
            "\"rocketbot-helper.exe\",\"999\",\"Console\",\"1\",\"1.000 K\"\n",
            "INFO: No tasks are running which match the specified criteria.\n",
            "\"ROCKETBOT.EXE\",\"42\",\"Console\",\"1\",\"1.000 K\"\n",
        );
        assert_eq!(parse_tasklist(csv), vec![10876, 42]);
    }

    #[test]
    fn tasklist_sin_coincidencias_devuelve_vacio() {
        let no_tasks = "INFO: No tasks are running which match the specified criteria.\r\n";
        assert!(parse_tasklist(no_tasks).is_empty());
        assert!(parse_tasklist("").is_empty());
    }

    #[test]
    fn el_nombre_del_proceso_no_depende_de_mayusculas() {
        assert!(is_rocketbot_process("rocketbot.exe"));
        assert!(is_rocketbot_process("\"Rocketbot.exe\""));
        assert!(!is_rocketbot_process("rocketbot-helper.exe"));
        assert!(!is_rocketbot_process("python.exe"));
    }

    #[test]
    fn el_log_anunciado_se_resuelve_contra_el_cwd_o_el_exe() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path().join("proyecto");
        let exe_dir = tmp.path().join("rocketbot");
        std::fs::create_dir_all(cwd.join("logs").join("261118")).expect("cwd logs");
        std::fs::create_dir_all(&exe_dir).expect("exe dir");
        std::fs::write(cwd.join("logs").join("261118").join("a.log"), "x").expect("log en cwd");

        // Rocketbot es Windows y anuncia la ruta con `\`; en el resto de hosts
        // el test usa el separador de la plataforma para probar la resolucion.
        let sep = std::path::MAIN_SEPARATOR;
        let stdout = format!("banner\nLog path:  logs{sep}261118{sep}a.log\nfin\n");
        let found = reported_log_path(&stdout, &cwd, &exe_dir.join("rocketbot.exe"))
            .expect("hay log anunciado");
        assert_eq!(found, cwd.join("logs").join("261118").join("a.log"));

        std::fs::create_dir_all(exe_dir.join("logs").join("261118")).expect("exe logs");
        std::fs::write(exe_dir.join("logs").join("261118").join("a.log"), "x").expect("log en exe");
        let found = reported_log_path(
            &stdout,
            &tmp.path().join("sin-logs"),
            &exe_dir.join("rocketbot.exe"),
        )
        .expect("hay log anunciado");
        assert_eq!(found, exe_dir.join("logs").join("261118").join("a.log"));

        assert!(
            reported_log_path("sin linea de log", &cwd, &exe_dir.join("rocketbot.exe")).is_none()
        );
    }

    #[test]
    fn el_timeout_no_se_elige_por_debajo_del_minimo() {
        // La fachada valida el argumento, pero el motor repite la regla: un
        // consumidor que la llame directo tampoco puede poner 0.
        assert_eq!(timeout_from_secs(0).as_secs(), MIN_TIMEOUT_SECS);
        assert_eq!(timeout_from_secs(-7).as_secs(), MIN_TIMEOUT_SECS);
    }

    #[test]
    fn el_reader_drena_todo_y_conserva_la_cola() {
        // Si el lector dejara de leer al llegar al tope, un robot hablador
        // bloquearia el pipe y la ejecucion moriria por timeout.
        let mut data = vec![b'a'; MAX_OUTPUT_BYTES];
        data.extend_from_slice(b"TAIL");
        let texto = spawn_reader(std::io::Cursor::new(data))
            .join()
            .expect("el lector termina");
        assert_eq!(texto.len(), MAX_OUTPUT_BYTES);
        assert!(
            texto.ends_with("TAIL"),
            "se conserva el final, no el principio"
        );
    }

    #[test]
    fn conserva_solo_los_directorios_de_ejecucion_mas_nuevos() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("rocketdebug-runs");
        for name in [
            "20260101-000000-1-a",
            "20260102-000000-2-b",
            "20260103-000000-3-c",
        ] {
            std::fs::create_dir_all(root.join(name)).expect("dir");
        }
        assert_eq!(prune_run_dirs(tmp.path(), 2), 1);
        let mut left: Vec<String> = std::fs::read_dir(&root)
            .expect("leo")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, vec!["20260102-000000-2-b", "20260103-000000-3-c"]);
    }
}
