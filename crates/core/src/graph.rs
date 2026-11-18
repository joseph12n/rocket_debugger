//! Grafo de llamadas entre robots y contrato implicito de variables.
//!
//! `execRocketBotDB` no arranca un proceso nuevo: ejecuta el hijo dentro del
//! interprete del padre, asi que el hijo ve **todas** las variables del padre (y
//! puede pisar el valor heredado si declara una con el mismo nombre). Ese
//! contrato no esta escrito ni en el robot ni en el JSON: solo existe en la
//! documentacion del comando, y es la razon por la que una regla de "variable
//! declarada y nunca usada" se equivoca en un orquestador.
//!
//! Medido sobre 28 robots reales: 86 falsos positivos de 89 en un orquestador con
//! 8 hijos, porque sus variables no estan muertas: son la superficie de API de
//! los subrobots. El grafo no es una feature opcional, es un requisito de
//! correccion de las reglas.
//!
//! El modulo separa los pasos para poder probarlos y reutilizarlos por separado:
//!
//! 1. [`reads_and_writes`]: uso de variables dentro de un robot.
//! 2. [`build_edges`]: quien llama a quien, resolviendo el nombre del hijo contra
//!    las dbs disponibles.
//! 3. [`contracts`] y [`unused_declared`]: que provee el padre en la linea de la
//!    llamada, que lee el hijo y nadie le da, y que variables del padre siguen
//!    sin uso incluso contando lo que consumen sus hijos.
//!
//! Los robots que no existen en ninguna db **se omiten** en [`build_edges`]: un
//! hijo que no esta no tiene contrato que analizar, y una arista a un nombre
//! inexistente solo produciria `missing` falsos. La llamada no se pierde: sale en
//! [`missing_calls`], porque en runtime el runner no resuelve ese nombre y el
//! padre falla. Si el nombre solo existe en otras dbs la arista se emite con
//! [`Edge::ambiguous`], porque en ese caso no se puede afirmar que sea el robot
//! que resuelve el runner.
//!
//! Un workspace son **varias dbs** y el mismo robot suele estar copiado en mas de
//! una. `analysis::load_workspace` deduplica por contenido y guarda las etiquetas
//! descartadas en [`RobotFacts::also_in`]: el runner busca el hijo en la db del
//! padre, asi que una copia identica cuenta como "esta en la db del padre" aunque
//! el hecho que sobrevive al dedupe lleve la etiqueta de otra db. Sin eso, una
//! copia deduplicada pareceria un homologo ausente y marcaria la arista ambigua
//! sin serlo.

use std::collections::{BTreeMap, BTreeSet};

use crate::tree::FlatNode;

/// Variables que inyecta el entorno de Rocketbot: no las declara el robot.
///
/// Se excluyen de las lecturas porque aparecen en casi cualquier `params` y no
/// dicen nada de la interfaz del robot. Contarlas convertiria cada nodo en una
/// lectura y el contrato en ruido.
const RUNTIME_VARS: [&str; 6] = [
    "rb_home",
    "base_path",
    "rocketbot_last_status",
    "rocketbot_last_error",
    "temp_dir",
    "user_dir",
];

fn is_runtime(name: &str) -> bool {
    RUNTIME_VARS.contains(&name)
}

/// Hechos de un robot ya extraidos de su snapshot: lo minimo para analizar el
/// grafo sin volver a tocar la db.
#[derive(Debug, Clone)]
pub struct RobotFacts {
    /// Etiqueta de la db donde vive el robot. Distingue homologos: el mismo
    /// nombre puede existir en varias dbs con contenido distinto.
    pub db: String,
    /// Otras dbs del workspace donde vive una copia **identica** de este robot.
    ///
    /// El dedupe por contenido deja un solo hecho, pero el runner resuelve el
    /// nombre contra la db del padre: si esa db era la copia descartada, la
    /// arista saldria ambigua sin serlo. Estas etiquetas son las que permiten
    /// resolverla. Un homologo con contenido distinto **no** va aqui: es otro
    /// hecho y se conserva aparte.
    pub also_in: Vec<String>,
    pub name: String,
    pub nodes: Vec<FlatNode>,
    /// Variables del panel del robot (`project.vars`), no las que aparecen en el
    /// flujo. El contrato que hereda un hijo es el panel del padre: una variable
    /// declarada a `null` tambien existe en el namespace del hijo.
    pub declared: BTreeSet<String>,
    /// Valor declarado de cada variable, para las comprobaciones que miran el
    /// contenido (rutas migradas). El contrato sigue usando solo `declared`.
    pub vars: Vec<crate::paths::VarValue>,
}

impl RobotFacts {
    /// Etiquetas de db donde vive el hecho: la primaria primero y despues las
    /// copias identicas. El orden importa para que la resolucion prefiera la db
    /// del hecho y los listados sean deterministas.
    pub fn db_labels(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.db.as_str()).chain(self.also_in.iter().map(String::as_str))
    }

    /// `true` si el hecho vive en esa db, como primaria o como copia identica.
    ///
    /// Es la pregunta del runner ("este nombre existe en mi db?") y no la del
    /// dedupe ("cual es la etiqueta del hecho que quedo?"). Confundirlas marca
    /// como ambigua una llamada que el runner resuelve sin problema.
    pub fn lives_in(&self, db: &str) -> bool {
        self.db == db || self.also_in.iter().any(|label| label == db)
    }
}

/// Arista padre -> hijo, una por nodo `execRocketBotDB`.
///
/// `line` es la linea del nodo de llamada **en el padre** (puede repetirse en
/// robots con Try/Catch: `line` no es direccion unica).
///
/// `same_db` es la resolucion preferente: el runner busca el nombre en la db del
/// padre. Cuenta tambien una copia identica en esa db ([`RobotFacts::also_in`]),
/// porque el contenido es el mismo y el nombre resuelve. `ambiguous` avisa de que
/// solo habia homologos en otras dbs, asi que la arista es la mejor conjetura y
/// el consumidor no debe tratarla como certeza.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub parent: String,
    pub child: String,
    pub line: String,
    pub parent_db: String,
    pub child_db: String,
    pub same_db: bool,
    pub ambiguous: bool,
}

/// Contrato implicito de una llamada.
///
/// `provided` es lo que el padre ofrece en esa linea: sus declaradas (heredar el
/// namespace incluye una variable declarada aunque siga a `null`) mas las que
/// escribe antes de la llamada. `missing` es lo que el hijo lee y nadie le da: ni
/// el padre, ni una declaracion propia, ni una escritura propia, ni el runtime.
/// Eso es exactamente lo que fallaria en runtime con `NameError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contract {
    pub parent: String,
    pub child: String,
    pub line: String,
    pub provided: Vec<String>,
    pub missing: Vec<String>,
}

/// Uso de variables de un robot.
///
/// Devuelve dos mapas `variable -> lineas` (en orden de flujo y sin repetir linea
/// por variable): lecturas de placeholders en `params` y escrituras de
/// `write_var`. Las variables de runtime no cuentan como lectura.
pub fn reads_and_writes(
    nodes: &[FlatNode],
) -> (BTreeMap<String, Vec<String>>, BTreeMap<String, Vec<String>>) {
    let mut reads: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut writes: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for node in nodes {
        for var in placeholders(&node.params) {
            if is_runtime(var) {
                continue;
            }
            push_line(&mut reads, var, &node.line);
        }
        if let Some(var) = &node.write_var {
            push_line(&mut writes, var, &node.line);
        }
    }

    (reads, writes)
}

fn push_line(map: &mut BTreeMap<String, Vec<String>>, var: &str, line: &str) {
    let lines = map.entry(var.to_string()).or_default();
    // Un nodo puede citar `{x}` dos veces en el mismo `params`; para el contrato
    // es una sola lectura en esa linea.
    if !lines.iter().any(|known| known == line) {
        lines.push(line.to_string());
    }
}

/// Nombres `{var}` de un texto, sin regex.
///
/// El formato es un `{`, un identificador y un `}`; una dependencia nueva solo
/// para esto no se justifica.
///
/// Cuando el contenido no es un identificador se avanza **un** caracter y no
/// hasta el `}`: los `params` son a veces JSON (`{"url":"{url}"}`) y ahi el primer
/// `}` no cierra el placeholder sino la llave del objeto. Retroceder de uno en uno
/// deja que el escaneo encuentre los placeholders anidados.
fn placeholders(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(offset) = text[i + 1..].find('}') {
                let end = i + 1 + offset;
                let name = &text[i + 1..end];
                if is_var_name(name) {
                    out.push(name);
                    i = end + 1;
                    continue;
                }
            }
        }
        i += 1;
    }

    out
}

/// Identificador simple: empieza por letra o `_` y sigue con alfanumericos o `_`.
///
/// Se descarta todo lo demas (expresiones, claves JSON, `obj.atributo`): no es una
/// variable que el contrato pueda proveer con una sustitucion textual.
fn is_var_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// Clave de comparacion de lineas.
///
/// `line` es una ruta de indices (`2.10.1`), no un decimal: `2.10` se ejecuta
/// **despues** de `2.9` y antes que `2.10.1`. Comparar como texto invierte el
/// primer caso, y ese es justo el error que decide si una variable estaba provista
/// antes de una llamada. Un tramo no numerico cuenta como 0: solo aparece en datos
/// mal formados y asi el orden sigue siendo total.
fn line_key(line: &str) -> Vec<u64> {
    line.split('.')
        .map(|part| part.trim().parse().unwrap_or(0))
        .collect()
}

/// `a` se ejecuta antes que `b` si su ruta de indices es menor. Una ruta que es
/// prefijo de otra va antes: `2.10` < `2.10.1`.
fn is_before(a: &str, b: &str) -> bool {
    line_key(a) < line_key(b)
}

/// Construye las aristas del grafo.
///
/// Una arista por nodo `execRocketBotDB`; el nombre del hijo es el `params`
/// recortado de comillas (el selector de Studio lo guarda tal cual y algunos
/// exports lo citan).
///
/// Resolucion: se prefiere una db que el padre tambien vea (la suya, o una copia
/// identica del hijo en ella, ver [`RobotFacts::also_in`]); si solo existe en dbs
/// ajenas se emite una arista por cada homologo, todas marcadas [`Edge::ambiguous`]
/// en orden de hechos. Los nombres que no existen en ninguna db no generan arista
/// ([`missing_calls`] avisa de ellos) y los `params` vacios tampoco.
pub fn build_edges(facts: &[RobotFacts]) -> Vec<Edge> {
    let mut edges = Vec::new();

    for parent in facts {
        for node in &parent.nodes {
            if node.cmd != "execRocketBotDB" {
                continue;
            }
            let Some(child) = child_name(&node.params) else {
                continue;
            };

            // Los hechos del hijo, no sus etiquetas: el dedupe deja un hecho por
            // contenido y es `also_in` quien conserva las demas dbs.
            let candidates: Vec<&RobotFacts> =
                facts.iter().filter(|fact| fact.name == child).collect();
            if candidates.is_empty() {
                // Hijo inexistente: no hay contrato que calcular. [`missing_calls`]
                // es quien avisa de la llamada (ver doc del modulo).
                continue;
            }

            match shared_db(parent, &candidates) {
                // El nombre existe en una db que el padre tambien ve: resolucion
                // preferente, igual que hace el runner.
                Some(db) => edges.push(Edge {
                    parent: parent.name.clone(),
                    child,
                    line: node.line.clone(),
                    parent_db: parent.db.clone(),
                    child_db: db,
                    same_db: true,
                    ambiguous: false,
                }),
                None => {
                    // Solo homologos en dbs ajenas: una arista por candidato, todas
                    // marcadas `ambiguous` porque no se puede elegir.
                    for fact in candidates {
                        edges.push(Edge {
                            parent: parent.name.clone(),
                            child: child.clone(),
                            line: node.line.clone(),
                            parent_db: parent.db.clone(),
                            child_db: fact.db.clone(),
                            same_db: false,
                            ambiguous: true,
                        });
                    }
                }
            }
        }
    }

    edges
}

/// Db donde el padre encontraria al hijo, si comparten alguna.
///
/// La db del padre manda: es donde el runner busca el nombre. Si el hijo solo
/// comparte una copia identica (dos dbs con la misma copia del padre), esa copia
/// resuelve igual porque el contenido es el mismo. Sin db compartida devuelve
/// `None`: solo hay homologos en otras dbs y la arista no se puede afirmar.
fn shared_db(parent: &RobotFacts, candidates: &[&RobotFacts]) -> Option<String> {
    if candidates.iter().any(|fact| fact.lives_in(&parent.db)) {
        return Some(parent.db.clone());
    }

    // Cualquier otra db compartida, la menor por etiqueta para que dos
    // ejecuciones produzcan el mismo grafo.
    let mut shared: BTreeSet<&str> = BTreeSet::new();
    for fact in candidates {
        for db in fact.db_labels() {
            if parent.lives_in(db) {
                shared.insert(db);
            }
        }
    }
    shared.into_iter().next().map(str::to_string)
}

/// Llamada a un robot que no existe en ninguna db del workspace.
///
/// `line` es la linea del nodo de llamada **en el padre**, que es lo unico que
/// permite localizarla: sin ella el aviso no se puede ni mirar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingCall {
    pub parent: String,
    pub parent_db: String,
    pub line: String,
    pub child: String,
}

/// Llamadas cuyo destino no existe en **ninguna** db del workspace.
///
/// [`build_edges`] las omite porque sin los hechos del hijo no hay contrato que
/// calcular, y esa omision silenciosa era el punto ciego: en runtime el runner no
/// resuelve el nombre y el padre falla. Se devuelven con la linea para que el
/// aviso sea accionable y sin deduplicar (`line` no es unico en robots con
/// Try/Catch: agrupar es decision del consumidor).
pub fn missing_calls(facts: &[RobotFacts]) -> Vec<MissingCall> {
    let mut out = Vec::new();

    for parent in facts {
        for node in &parent.nodes {
            if node.cmd != "execRocketBotDB" {
                continue;
            }
            let Some(child) = child_name(&node.params) else {
                continue;
            };
            // Basta el nombre: el runner falla igual si el hijo no esta en su db,
            // pero aqui la pregunta es si el robot existe en algun sitio del
            // workspace. Si existe solo en otra db, la arista es ambigua, no
            // inexistente.
            if facts.iter().any(|fact| fact.name == child) {
                continue;
            }
            out.push(MissingCall {
                parent: parent.name.clone(),
                parent_db: parent.db.clone(),
                line: node.line.clone(),
                child,
            });
        }
    }

    out
}

/// El nombre del hijo es el `params` recortado de comillas simples o dobles.
fn child_name(params: &str) -> Option<String> {
    let unquoted = strip_quotes(params.trim()).trim();
    if unquoted.is_empty() {
        None
    } else {
        Some(unquoted.to_string())
    }
}

fn strip_quotes(s: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = s
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    s
}

/// Calcula el contrato de cada arista.
///
/// Las aristas cuyo padre o hijo no estan en `facts` se omiten: sin los hechos de
/// los dos lados no hay contrato que calcular.
pub fn contracts(facts: &[RobotFacts], edges: &[Edge]) -> Vec<Contract> {
    let mut out = Vec::with_capacity(edges.len());

    for edge in edges {
        let Some(parent) = find_fact(facts, &edge.parent_db, &edge.parent) else {
            continue;
        };
        let Some(child) = find_fact(facts, &edge.child_db, &edge.child) else {
            continue;
        };

        // Declaradas + escritas antes de la llamada. El orden de escritura importa:
        // una variable asignada despues de la llamada no existe todavia para el
        // hijo, y eso es lo que distingue un contrato roto de uno correcto.
        let mut provided: BTreeSet<String> = parent.declared.clone();
        for node in &parent.nodes {
            let Some(var) = &node.write_var else {
                continue;
            };
            if is_before(&node.line, &edge.line) {
                provided.insert(var.clone());
            }
        }

        let (child_reads, child_writes) = reads_and_writes(&child.nodes);
        let mut missing = Vec::new();
        for var in child_reads.keys() {
            if is_runtime(var)
                || provided.contains(var)
                || child.declared.contains(var)
                || child_writes.contains_key(var)
            {
                continue;
            }
            missing.push(var.clone());
        }

        out.push(Contract {
            parent: edge.parent.clone(),
            child: edge.child.clone(),
            line: edge.line.clone(),
            provided: provided.into_iter().collect(),
            missing,
        });
    }

    out
}

/// Busca los hechos de un robot por `(db, nombre)` y cae al nombre suelto.
///
/// La caida sirve cuando el consumidor reconstruye aristas a mano o cuando las
/// etiquetas de db no coinciden literalmente; con homologos en varias dbs la
/// resolucion exacta sigue ganando. Entre dos homologos gana el que **vive** en
/// esa db (es el caso de una copia identica deduplicada, cuya etiqueta primaria es
/// otra): sin ese paso, la caida por nombre podria devolver el homologo con
/// contenido distinto y calcular su contrato.
fn find_fact<'a>(facts: &'a [RobotFacts], db: &str, name: &str) -> Option<&'a RobotFacts> {
    facts
        .iter()
        .find(|fact| fact.db == db && fact.name == name)
        .or_else(|| {
            facts
                .iter()
                .find(|fact| fact.name == name && fact.lives_in(db))
        })
        .or_else(|| facts.iter().find(|fact| fact.name == name))
}

/// Variables declaradas que ni se leen, ni se escriben, ni las lee un hijo.
///
/// Es el filtro que elimina los 86 falsos positivos del orquestador: una variable
/// puede no aparecer en ningun `params` del padre y aun asi ser la entrada de un
/// subrobot. Solo se devuelven los robots con al menos una variable muerta; un
/// mapa lleno de vectores vacios obligaria al consumidor a filtrar sin aportar
/// nada.
///
/// La salida va por nombre de robot, no por `(db, nombre)`: si el mismo nombre
/// aparece en varias dbs se unen sus hechos y sus hijos, de modo que una variable
/// usada en cualquiera de las copias no se marca (preferimos no marcarla a
/// inventar un hallazgo).
pub fn unused_declared(
    facts: &[RobotFacts],
    child_contracts: &[Contract],
) -> BTreeMap<String, Vec<String>> {
    // Lecturas de los hijos por robot padre. Se acumula el nombre de la variable,
    // no la linea: el mismo hijo puede llamarse varias veces y el uso es el mismo.
    let mut read_by_child: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for contract in child_contracts {
        for child in facts.iter().filter(|fact| fact.name == contract.child) {
            let used = read_by_child.entry(contract.parent.as_str()).or_default();
            for node in &child.nodes {
                for var in placeholders(&node.params) {
                    if !is_runtime(var) {
                        used.insert(var);
                    }
                }
            }
        }
    }

    let mut declared_by_robot: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut used_by_robot: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for fact in facts {
        let used = used_by_robot.entry(fact.name.as_str()).or_default();
        for node in &fact.nodes {
            if let Some(var) = &node.write_var {
                used.insert(var.as_str());
            }
            for var in placeholders(&node.params) {
                if !is_runtime(var) {
                    used.insert(var);
                }
            }
        }
        declared_by_robot
            .entry(fact.name.as_str())
            .or_default()
            .extend(fact.declared.iter().map(String::as_str));
    }

    let mut dead_by_robot: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (robot, declared) in &declared_by_robot {
        let used = used_by_robot.get(robot);
        let from_child = read_by_child.get(robot);
        let mut dead = BTreeSet::new();
        for var in declared {
            // Las runtime no se pueden "eliminar" y sus lecturas se excluyen a
            // proposito: contarlas aqui reinyectaria el ruido que quita el grafo.
            if is_runtime(var) {
                continue;
            }
            let alive = used.is_some_and(|set| set.contains(var))
                || from_child.is_some_and(|set| set.contains(var));
            if !alive {
                dead.insert((*var).to_string());
            }
        }
        if !dead.is_empty() {
            dead_by_robot.insert((*robot).to_string(), dead.into_iter().collect());
        }
    }

    dead_by_robot
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Branch;

    fn nodo(line: &str, cmd: &str, params: &str, write: Option<&str>) -> FlatNode {
        FlatNode {
            line: line.to_string(),
            id: format!("id-{line}-{cmd}"),
            cmd: cmd.to_string(),
            group: None,
            option: None,
            desc: None,
            params: params.to_string(),
            write_var: write.map(str::to_string),
            branch: Branch::Main,
            depth: 0,
        }
    }

    /// Nodo de llamada tal como lo guarda Studio: el nombre va en `params`, a
    /// veces entre comillas.
    fn llamada(line: &str, child: &str) -> FlatNode {
        nodo(line, "execRocketBotDB", &format!("\"{child}\""), None)
    }

    fn hechos(db: &str, name: &str, declared: &[&str], nodes: Vec<FlatNode>) -> RobotFacts {
        RobotFacts {
            db: db.to_string(),
            also_in: Vec::new(),
            name: name.to_string(),
            nodes,
            declared: declared.iter().map(|s| s.to_string()).collect(),
            vars: Vec::new(),
        }
    }

    #[test]
    fn orquestador_declara_tres_vars_y_llama_a_dos_hijos() {
        let orq = hechos(
            "entrega\\robot.db",
            "orq",
            &["busqueda", "ruta_salida", "destinatario"],
            vec![
                nodo("1", "setVar", "\"cafe\"", Some("busqueda")),
                llamada("2", "hijo_web"),
                llamada("3", "hijo_excel"),
            ],
        );
        let web = hechos(
            "entrega\\robot.db",
            "hijo_web",
            &[],
            vec![nodo("2.1", "log_message", "Buscando {busqueda}", None)],
        );
        let excel = hechos(
            "entrega\\robot.db",
            "hijo_excel",
            &[],
            vec![nodo("2.1", "setVar", "\"{ruta_salida}\"", Some("salida"))],
        );
        let facts = vec![orq, web, excel];

        let edges = build_edges(&facts);
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].child, "hijo_web");
        assert_eq!(edges[1].child, "hijo_excel");
        assert!(edges.iter().all(|e| e.same_db && !e.ambiguous));

        let cs = contracts(&facts, &edges);
        assert_eq!(cs.len(), 2);
        assert_eq!(
            cs[0].provided,
            vec![
                "busqueda".to_string(),
                "destinatario".to_string(),
                "ruta_salida".to_string()
            ],
            "las declaradas se heredan aunque esten a null"
        );
        assert!(cs[0].missing.is_empty());
        assert!(cs[1].missing.is_empty());

        // Solo `destinatario` esta muerta: las otras dos las consume un hijo o las
        // escribe el padre. Sin grafo, las tres parecerian muertas.
        let sin_uso = unused_declared(&facts, &cs);
        assert_eq!(sin_uso.len(), 1);
        assert_eq!(sin_uso["orq"], vec!["destinatario".to_string()]);
    }

    #[test]
    fn la_provision_depende_de_la_linea_de_la_llamada() {
        // Sin declaradas: aqui solo cuenta lo escrito antes de cada llamada.
        let padre = hechos(
            "A",
            "padre",
            &[],
            vec![
                nodo("2.1", "setVar", "1", Some("previa")),
                llamada("2.9", "hijo_a"),
                nodo("2.10", "setVar", "2", Some("tardia")),
                llamada("2.10.1", "hijo_b"),
            ],
        );
        let hijo_a = hechos(
            "A",
            "hijo_a",
            &[],
            // `base_path` es runtime y no debe aparecer en missing.
            vec![nodo(
                "1",
                "log_message",
                "{previa} {tardia} {base_path}",
                None,
            )],
        );
        let hijo_b = hechos(
            "A",
            "hijo_b",
            &[],
            vec![nodo("1", "log_message", "{previa} {tardia}", None)],
        );
        let facts = vec![padre, hijo_a, hijo_b];

        let edges = build_edges(&facts);
        let cs = contracts(&facts, &edges);
        assert_eq!(cs.len(), 2);

        assert_eq!(cs[0].provided, vec!["previa".to_string()]);
        assert_eq!(
            cs[0].missing,
            vec!["tardia".to_string()],
            "2.10 se ejecuta despues de 2.9 (comparar como texto diria lo contrario)"
        );

        assert_eq!(
            cs[1].provided,
            vec!["previa".to_string(), "tardia".to_string()]
        );
        assert!(
            cs[1].missing.is_empty(),
            "2.10 es prefijo de 2.10.1 y por tanto va antes"
        );
    }

    #[test]
    fn hijo_que_lee_una_var_que_nadie_provee_queda_en_missing() {
        let padre = hechos("A", "padre", &["declarada"], vec![llamada("2", "hijo")]);
        let hijo = hechos(
            "A",
            "hijo",
            &[],
            vec![nodo("1", "log_message", "{ruta_salida}", None)],
        );
        let facts = vec![padre, hijo];

        let cs = contracts(&facts, &build_edges(&facts));
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].provided, vec!["declarada".to_string()]);
        assert_eq!(cs[0].missing, vec!["ruta_salida".to_string()]);
    }

    #[test]
    fn una_var_que_consume_un_hijo_no_es_muerta() {
        let padre = hechos("A", "orq", &["busqueda"], vec![llamada("1", "hijo")]);
        let hijo = hechos(
            "A",
            "hijo",
            &[],
            vec![nodo("2", "log_message", "Buscando {busqueda}", None)],
        );
        let facts = vec![padre, hijo];

        let cs = contracts(&facts, &build_edges(&facts));
        assert!(cs[0].missing.is_empty());
        assert!(
            unused_declared(&facts, &cs).is_empty(),
            "la var es la API del hijo: no se puede marcar como muerta"
        );
    }

    #[test]
    fn las_vars_de_runtime_no_cuentan_como_lecturas() {
        let nodes = vec![nodo(
            "1",
            "log_message",
            "{base_path}\\x {rb_home} {temp_dir} {user_dir} {rocketbot_last_status} {rocketbot_last_error} {real}",
            None,
        )];
        let (reads, writes) = reads_and_writes(&nodes);
        assert_eq!(
            reads.keys().cloned().collect::<Vec<_>>(),
            vec!["real".to_string()]
        );
        assert!(writes.is_empty());
    }

    #[test]
    fn extrae_placeholders_de_json_y_expresiones() {
        let nodes = vec![
            nodo("1", "setVar", r#"{"url":"{url}","ruta":"{ruta}"}"#, None),
            nodo("2", "if", "len({filas}) > 0 and {contador} < 10", None),
            // Basura que no es un identificador: no se inventan variables.
            nodo("3", "log_message", "{} {a b} {obj.atributo}", None),
        ];
        let (reads, _) = reads_and_writes(&nodes);
        assert_eq!(
            reads.keys().cloned().collect::<Vec<_>>(),
            vec![
                "contador".to_string(),
                "filas".to_string(),
                "ruta".to_string(),
                "url".to_string()
            ]
        );
    }

    #[test]
    fn resuelve_same_db_y_marca_ambiguo_cuando_solo_esta_fuera() {
        let padre = hechos(
            "A",
            "padre",
            &[],
            vec![
                llamada("1", "comun"),
                llamada("2", "solo_en_b"),
                llamada("3", "fantasma"),
            ],
        );
        let comun_a = hechos("A", "comun", &[], vec![]);
        let comun_b = hechos("B", "comun", &[], vec![]);
        let solo_b = hechos("B", "solo_en_b", &[], vec![]);
        let facts = vec![padre, comun_a, comun_b, solo_b];

        let edges = build_edges(&facts);
        assert_eq!(edges.len(), 2, "el hijo inexistente se omite");
        assert_eq!(edges[0].child, "comun");
        assert_eq!(edges[0].child_db, "A");
        assert!(edges[0].same_db && !edges[0].ambiguous);

        assert_eq!(edges[1].child, "solo_en_b");
        assert_eq!(edges[1].child_db, "B");
        assert!(!edges[1].same_db && edges[1].ambiguous);
    }

    #[test]
    fn un_hijo_en_varias_dbs_ajenas_genera_una_arista_por_db() {
        let padre = hechos("A", "padre", &[], vec![llamada("1", "hijo")]);
        let en_b = hechos("B", "hijo", &[], vec![]);
        let en_c = hechos("C", "hijo", &[], vec![]);

        let edges = build_edges(&[padre, en_b, en_c]);
        assert_eq!(edges.len(), 2, "no se puede elegir entre dos homologos");
        assert!(edges.iter().all(|e| e.ambiguous && !e.same_db));
        assert_eq!(edges[0].child_db, "B");
        assert_eq!(edges[1].child_db, "C");
    }

    #[test]
    fn acepta_el_nombre_sin_comillas_y_recorta_espacios() {
        let padre = hechos(
            "A",
            "padre",
            &[],
            vec![nodo("1", "execRocketBotDB", "  hijo  ", None)],
        );
        let hijo = hechos("A", "hijo", &[], vec![]);
        let edges = build_edges(&[padre, hijo]);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].child, "hijo");
    }

    #[test]
    fn una_llamada_sin_destino_se_reporta_con_su_linea() {
        let facts = vec![
            hechos("A", "orq", &[], vec![llamada("4", "no_existe")]),
            // Un robot mas para que el padre no sea el unico hecho de la db.
            hechos("A", "otro", &[], vec![]),
        ];

        assert!(
            build_edges(&facts).is_empty(),
            "sin hechos del hijo no hay arista que analizar"
        );

        let faltan = missing_calls(&facts);
        assert_eq!(faltan.len(), 1);
        assert_eq!(faltan[0].parent, "orq");
        assert_eq!(faltan[0].parent_db, "A");
        assert_eq!(faltan[0].line, "4");
        assert_eq!(faltan[0].child, "no_existe");
    }

    #[test]
    fn una_llamada_resuelta_no_es_un_destino_perdido() {
        let facts = vec![
            hechos(
                "A",
                "orq",
                &[],
                vec![llamada("1", "hijo"), llamada("2", "hijo")],
            ),
            hechos("B", "hijo", &[], vec![]),
        ];

        // Existe en otra db: dos llamadas, dos aristas ambiguas, ninguna perdida.
        let edges = build_edges(&facts);
        assert_eq!(edges.len(), 2);
        assert!(edges.iter().all(|edge| edge.ambiguous));
        assert!(
            missing_calls(&facts).is_empty(),
            "el nombre existe en el workspace: no se puede avisar de inexistente"
        );
    }

    #[test]
    fn una_copia_identica_en_otra_db_resuelve_en_la_db_del_padre() {
        // El dedupe deja un solo hecho, con la etiqueta de la db B, pero la copia
        // A es identica: el padre vive en A y el runner resolveria el nombre alli.
        let padre = hechos("A", "orq", &[], vec![llamada("2", "hijo")]);
        let mut hijo = hechos("B", "hijo", &[], vec![]);
        hijo.also_in = vec!["A".to_string()];

        let edges = build_edges(&[padre, hijo]);
        assert_eq!(edges.len(), 1);
        assert!(
            edges[0].same_db && !edges[0].ambiguous,
            "una copia identica en la db del padre no es ambiguedad: {:?}",
            edges[0]
        );
        assert_eq!(edges[0].child_db, "A");
    }

    #[test]
    fn el_contrato_usa_el_homologo_que_vive_en_esa_db() {
        // `hijo` existe dos veces: una copia identica de A que tambien esta en C
        // (la db del padre) y un homologo distinto en B. El contrato tiene que
        // salir de la copia que el runner encontraria en C, no de la primera que
        // aparezca en la lista.
        let padre = hechos("C", "orq", &[], vec![llamada("2", "hijo")]);
        let homologo = hechos(
            "B",
            "hijo",
            &[],
            vec![nodo("1", "log_message", "{de_b}", None)],
        );
        let mut copia = hechos(
            "A",
            "hijo",
            &[],
            vec![nodo("1", "log_message", "{de_a}", None)],
        );
        copia.also_in = vec!["C".to_string()];

        let facts = vec![padre, homologo, copia];
        let cs = contracts(&facts, &build_edges(&facts));
        assert_eq!(cs.len(), 1);
        assert_eq!(
            cs[0].missing,
            vec!["de_a".to_string()],
            "el contrato de la copia que vive en C, no el del homologo de B"
        );
    }

    #[test]
    fn la_etiqueta_de_un_hecho_incluye_sus_copias() {
        let mut hecho = hechos("B", "hijo", &[], vec![]);
        hecho.also_in = vec!["A".to_string(), "C".to_string()];

        assert_eq!(hecho.db_labels().collect::<Vec<_>>(), ["B", "A", "C"]);
        assert!(hecho.lives_in("A") && hecho.lives_in("B") && hecho.lives_in("C"));
        assert!(
            !hecho.lives_in("D"),
            "una db sin copia no resuelve el nombre"
        );
    }
}
