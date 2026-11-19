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

pub const DEFAULT_EXE: &str = r"C:\Rocketbot\rocketbot.exe";

pub const DEFAULT_TIMEOUT_SECS: u64 = 180;

pub const MIN_TIMEOUT_SECS: u64 = 5;
pub const MAX_TIMEOUT_SECS: u64 = 900;

pub fn timeout_from_secs(secs: i64) -> Duration {
    let secs = secs.clamp(MIN_TIMEOUT_SECS as i64, MAX_TIMEOUT_SECS as i64);
    Duration::from_secs(secs as u64)
}

pub const KEEP_RUN_DIRS: usize = 20;

pub const MAX_REPORT_ERRORS: usize = 50;

const MAX_OUTPUT_BYTES: usize = 64 * 1024;

const FALLBACK_LOG_FILES: usize = 20;

const RUN_LOG_FILES: usize = 64;

const CLOCK_SLACK_SECS: i64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StudioProbe {
    System,
    Closed,
}

#[derive(Debug, Clone)]
pub struct ExecConfig {
    pub exe: PathBuf,
    pub logs_dir: PathBuf,
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

    pub fn command_spec(&self, db: &Path, robot: &str, log_dir: &Path) -> CommandSpec {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

impl CommandSpec {
    pub fn display(&self) -> String {
        let mut out = format!("\"{}\"", self.program.display());
        for arg in &self.args {
            out.push(' ');
            out.push_str(arg);
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct RunRequest {
    pub robot: String,
    pub db: PathBuf,
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

#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration: Duration,
    pub started_at: NaiveDateTime,
    pub log_dir: PathBuf,
    pub log_fallback: Option<PathBuf>,
    pub reported_log: Option<PathBuf>,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub spawn_error: Option<String>,
    pub command: String,
}

impl RunOutcome {
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

pub trait Runner: Send + Sync {
    fn studio_open(&self) -> Vec<u32> {
        Vec::new()
    }

    fn run(&self, request: &RunRequest) -> RunOutcome;
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunFailure {
    StudioOpen(Vec<u32>),
    Spawn(String),
    Timeout,
    Exit(Option<i32>),
    NoEvents,
}

impl RunFailure {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunErrorItem {
    pub line: String,
    pub command: String,
    pub message: String,
    pub signature: String,
    pub verdict: Verdict,
    pub version: Option<i64>,
    pub occurrences: usize,
}

#[derive(Debug, Clone)]
pub struct RunReport {
    pub robot: String,
    pub db_label: String,
    pub ok: bool,
    pub failure: Option<RunFailure>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    pub queue_wait_ms: u64,
    pub events: usize,
    pub errors: Vec<RunErrorItem>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerRunInfo {
    pub robot: String,
    pub db_label: String,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone)]
pub struct BrokerLast {
    pub robot: String,
    pub ok: bool,
    pub failure: Option<RunFailure>,
    pub at: Instant,
}

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

#[derive(Debug, Clone, Default)]
pub struct CollectedEvents {
    pub entries: Vec<logs::LogEntry>,
    pub files: Vec<PathBuf>,
}

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

fn attribute_run_errors(
    conn: &rusqlite::Connection,
    robot: &str,
    entries: &[logs::LogEntry],
) -> Vec<RunErrorItem> {
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

fn first_line(message: &str) -> String {
    let first = message.lines().next().unwrap_or("").trim();
    let mut out: String = first.chars().take(160).collect();
    if first.chars().count() > 160 {
        out.push('…');
    }
    out
}

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

#[cfg_attr(not(windows), allow(dead_code))]
fn is_rocketbot_process(name: &str) -> bool {
    name.trim_matches('"')
        .trim()
        .eq_ignore_ascii_case("rocketbot.exe")
}

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
        Err(_) => Vec::new(),
    }
}

#[cfg(not(windows))]
fn rocketbot_pids() -> Vec<u32> {
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

#[cfg(windows)]
#[repr(C)]
struct WinSystemTime {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetLocalTime(system_time: *mut WinSystemTime);
}

#[cfg(windows)]
fn now_local() -> NaiveDateTime {
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
        assert_eq!(timeout_from_secs(0).as_secs(), MIN_TIMEOUT_SECS);
        assert_eq!(timeout_from_secs(-7).as_secs(), MIN_TIMEOUT_SECS);
    }

    #[test]
    fn el_reader_drena_todo_y_conserva_la_cola() {
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
