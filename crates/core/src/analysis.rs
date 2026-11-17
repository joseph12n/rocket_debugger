//! Cableado del grafo, las reglas y los errores: lo que convierte cuatro modulos
//! en producto.
//!
//! [`crate::graph`] sabe de llamadas entre robots, [`crate::rules`] sabe de
//! defectos dentro de un flujo y [`crate::attribution`] sabe a que version
//! pertenece un error del log, pero ninguno sirve solo: el grafo, por separado, no
//! dice nada al agente; las reglas sin grafo marcan como muertas las variables que
//! en realidad son la interfaz de los subrobots (medido: 86 falsos positivos de 89
//! en un orquestador de 8 hijos); y los errores sin atribucion temporal mezclan
//! los de hoy con los de una version que ya no corre (medido: de las 51 firmas de
//! `reporteMercadolibre` en la entrega BBRE2, 49 son de versiones anteriores y
//! ninguna esta viva).
//!
//! Este modulo hace el recorrido completo y devuelve ya resuelto lo que el render
//! necesita: los hallazgos del robot pedido, sus contratos de entrada y salida,
//! las variables que siguen sin uso incluso contando lo que consumen sus hijos y
//! los errores del log ya clasificados por responsable.
//!
//! # Por que necesita la db entera
//!
//! El grafo solo existe si se tienen los hechos de **todos** los robots de la db:
//! para saber que provee el padre y que lee el hijo hay que mirar los dos lados.
//! Por eso se aplanan las ultimas versiones de toda la db, no solo la del robot
//! pedido.
//!
//! # Workspace: varias dbs y dedupe por contenido
//!
//! Una sola db no basta para juzgar una llamada: `execRocketBotDB` resuelve el
//! nombre contra la db del padre, pero el robot puede vivir en otra db del
//! workspace (una entrega corregida, el sandbox). Con una sola db el grafo no
//! distingue "no existe" de "no lo veo", asi que [`Analysis::ambiguous_calls`]
//! salia siempre vacia y una llamada a un robot inexistente se perdia en silencio
//! (issue #12), que es justo lo que fallaria en runtime. [`analyse_with_workspace`]
//! carga los hechos de todas las dbs y [`Analysis::missing_calls`] recoge las
//! llamadas cuyo destino no esta en ninguna.
//!
//! El mismo robot esta copiado en varias dbs de entrega, asi que [`load_workspace`]
//! deduplica por contenido: medido en el workspace real, 31 filas son 20 robots
//! distintos y sin dedupe se cuentan 11 robots de mas, con sus contratos
//! duplicados. La db analizada va primera para que el hecho del robot pedido
//! conserve su etiqueta.
//!
//! # Limite conocido
//!
//! El analisis describe el **ultimo** snapshot de cada robot, mientras que el pack
//! puede construirse sobre uno anterior (`--snapshot`). En ese caso el flujo
//! listado y los hallazgos hablan de versiones distintas del mismo robot; se
//! acepta porque el caso normal es diagnosticar el estado actual y el historico
//! se usa para atribuir errores, no para listar hallazgos. La excepcion son los
//! errores: para ellos se usa el historial entero, que es justo lo que los
//! distingue de un vistazo al log.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

use crate::attribution::{self, NodeRef, RawError, SnapshotView, Verdict};
use crate::db;
use crate::error::{Error, Result};
use crate::graph::{self, Contract, Edge, RobotFacts};
use crate::logs;
use crate::rules::{self, Finding};
use crate::signature::{self, ErrorKind};
use crate::tree;

/// Resultado del analisis cruzado de un robot dentro de su db.
#[derive(Debug, Clone)]
pub struct Analysis {
    /// Hallazgos **accionables**: senalan una linea con un defecto concreto.
    ///
    /// Van **sin recortar**: el tope y el aviso de lo que queda fuera son cosa del
    /// render. Recortar aqui era el defecto del issue #13: 18 variables muertas y
    /// solo 2 visibles, sin que el lector supiera que faltaban 16.
    pub findings: Vec<Finding>,
    /// Hallazgos de **higiene**: afectan al robot entero (sin Try/Catch, IF sin
    /// else, anidamiento, variables muertas).
    ///
    /// Se separan para que no compitan por el cupo con lo accionable. Medido: eran
    /// el 36% de los hallazgos mostrados y son justo los que un lector ignora; en
    /// una seccion aparte no ensucian la lista que hay que atender.
    pub higiene: Vec<Finding>,
    /// Contratos en los que este robot es el padre: lo que ofrece a sus hijos.
    pub contracts_out: Vec<Contract>,
    /// Contratos en los que este robot es el hijo: lo que espera de sus padres.
    pub contracts_in: Vec<Contract>,
    /// Declaradas que siguen sin uso despues de descontar lo que leen los hijos.
    pub unused_declared: Vec<String>,
    /// Llamadas cuyo hijo no se puede resolver con certeza, como `"padre -> hijo"`.
    ///
    /// Se llena cuando el nombre existe **solo** fuera de la db del padre
    /// ([`Edge::ambiguous`]): el runner lo buscaria en su propia db, asi que la
    /// arista es la mejor conjetura y no una certeza. Con una sola db de hechos no
    /// hay forma de saberlo y la lista queda vacia; [`analyse_with_workspace`] es
    /// quien la puede llenar.
    pub ambiguous_calls: Vec<String>,
    /// Llamadas cuyo destino no existe en **ninguna** db del workspace, como
    /// `"padre L2.3 -> hijo"`.
    ///
    /// [`graph::build_edges`] las omite porque sin los hechos del hijo no hay
    /// contrato que calcular, y por eso una llamada a un robot inexistente se
    /// perdia en silencio: en runtime el runner no resuelve el nombre y el padre
    /// falla. La linea es la del nodo de llamada en el padre, que es lo unico que
    /// permite localizarla.
    pub missing_calls: Vec<String>,
    /// Errores del log del robot, ya clasificados y agrupados por firma.
    ///
    /// Vacio cuando no se pidieron logs: el analisis del codigo no depende de que
    /// el entorno tenga logs, y un pack sin ellos sigue siendo valido.
    pub errors: ErrorReport,
}

/// Longitud a la que se recorta el mensaje de un error.
///
/// El mensaje completo de un error real puede llevar una traza de Python de 20
/// lineas; con 160 caracteres cabe el defecto y el detalle que lo identifica de un
/// vistazo, que es lo que el agente necesita para decidir si abre el log.
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
/// La separacion no es cosmetica: sumar los cinco cubos es el panel en rojo
/// permanente que hace que se deje de mirar. `live` es lo accionable hoy;
/// `historical` es evidencia de un defecto ya corregido (o de codigo retirado);
/// `platform` y `noise` no se arreglan desde el robot.
#[derive(Debug, Clone, Default)]
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

/// Analiza un robot usando el resto de la db como contexto, **sin** logs.
///
/// Es la firma historica y la que usan los consumidores que no tienen (o no
/// quieren) el directorio de logs del entorno. Delega en [`analyse_with_logs`] con
/// `None`, asi que el informe de errores sale vacio y el render omite su seccion.
pub fn analyse(conn: &rusqlite::Connection, db_label: &str, robot: &str) -> Result<Analysis> {
    analyse_with_logs(conn, db_label, robot, None)
}

/// Analiza un robot usando el resto de la db como contexto y, si se pide, los
/// logs de ejecucion.
///
/// `logs` es `(directorio, maximo de ficheros)`. Se pide como `Option` en vez de
/// tener una ruta por defecto aqui a proposito: el motor no decide leer disco. El
/// que sabe si hay logs en este entorno es `pack::build_from_conn`, y asi un test
/// nunca acaba dependiendo de `C:\Rocketbot`.
///
/// Devuelve [`Error::RobotNotFound`] si el robot no esta en la db y deja pasar el
/// error de lectura si es **el robot pedido** el que no se puede decodificar: un
/// pack del que no se pueden leer los nodos no es un pack degradado, es una
/// mentira. Un robot distinto e ilegible solo se omite, porque como mucho hara
/// que su contrato no aparezca.
pub fn analyse_with_logs(
    conn: &rusqlite::Connection,
    db_label: &str,
    robot: &str,
    logs: Option<(&Path, usize)>,
) -> Result<Analysis> {
    analyse_with_workspace(&[(conn, db_label)], db_label, robot, logs)
}

/// Analiza un robot con los hechos de **todas** las dbs del workspace.
///
/// `conns` son las bases abiertas en solo lectura y su etiqueta (sin ruta
/// absoluta); `db_label` marca cual es la db del robot analizado, que debe estar
/// en la lista: es de donde salen sus nodos y, si se piden, sus logs.
///
/// La db pedida se procesa primero. Si el robot esta copiado en varias dbs, el
/// hecho que sobrevive al dedupe conserva su etiqueta, y las demas quedan en
/// [`RobotFacts::also_in`], que es lo que permite resolver las aristas que el
/// runner resolveria en la db del padre.
///
/// Devuelve [`Error::RobotNotFound`] si el robot no esta en `db_label` —aunque
/// exista en otra db del workspace— o si no se puede leer de ella: el pack de un
/// robot que no se puede decodificar es una mentira, no un pack degradado.
pub fn analyse_with_workspace(
    conns: &[(&rusqlite::Connection, &str)],
    db_label: &str,
    robot: &str,
    logs: Option<(&Path, usize)>,
) -> Result<Analysis> {
    // La db pedida primero: el dedupe conserva la primera aparicion, asi que el
    // hecho del robot analizado lleva la etiqueta de la db que el consumidor pidio.
    let mut ordered: Vec<(&rusqlite::Connection, &str)> = Vec::with_capacity(conns.len());
    ordered.extend(
        conns
            .iter()
            .copied()
            .filter(|(_, label)| *label == db_label),
    );
    ordered.extend(
        conns
            .iter()
            .copied()
            .filter(|(_, label)| *label != db_label),
    );

    let facts = load_workspace_of(&ordered, Some((db_label, robot)))?;
    let edges = graph::build_edges(&facts);
    let all_contracts = graph::contracts(&facts, &edges);

    // Un homonimo de otra db es otro hecho: se busca el que vive en la db pedida.
    let this = facts
        .iter()
        .find(|fact| fact.name == robot && fact.lives_in(db_label))
        .ok_or_else(|| Error::RobotNotFound(robot.to_string()))?;

    // `unused_declared` devuelve un mapa por robot y omite los que no tienen
    // variables muertas: hay que sacar la entrada de este robot y asumir vacio.
    let mut dead_by_robot = graph::unused_declared(&facts, &all_contracts);
    let unused_declared = dead_by_robot.remove(robot).unwrap_or_default();

    let collisions = tree::collisions(&this.nodes);
    // `usize::MAX`: no se recorta aqui. El render decide el tope por seccion y dice
    // cuantos hallazgos quedaron fuera.
    let (findings, higiene) = rules::evaluate_split(
        &this.nodes,
        &collisions,
        &unused_declared,
        this.declared.len(),
        usize::MAX,
    );

    // Los contratos se recalculan sobre las aristas de **este** hecho: las de un
    // homologo con contenido distinto no son suyas y presentarlas juntas haria que
    // el pack de un robot hablara de las llamadas de otro.
    let own_edges: Vec<Edge> = edges
        .iter()
        .filter(|edge| edge.parent == this.name && this.lives_in(&edge.parent_db))
        .cloned()
        .collect();
    let contracts_out = graph::contracts(&facts, &own_edges);
    let inbound_edges: Vec<Edge> = edges
        .iter()
        .filter(|edge| edge.child == this.name && this.lives_in(&edge.child_db))
        .cloned()
        .collect();
    let contracts_in = graph::contracts(&facts, &inbound_edges);

    // Los logs se leen despues de cargar los hechos: si el robot no existe, el
    // error tiene que ser el de siempre y no el de un directorio de logs ausente.
    let errors = match logs {
        Some((dir, max_files)) => {
            // La db pedida esta en `conns` por construccion: el robot se encontro en
            // ella, luego alguna conexion lleva esa etiqueta.
            let Some(analysed) = conns
                .iter()
                .find(|(_, label)| *label == db_label)
                .map(|(conn, _)| *conn)
            else {
                return Err(Error::RobotNotFound(robot.to_string()));
            };
            errors_for(analysed, robot, dir, max_files)?
        }
        None => ErrorReport::default(),
    };

    Ok(Analysis {
        findings,
        higiene,
        contracts_out,
        contracts_in,
        unused_declared,
        ambiguous_calls: ambiguous_calls(&edges, this),
        missing_calls: missing_call_labels(&graph::missing_calls(&facts), this),
        errors,
    })
}

/// Lee los logs del directorio y devuelve los errores del robot ya clasificados.
///
/// El recorrido es: ficheros mas recientes primero ([`logs::recent_files`]) ->
/// parseo -> solo lo del robot -> solo los `ERROR`/`EXCEPTION`/`FATAL` -> atribucion
/// temporal contra los snapshots -> cubo.
///
/// Un directorio sin logs devuelve un informe vacio, no un error: el pack sin
/// seccion de errores es valido y "no hay logs" no es un defecto del robot.
pub fn errors_for(
    conn: &rusqlite::Connection,
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
enum Bucket {
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
    fn from_verdict(verdict: Verdict) -> Self {
        match verdict {
            Verdict::Vivo => Bucket::Live,
            Verdict::PayloadCambiado => Bucket::PayloadChanged,
            Verdict::Historico | Verdict::Desconocido => Bucket::Historical,
        }
    }
}

/// Cubo decidible solo con el texto del error; `None` si hay que mirar el veredicto.
fn bucket_of(message: &str) -> Option<Bucket> {
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
fn robot_errors(robot: &str, logs_dir: &Path, max_files: usize) -> Vec<logs::LogEntry> {
    let mut out = Vec::new();
    for path in logs::recent_files(logs_dir, max_files) {
        // Un log ilegible se omite: es evidencia que falta, no un fallo del pack.
        let Some(texto) = logs::read_text(&path) else {
            continue;
        };
        out.extend(
            logs::for_robot(&logs::parse_text(&texto), robot)
                .into_iter()
                .filter(logs::LogEntry::is_error),
        );
    }
    // `recent_files` devuelve del mas nuevo al mas viejo y "la primera ocurrencia"
    // solo significa algo en orden cronologico. La ordenacion es estable, asi que
    // dos errores del mismo instante conservan el orden en que los escribio el
    // runner.
    out.sort_by_key(|e| e.timestamp);
    out
}

/// Snapshots del robot reducidos a `cmd_id -> nodo`, para la atribucion.
fn snapshot_views(conn: &rusqlite::Connection, robot: &str) -> Result<Vec<SnapshotView>> {
    let mut out = Vec::new();
    for snap in db::load_snapshots(conn, robot)? {
        let root = db::project_root(&snap.project);
        // El aplanado es el de `tree`: reimplementarlo aqui acabaria en dos
        // definiciones de "linea" y "parametros" que se desincronizan.
        let nodes = tree::flatten(commands_of(root))
            .into_iter()
            // Sin `id` el nodo no es direccionable y no puede cruzar con el log.
            .filter(|n| !n.id.is_empty())
            .map(|n| {
                (
                    n.id,
                    NodeRef {
                        line: n.line,
                        params: n.params,
                    },
                )
            })
            .collect();
        out.push(SnapshotView {
            id: snap.id,
            created_at: snap.created_at,
            nodes,
        });
    }
    Ok(out)
}

/// Primera linea del mensaje, acotada: la traza de Python no es parte del defecto.
fn short_message(message: &str) -> String {
    let first = message.lines().next().unwrap_or("").trim();
    let mut out: String = first.chars().take(ERROR_MESSAGE_MAX).collect();
    if first.chars().count() > ERROR_MESSAGE_MAX {
        // Un solo caracter en vez de "..." porque esta cadena se repite por cada
        // error del informe y el presupuesto del pack es de ~2,5k tokens.
        out.push('…');
    }
    out
}

fn max_version(a: Option<i64>, b: Option<i64>) -> Option<i64> {
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

/// Carga los hechos del ultimo snapshot de cada robot de cada db, deduplicando
/// por contenido: el mismo robot esta copiado en `robot_run.db` y
/// `entrega_corregida/robot_run.db`, y sin dedupe un workspace de 28 robots se
/// cuenta como 50 (medido en el real: 31 filas -> 20 robots distintos).
///
/// La clave es `(nombre, hash del raw_b64)`. Dos homonimos con contenido distinto
/// son dos hechos y se conservan: la db del padre decide cual resuelve el runner y
/// el consumidor necesita ver los dos. El mismo contenido bajo el mismo nombre es
/// el mismo robot copiado: se queda un hecho y las demas etiquetas van a
/// [`RobotFacts::also_in`], porque el runner busca el nombre en la db del padre y
/// una copia descartada no puede convertir una arista en ambigua. El nombre va en
/// la clave porque dos robots distintos con el mismo contenido (un renombrado)
/// siguen siendo dos nombres en el grafo: colapsarlos haria que las llamadas al
/// nombre descartado parecieran inexistentes.
///
/// El hash es el mismo `blake3` del `raw_b64` que usa
/// [`crate::versions::VersionInfo::hash`], sin recortar: aqui no se muestra, solo
/// se compara.
pub fn load_workspace(dbs: &[(&rusqlite::Connection, &str)]) -> Result<Vec<RobotFacts>> {
    load_workspace_of(dbs, None)
}

/// [`load_workspace`] con una db y un robot que el consumidor **exige**.
///
/// `required` es `(etiqueta_db, robot)`. De esa db el robot no puede faltar
/// ([`Error::RobotNotFound`]) ni fallar al decodificar (el error se propaga): es la
/// diferencia entre un pack degradado y un pack que miente. En las demas dbs un
/// robot ilegible solo se omite, porque como mucho hara que falte su contrato.
fn load_workspace_of(
    dbs: &[(&rusqlite::Connection, &str)],
    required: Option<(&str, &str)>,
) -> Result<Vec<RobotFacts>> {
    let mut facts: Vec<RobotFacts> = Vec::new();
    let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();

    for (conn, label) in dbs {
        let strict = required
            .filter(|(db_label, _)| db_label == label)
            .map(|(_, robot)| robot);
        for (fact, hash) in load_db_facts(conn, label, strict)? {
            let key = (fact.name.clone(), hash);
            match seen.get(&key).copied() {
                Some(idx) => {
                    // Copia identica en otra db: el hecho ya existe y solo se
                    // anota donde mas vive.
                    let kept = &mut facts[idx];
                    if !kept.lives_in(label) {
                        kept.also_in.push((*label).to_string());
                    }
                }
                None => {
                    seen.insert(key, facts.len());
                    facts.push(fact);
                }
            }
        }
    }

    // Sin conexion con la etiqueta pedida no se puede haber encontrado el robot.
    if let Some((db_label, robot)) = required {
        if !facts
            .iter()
            .any(|fact| fact.name == robot && fact.lives_in(db_label))
        {
            return Err(Error::RobotNotFound(robot.to_string()));
        }
    }

    Ok(facts)
}

/// Hechos del **ultimo** snapshot de cada robot de una db, con el hash del
/// contenido que usa el dedupe.
///
/// `load_snapshots` decodifica todas las versiones porque no hay consulta que pida
/// solo la ultima; con las dbs reales (decenas de versiones, cientos de KB) es
/// barato y evita una segunda forma de leer `bots` que se desincronice cuando
/// cambie el formato. Cuando llegue el daemon, esto se cachea por (db, robot).
fn load_db_facts(
    conn: &rusqlite::Connection,
    db_label: &str,
    required: Option<&str>,
) -> Result<Vec<(RobotFacts, String)>> {
    let mut out = Vec::new();

    for summary in db::list_robots(conn)? {
        let snaps = match db::load_snapshots(conn, &summary.name) {
            Ok(snaps) => snaps,
            // Ver doc de `analyse_with_workspace`: omite el resto, propaga el del
            // robot exigido.
            Err(_) if Some(summary.name.as_str()) != required => continue,
            Err(e) => return Err(e),
        };
        let Some(snap) = snaps.last() else {
            continue;
        };
        let root = db::project_root(&snap.project);
        out.push((
            RobotFacts {
                db: db_label.to_string(),
                also_in: Vec::new(),
                name: summary.name.clone(),
                nodes: tree::flatten(commands_of(root)),
                declared: declared_of(root),
            },
            content_hash(&snap.raw_b64),
        ));
    }

    if let Some(robot) = required {
        if !out.iter().any(|(fact, _)| fact.name == robot) {
            return Err(Error::RobotNotFound(robot.to_string()));
        }
    }
    Ok(out)
}

/// Hash del contenido de un snapshot: `blake3` del base64 tal cual esta en `bots`.
///
/// Se usa el base64 y no el JSON decodificado por la misma razon que
/// [`crate::versions::VersionInfo::hash`]: es lo que se copia entre dbs de entrega
/// y no depende de como se re-serialice el JSON.
fn content_hash(raw_b64: &str) -> String {
    blake3::hash(raw_b64.as_bytes()).to_hex().to_string()
}

/// Aristas ambiguas del robot, ya formateadas para el consumidor.
///
/// Se deduplican porque un hijo homologo en dos dbs ajenas genera una arista por
/// db y el agente solo necesita saber que la llamada no esta resuelta aqui. El
/// filtro es por hecho (`db` o copia), no solo por nombre: las llamadas de un
/// homonimo de otra db no son las de este robot.
fn ambiguous_calls(edges: &[Edge], this: &RobotFacts) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for edge in edges {
        if !edge.ambiguous || edge.parent != this.name || !this.lives_in(&edge.parent_db) {
            continue;
        }
        let call = format!("{} -> {}", edge.parent, edge.child);
        if !out.contains(&call) {
            out.push(call);
        }
    }
    out
}

/// Llamadas sin destino del robot, con la linea para que el aviso sea accionable.
///
/// Mismo criterio que [`ambiguous_calls`]: solo las de este hecho, sin repetir.
fn missing_call_labels(calls: &[graph::MissingCall], this: &RobotFacts) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for call in calls {
        if call.parent != this.name || !this.lives_in(&call.parent_db) {
            continue;
        }
        let label = format!("{} L{} -> {}", call.parent, call.line, call.child);
        if !out.contains(&label) {
            out.push(label);
        }
    }
    out
}

fn commands_of(root: &Value) -> &[Value] {
    match root.get("commands").and_then(Value::as_array) {
        Some(v) => v.as_slice(),
        None => &[],
    }
}

/// Variables del panel del robot (`project.vars`).
///
/// Es la misma fuente que usa el contrato: el panel del padre es exactamente lo
/// que hereda el hijo al ejecutarse dentro de su interprete. Se ignoran nombres
/// vacios y repetidos para que el recuento no se infle.
fn declared_of(root: &Value) -> BTreeSet<String> {
    let Some(arr) = root.get("vars").and_then(Value::as_array) else {
        return BTreeSet::new();
    };
    arr.iter()
        .filter_map(|v| v.get("name")?.as_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(parent: &str, child: &str, db: &str, ambiguous: bool) -> Edge {
        Edge {
            parent: parent.to_string(),
            child: child.to_string(),
            line: "1".to_string(),
            parent_db: "A".to_string(),
            child_db: db.to_string(),
            same_db: !ambiguous,
            ambiguous,
        }
    }

    fn hecho(db: &str, name: &str) -> RobotFacts {
        RobotFacts {
            db: db.to_string(),
            also_in: Vec::new(),
            name: name.to_string(),
            nodes: Vec::new(),
            declared: BTreeSet::new(),
        }
    }

    #[test]
    fn solo_las_aristas_ambiguas_del_padre_pedido() {
        let edges = vec![
            edge("orq", "comun", "A", false),
            edge("orq", "fantasma", "B", true),
            // Mismo hijo homologo en dos dbs: una sola mencion para el agente.
            edge("orq", "fantasma", "C", true),
            edge("otro", "fantasma", "B", true),
        ];

        assert_eq!(
            ambiguous_calls(&edges, &hecho("A", "orq")),
            vec!["orq -> fantasma".to_string()],
            "sin duplicados y sin las aristas de otros padres"
        );
        assert!(ambiguous_calls(&edges, &hecho("A", "nadie")).is_empty());
    }

    #[test]
    fn las_aristas_de_un_homonimo_de_otra_db_no_son_de_este_robot() {
        // `orq` existe en A y en B con contenido distinto: son dos hechos. Las
        // llamadas del de B no pueden aparecer en el analisis del de A.
        let mut edges = vec![edge("orq", "fantasma", "B", true)];
        edges[0].parent_db = "B".to_string();
        edges.push(edge("orq", "comun", "C", true));

        assert_eq!(
            ambiguous_calls(&edges, &hecho("A", "orq")),
            vec!["orq -> comun".to_string()],
            "solo la arista cuyo padre es el hecho de A"
        );

        let mut copia = hecho("A", "orq");
        copia.also_in = vec!["B".to_string()];
        assert_eq!(
            ambiguous_calls(&edges, &copia),
            vec!["orq -> fantasma".to_string(), "orq -> comun".to_string()],
            "una copia identica si es el mismo hecho en las dos dbs"
        );
    }

    #[test]
    fn las_llamadas_sin_destino_llevan_linea_y_no_se_repiten() {
        let calls = vec![
            graph::MissingCall {
                parent: "orq".to_string(),
                parent_db: "A".to_string(),
                line: "4".to_string(),
                child: "no_existe".to_string(),
            },
            // Misma linea repetida (Try/Catch comparten numeracion): un aviso.
            graph::MissingCall {
                parent: "orq".to_string(),
                parent_db: "A".to_string(),
                line: "4".to_string(),
                child: "no_existe".to_string(),
            },
            graph::MissingCall {
                parent: "otro".to_string(),
                parent_db: "A".to_string(),
                line: "2.1".to_string(),
                child: "tampoco".to_string(),
            },
        ];

        assert_eq!(
            missing_call_labels(&calls, &hecho("A", "orq")),
            vec!["orq L4 -> no_existe".to_string()],
            "con linea, sin duplicados y sin las llamadas de otros padres"
        );
        assert!(missing_call_labels(&calls, &hecho("A", "nadie")).is_empty());
    }

    #[test]
    fn las_declaradas_salen_del_panel_y_se_deduplican() {
        let root = serde_json::json!({
            "vars": [
                {"name": "url", "data": null},
                {"name": "  url  ", "data": "repetida"},
                {"name": "   ", "data": null},
                {"sin_nombre": true}
            ]
        });
        let declared = declared_of(&root);
        assert_eq!(declared.len(), 1);
        assert!(declared.contains("url"));

        let sin_panel = serde_json::json!({"commands": []});
        assert!(declared_of(&sin_panel).is_empty());
    }

    #[test]
    fn un_root_sin_commands_no_rompe_el_aplanado() {
        assert!(commands_of(&serde_json::json!({"vars": []})).is_empty());
    }

    #[test]
    fn el_mensaje_del_error_se_queda_en_la_primera_linea() {
        assert_eq!(
            short_message("NameError: name 'x' is not defined\nTraceback (most recent call last):"),
            "NameError: name 'x' is not defined",
            "la traza de Python no es parte del defecto"
        );
        assert_eq!(short_message("  con espacios  "), "con espacios");
        assert_eq!(short_message(""), "");
    }

    #[test]
    fn el_mensaje_largo_se_recorta_sin_romper_unicode() {
        let largo = "a".repeat(ERROR_MESSAGE_MAX + 10);
        let recortado = short_message(&largo);
        assert_eq!(recortado.chars().count(), ERROR_MESSAGE_MAX + 1);
        assert!(recortado.ends_with('…'));

        // Recortar por bytes partiria un caracter de dos y dejaria el pack con
        // texto invalido.
        let unicode = "ñ".repeat(ERROR_MESSAGE_MAX + 1);
        let recortado = short_message(&unicode);
        assert_eq!(recortado.chars().count(), ERROR_MESSAGE_MAX + 1);
        assert!(recortado.starts_with('ñ'));
    }

    #[test]
    fn el_veredicto_decide_el_cubo_y_lo_desconocido_no_parece_vivo() {
        assert_eq!(Bucket::from_verdict(Verdict::Vivo), Bucket::Live);
        assert_eq!(
            Bucket::from_verdict(Verdict::PayloadCambiado),
            Bucket::PayloadChanged
        );
        assert_eq!(Bucket::from_verdict(Verdict::Historico), Bucket::Historical);
        assert_eq!(
            Bucket::from_verdict(Verdict::Desconocido),
            Bucket::Historical,
            "sin version no es accionable contra el codigo de hoy"
        );
    }

    #[test]
    fn el_cubo_por_mensaje_separa_ruido_y_plataforma() {
        // Ruido de motor: no se arregla desde el robot.
        assert_eq!(
            bucket_of(r#"EXCEPTION IN (<string> zdev, L_1375 ""): Error: Incorrect padding"#),
            Some(Bucket::Noise)
        );
        // Defecto de un modulo de la plataforma: no es ruido transitorio.
        assert_eq!(
            bucket_of(r#"EXCEPTION IN (<string> datatransform, L_212 ""): AttributeError: x"#),
            Some(Bucket::Platform)
        );
        // Error del robot: hay que atribuirlo antes de colocarlo.
        assert_eq!(
            bucket_of(r#"EXCEPTION IN (<string> Rocketbot, L_91 ""): NameError: name 'x'"#),
            None
        );
    }

    #[test]
    fn la_version_del_grupo_es_la_mas_alta() {
        assert_eq!(max_version(None, None), None);
        assert_eq!(max_version(Some(25), None), Some(25));
        assert_eq!(max_version(None, Some(29)), Some(29));
        assert_eq!(max_version(Some(25), Some(29)), Some(29));
        assert_eq!(max_version(Some(29), Some(25)), Some(29));
    }
}
