//! Tipos del informe de errores: lo que el render pinta y la fachada devuelve.
//!
//! Viven aparte del cableado para que `render` y `mcp` los usen sin arrastrar la
//! carga de snapshots ni la atribucion temporal; el como se llenan esta en
//! [`errors_for`].

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::Connection;

use crate::attribution::{self, RawError, Verdict};
use crate::error::Result;
use crate::logs;
use crate::signature::{self, ErrorKind};

use super::snapshot_views;

/// Longitud a la que se recorta el mensaje de un error.
///
/// Con 160 caracteres caben el defecto y su detalle; la traza de Python de 20
/// lineas no es parte de el y el presupuesto del pack es de ~2,5k tokens.
pub const ERROR_MESSAGE_MAX: usize = 160;

/// Un error del log agrupado por firma.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorItem {
    /// Etiqueta de linea de Studio **en la version donde ocurrio**; vacia si el
    /// nodo no existe en ese snapshot (no se inventa una linea).
    pub line: String,
    /// Comando del nodo que estaba ejecutando el runner.
    pub command: String,
    /// Primera linea del mensaje, acotada a [`ERROR_MESSAGE_MAX`].
    pub message: String,
    /// Veces que aparecio esta firma.
    pub occurrences: usize,
    /// Firma estable (`signature::normalize`): es la clave de agrupacion.
    pub signature: String,
    /// `bots.id` de la version viva en el instante del error, si se pudo atribuir.
    pub version: Option<i64>,
}

/// Errores del robot repartidos por lo que se puede hacer con ellos.
///
/// Sumar los cinco cubos es el panel en rojo permanente que hace que se deje de
/// mirar: `live` es lo accionable hoy; `historical`, evidencia de un defecto ya
/// corregido (o de codigo retirado); `platform` y `noise` no se arreglan desde el
/// robot.
///
/// `PartialEq` fija en un test que reutilizar los logs parseados no cambia un cubo.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErrorReport {
    /// Viven en la version actual y el nodo conserva sus parametros.
    pub live: Vec<ErrorItem>,
    /// Viven en la version actual, pero los parametros del nodo cambiaron.
    pub payload_changed: Vec<ErrorItem>,
    /// Ocurrieron en una version anterior, en un nodo retirado o no se pudieron
    /// atribuir. Es lo que hay que separar del resto: el robot de hoy ya no los
    /// reproduce.
    pub historical: Vec<ErrorItem>,
    /// Defecto de un modulo de la plataforma (`datatransform`, `getbot`...), no del
    /// robot.
    pub platform: Vec<ErrorItem>,
    /// Ruido del motor (`zdev`, `getbot`): fallos del propio Rocketbot.
    pub noise: Vec<ErrorItem>,
}

impl ErrorReport {
    /// Sin nada que contar: el render omite la seccion entera.
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// Firmas distintas en el informe, no ocurrencias.
    ///
    /// Es lo que muestra la cabecera del render: tres `zdev` iguales son un
    /// problema, no tres, y contarlos como tres infla el panel.
    pub fn total(&self) -> usize {
        self.live.len()
            + self.payload_changed.len()
            + self.historical.len()
            + self.platform.len()
            + self.noise.len()
    }
}

/// Lee los logs del directorio y devuelve los errores del robot ya clasificados.
///
/// El recorrido es: ficheros mas recientes primero ([`logs::recent_files`]) ->
/// parseo cacheado ([`logs::parse_file_cached`]) -> solo lo del robot -> solo los
/// `ERROR`/`EXCEPTION`/`FATAL` -> atribucion temporal contra los snapshots -> cubo.
///
/// Un directorio sin logs devuelve un informe vacio, no un error: el pack sin
/// seccion de errores es valido y "no hay logs" no es un defecto del robot.
pub fn errors_for(
    conn: &Connection,
    robot: &str,
    logs_dir: &Path,
    max_files: usize,
) -> Result<ErrorReport> {
    let entries = robot_errors(robot, logs_dir, max_files);
    if entries.is_empty() {
        return Ok(ErrorReport::default());
    }

    // Solo se decodifican los snapshots del robot pedido: la atribucion de sus
    // errores no necesita los de los demas, a diferencia del grafo.
    let snapshots = snapshot_views(conn, robot)?;

    let mut raws: Vec<RawError> = Vec::with_capacity(entries.len());
    // El cubo que se puede decidir sin veredicto (ruido y modulo) se resuelve
    // antes: no depende de la version y ahorra confundir dos clasificaciones.
    let mut por_mensaje: Vec<Option<Bucket>> = Vec::with_capacity(entries.len());
    for entry in &entries {
        // `for_robot` ya exige peticion; este `let else` documenta la invariante y
        // evita que un cambio futuro cuele un error sin nodo.
        let Some(request) = entry.request.as_ref() else {
            continue;
        };
        raws.push(RawError {
            timestamp: entry.timestamp,
            cmd_id: request.cmd_id.clone(),
            command: request.command.clone(),
            payload: request.payload.clone(),
            message: short_message(&entry.message),
            occurrences: 1,
            signature: signature::normalize(&entry.message),
        });
        por_mensaje.push(bucket_of(&entry.message));
    }

    let atrs = attribution::attribute(&raws, &snapshots);

    let mut cubos: [BTreeMap<String, ErrorItem>; 5] = std::array::from_fn(|_| BTreeMap::new());
    for ((raw, atr), decidido) in raws.iter().zip(&atrs).zip(&por_mensaje) {
        let bucket = decidido.unwrap_or_else(|| Bucket::from_verdict(atr.verdict));
        cubos[bucket.index()]
            .entry(raw.signature.clone())
            .and_modify(|item| {
                item.occurrences += raw.occurrences;
                // La linea de la primera ocurrencia que se pudo ubicar: es la que
                // el agente va a mirar, y las siguientes son la misma.
                if item.line.is_empty() {
                    if let Some(line) = &atr.line {
                        item.line = line.clone();
                    }
                }
                // La version maxima: si la misma firma se repitio en varias
                // versiones, la ultima es la unica que puede seguir viva.
                item.version = max_version(item.version, atr.version);
            })
            .or_insert_with(|| ErrorItem {
                line: atr.line.clone().unwrap_or_default(),
                command: raw.command.clone(),
                message: raw.message.clone(),
                occurrences: raw.occurrences,
                signature: raw.signature.clone(),
                version: atr.version,
            });
    }

    // Se desestructura por valor: mover los cubos cuesta menos que clonarlos, y
    // el orden de los nombres es el del enum.
    let [live, payload_changed, historical, platform, noise] = cubos;
    Ok(ErrorReport {
        live: sorted(live),
        payload_changed: sorted(payload_changed),
        historical: sorted(historical),
        platform: sorted(platform),
        noise: sorted(noise),
    })
}

/// Cubo de clasificacion de un error.
///
/// El orden del `enum` es el de la cabecera del render y el indice de los cubos:
/// cambiarlo mueve la seccion, no solo el orden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Bucket {
    Live = 0,
    PayloadChanged = 1,
    Historical = 2,
    Platform = 3,
    Noise = 4,
}

impl Bucket {
    /// Indice en la tabla de cubos. Es la unica traduccion de "cubo" a "posicion":
    /// en el resto del codigo los cubos se nombran, no se numeran.
    fn index(self) -> usize {
        self as usize
    }

    /// Veredicto de la atribucion -> cubo del informe.
    ///
    /// [`Verdict::Desconocido`] va con los historicos: no se puede atar al codigo
    /// de hoy, asi que presentarlo como vivo seria el falso positivo que la
    /// atribucion temporal existe para evitar. Se conserva igualmente porque es
    /// evidencia (un error anterior al primer guardado sigue siendo un fallo real).
    pub(super) fn from_verdict(verdict: Verdict) -> Self {
        match verdict {
            Verdict::Vivo => Bucket::Live,
            Verdict::PayloadCambiado => Bucket::PayloadChanged,
            Verdict::Historico | Verdict::Desconocido => Bucket::Historical,
        }
    }
}

/// Cubo decidible solo con el texto del error; `None` si hay que mirar el veredicto.
pub(super) fn bucket_of(message: &str) -> Option<Bucket> {
    // El ruido de motor va primero: `zdev` y `getbot` tambien son modulos, pero
    // "se puede arreglar desde el robot?" tiene la misma respuesta en los dos
    // casos, y es la pregunta que separa lo accionable del resto.
    if signature::is_engine_noise(message) {
        return Some(Bucket::Noise);
    }
    if signature::classify(message) == ErrorKind::Module {
        return Some(Bucket::Platform);
    }
    None
}

/// Entradas de error del robot, en orden cronologico.
///
/// Reutiliza el cache de ficheros parseados: la bandeja y el vigilante piden los
/// mismos logs una vez por robot, y sin cache el coste crece con robots x ficheros
/// aunque el resultado sea identico. Se filtra **por referencia** sobre la lista
/// compartida y solo se clona lo que sobrevive al filtro (los errores del robot),
/// que de un log entero es una fraccion: no se copia el fichero para descartarlo.
fn robot_errors(robot: &str, logs_dir: &Path, max_files: usize) -> Vec<logs::LogEntry> {
    let mut out = Vec::new();
    for path in logs::recent_files(logs_dir, max_files) {
        // Un log ilegible se omite: es evidencia que falta, no un fallo del pack.
        let Some(entries) = logs::parse_file_cached(&path) else {
            continue;
        };
        out.extend(
            logs::of_robot(entries.as_ref(), robot)
                .filter(|entry| entry.is_error())
                .cloned(),
        );
    }
    // `recent_files` devuelve del mas nuevo al mas viejo y "la primera ocurrencia"
    // solo significa algo en orden cronologico. La ordenacion es estable, asi que
    // dos errores del mismo instante conservan el orden en que los escribio el
    // runner.
    out.sort_by_key(|e| e.timestamp);
    out
}

/// Primera linea del mensaje, acotada: la traza de Python no es parte del defecto.
pub(super) fn short_message(message: &str) -> String {
    let first = message.lines().next().unwrap_or("").trim();
    let mut out: String = first.chars().take(ERROR_MESSAGE_MAX).collect();
    if first.chars().count() > ERROR_MESSAGE_MAX {
        // Un solo caracter en vez de "..." porque esta cadena se repite por cada
        // error del informe y el presupuesto del pack es de ~2,5k tokens.
        out.push('…');
    }
    out
}

pub(super) fn max_version(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Mas repetidos primero; el desempate por firma mantiene el render estable entre
/// ejecuciones (dos firmas con las mismas ocurrencias no deben bailar).
fn sorted(cubo: BTreeMap<String, ErrorItem>) -> Vec<ErrorItem> {
    let mut out: Vec<ErrorItem> = cubo.into_values().collect();
    out.sort_by(|a, b| {
        b.occurrences
            .cmp(&a.occurrences)
            .then_with(|| a.signature.cmp(&b.signature))
    });
    out
}
