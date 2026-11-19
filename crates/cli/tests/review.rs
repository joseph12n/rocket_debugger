use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;

const ROBOT: &str = r#"{"project":{"commands":[{"father":"log_message","line":"1","command":"__NAME__","children":[],"else":[]}],"vars":[],"profile":{"name":"__NAME__"}}}"#;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rocketdebug-cli-review-{tag}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("creo el directorio temporal");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn make_db(dir: &Path, robots: &[&str]) -> PathBuf {
    std::fs::create_dir_all(dir).expect("creo la carpeta de la db");
    let path = dir.join("robot.db");
    let conn = rusqlite::Connection::open(&path).expect("creo la db temporal");
    conn.execute_batch(
        "CREATE TABLE bots (
            id INTEGER PRIMARY KEY,
            name TEXT,
            created_at TEXT,
            data TEXT
        );",
    )
    .expect("creo la tabla bots");

    for (index, name) in robots.iter().enumerate() {
        let json = ROBOT.replace("__NAME__", name);
        let b64 = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
        conn.execute(
            "INSERT INTO bots (id, name, created_at, data) \
             VALUES (?1, ?2, '2026-11-17 10:00:00', ?3)",
            rusqlite::params![index as i64 + 1, name, b64],
        )
        .expect("inserto el snapshot");
    }
    path
}

fn review(args: &[&std::ffi::OsStr]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_rocketdebug"))
        .arg("review")
        .args(args)
        .output()
        .expect("lanzo rocketdebug review");
    assert!(
        out.status.success(),
        "el comando fallo:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn run(args: &[&std::ffi::OsStr]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_rocketdebug"))
        .args(args)
        .output()
        .expect("lanzo rocketdebug");
    assert!(
        out.status.success(),
        "el comando fallo:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn el_workspace_distingue_dbs_con_la_misma_cola_de_ruta() {
    let dir = TempDir::new("etiquetas");
    make_db(&dir.path().join("a").join("sandbox"), &["alpha"]);
    make_db(&dir.path().join("b").join("sandbox"), &["beta"]);

    let stdout = run(&[
        std::ffi::OsStr::new("list"),
        std::ffi::OsStr::new("--workspace"),
        dir.path().as_os_str(),
    ]);

    assert!(
        stdout.contains(r"a\sandbox\robot.db"),
        "falta la etiqueta alargada de la primera entrega:\n{stdout}"
    );
    assert!(
        stdout.contains(r"b\sandbox\robot.db"),
        "falta la etiqueta alargada de la segunda entrega:\n{stdout}"
    );
    assert!(
        !stdout.contains(&dir.path().display().to_string()),
        "la salida filtra la ruta absoluta:\n{stdout}"
    );
}

#[test]
fn el_panorama_muestra_los_dos_robots_de_la_db_y_el_total() {
    let dir = TempDir::new("dos-robots");
    let db = make_db(dir.path(), &["alpha", "beta"]);

    let stdout = review(&["--db".as_ref(), db.as_os_str(), "--no-logs".as_ref()]);

    assert!(
        stdout.contains("| 2 robots |"),
        "cabecera inesperada:\n{stdout}"
    );
    assert!(stdout.contains("alpha"), "falta alpha:\n{stdout}");
    assert!(stdout.contains("beta"), "falta beta:\n{stdout}");
    let total = stdout
        .lines()
        .find(|line| line.starts_with("resultado: "))
        .unwrap_or_else(|| panic!("falta la linea de totales:\n{stdout}"));
    assert!(total.ends_with("en 2 robots"), "total inesperado: {total}");
    assert!(
        !stdout.contains(&dir.path().display().to_string()),
        "la salida filtra la ruta absoluta:\n{stdout}"
    );
}

#[test]
fn el_workspace_recorre_todas_las_dbs_y_el_limite_avisa_de_lo_que_queda() {
    let dir = TempDir::new("workspace");
    make_db(&dir.path().join("entrega_a"), &["alpha"]);
    make_db(&dir.path().join("entrega_b"), &["beta"]);

    let stdout = review(&[
        "--workspace".as_ref(),
        dir.path().as_os_str(),
        "--no-logs".as_ref(),
        "--limit".as_ref(),
        "1".as_ref(),
    ]);

    assert!(
        stdout.contains("workspace: 2 dbs | 2 robots"),
        "cabecera inesperada:\n{stdout}"
    );
    assert!(
        stdout.contains("... y 1 robot mas"),
        "el limite no avisa de lo que queda fuera:\n{stdout}"
    );
    let total = stdout
        .lines()
        .find(|line| line.starts_with("resultado: "))
        .unwrap_or_else(|| panic!("falta la linea de totales:\n{stdout}"));
    assert!(total.ends_with("en 2 robots"), "total inesperado: {total}");
}
