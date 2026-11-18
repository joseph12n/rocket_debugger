//! Registro historico de problemas: lo que el motor recuerda entre sesiones.
//!
//! Hoy el motor detecta un defecto en el context pack y el agente lo arregla,
//! pero ese conocimiento muere con la sesion: a la semana siguiente el mismo
//! sintoma vuelve a costar el mismo diagnostico, y el arreglo que funciono no
//! esta escrito en ningun sitio. Este modulo es esa memoria. Guarda tres cosas:
//!
//! | Tabla | Que guarda |
//! |---|---|
//! | `signatures` | cada firma normalizada (ver [`crate::signature::normalize`]) y cuantas veces se vio |
//! | `observations` | cada avistamiento: robot, db, version, linea, mensaje e instante |
//! | `fixes` | como se cerro: version de origen y destino, resumen, detalle y nota |
//!
//! # Es el unico sitio donde la herramienta escribe
//!
//! `robot.db` se abre siempre en solo lectura ([`crate::db::open_readonly`]) y
//! nunca se toca: un diagnostico no puede alterar el entorno que diagnostica.
//! El registro vive en un fichero sqlite propio ([`default_path`]) que se puede
//! borrar sin perder nada del proyecto, solo el historial.
//!
//! # El reloj lo pone quien llama
//!
//! [`Observation`] y [`FixRecord`] llevan el instante ya resuelto. El motor no
//! llama a `SystemTime::now()` a proposito: el runner escribe hora local sin
//! zona y mezclar husos en el mismo historial desordenaria `first_seen` y
//! `last_seen`. Quien conoce el reloj (CLI, daemon, MCP) lo inyecta; de paso, el
//! registro queda reproducible en tests.
//!
//! # Idempotencia obligatoria
//!
//! El motor va a volver a ver el mismo log en cada barrido. Registrar dos veces
//! la misma observacion **no** suma: [`Registry::record_observation`] devuelve
//! `false` y los contadores no se mueven. La identidad de una observacion es
//! `(firma, robot, version, linea, instante)`.
//!
//! ```
//! use rocketdebug_core::db::parse_date;
//! use rocketdebug_core::registry::{Observation, Registry};
//!
//! let mut registro = Registry::in_memory().unwrap();
//! let obs = Observation {
//!     signature: "NameError:name:{var}:not_defined".into(),
//!     robot: "DEMO_ciclo".into(),
//!     db_label: "rocketbot/robot.db".into(),
//!     version: Some(59),
//!     line: "2".into(),
//!     message: "NameError: name 'contador' is not defined".into(),
//!     seen_at: parse_date("2026-11-17 12:49:22"),
//! };
//!
//! assert!(registro.record_observation(&obs).unwrap(), "primera vez");
//! assert!(!registro.record_observation(&obs).unwrap(), "el motor lo vuelve a ver");
//!
//! let hits = registro.search("demo", 10).unwrap();
//! assert_eq!(hits.len(), 1);
//! assert_eq!(hits[0].occurrences, 1);
//! ```

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::NaiveDateTime;
use rusqlite::{params, Connection, TransactionBehavior};

use crate::error::{Error, Result};

/// Nombre del fichero sqlite del registro.
const FILE_NAME: &str = "problems.db";

/// Carpeta de datos de la herramienta dentro del directorio del usuario.
const APP_DIR: &str = "rocket_debugger";

/// Variable de entorno que mueve el registro a otro directorio. Util para
/// separar el historial de una entrega del de la maquina, y para tests.
pub const HOME_ENV: &str = "ROCKETDEBUG_HOME";

/// Tope de espera cuando otra conexion tiene la base bloqueada. El daemon y el
/// CLI pueden escribir a la vez; sin esto, el segundo recibe `database is
/// locked` y pierde el avistamiento en vez de esperar unos milisegundos.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Formato de los instantes: ISO-8601 de ancho fijo y sin zona.
///
/// Ancho fijo para que el orden del texto sea el orden del tiempo: es lo que
/// permite conservar `first_seen` con `MIN()` en SQL, sin convertir nada.
const TS_FMT: &str = "%Y-%m-%dT%H:%M:%S%.9f";

/// Esquema. Todo con `IF NOT EXISTS` para que abrir dos veces no falle.
const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS signatures (
    id          INTEGER PRIMARY KEY,
    signature   TEXT    NOT NULL UNIQUE,
    occurrences INTEGER NOT NULL DEFAULT 0,
    first_seen  TEXT    NOT NULL,
    last_seen   TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS observations (
    id           INTEGER PRIMARY KEY,
    signature_id INTEGER NOT NULL REFERENCES signatures(id),
    robot        TEXT    NOT NULL,
    db_label     TEXT    NOT NULL,
    version      INTEGER,
    line         TEXT    NOT NULL,
    message      TEXT    NOT NULL,
    seen_at      TEXT    NOT NULL
);

-- La identidad incluye `version`, que puede no conocerse. En un UNIQUE normal
-- los NULL son distintos entre si, asi que dos observaciones sin version nunca
-- colisionarian y el motor volveria a contar el mismo log en cada barrido.
-- COALESCE lleva el \"no se sabe\" a un valor concreto para que la unicidad
-- tambien aplique ahi; -1 no colisiona porque las versiones de `robot.db` son
-- ids positivos.
CREATE UNIQUE INDEX IF NOT EXISTS observations_identity
    ON observations (signature_id, robot, COALESCE(version, -1), line, seen_at);

CREATE TABLE IF NOT EXISTS fixes (
    id           INTEGER PRIMARY KEY,
    signature_id INTEGER NOT NULL REFERENCES signatures(id),
    robot        TEXT    NOT NULL,
    from_version INTEGER,
    to_version   INTEGER,
    summary      TEXT    NOT NULL,
    detail       TEXT    NOT NULL,
    note         TEXT    NOT NULL,
    recorded_at  TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS fixes_by_signature ON fixes (signature_id);
";

/// Un avistamiento concreto de un problema en una ejecucion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// Firma normalizada: dos variantes del mismo defecto dan la misma cadena.
    pub signature: String,
    pub robot: String,
    /// Etiqueta relativa de la db de la que salio el robot. Absoluta nunca: el
    /// registro se comparte y no debe llevar el arbol de directorios de nadie.
    pub db_label: String,
    /// Id del snapshot que estaba vivo, si se pudo atribuir.
    pub version: Option<i64>,
    /// Linea del robot (`2`, `2.1`) tal cual la muestra el pack. Es una
    /// etiqueta, no una direccion: hay lineas repetidas entre ramas.
    pub line: String,
    pub message: String,
    pub seen_at: NaiveDateTime,
}

/// Un arreglo conocido, con lo necesario para reconocer el mismo caso despues.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixRecord {
    pub signature: String,
    pub robot: String,
    /// Version de la que se partia y version arreglada, si se conocen.
    pub from_version: Option<i64>,
    pub to_version: Option<i64>,
    /// Una linea: que se cambio.
    pub summary: String,
    /// El porque y el como, para que otro agente no tenga que redescubrirlo.
    pub detail: String,
    /// Avisos: lo que se probo y no funciono, efectos laterales, dudas.
    pub note: String,
    pub recorded_at: NaiveDateTime,
}

/// Lo que el registro sabe de una firma, que es lo que se le muestra al agente
/// antes de diagnosticar: cuantas veces se vio, donde y si ya se cerro.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureStats {
    pub signature: String,
    pub occurrences: usize,
    pub first_seen: NaiveDateTime,
    pub last_seen: NaiveDateTime,
    /// Robots afectados, sin repetir y en orden alfabetico.
    pub robots: Vec<String>,
    pub fixes: usize,
}

/// Conexion al fichero sqlite del registro.
#[derive(Debug)]
pub struct Registry {
    conn: Connection,
}

impl Registry {
    /// Abre el registro, creando el directorio y las tablas si no existen.
    ///
    /// Es idempotente: dos aperturas seguidas sobre el mismo fichero dejan el
    /// mismo esquema y no fallan.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            // El `parent` de un nombre relativo es "" y no hay nada que crear.
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir).map_err(|source| Error::Open {
                    path: path.to_path_buf(),
                    source: io_error(source),
                })?;
            }
        }
        let conn = Connection::open(path).map_err(|source| Error::Open {
            path: path.to_path_buf(),
            source,
        })?;
        Self::init(conn)
    }

    /// Registro en memoria. Para tests y para consultas de un solo uso.
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // Las tablas declaran `REFERENCES`; sqlite no las aplica si no se pide,
        // y una observacion huerfana es un error de programacion, no un dato.
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Registra un avistamiento. Devuelve `false` si ya estaba registrado.
    ///
    /// El upsert de la firma incrementa `occurrences` y avanza `last_seen`,
    /// conservando `first_seen` (el primer avistamiento no se reescribe: es el
    /// dato que dice desde cuando duele). La observacion en si entra una sola
    /// vez por la identidad `(firma, robot, version, linea, instante)`.
    pub fn record_observation(&mut self, o: &Observation) -> Result<bool> {
        let seen_at = format_ts(o.seen_at);
        // `IMMEDIATE` toma el bloqueo de escritura al empezar: sin el, dos
        // procesos que registren el mismo log a la vez podrian colarse entre el
        // "ya existe?" y el "insert" y contar el avistamiento dos veces.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let signature_id = ensure_signature(&tx, &o.signature, &seen_at)?;

        // `version IS ?` compara NULL con NULL; con `=` el resultado seria
        // siempre falso y una observacion sin version entraria en cada barrido.
        //
        // `seen_at` NO es la hora del barrido: quien registra pasa la hora del
        // snapshot observado (ver `db::latest_created_at`). Si fuera el reloj del
        // escaneo, dos pasadas de la misma version contarian como dos
        // avistamientos y el contador dejaria de significar nada.
        let already: bool = tx.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM observations
                  WHERE signature_id = ?1 AND robot = ?2 AND version IS ?3
                    AND line = ?4 AND seen_at = ?5)",
            params![signature_id, o.robot, o.version, o.line, seen_at],
            |r| r.get(0),
        )?;
        if already {
            tx.commit()?;
            return Ok(false);
        }

        tx.execute(
            "INSERT INTO observations
                 (signature_id, robot, db_label, version, line, message, seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                signature_id,
                o.robot,
                o.db_label,
                o.version,
                o.line,
                o.message,
                seen_at
            ],
        )?;
        // MIN/MAX comparan el texto, y el formato de `seen_at` ordena igual que
        // el tiempo: el avistamiento viejo no mueve `first_seen` ni el nuevo
        // retrocede `last_seen` si llegan desordenados.
        tx.execute(
            "UPDATE signatures
                SET occurrences = occurrences + 1,
                    first_seen  = MIN(first_seen, ?2),
                    last_seen   = MAX(last_seen, ?2)
              WHERE id = ?1",
            params![signature_id, seen_at],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Registra un arreglo y devuelve su id.
    ///
    /// Un fix puede llegar antes que su primera observacion (se arreglo algo que
    /// el motor aun no habia visto). La firma se crea con `0` ocurrencias: un
    /// arreglo no es un avistamiento y no debe inflar el contador.
    pub fn record_fix(&mut self, f: &FixRecord) -> Result<i64> {
        let recorded_at = format_ts(f.recorded_at);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let signature_id = ensure_signature(&tx, &f.signature, &recorded_at)?;
        tx.execute(
            "INSERT INTO fixes
                 (signature_id, robot, from_version, to_version, summary, detail, note, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                signature_id,
                f.robot,
                f.from_version,
                f.to_version,
                f.summary,
                f.detail,
                f.note,
                recorded_at
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    /// Busca firmas por subcadena en la firma **o** en el nombre de robot, sin
    /// distinguir mayusculas. `query` vacia devuelve todas; `limit` es un tope
    /// literal (0 devuelve nada), no un "sin limite".
    ///
    /// Orden: mas ocurrencias primero; a igualdad, lo visto mas recientemente;
    /// y la firma alfabeticamente para que el resultado sea estable.
    ///
    /// El filtro se hace en Rust a proposito: el `lower()` de sqlite solo pliega
    /// ASCII, asi que un robot con tilde no encontraria su propia firma. De paso
    /// el texto del usuario no entra nunca en una sentencia SQL.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SignatureStats>> {
        let needle = query.trim().to_lowercase();

        let mut stmt = self.conn.prepare(
            "SELECT id, signature, occurrences, first_seen, last_seen
               FROM signatures
              ORDER BY occurrences DESC, last_seen DESC, signature",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let firmas = rows.collect::<rusqlite::Result<Vec<_>>>()?;

        // El registro es pequeno (firmas distintas, no lineas de log): tres
        // recorridos completos son mas baratos y mas simples que una consulta
        // por firma, que seria N+1.
        let mut robots: HashMap<i64, Vec<String>> = HashMap::new();
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT signature_id, robot FROM observations ORDER BY signature_id, robot",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (id, robot) = row?;
            robots.entry(id).or_default().push(robot);
        }

        let mut fixes: HashMap<i64, usize> = HashMap::new();
        let mut stmt = self
            .conn
            .prepare("SELECT signature_id, COUNT(*) FROM fixes GROUP BY signature_id")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (id, n) = row?;
            fixes.insert(id, n as usize);
        }

        let mut out = Vec::new();
        for (id, signature, occurrences, first_seen, last_seen) in firmas {
            if out.len() >= limit {
                break;
            }
            let robots_de_la_firma = robots.get(&id).cloned().unwrap_or_default();
            let coincide = needle.is_empty()
                || signature.to_lowercase().contains(&needle)
                || robots_de_la_firma
                    .iter()
                    .any(|r| r.to_lowercase().contains(&needle));
            if !coincide {
                continue;
            }
            out.push(SignatureStats {
                signature,
                occurrences: occurrences as usize,
                first_seen: parse_ts(&first_seen),
                last_seen: parse_ts(&last_seen),
                robots: robots_de_la_firma,
                fixes: fixes.get(&id).copied().unwrap_or(0),
            });
        }
        Ok(out)
    }

    /// Numero de firmas distintas y de fixes registrados.
    pub fn counts(&self) -> Result<(usize, usize)> {
        let (firmas, fixes): (i64, i64) = self.conn.query_row(
            "SELECT (SELECT COUNT(*) FROM signatures), (SELECT COUNT(*) FROM fixes)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((firmas as usize, fixes as usize))
    }
}

/// Ruta por defecto del registro.
///
/// Orden de resolucion:
///
/// 1. `ROCKETDEBUG_HOME` (directorio) -> `<dir>/problems.db`. Permite separar el
///    historial de una entrega del de la maquina sin tocar codigo.
/// 2. Windows: `%LOCALAPPDATA%\rocket_debugger\problems.db`.
/// 3. Otros: `$HOME/.local/share/rocket_debugger/problems.db`.
/// 4. Si no existe ninguna de esas variables, el directorio temporal del
///    sistema. Es el ultimo recurso, y esta documentado: perder el registro por
///    no tener `LOCALAPPDATA` (un servicio, un contenedor) es peor que
///    guardarlo en un sitio raro. [`Registry::open`] crea el directorio.
///
/// La ruta devuelta no se crea aqui: la crea `open`.
pub fn default_path() -> PathBuf {
    resolve_path(std::env::var_os(HOME_ENV), platform_base())
}

/// Compone la ruta final. Se separa de [`default_path`] para poder probar la
/// resolucion sin mutar el entorno del proceso: los tests corren en paralelo y
/// `set_var` no es seguro fuera de un unico hilo.
fn resolve_path(home: Option<OsString>, base: PathBuf) -> PathBuf {
    match home.as_deref().filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir).join(FILE_NAME),
        None => base.join(FILE_NAME),
    }
}

#[cfg(windows)]
fn platform_base() -> PathBuf {
    match std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir).join(APP_DIR),
        None => std::env::temp_dir().join(APP_DIR),
    }
}

#[cfg(not(windows))]
fn platform_base() -> PathBuf {
    match std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir)
            .join(".local")
            .join("share")
            .join(APP_DIR),
        None => std::env::temp_dir().join(APP_DIR),
    }
}

/// Crea la firma si no existe y devuelve su id, sin tocar los contadores.
///
/// La primera vez entra con `0` ocurrencias: el avistamiento que la crea es
/// quien la sube, y un fix sobre una firma desconocida no debe inventar uno.
fn ensure_signature(tx: &Connection, signature: &str, at: &str) -> Result<i64> {
    tx.execute(
        "INSERT INTO signatures (signature, occurrences, first_seen, last_seen)
         VALUES (?1, 0, ?2, ?2)
         ON CONFLICT(signature) DO NOTHING",
        params![signature, at],
    )?;
    let id = tx.query_row(
        "SELECT id FROM signatures WHERE signature = ?1",
        params![signature],
        |r| r.get(0),
    )?;
    Ok(id)
}

/// Instante -> texto. Se compone a mano en vez de fiarse de los modificadores de
/// precision de `strftime`: hacen falta siempre 9 digitos para que la
/// comparacion de textos ordene como el tiempo.
fn format_ts(t: NaiveDateTime) -> String {
    format!(
        "{}.{:09}",
        t.format("%Y-%m-%dT%H:%M:%S"),
        t.and_utc().timestamp_subsec_nanos()
    )
}

/// Texto -> instante. Tolerante igual que [`crate::db::parse_date`]: un valor
/// raro en el fichero no debe tumbar una busqueda. Solo se lee lo que escribe
/// este modulo, pero el fichero es del usuario y puede quedar a medias.
fn parse_ts(s: &str) -> NaiveDateTime {
    NaiveDateTime::parse_from_str(s, TS_FMT)
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f"))
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f"))
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
        .unwrap_or_default()
}

/// El fallo de E/S al preparar la carpeta se presenta como error de apertura:
/// [`crate::error::Error`] no tiene variante de E/S y este modulo no puede
/// anadirla sin tocar ficheros que no le tocan. El motivo real viaja dentro.
fn io_error(e: std::io::Error) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
        Some(e.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: &str = "2026-11-17 12:00:00";
    const FIRMA_NOMBRE: &str = "NameError:name:{var}:not_defined";
    const FIRMA_CLAVE: &str = "KeyError:<key>";

    fn obs(
        signature: &str,
        robot: &str,
        version: Option<i64>,
        line: &str,
        seen_at: &str,
    ) -> Observation {
        Observation {
            signature: signature.to_string(),
            robot: robot.to_string(),
            db_label: "entrega/robot_run.db".to_string(),
            version,
            line: line.to_string(),
            message: format!("mensaje de {signature}"),
            seen_at: crate::db::parse_date(seen_at),
        }
    }

    fn fix(
        signature: &str,
        robot: &str,
        from: Option<i64>,
        to: Option<i64>,
        recorded_at: &str,
    ) -> FixRecord {
        FixRecord {
            signature: signature.to_string(),
            robot: robot.to_string(),
            from_version: from,
            to_version: to,
            summary: "asignar 1 en vez de contador + 1".to_string(),
            detail: "setVar contador = contador + 1 se lee a si misma".to_string(),
            note: "verificado en studio".to_string(),
            recorded_at: crate::db::parse_date(recorded_at),
        }
    }

    #[test]
    fn los_instantes_se_guardan_en_ancho_fijo_y_ordenan_como_el_tiempo() {
        let t = crate::db::parse_date("2026-11-17 12:49:22");
        assert_eq!(format_ts(t), "2026-11-17T12:49:22.000000000");
        assert_eq!(parse_ts(&format_ts(t)), t);
        // La comparacion de textos tiene que dar el mismo orden que el tiempo.
        let antes = format_ts(crate::db::parse_date("2026-11-17 12:49:22"));
        let despues = format_ts(crate::db::parse_date("2026-11-17 12:49:22.5"));
        assert!(antes < despues, "{antes} no ordena antes de {despues}");
        // Un valor corrupto degrada a epoch en vez de tumbar la busqueda.
        assert_eq!(parse_ts("no-es-fecha"), NaiveDateTime::default());
    }

    /// Re-escanear la misma version no puede inflar el contador: la identidad
    /// incluye el instante, pero quien registra pasa el del **snapshot**, no el del
    /// barrido (ver `db::latest_created_at`). Con la misma hora, la segunda pasada
    /// es idempotente.
    #[test]
    fn la_misma_version_escaneada_dos_veces_no_suma() {
        let mut registro = Registry::in_memory().expect("registro");
        let obs = Observation {
            signature: "NameError:name:{var}:not_defined".to_string(),
            robot: "ML_06_Legacy".to_string(),
            db_label: "test\\robot.db".to_string(),
            version: Some(25),
            line: "2.10".to_string(),
            message: "NameError: name 'contador' is not defined".to_string(),
            // La hora del snapshot v25, no la del reloj.
            seen_at: crate::db::parse_date("2026-11-17 12:47:10"),
        };
        assert!(registro.record_observation(&obs).expect("primera"));
        assert!(
            !registro.record_observation(&obs).expect("segunda"),
            "la misma version no debe sumar dos veces"
        );
        let stats = registro.search("NameError", 5).expect("busqueda");
        assert_eq!(stats[0].occurrences, 1);
    }

    #[test]
    fn tres_observaciones_de_dos_firmas_y_la_repetida_no_suma() {
        let mut reg = Registry::in_memory().unwrap();
        let primera = obs(
            FIRMA_NOMBRE,
            "DEMO_ciclo",
            Some(59),
            "2",
            "2026-11-17 12:49:22",
        );
        // Mismo defecto en otra ejecucion: otro instante, misma firma -> cuenta.
        let segunda = obs(
            FIRMA_NOMBRE,
            "DEMO_ciclo",
            Some(59),
            "2",
            "2026-11-17 12:51:00",
        );
        let otra = obs(FIRMA_CLAVE, "reporteMercadolibre", Some(7), "3.1", T0);

        assert!(reg.record_observation(&primera).unwrap(), "primera vez");
        assert!(reg.record_observation(&segunda).unwrap(), "otro instante");
        assert!(reg.record_observation(&otra).unwrap(), "otra firma");

        // El motor vuelve a ver la misma linea del mismo log: no suma.
        assert!(
            !reg.record_observation(&segunda).unwrap(),
            "la observacion repetida devuelve false"
        );

        let stats = reg.search("name:{var}", 10).unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(
            stats[0].occurrences, 2,
            "el contador no sube con el duplicado"
        );
        assert_eq!(
            stats[0].first_seen,
            crate::db::parse_date("2026-11-17 12:49:22"),
            "first_seen conserva el primer avistamiento"
        );
        assert_eq!(
            stats[0].last_seen,
            crate::db::parse_date("2026-11-17 12:51:00")
        );
    }

    #[test]
    fn una_version_desconocida_tambien_es_idempotente() {
        let mut reg = Registry::in_memory().unwrap();
        // En SQL un NULL nunca colisiona con otro NULL: sin el indice con
        // COALESCE, esta observacion entraria en cada barrido.
        let sin_version = obs("Error:Incorrect padding", "ruido", None, "-", T0);
        assert!(reg.record_observation(&sin_version).unwrap());
        assert!(!reg.record_observation(&sin_version).unwrap());
        assert_eq!(reg.search("", 10).unwrap()[0].occurrences, 1);
    }

    #[test]
    fn search_encuentra_por_trozo_de_firma_y_de_robot() {
        let mut reg = Registry::in_memory().unwrap();
        reg.record_observation(&obs(FIRMA_NOMBRE, "DEMO_ciclo", Some(59), "2", T0))
            .unwrap();
        reg.record_observation(&obs(FIRMA_CLAVE, "reporteMercadolibre", Some(7), "3.1", T0))
            .unwrap();
        reg.record_observation(&obs(FIRMA_CLAVE, "ventas_mensuales", Some(3), "1", T0))
            .unwrap();

        // Subcadena de la firma, en minusculas aunque la firma tenga mayusculas.
        let por_firma = reg.search("nameerror", 10).unwrap();
        assert_eq!(por_firma.len(), 1);
        assert_eq!(por_firma[0].signature, FIRMA_NOMBRE);

        // Subcadena del robot: devuelve la firma que ese robot sufrio.
        let por_robot = reg.search("MERCADO", 10).unwrap();
        assert_eq!(por_robot.len(), 1);
        assert_eq!(por_robot[0].signature, FIRMA_CLAVE);
        assert_eq!(
            por_robot[0].robots,
            vec!["reporteMercadolibre", "ventas_mensuales"],
            "todos los robots de la firma, en orden"
        );

        // La misma firma se encuentra tambien por el segundo robot.
        assert_eq!(reg.search("ventas", 10).unwrap().len(), 1);

        // Query vacia (o solo espacios): todo.
        assert_eq!(reg.search("", 10).unwrap().len(), 2);
        assert_eq!(reg.search("   ", 10).unwrap().len(), 2);

        assert!(reg.search("no-existe", 10).unwrap().is_empty());
    }

    #[test]
    fn search_ordena_por_ocurrencias_y_respeta_el_limite() {
        let mut reg = Registry::in_memory().unwrap();
        reg.record_observation(&obs("firma_una", "r1", Some(1), "1", "2026-11-17 12:00:00"))
            .unwrap();
        for minuto in 1..=3 {
            let visto = format!("2026-11-17 12:0{minuto}:00");
            reg.record_observation(&obs("firma_tres", "r1", Some(1), "1", &visto))
                .unwrap();
        }

        let todas = reg.search("firma", 10).unwrap();
        assert_eq!(todas.len(), 2);
        assert_eq!(todas[0].signature, "firma_tres", "la mas vista va primero");
        assert_eq!(todas[0].occurrences, 3);
        assert_eq!(todas[1].signature, "firma_una");
        assert_eq!(todas[1].occurrences, 1);

        let tope = reg.search("firma", 1).unwrap();
        assert_eq!(tope.len(), 1);
        assert_eq!(tope[0].signature, "firma_tres");
        assert!(
            reg.search("firma", 0).unwrap().is_empty(),
            "el tope es literal"
        );
    }

    #[test]
    fn un_fix_cuenta_en_las_estadisticas_y_no_inventa_avistamientos() {
        let mut reg = Registry::in_memory().unwrap();
        reg.record_observation(&obs(FIRMA_NOMBRE, "DEMO_ciclo", Some(59), "2", T0))
            .unwrap();

        let id = reg
            .record_fix(&fix(
                FIRMA_NOMBRE,
                "DEMO_ciclo",
                Some(59),
                Some(60),
                "2026-11-17 13:00:00",
            ))
            .unwrap();
        assert!(id > 0, "devuelve el id del fix");

        let stats = reg.search("nameerror", 10).unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].fixes, 1);
        assert_eq!(stats[0].occurrences, 1);

        // Un fix de una firma que el motor aun no habia visto: la firma existe
        // para que el historial la muestre, pero con 0 avistamientos.
        reg.record_fix(&fix(
            "TimeoutError:esperando_ventana",
            "otro_robot",
            None,
            Some(3),
            "2026-11-17 13:05:00",
        ))
        .unwrap();
        let huerfana = reg.search("timeout", 10).unwrap();
        assert_eq!(huerfana.len(), 1);
        assert_eq!(huerfana[0].occurrences, 0);
        assert_eq!(huerfana[0].fixes, 1);
        assert!(huerfana[0].robots.is_empty());

        assert_eq!(reg.counts().unwrap(), (2, 2));
    }

    #[test]
    fn counts_de_un_registro_vacio() {
        let reg = Registry::in_memory().unwrap();
        assert_eq!(reg.counts().unwrap(), (0, 0));
        assert!(reg.search("", 10).unwrap().is_empty());
    }

    #[test]
    fn un_query_con_comillas_o_drop_table_no_rompe_el_esquema() {
        let mut reg = Registry::in_memory().unwrap();
        let venenosa = "NameError: name 'x'; DROP TABLE observations; --";
        let robot = "robot'; DROP TABLE fixes; --";
        reg.record_observation(&obs(venenosa, robot, None, "1", T0))
            .unwrap();

        // El texto se busca tal cual lo escribio el usuario, comillas incluidas.
        let hits = reg.search("'; DROP TABLE observations; --", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].signature, venenosa);
        assert_eq!(hits[0].robots, vec![robot]);

        // La base sigue entera: se puede seguir escribiendo y contando.
        assert_eq!(reg.counts().unwrap(), (1, 0));
        assert!(reg
            .record_observation(&obs(venenosa, "otro_robot", None, "1", T0))
            .unwrap());

        // `%` y `_` son literales, no comodines de LIKE: si algun dia el filtro
        // pasa a SQL, esto lo delata. `%` no aparece en ningun dato, asi que no
        // encuentra nada aunque un LIKE lo tomase por "cualquier cosa"; `_` solo
        // casa con el robot que lo lleva escrito, no con cualquier caracter.
        assert!(reg.search("%", 10).unwrap().is_empty());
        assert_eq!(reg.search("_", 10).unwrap().len(), 1);
    }

    #[test]
    fn open_crea_el_esquema_y_abrir_dos_veces_no_falla() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("datos-nuevos").join("problems.db");
        assert!(!path.exists());

        {
            let mut reg = Registry::open(&path).unwrap();
            reg.record_observation(&obs("firma", "r1", Some(1), "1", T0))
                .unwrap();
        }
        assert!(path.exists(), "el fichero se crea al abrir");

        // Reabrir no falla: todo el esquema es `IF NOT EXISTS`.
        let reg = Registry::open(&path).unwrap();
        assert_eq!(reg.counts().unwrap(), (1, 0));
        assert_eq!(reg.search("firma", 10).unwrap()[0].occurrences, 1);

        // Y sigue siendo escribible: el duplicado no entra aunque se reabra.
        drop(reg);
        let mut reg = Registry::open(&path).unwrap();
        assert!(!reg
            .record_observation(&obs("firma", "r1", Some(1), "1", T0))
            .unwrap());
    }

    #[test]
    fn el_registro_aplica_las_claves_foraneas() {
        let reg = Registry::in_memory().unwrap();
        let activas: i64 = reg
            .conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(activas, 1, "las REFERENCES declaradas se cumplen");
    }

    #[test]
    fn default_path_termina_en_problems_db_y_respeta_el_override() {
        let base = PathBuf::from("C:\\datos");
        assert_eq!(resolve_path(None, base.clone()), base.join(FILE_NAME));
        assert_eq!(
            resolve_path(Some(OsString::new()), base.clone()),
            base.join(FILE_NAME),
            "una variable definida pero vacia no cuenta"
        );
        let override_dir = PathBuf::from("C:\\registro");
        assert_eq!(
            resolve_path(Some(override_dir.clone().into_os_string()), base),
            override_dir.join(FILE_NAME)
        );

        assert_eq!(
            default_path().file_name().and_then(|n| n.to_str()),
            Some(FILE_NAME),
            "sin entorno raro, el fichero se llama siempre igual"
        );
    }
}
