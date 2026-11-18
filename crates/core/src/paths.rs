//! Deteccion de rutas horneadas de **otro dispositivo** y propuesta de remapeo.
//!
//! `RUTA_ABSOLUTA` solo mira el formato: una ruta `C:\...` es poco portable, pero
//! puede ser perfectamente valida en la maquina donde corre el robot. Este modulo
//! responde una pregunta distinta y comprobable: *¿esta ruta existe aqui?*.
//!
//! # Por que agrupa por carpeta
//!
//! Un robot real trae diez variables bajo la misma carpeta de origen
//! (`ENTREGA_BBRE2`): una por salida, una por XML, una por perfil de Chrome. Un
//! hallazgo por variable se come el presupuesto de cinco del pack y repite diez
//! veces el mismo diagnostico. Se agrupa por la **primera carpeta que no existe**
//! y se propone el remapeo de esa carpeta, que es la unidad que el agente puede
//! aplicar de una vez.
//!
//! # Por que no marca todas las rutas
//!
//! - Si la ruta existe, no es un defecto.
//! - Las rutas del sistema (`C:\Windows`, `C:\Program Files`, `C:\Rocketbot`,
//!   `/usr`, ...) no se migran con el robot: se excluyen.
//! - Las rutas con placeholders (`{carpeta}`, `%USERPROFILE%`) no se pueden
//!   comprobar sin ejecutar y no se inventan.
//! - Una URL no es una ruta.
//!
//! # Por que el acceso al disco es un trait
//!
//! El motor es deterministico y sus tests no tocan `C:\Rocketbot`. [`PathProbe`]
//! permite que el pack de un test use un probe vacio o uno con raices temporales,
//! y que el CLI/MCP use [`LocalPaths`] para comprobar el disco de verdad. Sin un
//! probe que lo pida (`NoPaths`), la regla no se evalua y el golden no cambia.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::pathscan;
use crate::rules::{self, Finding};
use crate::tree::FlatNode;

/// Id de la regla en `rules/builtin.json`.
pub const RULE_ID: &str = "RUTA_MIGRADA";

/// Variables que se listan en el mensaje antes de resumir el resto.
const MAX_GROUP_VARS: usize = 4;

/// Profundidad y entradas maximas de la busqueda de un equivalente local.
///
/// Un robot puede tener varias rutas migradas y el pack de la bandeja se pide al
/// seleccionar una fila: recorrer el arbol entero de `C:\` no es aceptable. Con
/// estas cotas la busqueda queda en decenas de milisegundos en las carpetas del
/// proyecto.
const SEARCH_DEPTH: usize = 4;
const SEARCH_ENTRIES: usize = 2_000;

/// Una variable declarada, con su valor si es un escalar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarValue {
    pub name: String,
    pub value: Option<String>,
}

/// Variables del panel (`project.vars`): nombre y valor crudo, sin nombres vacios.
///
/// Es la fuente unica de la lista: [`vars_of`] aqui y `pack::vars_of` la consumen y
/// cada uno decide que hacer con el crudo (aqui, solo escalares; el pack, redactar).
/// Si duplicaran el filtro, un cambio en las reglas del panel solo valdria en uno.
pub fn declared_vars(root: &Value) -> Vec<(&str, Option<&Value>)> {
    let Some(array) = root.get("vars").and_then(Value::as_array) else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(|var| {
            let name = var.get("name")?.as_str()?.trim();
            if name.is_empty() {
                return None;
            }
            Some((name, var.get("data")))
        })
        .collect()
}

/// Variables de un proyecto ya decodificado, con el valor solo si es escalar.
pub fn vars_of(root: &Value) -> Vec<VarValue> {
    declared_vars(root)
        .into_iter()
        .map(|(name, data)| VarValue {
            name: name.to_string(),
            value: match data {
                Some(Value::String(text)) => Some(text.clone()),
                Some(Value::Number(number)) => Some(number.to_string()),
                _ => None,
            },
        })
        .collect()
}

/// Un candidato local para una ruta que no existe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemapCandidate {
    pub path: PathBuf,
    /// 0-100. Mas alto = mas componentes finales coinciden.
    pub score: u8,
}

/// Acceso al dispositivo donde corre el motor.
///
/// `exists` devuelve `None` cuando no se puede saber; en ese caso la ruta no se
/// marca (mejor no avisar que inventar un defecto).
pub trait PathProbe {
    /// `false` desactiva la regla entera. `NoPaths` lo usa para que el motor
    /// hermetico no toque el disco.
    fn enabled(&self) -> bool {
        true
    }

    fn exists(&self, path: &Path) -> Option<bool>;

    /// Equivalente local de una ruta que no existe. `None` = no hay.
    fn propose(&self, _missing: &Path) -> Option<RemapCandidate> {
        None
    }
}

/// Probe que no comprueba nada: es el que usan los tests y cualquier consumidor
/// que quiera un pack puro (solo db).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoPaths;

impl PathProbe for NoPaths {
    fn enabled(&self) -> bool {
        false
    }

    fn exists(&self, _path: &Path) -> Option<bool> {
        None
    }
}

/// Probe real: comprueba el sistema de ficheros y busca equivalentes bajo raices
/// concretas.
#[derive(Debug, Clone)]
pub struct LocalPaths {
    roots: Vec<PathBuf>,
}

impl LocalPaths {
    pub fn with_roots(roots: Vec<PathBuf>) -> Self {
        let mut unique: Vec<PathBuf> = Vec::new();
        for root in roots {
            if root.as_os_str().is_empty() || unique.contains(&root) {
                continue;
            }
            unique.push(root);
        }
        Self { roots: unique }
    }

    /// Raices naturales del proyecto: la carpeta de la db y su padre.
    ///
    /// Para `C:\rocketbot_bots\BBRE2\robot_run.db` son `BBRE2` y
    /// `C:\rocketbot_bots`: donde el usuario guarda los proyectos y sus entregas.
    pub fn for_db(db: &Path) -> Self {
        let mut roots = Vec::new();
        if let Some(parent) = db.parent().filter(|p| !p.as_os_str().is_empty()) {
            roots.push(parent.to_path_buf());
            if let Some(grandparent) = parent.parent().filter(|p| !p.as_os_str().is_empty()) {
                roots.push(grandparent.to_path_buf());
            }
        }
        Self::with_roots(roots)
    }

    /// `ROCKETDEBUG_LOCAL_ROOTS` (separado por `;`) manda; si no, [`Self::for_db`].
    pub fn from_env(db: &Path) -> Self {
        if let Ok(raw) = std::env::var("ROCKETDEBUG_LOCAL_ROOTS") {
            let roots: Vec<PathBuf> = raw
                .split(';')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(PathBuf::from)
                .collect();
            if !roots.is_empty() {
                return Self::with_roots(roots);
            }
        }
        Self::for_db(db)
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }
}

impl PathProbe for LocalPaths {
    fn exists(&self, path: &Path) -> Option<bool> {
        Some(path.exists())
    }

    fn propose(&self, missing: &Path) -> Option<RemapCandidate> {
        propose_in(&self.roots, missing)
    }
}

/// Rutas migradas de un robot, ya convertidas en hallazgos.
///
/// `line_hints` permite que el analisis aporte la linea de una variable que no se
/// usa en este robot sino en un subrobot: en ese caso la linea util es la de la
/// llamada `execRocketBotDB` en el padre. Si no hay pista, se busca el primer uso
/// en los nodos del propio robot.
///
/// Es `pub` para que un consumidor que ya tiene los nodos y las variables pueda
/// pedir solo esta regla; el pack la usa a traves del analisis.
pub fn findings(
    nodes: &[FlatNode],
    vars: &[VarValue],
    probe: &dyn PathProbe,
    line_hints: &BTreeMap<String, String>,
) -> Vec<Finding> {
    if !probe.enabled() {
        return Vec::new();
    }
    let Some(rule) = rules::builtin_rule(RULE_ID) else {
        return Vec::new();
    };

    let first_line = first_use_lines(nodes, vars);
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();

    // Variables primero: en los robots reales es donde vive la ruta horneada.
    for var in vars {
        let Some(value) = &var.value else {
            continue;
        };
        let Some(path) = absolute_value(value) else {
            continue;
        };
        if is_system_path(&path) {
            continue;
        }
        let Some(root) = missing_root(&path, probe) else {
            continue;
        };
        let group = groups.entry(root.clone()).or_default();
        group.root = root;
        group.vars.insert(var.name.clone());
        group.paths.insert(path);
        if let Some(line) = line_hints
            .get(&var.name)
            .or_else(|| first_line.get(&var.name))
        {
            group.push_line(line);
        }
    }

    // Parametros: una ruta literal en un comando tiene linea propia.
    for node in nodes {
        for path in paths_in_text(&node.params) {
            if is_system_path(&path) {
                continue;
            }
            let Some(root) = missing_root(&path, probe) else {
                continue;
            };
            let group = groups.entry(root.clone()).or_default();
            group.root = root;
            group.paths.insert(path);
            group.push_line(&node.line);
        }
    }

    merge_nested(&mut groups);

    groups
        .into_values()
        .map(|group| {
            let candidate = probe.propose(Path::new(&group.root));
            let mut message = format!(
                "ruta de otro dispositivo: {} no existe en este equipo",
                group.root
            );
            if !group.vars.is_empty() {
                let shown: Vec<&str> = group
                    .vars
                    .iter()
                    .take(MAX_GROUP_VARS)
                    .map(String::as_str)
                    .collect();
                message.push_str(&format!("; variables: {}", shown.join(", ")));
                if group.vars.len() > MAX_GROUP_VARS {
                    message.push_str(&format!(" (+{} mas)", group.vars.len() - MAX_GROUP_VARS));
                }
            }
            if group.paths.len() > 1 {
                message.push_str(&format!(" ({} rutas)", group.paths.len()));
            }

            let fix = match candidate {
                Some(candidate) => format!(
                    "remapea a {} (similitud {}/100) y aplica el cambio con el agente \
                     (rb_plan_patch/rb_apply_patch); no escribas robot.db",
                    candidate.path.display(),
                    candidate.score
                ),
                None => "sin equivalente local: decide la carpeta destino y aplica el cambio \
                         con el agente (rb_plan_patch/rb_apply_patch); no escribas robot.db"
                    .to_string(),
            };

            Finding {
                rule_id: rule.id.clone(),
                severity: rule.severity,
                kind: rule.kind,
                line: group.lines.first().cloned().unwrap_or_default(),
                message,
                fix,
            }
        })
        .collect()
}

#[derive(Debug, Default)]
struct Group {
    root: String,
    vars: BTreeSet<String>,
    paths: BTreeSet<String>,
    lines: Vec<String>,
}

impl Group {
    fn push_line(&mut self, line: &str) {
        if !line.is_empty() && !self.lines.iter().any(|existing| existing == line) {
            self.lines.push(line.to_string());
        }
    }
}

/// Funde un grupo que cuelga de otro: si falta `ENTREGA_BBRE2` y tambien su
/// subcarpeta `reportes`, el diagnostico es uno, no dos.
fn merge_nested(groups: &mut BTreeMap<String, Group>) {
    let mut roots: Vec<String> = groups.keys().cloned().collect();
    // Primero los cortos (menos profundidad): los hijos caen en ellos.
    roots.sort_by_key(|root| (root.matches('/').count(), root.len()));

    let mut merged: BTreeMap<String, Group> = BTreeMap::new();
    for root in roots {
        let Some(group) = groups.remove(&root) else {
            continue;
        };
        let parent = merged
            .keys()
            .find(|candidate| is_under(&root, candidate))
            .cloned();
        match parent {
            Some(parent) => {
                if let Some(target) = merged.get_mut(&parent) {
                    target.merge(group);
                }
            }
            None => {
                merged.insert(root, group);
            }
        }
    }
    *groups = merged;
}

impl Group {
    fn merge(&mut self, other: Group) {
        self.vars.extend(other.vars);
        self.paths.extend(other.paths);
        for line in other.lines {
            self.push_line(&line);
        }
    }
}

fn is_under(path: &str, parent: &str) -> bool {
    let path = path.to_ascii_lowercase();
    let parent = parent.to_ascii_lowercase();
    path.len() > parent.len()
        && path.starts_with(&parent)
        && path.as_bytes().get(parent.len()) == Some(&b'/')
}

/// Linea del primer nodo que lee cada variable (`{nombre}`).
fn first_use_lines(nodes: &[FlatNode], vars: &[VarValue]) -> BTreeMap<String, String> {
    let names: BTreeSet<&str> = vars.iter().map(|var| var.name.as_str()).collect();
    let mut out = BTreeMap::new();
    for node in nodes {
        for name in &names {
            if out.contains_key(*name) {
                continue;
            }
            if node.params.contains(&format!("{{{name}}}")) {
                out.insert((*name).to_string(), node.line.clone());
            }
        }
    }
    out
}

/// Ruta absoluta cuando el valor **entero** es una ruta.
///
/// Es el caso de una variable: `C:/Users/x/ENTREGA_BBRE2` con espacios en el
/// nombre se conserva entero, a diferencia del escaneo de un parametro.
fn absolute_value(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || is_dynamic(value) || value.contains("://") {
        return None;
    }
    let forward = value.replace('\\', "/");
    if is_drive_path(&forward) || (forward.starts_with('/') && !forward.starts_with("//")) {
        Some(value.to_string())
    } else {
        None
    }
}

/// Rutas absolutas embebidas en el texto de un parametro.
///
/// Reconoce `C:\...`, `C:/...` y `/...`. No reconoce URLs (`://`), rutas
/// relativas ni rutas con placeholders.
pub fn paths_in_text(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // Los indices se recorren byte a byte para encontrar `C:` o `/`; en un
        // texto con tildes hay posiciones que no son frontera de caracter y
        // cortar ahi entraria en panico.
        if !text.is_char_boundary(i) {
            i += 1;
            continue;
        }
        let start = if is_drive_at(bytes, i) {
            (!pathscan::continues_scheme(text, i) && !pathscan::inside_url_token(text, i))
                .then_some(i)
        } else if is_posix_at(text, i) {
            (!pathscan::inside_url_token(text, i)).then_some(i)
        } else {
            None
        };
        let Some(start) = start else {
            i += 1;
            continue;
        };
        let snippet = path_snippet(&text[start..]);
        if snippet.len() > 1 && !is_dynamic(&snippet) && !out.contains(&snippet) {
            out.push(snippet);
        }
        i = start + 1;
    }
    out
}

fn is_drive_at(bytes: &[u8], i: usize) -> bool {
    i + 2 < bytes.len()
        && bytes[i].is_ascii_alphabetic()
        && bytes[i + 1] == b':'
        && matches!(bytes[i + 2], b'\\' | b'/')
}

fn is_posix_at(text: &str, i: usize) -> bool {
    let bytes = text.as_bytes();
    if bytes.get(i) != Some(&b'/') || bytes.get(i + 1) == Some(&b'/') {
        return false;
    }
    // Un `/` dentro de `C:/...` no abre otra ruta: la anterior es `:`.
    let prev = text[..i].chars().next_back();
    matches!(
        prev,
        None | Some(' ' | '\t' | '\n' | '\r' | '"' | '\'' | '=' | '(' | '[' | '{' | ',')
    )
}

fn path_snippet(rest: &str) -> String {
    rest.chars()
        .take(260)
        .take_while(|c| {
            !matches!(
                c,
                '"' | '\'' | '\n' | '\r' | '}' | ']' | ';' | '|' | '<' | '>'
            )
        })
        .collect::<String>()
        .trim_end_matches(|c: char| c.is_whitespace() || c == ',' || c == '.')
        .to_string()
}

/// ¿La ruta esta en un arbol del sistema que no se migra con el robot?
fn is_system_path(path: &str) -> bool {
    let mut normalized = path.replace('\\', "/").to_ascii_lowercase();
    while normalized.starts_with("//") {
        normalized.remove(0);
    }
    const WINDOWS: [&str; 4] = [
        "c:/windows",
        "c:/program files",
        "c:/programdata",
        "c:/rocketbot",
    ];
    const POSIX: [&str; 9] = [
        "/usr",
        "/etc",
        "/var",
        "/bin",
        "/sbin",
        "/opt",
        "/system",
        "/library",
        "/applications",
    ];
    let posix = normalized.trim_start_matches('/');
    for prefix in WINDOWS {
        if normalized == prefix || normalized.starts_with(&format!("{prefix}/")) {
            return true;
        }
    }
    for prefix in POSIX {
        let prefix = prefix.trim_start_matches('/');
        if posix == prefix || posix.starts_with(&format!("{prefix}/")) {
            return true;
        }
    }
    // Git-Bash monta la unidad como `/c/...`; en Windows se normaliza antes de
    // comprobar, pero un consumidor puede pasar la forma cruda.
    normalized.starts_with("/c/windows")
}

/// Primera carpeta de la ruta que no existe en este dispositivo.
///
/// Devuelve `None` si la ruta existe entera o si no se puede comprobar (un probe
/// sin respuesta o una ruta dinamica).
fn missing_root(path: &str, probe: &dyn PathProbe) -> Option<String> {
    let (anchor, components) = split_components(path)?;
    let mut current = anchor;
    if probe.exists(Path::new(&current)) != Some(true) {
        return Some(current);
    }
    for component in components {
        current = join_component(&current, &component);
        match probe.exists(Path::new(&current)) {
            Some(true) => continue,
            Some(false) => return Some(current),
            None => return None,
        }
    }
    None
}

fn split_components(path: &str) -> Option<(String, Vec<String>)> {
    let mut forward = path.replace('\\', "/");
    if cfg!(windows) {
        if let Some(rest) = forward
            .strip_prefix("/c/")
            .or_else(|| forward.strip_prefix("/C/"))
        {
            forward = format!("C:/{rest}");
        }
    }
    if forward.contains("://") || is_dynamic(&forward) {
        return None;
    }

    let bytes = forward.as_bytes();
    let (anchor, rest) = if is_drive_path(&forward) {
        let letter = (bytes[0] as char).to_ascii_uppercase();
        (format!("{letter}:/"), &forward[3..])
    } else if forward.starts_with('/') && !forward.starts_with("//") {
        ("/".to_string(), &forward[1..])
    } else {
        return None;
    };

    let components: Vec<String> = rest
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .map(str::to_string)
        .collect();
    if components.is_empty() || components.iter().any(|part| part == "..") {
        return None;
    }
    Some((anchor, components))
}

fn is_drive_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/'
}

fn dynamic_mark(c: char) -> bool {
    matches!(c, '{' | '}' | '%' | '$')
}

fn is_dynamic(text: &str) -> bool {
    text.chars().any(dynamic_mark)
}

fn join_component(current: &str, component: &str) -> String {
    if current.ends_with('/') {
        format!("{current}{component}")
    } else {
        format!("{current}/{component}")
    }
}

/// Busca un equivalente local por coincidencia del final de la ruta.
///
/// La hoja (ultimo componente) es obligatoria; cada componente padre que tambien
/// coincida sube el puntaje. No se compara el principio de la ruta: la carpeta de
/// usuario cambia entre dispositivos y no es lo que hay que remapear.
fn propose_in(roots: &[PathBuf], missing: &Path) -> Option<RemapCandidate> {
    let text = missing.to_string_lossy();
    let (_, components) = split_components(&text)?;
    let leaf = components.last()?;
    let want_file = Path::new(leaf).extension().is_some();

    let mut best: Option<RemapCandidate> = None;
    let mut budget = SEARCH_ENTRIES;
    for root in roots {
        walk_for_leaf(
            root,
            0,
            leaf,
            &components,
            want_file,
            &mut best,
            &mut budget,
        );
        if budget == 0 {
            break;
        }
    }
    best
}

fn walk_for_leaf(
    dir: &Path,
    depth: usize,
    leaf: &str,
    components: &[String],
    want_file: bool,
    best: &mut Option<RemapCandidate>,
    budget: &mut usize,
) {
    if depth > SEARCH_DEPTH || *budget == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let matches_leaf = entry
            .file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case(leaf);
        let type_matches = if want_file {
            file_type.is_file()
        } else {
            file_type.is_dir()
        };
        if matches_leaf && type_matches {
            let score = score_candidate(&path, components);
            let better = match best {
                None => true,
                Some(current) => {
                    score > current.score
                        || (score == current.score && path.as_os_str() < current.path.as_os_str())
                }
            };
            if better {
                *best = Some(RemapCandidate {
                    path: path.clone(),
                    score,
                });
            }
        }
        if file_type.is_dir() && !skip_dir(&entry.file_name().to_string_lossy()) {
            walk_for_leaf(&path, depth + 1, leaf, components, want_file, best, budget);
        }
    }
}

fn skip_dir(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        ".git"
            | "node_modules"
            | "target"
            | "backups"
            | "backup"
            | "windows"
            | "program files"
            | "programdata"
            | "appdata"
            | "$recycle.bin"
    )
}

fn score_candidate(candidate: &Path, remote_components: &[String]) -> u8 {
    let candidate_text = candidate.to_string_lossy().replace('\\', "/");
    let candidate_components: Vec<String> = candidate_text
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect();
    let mut matched_parents = 0usize;
    for (remote, local) in remote_components
        .iter()
        .rev()
        .skip(1)
        .zip(candidate_components.iter().rev().skip(1))
    {
        if remote.eq_ignore_ascii_case(local) {
            matched_parents += 1;
        } else {
            break;
        }
    }
    (60 + 8 * matched_parents).min(92) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn el_escaneo_encuentra_drive_posix_y_evita_urls() {
        let text = concat!(
            "{\"src\":\"C:/Users/josep/Downloads/ENTREGA_BBRE2/mercadolibre.xlsx\",",
            "\"dst\":\"/c/Users/josep/Downloads/ENTREGA_BBRE2/reportes\",",
            "\"url\":\"https://ejemplo.com/C:/no-es-ruta\",",
            "\"plantilla\":\"{base_path}/x.xlsx\",",
            "\"relativa\":\"carpeta/local.xlsx\"}"
        );
        let paths = paths_in_text(text);
        assert!(
            paths.contains(&"C:/Users/josep/Downloads/ENTREGA_BBRE2/mercadolibre.xlsx".to_string())
        );
        assert!(paths.contains(&"/c/Users/josep/Downloads/ENTREGA_BBRE2/reportes".to_string()));
        assert!(
            paths.iter().all(|path| !path.contains("ejemplo.com")),
            "una URL no es una ruta: {paths:?}"
        );
        assert!(
            paths.iter().all(|path| !path.contains("{base_path}")),
            "una ruta con placeholder no se comprueba: {paths:?}"
        );
        assert!(
            paths.iter().all(|path| !path.starts_with("carpeta/")),
            "una ruta relativa no es absoluta: {paths:?}"
        );
    }

    #[test]
    fn una_ruta_con_espacios_se_conserva_entera() {
        let paths = paths_in_text(r#""exe":"C:/Program Files/MariaDB/bin/mariadb-dump.exe""#);
        assert_eq!(
            paths,
            vec!["C:/Program Files/MariaDB/bin/mariadb-dump.exe".to_string()]
        );
    }

    #[test]
    fn las_rutas_del_sistema_no_se_marcan() {
        for path in [
            r"C:\Windows\System32\cmd.exe",
            "C:/Program Files/MariaDB/bin/mariadb-dump.exe",
            "C:/ProgramData/Rocketbot/x",
            "C:/Rocketbot",
            "/usr/bin/python",
        ] {
            assert!(is_system_path(path), "deberia ser sistema: {path}");
        }
        for path in [
            "C:/Users/josep/Downloads/ENTREGA_BBRE2",
            "/c/Users/josep/Downloads/ENTREGA_BBRE2/reportes",
            "/home/josep/proyecto",
            // Un perfil de Chrome bajo AppData es dato del robot, no del SO:
            // si falta, es una ruta migrada mas.
            "C:/Users/josep/AppData/Local/chrome_profile",
        ] {
            assert!(!is_system_path(path), "no es sistema: {path}");
        }
    }

    #[test]
    fn el_valor_de_una_variable_solo_es_ruta_si_es_absoluta() {
        assert_eq!(
            absolute_value("C:/Users/x/ENTREGA_BBRE2/factura.xml").as_deref(),
            Some("C:/Users/x/ENTREGA_BBRE2/factura.xml")
        );
        assert_eq!(
            absolute_value("/Users/x/ENTREGA_BBRE2").as_deref(),
            Some("/Users/x/ENTREGA_BBRE2")
        );
        assert!(absolute_value("{base_path}/x.xlsx").is_none());
        assert!(absolute_value("https://ejemplo.com/x").is_none());
        assert!(absolute_value("relativa/x").is_none());
    }

    #[test]
    fn la_primera_carpeta_que_falta_es_la_raiz_del_hallazgo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let existing = tmp.path().join("Users").join("x").join("Downloads");
        std::fs::create_dir_all(&existing).expect("dirs");
        let path = existing.join("ENTREGA_BBRE2").join("mercadolibre.xlsx");
        let probe = LocalPaths::with_roots(vec![tmp.path().to_path_buf()]);
        let root = missing_root(&path.to_string_lossy(), &probe).expect("hay raiz que falta");
        assert_eq!(
            root,
            existing
                .join("ENTREGA_BBRE2")
                .to_string_lossy()
                .replace('\\', "/")
        );

        // Si existe entera, no hay hallazgo.
        std::fs::create_dir_all(existing.join("ENTREGA_BBRE2")).expect("dir");
        std::fs::write(
            existing.join("ENTREGA_BBRE2").join("mercadolibre.xlsx"),
            "x",
        )
        .expect("file");
        assert!(missing_root(&path.to_string_lossy(), &probe).is_none());
    }

    #[test]
    fn el_remapeo_encuentra_la_misma_hoja_y_puntua_los_padres() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let remote = tmp
            .path()
            .join("remote")
            .join("Downloads")
            .join("ENTREGA_BBRE2");
        let local = tmp
            .path()
            .join("local")
            .join("Downloads")
            .join("ENTREGA_BBRE2");
        std::fs::create_dir_all(&remote).expect("remote");
        std::fs::create_dir_all(&local).expect("local");
        let probe = LocalPaths::with_roots(vec![tmp.path().join("local")]);

        let candidate = probe
            .propose(&remote)
            .expect("hay carpeta local con la misma hoja");
        assert_eq!(candidate.path, local);
        assert!(
            candidate.score >= 68,
            "coincide la hoja y un padre: {candidate:?}"
        );

        let missing_leaf = tmp
            .path()
            .join("remote")
            .join("Downloads")
            .join("NO_EXISTE");
        std::fs::create_dir_all(&missing_leaf).expect("remote dir");
        let candidate = probe.propose(&missing_leaf);
        assert!(
            candidate.is_none(),
            "sin hoja igual no se inventa un candidato: {candidate:?}"
        );
    }

    #[test]
    fn dos_rutas_bajo_la_misma_carpeta_se_agrupan_en_un_hallazgo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let remote = tmp.path().join("remote");
        let base = remote.join("ENTREGA_BBRE2");
        std::fs::create_dir_all(&remote).expect("remote");
        // `base` no existe: las dos rutas comparten la misma raiz que falta.
        let vars = vec![
            VarValue {
                name: "ruta_salida".to_string(),
                value: Some(
                    base.join("mercadolibre.xlsx")
                        .to_string_lossy()
                        .into_owned(),
                ),
            },
            VarValue {
                name: "carpeta_salida".to_string(),
                value: Some(base.to_string_lossy().into_owned()),
            },
        ];
        let nodes = vec![FlatNode {
            line: "2".to_string(),
            id: "id".to_string(),
            cmd: "savexlsx".to_string(),
            group: None,
            option: None,
            desc: None,
            params: "{ruta_salida}".to_string(),
            write_var: None,
            branch: crate::tree::Branch::Main,
            depth: 0,
        }];
        let probe = LocalPaths::with_roots(vec![tmp.path().to_path_buf()]);
        let findings = findings(&nodes, &vars, &probe, &BTreeMap::new());
        assert_eq!(findings.len(), 1, "{findings:?}");
        let finding = &findings[0];
        assert_eq!(finding.rule_id, RULE_ID);
        assert_eq!(finding.line, "2");
        assert!(finding.message.contains("ruta_salida"), "{finding:?}");
        assert!(finding.message.contains("carpeta_salida"), "{finding:?}");
        assert!(finding.fix.contains("sin equivalente local"), "{finding:?}");
    }

    #[test]
    fn sin_probe_no_se_evalua_la_regla() {
        let nodes = vec![FlatNode {
            line: "1".to_string(),
            id: "id".to_string(),
            cmd: "setVar".to_string(),
            group: None,
            option: None,
            desc: None,
            params: "C:/Users/x/ENTREGA_BBRE2".to_string(),
            write_var: None,
            branch: crate::tree::Branch::Main,
            depth: 0,
        }];
        assert!(findings(&nodes, &[], &NoPaths, &BTreeMap::new()).is_empty());
    }
}
