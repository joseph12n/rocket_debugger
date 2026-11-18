//! Las tres tools de la fachada y su despacho.
//!
//! Cada tool es una funcion pequena; lo que comparten (resolucion de la db,
//! lectura de argumentos, recorte de la seccion de errores) esta en ayudantes
//! para que el despacho no sea un `match` monolitico.
//!
//! # Formato de respuesta
//!
//! Todas devuelven `{"content":[{"type":"text","text":"..."}],"isError":bool}`.
//! Un fallo de negocio (robot inexistente) es `isError: true` con el mensaje del
//! motor ya redactado: el agente tiene que poder leerlo, no recibir un error de
//! protocolo que su cliente puede tratar como caida del servidor.

use std::fmt::Write as _;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use rocketdebug_core::attribution::Verdict;
use rocketdebug_core::db;
use rocketdebug_core::exec::{self, BrokerStatus, RunFailure, RunReport, RunRequest};
use rocketdebug_core::logs;
use rocketdebug_core::redact;
use rocketdebug_core::{build_from_conn_with_logs, Error};
use serde_json::{json, Map, Value};

use crate::Settings;

pub const LIST_ROBOTS: &str = "list_robots";
pub const FLOW_PACK: &str = "flow_pack";
pub const ROBOT_ERRORS: &str = "robot_errors";
pub const FLOW_DIFF: &str = "flow_diff";
pub const RUN_ROBOT: &str = "run_robot";
pub const RUN_STATUS: &str = "run_status";
pub const PROBLEMS_SEARCH: &str = "problems_search";
pub const PROBLEMS_RECORD: &str = "problems_record";
pub const PROBLEMS_FIX: &str = "problems_fix";
pub const UI_RENDER: &str = "ui_render";

/// Errores distintos que se pintan en la respuesta de `run_robot`.
const MAX_ERRORS_IN_TEXT: usize = 10;

/// Resultado de una tool, ya listo para el `result` de JSON-RPC.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub text: String,
    /// `true` = fallo de negocio (la tool se ejecuto, el robot no esta). No es
    /// un error de protocolo: el agente lee el mensaje y decide.
    pub is_error: bool,
}

impl ToolOutput {
    fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    fn business_error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "content": [{"type": "text", "text": self.text}],
            "isError": self.is_error,
        })
    }
}

/// Fallos que si son de protocolo.
#[derive(Debug, Clone)]
pub enum CallError {
    /// La llamada esta mal formada: falta un argumento o su tipo no encaja.
    InvalidParams(String),
    /// La tool se rompio por dentro (panic del motor o fallo inesperado).
    Internal(String),
}

/// Catalogo de tools para `tools/list`.
///
/// Los `inputSchema` son JSON Schema de objeto, con `properties` y `required`:
/// es lo que un cliente usa para construir la llamada sin adivinar.
pub fn descriptors() -> Value {
    json!({
        "tools": [
            {
                "name": LIST_ROBOTS,
                "description": "Lista los robots de robot.db (solo lectura): nombre, versiones y \
                                ultimo snapshot. Sirve para descubrir que hay antes de pedir un pack.",
                "inputSchema": object_schema(json!({"db": db_property()}), &[]),
            },
            {
                "name": FLOW_PACK,
                "description": "Devuelve el context pack en markdown de un robot: cabecera, \
                                hallazgos, contrato de subrobots, errores, variables y flujo. Es el \
                                documento con el que se diagnostica; sustituye al JSON crudo del robot.",
                "inputSchema": object_schema(
                    json!({
                        "robot": robot_property(),
                        "db": db_property(),
                        "snapshot": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "bots.id del snapshot; por defecto, el ultimo guardado.",
                        },
                    }),
                    &["robot"],
                ),
            },
            {
                "name": ROBOT_ERRORS,
                "description": "Devuelve solo la seccion de errores del robot, ya atribuida a la \
                                version que estaba viva: vivos, payload cambiado, historicos, modulo y \
                                ruido. Cuesta una fraccion de lo que cuesta el pack entero.",
                "inputSchema": object_schema(
                    json!({"robot": robot_property(), "db": db_property()}),
                    &["robot"],
                ),
            },
            {
                "name": FLOW_DIFF,
                "description": "Diff estructural entre dos snapshots del mismo robot, emparejando \
                                por id de nodo. Responde 'que cambio entre la v26 y la v27': es lo \
                                que cierra un diagnostico cuando ya se sabe que fallo pero no que se \
                                toco para arreglarlo.",
                "inputSchema": object_schema(
                    json!({
                        "robot": robot_property(),
                        "from": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "bots.id del snapshot anterior.",
                        },
                        "to": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "bots.id del snapshot nuevo.",
                        },
                        "db": db_property(),
                    }),
                    &["robot", "from", "to"],
                ),
            },
            {
                "name": RUN_ROBOT,
                "description": "Ejecuta un robot por el broker de ejecucion: una ejecucion a la \
                                vez en el workspace, deteccion de Studio abierto antes de lanzar y \
                                verificacion de que hubo eventos del robot. Un exit_code 0 sin \
                                eventos es fallo, nunca exito. Devuelve eventos, errores \
                                atribuidos al snapshot vivo y duracion. Puede tardar (timeout por \
                                defecto 180 s); usa run_status para ver la cola.",
                "inputSchema": object_schema(
                    json!({
                        "robot": robot_property(),
                        "db": db_property(),
                        "timeout": {
                            "type": "integer",
                            "minimum": 5,
                            "maximum": 900,
                            "description": "Segundos maximos de ejecucion (por defecto 180).",
                        },
                    }),
                    &["robot"],
                ),
            },
            {
                "name": RUN_STATUS,
                "description": "Estado del broker de ejecucion: que robot se esta ejecutando ahora, \
                                cuales esperan turno y como termino la ultima ejecucion. Sirve para \
                                mirar la cola mientras un run_robot esta bloqueado.",
                "inputSchema": object_schema(json!({}), &[]),
            },
            {
                "name": PROBLEMS_SEARCH,
                "description": "Busca en el registro historico de problemas: que firmas ya se \
                                vieron, cuantas veces, en que robots y si alguien las arreglo. Es \
                                la memoria que evita redescubrir el mismo fallo en cada sesion.",
                "inputSchema": object_schema(
                    json!({
                        "query": {
                            "type": "string",
                            "description": "Subcadena a buscar en la firma o en el nombre de robot. Vacio = todo.",
                        },
                        "limit": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 200,
                            "description": "Maximo de firmas a devolver (por defecto 20).",
                        },
                    }),
                    &[],
                ),
            },
            {
                "name": PROBLEMS_RECORD,
                "description": "Guarda en el registro los errores que el motor ve ahora en un \
                                robot (vivos, historicos y defectos de modulo; el ruido del motor no \
                                se registra). Es idempotente: repetir el barrido no infla los contadores.",
                "inputSchema": object_schema(
                    json!({"robot": robot_property(), "db": db_property()}),
                    &["robot"],
                ),
            },
            {
                "name": PROBLEMS_FIX,
                "description": "Registra como se cerro un problema: el diff entre la version que \
                                fallaba y la arreglada, mas una nota del agente. Convierte un \
                                diagnostico en conocimiento reutilizable.",
                "inputSchema": object_schema(
                    json!({
                        "robot": robot_property(),
                        "signature": {
                            "type": "string",
                            "description": "Firma normalizada del problema (tal como la devuelve problems_search).",
                        },
                        "from": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "bots.id de la version que fallaba.",
                        },
                        "to": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "bots.id de la version arreglada.",
                        },
                        "note": {
                            "type": "string",
                            "description": "Que se probo, que no funciono, efectos laterales.",
                        },
                        "db": db_property(),
                    }),
                    &["robot", "signature", "from", "to"],
                ),
            },
            {
                "name": UI_RENDER,
                "description": "Devuelve el enlace que abre la UI en un robot concreto. Sirve para \
                                que el agente mande al humano a mirar en vez de pegar un pack de 2k \
                                tokens en el chat: el agente ordena, el humano mira. Requiere que \
                                el servidor HTTP este arrancado y que se haya configurado su URL \
                                en ROCKETDEBUG_UI_URL.",
                "inputSchema": object_schema(
                    json!({
                        "robot": robot_property(),
                        "line": {
                            "type": "string",
                            "description": "Linea de Studio a la que se quiere llevar al humano. Queda en el enlace; la UI actual abre el robot.",
                        },
                        "db": db_property(),
                    }),
                    &["robot"],
                ),
            },
        ]
    })
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn robot_property() -> Value {
    json!({
        "type": "string",
        "description": "Nombre del robot tal como aparece en la tabla `bots`.",
    })
}

fn db_property() -> Value {
    json!({
        "type": "string",
        "description": format!(
            "Ruta a un robot.db. Por defecto ROCKETBOT_DB o {}.",
            db::DEFAULT_DB
        ),
    })
}

/// Despacha una tool por nombre.
///
/// El `catch_unwind` es la red de seguridad del servidor: un bug del motor sobre
/// un robot raro se convierte en `-32603` y el agente pierde una llamada, no la
/// sesion entera (el proceso sigue leyendo lineas).
pub fn call(name: &str, arguments: &Value, settings: &Settings) -> Result<ToolOutput, CallError> {
    match catch_unwind(AssertUnwindSafe(|| dispatch(name, arguments, settings))) {
        Ok(result) => result,
        Err(_) => Err(CallError::Internal(format!(
            "la tool `{name}` aborto (panic del motor); el servidor sigue vivo"
        ))),
    }
}

fn dispatch(name: &str, arguments: &Value, settings: &Settings) -> Result<ToolOutput, CallError> {
    match name {
        LIST_ROBOTS => list_robots(arguments, settings),
        FLOW_PACK => flow_pack(arguments, settings),
        ROBOT_ERRORS => robot_errors(arguments, settings),
        FLOW_DIFF => flow_diff(arguments, settings),
        RUN_ROBOT => run_robot(arguments, settings),
        RUN_STATUS => run_status(arguments, settings),
        PROBLEMS_SEARCH => problems_search(arguments, settings),
        PROBLEMS_RECORD => problems_record(arguments, settings),
        PROBLEMS_FIX => problems_fix(arguments, settings),
        UI_RENDER => ui_render(arguments),
        other => Err(CallError::InvalidParams(format!(
            "tool desconocida: `{other}`"
        ))),
    }
}

/// `list_robots`: que hay en la db. Texto, no JSON: el agente lo lee igual y no
/// paga las claves repetidas.
fn list_robots(arguments: &Value, settings: &Settings) -> Result<ToolOutput, CallError> {
    let args = Args::new(arguments)?;
    let db_path = resolve_db(args.opt_str("db")?, settings);

    match robots_text(&db_path) {
        Ok(text) => Ok(ToolOutput::ok(text)),
        Err(source) => Ok(ToolOutput::business_error(describe_failure(
            &source, &db_path,
        ))),
    }
}

/// `flow_pack`: el entregable real.
fn flow_pack(arguments: &Value, settings: &Settings) -> Result<ToolOutput, CallError> {
    let args = Args::new(arguments)?;
    let robot = args.required_str("robot")?;
    let db_path = resolve_db(args.opt_str("db")?, settings);
    let snapshot = args.opt_i64("snapshot")?;

    match pack_markdown(&db_path, &robot, snapshot, settings) {
        Ok(markdown) => Ok(ToolOutput::ok(markdown)),
        Err(source) => Ok(ToolOutput::business_error(describe_failure(
            &source, &db_path,
        ))),
    }
}

/// `flow_diff`: que cambio entre dos snapshots.
///
/// Es la tool que cierra el circulo del diagnostico: el pack dice que falla y
/// donde; el diff dice que se toco entre la version que fallaba y la siguiente.
fn flow_diff(arguments: &Value, settings: &Settings) -> Result<ToolOutput, CallError> {
    let args = Args::new(arguments)?;
    let robot = args.required_str("robot")?;
    let db_path = resolve_db(args.opt_str("db")?, settings);
    let from = args.opt_i64("from")?.ok_or_else(|| {
        CallError::InvalidParams("falta `from` (bots.id del snapshot anterior)".to_string())
    })?;
    let to = args.opt_i64("to")?.ok_or_else(|| {
        CallError::InvalidParams("falta `to` (bots.id del snapshot nuevo)".to_string())
    })?;

    match diff_markdown(&db_path, &robot, from, to) {
        Ok(texto) => Ok(ToolOutput::ok(texto)),
        Err(source) => Ok(ToolOutput::business_error(describe_failure(
            &source, &db_path,
        ))),
    }
}

fn diff_markdown(db_path: &Path, robot: &str, from: i64, to: i64) -> Result<String, Error> {
    let antes = nodes_of(db_path, robot, from)?;
    let despues = nodes_of(db_path, robot, to)?;
    Ok(rocketdebug_core::render::render_changes(
        &rocketdebug_core::diff::diff_nodes(&antes, &despues),
    ))
}

/// Nodos de un snapshot concreto. Abre su propia conexion para no arrastrar
/// `rusqlite` como dependencia del crate: la fachada solo conoce el motor.
fn nodes_of(
    db_path: &Path,
    robot: &str,
    snapshot: i64,
) -> Result<Vec<rocketdebug_core::tree::FlatNode>, Error> {
    let conn = db::open_readonly(db_path)?;
    let snap = db::load_snapshot(&conn, robot, snapshot)?;
    let root = db::project_root(&snap.project);
    let commands = root
        .get("commands")
        .and_then(Value::as_array)
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    Ok(rocketdebug_core::tree::flatten(commands))
}

// ---------------------------------------------------------------- ejecucion

/// Texto cuando no hay `rocketbot.exe` configurado.
fn sin_broker() -> String {
    "la ejecucion no esta configurada: define ROCKETBOT_EXE con la ruta a \
     rocketbot.exe y reinicia el servidor. Con Studio abierto el runner CLI delega \
     en la instancia existente y no ejecuta; el broker lo detecta y se niega antes \
     de lanzar."
        .to_string()
}

/// Comprueba que el robot existe **antes** de ocupar un turno de la cola.
fn ensure_robot(db_path: &Path, robot: &str) -> Result<(), Error> {
    let conn = db::open_readonly(db_path)?;
    let robots = db::list_robots(&conn)?;
    if robots.iter().any(|summary| summary.name == robot) {
        Ok(())
    } else {
        Err(Error::RobotNotFound(robot.to_string()))
    }
}

/// `run_robot`: el agente pide, el broker serializa.
fn run_robot(arguments: &Value, settings: &Settings) -> Result<ToolOutput, CallError> {
    let args = Args::new(arguments)?;
    let robot = args.required_str("robot")?;
    let db_path = resolve_db(args.opt_str("db")?, settings);
    let Some(exec_settings) = &settings.exec else {
        return Ok(ToolOutput::business_error(sin_broker()));
    };

    let timeout = match args.opt_i64("timeout")? {
        Some(seconds) => exec::timeout_from_secs(seconds),
        None => exec_settings.default_timeout,
    };

    // Un nombre mal escrito no debe esperar a que termine otra ejecucion para
    // descubrirse: se valida antes de encolar.
    if let Err(source) = ensure_robot(&db_path, &robot) {
        return Ok(ToolOutput::business_error(describe_failure(
            &source, &db_path,
        )));
    }

    let report = exec_settings.broker.execute(RunRequest {
        robot,
        db: db_path,
        timeout,
    });
    let text = render_run(&report);
    if report.ok {
        Ok(ToolOutput::ok(text))
    } else {
        Ok(ToolOutput::business_error(text))
    }
}

/// `run_status`: la cola visible mientras un `run_robot` esta bloqueado.
fn run_status(arguments: &Value, settings: &Settings) -> Result<ToolOutput, CallError> {
    let _ = Args::new(arguments)?;
    let Some(exec_settings) = &settings.exec else {
        return Ok(ToolOutput::business_error(sin_broker()));
    };
    Ok(ToolOutput::ok(render_status(
        &exec_settings.broker.status(),
    )))
}

fn render_run(report: &RunReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# run {}  ({})", report.robot, report.db_label);
    let _ = writeln!(out, "ok: {}", report.ok);
    if let Some(failure) = &report.failure {
        let _ = writeln!(
            out,
            "motivo: {}",
            clip(&redact::mask_user_paths(&failure.explain()), 400)
        );
    }
    let exit = report
        .exit_code
        .map_or_else(|| "sin codigo".to_string(), |code| code.to_string());
    let _ = writeln!(
        out,
        "exit: {exit} | timeout: {} | duracion: {:.1} s | cola: {:.1} s",
        if report.timed_out { "si" } else { "no" },
        report.duration_ms as f64 / 1000.0,
        report.queue_wait_ms as f64 / 1000.0
    );
    let _ = writeln!(out, "eventos del robot: {}", report.events);
    if let Some(id) = report.snapshot_id {
        let at = report
            .snapshot_at
            .map(|date| date.to_string())
            .unwrap_or_default();
        let _ = writeln!(out, "snapshot vivo: {id} ({at})");
    }
    if !report.log_files.is_empty() {
        let _ = writeln!(out, "logs: {} fichero(s)", report.log_files.len());
    }

    if !report.errors.is_empty() {
        let _ = writeln!(out, "\n## Errores ({} distintos)", report.errors.len());
        for error in report.errors.iter().take(MAX_ERRORS_IN_TEXT) {
            let line = if error.line.is_empty() {
                "L-".to_string()
            } else {
                format!("L{}", error.line)
            };
            let version = error
                .version
                .map_or_else(String::new, |version| format!(" v{version}"));
            let times = if error.occurrences > 1 {
                format!(" x{}", error.occurrences)
            } else {
                String::new()
            };
            let message = clip(&redact::mask_user_paths(&error.message), 140);
            let _ = writeln!(
                out,
                "- {line} {} [{}{version}]{times}: {message}",
                error.command,
                verdict_label(error.verdict)
            );
        }
        if report.errors.len() > MAX_ERRORS_IN_TEXT {
            let _ = writeln!(
                out,
                "… y {} errores distintos mas",
                report.errors.len() - MAX_ERRORS_IN_TEXT
            );
        }
    } else if report.ok {
        let _ = writeln!(out, "errores: ninguno");
    }

    if !report.ok {
        if let Some(hint) = output_hint(report) {
            let _ = writeln!(out, "\nsalida: {hint}");
        }
    }
    out.trim_end().to_string()
}

fn verdict_label(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Vivo => "vivo",
        Verdict::PayloadCambiado => "payload cambiado",
        Verdict::Historico => "historico",
        Verdict::Desconocido => "sin atribuir",
    }
}

/// Ultimas lineas utiles de la salida del runner, ya enmascaradas.
///
/// El caso tipico es el aviso de licencia de produccion: explica por que un
/// `exit_code 0` no ejecuto nada. Se filtran las lineas de asteriscos del banner
/// para que no se coman el unico dato que importa.
fn output_hint(report: &RunReport) -> Option<String> {
    let source = if report.stderr_tail.trim().is_empty() {
        &report.stdout_tail
    } else {
        &report.stderr_tail
    };
    let lines: Vec<&str> = source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.trim_matches('*').trim().is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }
    let tail = lines
        .iter()
        .rev()
        .take(3)
        .rev()
        .copied()
        .collect::<Vec<_>>()
        .join(" / ");
    Some(clip(&redact::mask_user_paths(&tail), 400))
}

fn render_status(status: &BrokerStatus) -> String {
    let mut out = String::new();
    match &status.running {
        Some(running) => {
            let _ = writeln!(
                out,
                "broker: ocupado — {} ({}) desde hace {:.1} s",
                running.robot,
                running.db_label,
                running.elapsed_ms as f64 / 1000.0
            );
        }
        None => {
            let _ = writeln!(out, "broker: libre");
        }
    }
    if !status.queue.is_empty() {
        let _ = writeln!(out, "en cola ({}):", status.queue.len());
        for (index, queued) in status.queue.iter().enumerate() {
            let _ = writeln!(
                out,
                "  {}. {} ({}) esperando {:.1} s",
                index + 1,
                queued.robot,
                queued.db_label,
                queued.elapsed_ms as f64 / 1000.0
            );
        }
    }
    if let Some(last) = &status.last {
        let result = if last.ok {
            "ok".to_string()
        } else {
            last.failure
                .as_ref()
                .map(RunFailure::explain)
                .unwrap_or_else(|| "fallo".to_string())
        };
        let _ = writeln!(
            out,
            "ultima: {} — {} hace {:.1} s",
            last.robot,
            clip(&redact::mask_user_paths(&result), 140),
            last.at.elapsed().as_secs_f64()
        );
    }
    out.trim_end().to_string()
}

fn clip(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

// ---------------------------------------------------------------- registro

/// Texto que se devuelve cuando no hay registro configurado.
fn sin_registro() -> String {
    "no hay registro de problemas configurado: define ROCKETDEBUG_REGISTRY (o \
     ROCKETDEBUG_HOME) y vuelve a arrancar el servidor"
        .to_string()
}

fn abrir_registro(settings: &Settings) -> Result<rocketdebug_core::registry::Registry, String> {
    let path = settings.registry.as_deref().ok_or_else(sin_registro)?;
    rocketdebug_core::registry::Registry::open(path)
        .map_err(|e| format!("no pude abrir el registro {}: {e}", path.display()))
}

/// Hora del sello de una observacion.
///
/// `chrono` va sin la feature `clock` en todo el workspace, asi que el reloj lo
/// pone quien llama; aqui se toma del sistema una sola vez por peticion.
fn ahora() -> chrono::NaiveDateTime {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|d| d.naive_utc())
        .unwrap_or_default()
}

fn render_problems(stats: &[rocketdebug_core::registry::SignatureStats]) -> String {
    let mut o = String::new();
    if stats.is_empty() {
        return "sin coincidencias en el registro\n".to_string();
    }
    let _ = writeln!(o, "{} firmas en el registro", stats.len());
    for s in stats {
        let _ = writeln!(o);
        let _ = writeln!(o, "{}", s.signature);
        let _ = writeln!(
            o,
            "  {} avistamientos | {} .. {} | {} fixes",
            s.occurrences, s.first_seen, s.last_seen, s.fixes
        );
        let robots = if s.robots.is_empty() {
            "(ninguno)".to_string()
        } else {
            s.robots.join(", ")
        };
        let _ = writeln!(o, "  robots: {robots}");
    }
    o
}

/// `problems_search`: que se ha visto ya.
fn problems_search(arguments: &Value, settings: &Settings) -> Result<ToolOutput, CallError> {
    let args = Args::new(arguments)?;
    let query = args.opt_str("query")?.unwrap_or_default();
    let limit = args.opt_i64("limit")?.unwrap_or(20).clamp(1, 200) as usize;

    let registro = match abrir_registro(settings) {
        Ok(r) => r,
        Err(texto) => return Ok(ToolOutput::business_error(texto)),
    };
    match registro.search(&query, limit) {
        Ok(stats) => Ok(ToolOutput::ok(render_problems(&stats))),
        Err(e) => Ok(ToolOutput::business_error(format!(
            "no pude consultar el registro: {e}"
        ))),
    }
}

/// `problems_record`: guardar lo que el motor ve ahora.
///
/// Es la unica tool que escribe algo, y escribe **su** fichero: el registro.
/// `robot.db` se sigue abriendo en solo lectura.
fn problems_record(arguments: &Value, settings: &Settings) -> Result<ToolOutput, CallError> {
    let args = Args::new(arguments)?;
    let robot = args.required_str("robot")?;
    let db_path = resolve_db(args.opt_str("db")?, settings);

    let mut registro = match abrir_registro(settings) {
        Ok(r) => r,
        Err(texto) => return Ok(ToolOutput::business_error(texto)),
    };
    let conn = match db::open_readonly(&db_path) {
        Ok(c) => c,
        Err(source) => {
            return Ok(ToolOutput::business_error(describe_failure(
                &source, &db_path,
            )))
        }
    };
    let label = db_label(&db_path);
    let log_source = settings
        .logs
        .as_ref()
        .map(|(dir, max_files)| (dir.as_path(), *max_files));
    let analysis =
        match rocketdebug_core::analysis::analyse_with_logs(&conn, &label, &robot, log_source) {
            Ok(a) => a,
            Err(source) => {
                return Ok(ToolOutput::business_error(describe_failure(
                    &source, &db_path,
                )))
            }
        };

    // Hora del snapshot observado, no del barrido: con el reloj, dos pasadas de la
    // misma version contarian como dos avistamientos.
    let visto_en = db::latest_created_at(&conn, &robot)
        .ok()
        .flatten()
        .unwrap_or_else(ahora);
    let mut vistas = 0usize;
    let mut nuevas = 0usize;
    let mut omitidas = 0usize;
    let mut fallos: Vec<String> = Vec::new();
    // El ruido del motor no se registra: no es un defecto del robot y llenaria el
    // historial de firmas que nadie va a arreglar.
    for item in analysis
        .errors
        .live
        .iter()
        .chain(analysis.errors.payload_changed.iter())
        .chain(analysis.errors.historical.iter())
        .chain(analysis.errors.platform.iter())
    {
        vistas += 1;
        // Los mensajes que el propio robot escribe con `log_message` no son firmas
        // de defecto: nadie los va a "arreglar" y ensucian el registro.
        if item.command == "log_message" {
            omitidas += 1;
            continue;
        }
        let observacion = rocketdebug_core::registry::Observation {
            signature: item.signature.clone(),
            robot: robot.clone(),
            db_label: label.clone(),
            version: item.version,
            line: item.line.clone(),
            message: item.message.clone(),
            seen_at: visto_en,
        };
        match registro.record_observation(&observacion) {
            Ok(true) => nuevas += 1,
            Ok(false) => {}
            Err(e) => fallos.push(e.to_string()),
        }
    }

    let mut o = String::new();
    let _ = writeln!(o, "robot: {robot} ({label})");
    let _ = writeln!(
        o,
        "{vistas} errores vistos, {nuevas} observaciones nuevas (idempotente: repetir no infla)"
    );
    if omitidas > 0 {
        let _ = writeln!(
            o,
            "{omitidas} mensajes de log_message omitidos: son del robot, no defectos"
        );
    }
    if !fallos.is_empty() {
        let _ = writeln!(o, "avisos: {}", fallos.join("; "));
    }
    Ok(ToolOutput::ok(o))
}

/// `problems_fix`: convertir un diagnostico en conocimiento.
fn problems_fix(arguments: &Value, settings: &Settings) -> Result<ToolOutput, CallError> {
    let args = Args::new(arguments)?;
    let robot = args.required_str("robot")?;
    let signature = args.required_str("signature")?;
    let db_path = resolve_db(args.opt_str("db")?, settings);
    let from = args.opt_i64("from")?.ok_or_else(|| {
        CallError::InvalidParams("falta `from` (version que fallaba)".to_string())
    })?;
    let to = args
        .opt_i64("to")?
        .ok_or_else(|| CallError::InvalidParams("falta `to` (version arreglada)".to_string()))?;
    let note = args.opt_str("note")?.unwrap_or_default();

    let mut registro = match abrir_registro(settings) {
        Ok(r) => r,
        Err(texto) => return Ok(ToolOutput::business_error(texto)),
    };
    let detalle = match diff_markdown(&db_path, &robot, from, to) {
        Ok(t) => t,
        Err(source) => {
            return Ok(ToolOutput::business_error(describe_failure(
                &source, &db_path,
            )))
        }
    };
    let summary = detalle
        .lines()
        .next()
        .unwrap_or("sin cambios")
        .trim()
        .to_string();

    let fix = rocketdebug_core::registry::FixRecord {
        signature: signature.clone(),
        robot: robot.clone(),
        from_version: Some(from),
        to_version: Some(to),
        summary: summary.clone(),
        detail: detalle,
        note,
        recorded_at: ahora(),
    };
    match registro.record_fix(&fix) {
        Ok(id) => Ok(ToolOutput::ok(format!(
            "fix #{id} registrado: {signature} en {robot} v{from}->v{to} ({summary})"
        ))),
        Err(e) => Ok(ToolOutput::business_error(format!(
            "no pude registrar el fix: {e}"
        ))),
    }
}

// ---------------------------------------------------------------- UI

/// URL base de la UI.
///
/// El servidor HTTP no tiene puerto por defecto (se arranca con `--http`), asi que
/// la fachada no puede adivinarlo: se configura con `ROCKETDEBUG_UI_URL`. El valor
/// por defecto es el puerto que se usa en la documentacion, para que un agente que
/// no lea la variable al menos acierte con el caso comun.
fn ui_base() -> String {
    std::env::var("ROCKETDEBUG_UI_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8765".to_string())
}

/// `%xx` de todo lo que no sea incondicionalmente seguro en una URL.
///
/// Hace falta porque los nombres de robot y las etiquetas de db llevan `\`, tildes y
/// espacios, y un enlace que el humano pega en el navegador tiene que funcionar.
fn percent_encode(texto: &str) -> String {
    let mut out = String::with_capacity(texto.len());
    for byte in texto.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `ui_render`: el enlace para que el humano mire lo que el agente esta viendo.
fn ui_render(arguments: &Value) -> Result<ToolOutput, CallError> {
    let args = Args::new(arguments)?;
    let robot = args.required_str("robot")?;
    let db = args.opt_str("db")?;
    let line = args.opt_str("line")?.unwrap_or_default();

    let mut url = format!("{}/?robot={}", ui_base(), percent_encode(&robot));
    if let Some(db) = &db {
        url.push_str(&format!("&db={}", percent_encode(db)));
    }
    if !line.is_empty() {
        url.push_str(&format!("&line={}", percent_encode(&line)));
    }

    let mut o = String::new();
    let _ = writeln!(o, "{url}");
    let _ = writeln!(o);
    let _ = writeln!(
        o,
        "Abre ese enlace: la bandeja queda con `{robot}` seleccionado."
    );
    if !line.is_empty() {
        let _ = writeln!(
            o,
            "La linea {line} viaja en el enlace; la UI actual abre el robot y deja la linea para cuando la pinte."
        );
    }
    let _ = writeln!(
        o,
        "Si no responde: `rocketdebug-mcp --http <puerto>` y ROCKETDEBUG_UI_URL={}",
        ui_base()
    );
    Ok(ToolOutput::ok(o))
}

/// `robot_errors`: solo la seccion de errores del pack.
///
/// Se extrae del render en vez de reimplementarla: el formato de la seccion es
/// el del pack (topes y avisos incluidos) y duplicarlo aqui acabaria en dos
/// formatos que se desincronizan.
fn robot_errors(arguments: &Value, settings: &Settings) -> Result<ToolOutput, CallError> {
    let args = Args::new(arguments)?;
    let robot = args.required_str("robot")?;
    let db_path = resolve_db(args.opt_str("db")?, settings);

    match pack_markdown(&db_path, &robot, None, settings) {
        Ok(markdown) => Ok(ToolOutput::ok(errors_text(
            &markdown, &robot, settings, &db_path,
        ))),
        Err(source) => Ok(ToolOutput::business_error(describe_failure(
            &source, &db_path,
        ))),
    }
}

/// Lee los argumentos de una tool.
///
/// `Null` se acepta como "sin argumentos": la tool dira cual falta. Un valor que
/// no sea objeto es un error de argumentos, no de protocolo, para que el agente
/// vea el mensaje.
struct Args<'a> {
    map: Option<&'a Map<String, Value>>,
}

impl<'a> Args<'a> {
    fn new(value: &'a Value) -> Result<Self, CallError> {
        match value {
            Value::Null => Ok(Self { map: None }),
            Value::Object(map) => Ok(Self { map: Some(map) }),
            other => Err(CallError::InvalidParams(format!(
                "`arguments` tiene que ser un objeto JSON, llego {other}"
            ))),
        }
    }

    fn opt_str(&self, key: &str) -> Result<Option<String>, CallError> {
        let Some(map) = self.map else {
            return Ok(None);
        };
        match map.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.clone())),
            Some(other) => Err(CallError::InvalidParams(format!(
                "`{key}` tiene que ser texto, llego {other}"
            ))),
        }
    }

    fn opt_i64(&self, key: &str) -> Result<Option<i64>, CallError> {
        let Some(map) = self.map else {
            return Ok(None);
        };
        match map.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(number)) => match number.as_i64() {
                Some(value) => Ok(Some(value)),
                None => Err(CallError::InvalidParams(format!(
                    "`{key}` tiene que ser un entero, llego {number}"
                ))),
            },
            Some(other) => Err(CallError::InvalidParams(format!(
                "`{key}` tiene que ser un entero, llego {other}"
            ))),
        }
    }

    fn required_str(&self, key: &str) -> Result<String, CallError> {
        match self.opt_str(key)? {
            Some(value) if !value.trim().is_empty() => Ok(value),
            _ => Err(CallError::InvalidParams(format!(
                "falta el argumento requerido: `{key}`"
            ))),
        }
    }
}

/// Prioridad de la db: argumento, `ROCKETBOT_DB`, Studio local.
///
/// Un `db` vacio o de espacios se trata como ausente: un cliente que manda `""`
/// quiere decir "la de siempre", no "abre la cadena vacia".
fn resolve_db(arg: Option<String>, settings: &Settings) -> PathBuf {
    match arg.filter(|path| !path.trim().is_empty()) {
        Some(path) => PathBuf::from(path),
        None => settings.default_db(),
    }
}

/// Listado de robots. Cabecera con la db y una linea por robot.
fn robots_text(db_path: &Path) -> Result<String, Error> {
    let conn = db::open_readonly(db_path)?;
    let robots = db::list_robots(&conn)?;

    let mut out = String::new();
    let _ = writeln!(out, "db: {} (solo lectura)", db_path.display());
    let _ = writeln!(
        out,
        "{} {}",
        robots.len(),
        if robots.len() == 1 { "robot" } else { "robots" }
    );
    if robots.is_empty() {
        let _ = writeln!(
            out,
            "(la base no tiene robots: revisa que la ruta sea la correcta)"
        );
    }
    for robot in &robots {
        let versiones = if robot.versions == 1 {
            "version"
        } else {
            "versiones"
        };
        let _ = writeln!(
            out,
            "- {:<28} {} {}  (ultimo snapshot {})",
            robot.name, robot.versions, versiones, robot.latest_id
        );
    }
    Ok(out.trim_end().to_string())
}

/// Render del pack. La seccion de errores se llena solo si hay logs: es la
/// misma politica que el CLI, y el motor no decide leer disco por su cuenta.
pub(crate) fn pack_markdown(
    db_path: &Path,
    robot: &str,
    snapshot: Option<i64>,
    settings: &Settings,
) -> Result<String, Error> {
    let conn = db::open_readonly(db_path)?;
    let label = db_label(db_path);
    let log_source = settings
        .logs
        .as_ref()
        .map(|(dir, max_files)| (dir.as_path(), *max_files));

    let pack = build_from_conn_with_logs(&conn, robot, snapshot, &label, log_source)?;
    Ok(pack.render_markdown())
}

/// Texto de `robot_errors`: la seccion del pack, o la explicacion de por que no
/// hay seccion.
///
/// La distincion entre "no hay errores" y "no hay logs" es el punto de esta
/// tool: un pack sin seccion de errores no dice que el robot este limpio, dice
/// que no hay evidencia. Confundirlas es justo el falso verde que el motor evita
/// en el resto del producto.
fn errors_text(markdown: &str, robot: &str, settings: &Settings, db_path: &Path) -> String {
    if let Some(section) = errors_section(markdown) {
        return section;
    }
    match &settings.logs {
        Some((dir, max_files)) => format!(
            "sin errores: 0 firmas atribuidas a `{robot}` en los ultimos {max_files} ficheros de {}.\n\
             db: {} (solo lectura)",
            dir.display(),
            db_path.display()
        ),
        None => format!(
            "sin datos de errores: no se leyeron logs de ejecucion.\n\
             El directorio por defecto es {}; si no existe, el pack omite la seccion.\n\
             No es lo mismo \"no fallo\" que \"no lo se\": sin logs no puedo afirmar que `{robot}` este limpio.\n\
             db: {} (solo lectura)",
            logs::DEFAULT_LOGS_DIR,
            db_path.display()
        ),
    }
}

/// Extrae la seccion de errores del render.
///
/// La seccion es plana (una cabecera de nivel 2 y vinetas), asi que termina en
/// la siguiente cabecera de nivel 2. Se corta por ahi y no por "## Variables"
/// porque el orden de secciones es decision del render: si manana la ultima
/// seccion cambia, esta funcion sigue sirviendo.
fn errors_section(markdown: &str) -> Option<String> {
    let start = markdown
        .lines()
        .position(|line| line.starts_with("## Errores"))?;

    let mut out = String::new();
    for line in markdown.lines().skip(start) {
        if !out.is_empty() && line.starts_with("## ") {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }

    let section = out.trim_end();
    if section.is_empty() {
        None
    } else {
        Some(section.to_string())
    }
}

/// Etiqueta de la db sin ruta absoluta: `BBRE2\robot_run.db`.
///
/// El calculo vive en el motor ([`db::label`]) porque el informe del broker
/// tambien lo necesita y dos implementaciones acabarian discrepando.
pub(crate) fn db_label(path: &Path) -> String {
    db::label(path)
}

/// Mensaje de negocio: el del motor y, detras, la db que se consulto.
///
/// Sin la db, "robot no encontrado" no dice donde se busco, y con una db por
/// defecto el agente no sabe que estaba mirando `C:\Rocketbot\robot.db`.
pub(crate) fn describe_failure(source: &Error, db_path: &Path) -> String {
    format!("{source}\ndb: {} (solo lectura)", db_path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_etiqueta_de_la_db_no_filtra_la_ruta_absoluta() {
        assert_eq!(
            db_label(Path::new(r"C:\Users\josep\proyecto\entrega\robot_run.db")),
            "entrega\\robot_run.db"
        );
        // La misma ruta en forma posix: el resultado no depende del host.
        assert_eq!(
            db_label(Path::new("/home/josep/proyecto/entrega/robot_run.db")),
            "entrega\\robot_run.db"
        );
        assert_eq!(db_label(Path::new("robot.db")), "robot.db");
    }

    #[test]
    fn la_db_se_resuelve_por_argumento_entorno_y_defecto() {
        let settings = Settings {
            env_db: Some(PathBuf::from(r"D:\proyecto\robot.db")),
            logs: None,
            registry: None,
            workspace: None,
            exec: None,
        };
        assert_eq!(
            resolve_db(Some(r"C:\otra\robot.db".to_string()), &settings),
            PathBuf::from(r"C:\otra\robot.db"),
            "el argumento manda"
        );
        assert_eq!(
            resolve_db(Some("   ".to_string()), &settings),
            PathBuf::from(r"D:\proyecto\robot.db"),
            "un db en blanco es 'la de siempre'"
        );
        assert_eq!(
            resolve_db(None, &settings),
            PathBuf::from(r"D:\proyecto\robot.db")
        );
        assert_eq!(
            resolve_db(None, &Settings::hermetic()),
            PathBuf::from(db::DEFAULT_DB)
        );
    }

    #[test]
    fn la_seccion_de_errores_se_extrae_sola() {
        let markdown = concat!(
            "# demo  (snapshot 1, 1 versiones)\n",
            "db: prueba\\robot.db | nodos 2 | vars 1 | prof 0\n",
            "\n",
            "## Errores (vivos 1 · payload cambiado 0 · historicos 0 · modulo 0 · ruido 0)\n",
            "- x2 L1 log_message: NameError: name 'contador' is not defined\n",
            "\n",
            "## Variables (1)\n",
            "- saludo = (sin valor inicial)\n",
        );

        let section = errors_section(markdown).expect("hay seccion de errores");
        assert!(section.starts_with("## Errores"), "{section}");
        assert!(section.contains("NameError"), "{section}");
        assert!(
            !section.contains("## Variables") && !section.contains("saludo"),
            "la seccion termina en la siguiente cabecera:\n{section}"
        );
    }

    #[test]
    fn sin_errores_no_se_inventa_una_seccion() {
        let markdown = "# demo  (snapshot 1, 1 versiones)\n## Flujo\n1 log_message\n";
        assert!(errors_section(markdown).is_none());
    }

    #[test]
    fn sin_logs_el_texto_avisa_de_que_falta_evidencia() {
        let texto = errors_text(
            "# demo\n## Flujo\n1 log_message\n",
            "demo",
            &Settings::hermetic(),
            Path::new(r"C:\tmp\robot.db"),
        );
        assert!(texto.contains("sin datos de errores"), "{texto}");
        assert!(texto.contains("demo"), "{texto}");
        assert!(
            !texto.contains("## Errores"),
            "sin logs no hay seccion que mostrar:\n{texto}"
        );
    }

    #[test]
    fn los_argumentos_se_validan_con_mensajes_legibles() {
        let vacio = json!({"robot": "   "});
        let args = Args::new(&vacio).expect("objeto");
        let error = args.required_str("robot").expect_err("vacio no vale");
        assert!(
            matches!(&error, CallError::InvalidParams(mensaje) if mensaje.contains("robot")),
            "{error:?}"
        );

        let entero_como_texto = json!({"snapshot": "10"});
        let args = Args::new(&entero_como_texto).expect("objeto");
        assert!(args.opt_i64("snapshot").is_err(), "un texto no es entero");

        let ruta_como_numero = json!({"db": 7});
        let args = Args::new(&ruta_como_numero).expect("objeto");
        assert!(args.opt_str("db").is_err(), "un numero no es ruta");

        let lista = json!([1, 2]);
        assert!(
            Args::new(&lista).is_err(),
            "los argumentos tienen que ser un objeto"
        );
    }

    #[test]
    fn el_catalogo_declara_las_diez_tools_con_schema() {
        let catalogo = descriptors();
        let tools = catalogo["tools"].as_array().expect("array de tools");
        assert_eq!(tools.len(), 10);
        let nombres: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(
            nombres,
            vec![
                LIST_ROBOTS,
                FLOW_PACK,
                ROBOT_ERRORS,
                FLOW_DIFF,
                RUN_ROBOT,
                RUN_STATUS,
                PROBLEMS_SEARCH,
                PROBLEMS_RECORD,
                PROBLEMS_FIX,
                UI_RENDER
            ],
            "el orden del catalogo es el que ve el agente"
        );
        for tool in tools {
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert!(tool["inputSchema"]["properties"].is_object());
        }
    }

    #[test]
    fn run_robot_sin_broker_es_error_de_negocio() {
        let salida = call(RUN_ROBOT, &json!({"robot": "demo"}), &Settings::hermetic())
            .expect("la tool no rompe");
        assert!(salida.is_error, "{salida:?}");
        assert!(salida.text.contains("ROCKETBOT_EXE"), "{}", salida.text);
    }

    #[test]
    fn run_status_sin_broker_es_error_de_negocio() {
        let salida = call(RUN_STATUS, &json!({}), &Settings::hermetic()).expect("la tool no rompe");
        assert!(salida.is_error, "{salida:?}");
        assert!(salida.text.contains("ROCKETBOT_EXE"), "{}", salida.text);
    }
}
