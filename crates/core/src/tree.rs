use std::collections::BTreeMap;

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Branch {
    Main,
    Then,
    Else,
}

impl Branch {
    pub fn is_else(self) -> bool {
        matches!(self, Branch::Else)
    }
}

#[derive(Debug, Clone)]
pub struct FlatNode {
    pub line: String,
    pub id: String,
    pub cmd: String,
    pub group: Option<String>,
    pub option: Option<String>,
    pub desc: Option<String>,
    pub params: String,
    pub write_var: Option<String>,
    pub branch: Branch,
    pub depth: usize,
}

fn text_of(node: &Value, key: &str) -> Option<String> {
    text(node.get(key)?)
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

pub fn normalize_line(v: &Value) -> String {
    match v {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.trim().to_string(),
        _ => "?".to_string(),
    }
}

pub fn params_text(cmd: &Value) -> String {
    match cmd {
        Value::String(s) => {
            let t = s.trim();
            if t.starts_with('{') && t.ends_with('}') {
                if let Ok(obj @ Value::Object(_)) = serde_json::from_str::<Value>(t) {
                    return obj.to_string();
                }
            }
            t.to_string()
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn write_var(node: &Value) -> Option<String> {
    for key in ["var", "getvar", "var_"] {
        if let Some(s) = text_of(node, key) {
            return Some(s);
        }
    }
    None
}

fn walk(list: &[Value], branch: Branch, depth: usize, out: &mut Vec<FlatNode>) {
    for node in list {
        if !node.is_object() {
            continue;
        }
        let cmd = text_of(node, "father").unwrap_or_else(|| "?".to_string());
        out.push(FlatNode {
            line: normalize_line(node.get("line").unwrap_or(&Value::Null)),
            id: text_of(node, "id").unwrap_or_default(),
            cmd,
            group: text_of(node, "group"),
            option: text_of(node, "option"),
            desc: text_of(node, "description"),
            params: params_text(node.get("command").unwrap_or(&Value::Null)),
            write_var: write_var(node),
            branch,
            depth,
        });
        if let Some(children) = node.get("children").and_then(Value::as_array) {
            walk(children, Branch::Then, depth + 1, out);
        }
        if let Some(els) = node.get("else").and_then(Value::as_array) {
            walk(els, Branch::Else, depth + 1, out);
        }
    }
}

pub fn flatten(commands: &[Value]) -> Vec<FlatNode> {
    let mut out = Vec::new();
    walk(commands, Branch::Main, 0, &mut out);
    out
}

pub fn collisions(nodes: &[FlatNode]) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for n in nodes {
        *counts.entry(n.line.clone()).or_insert(0) += 1;
    }
    counts.retain(|_, c| *c > 1);
    counts
}

pub fn max_depth(nodes: &[FlatNode]) -> usize {
    nodes.iter().map(|n| n.depth + 1).max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn nodo(father: &str, line: Value, id: &str, children: Vec<Value>, els: Vec<Value>) -> Value {
        json!({
            "father": father,
            "command": "",
            "line": line,
            "id": id,
            "children": children,
            "else": els,
        })
    }

    #[test]
    fn aplana_en_preorden_con_ramas_y_profundidad() {
        let arbol = vec![nodo(
            "trycatch",
            json!(2),
            "t",
            vec![nodo("setVar", json!("2.1"), "a", vec![], vec![])],
            vec![nodo("log_message", json!("2.1"), "b", vec![], vec![])],
        )];
        let flat = flatten(&arbol);
        assert_eq!(flat.len(), 3);
        assert_eq!(flat[0].depth, 0);
        assert_eq!(flat[0].branch, Branch::Main);
        assert_eq!(flat[1].branch, Branch::Then);
        assert_eq!(flat[2].branch, Branch::Else);
        assert_eq!(flat[1].line, "2.1");
        assert_eq!(flat[2].line, "2.1");
    }

    #[test]
    fn detecta_lineas_duplicadas_entre_ramas() {
        let arbol = vec![nodo(
            "trycatch",
            json!(2),
            "t",
            vec![nodo("a", json!("2.1"), "x", vec![], vec![])],
            vec![nodo("b", json!("2.1"), "y", vec![], vec![])],
        )];
        let flat = flatten(&arbol);
        let col = collisions(&flat);
        assert_eq!(col.get("2.1"), Some(&2));
        assert_eq!(max_depth(&flat), 2);
    }

    #[test]
    fn params_json_se_normalizan_a_compacto() {
        let cmd = json!("{ \"url\": \"https://x\", \"id\": \"\" }");
        assert_eq!(params_text(&cmd), r#"{"url":"https://x","id":""}"#);
    }

    #[test]
    fn params_texto_y_expresion_se_conservan() {
        assert_eq!(params_text(&json!("{contador} < 10")), "{contador} < 10");
        assert_eq!(params_text(&json!("len({filas}) > 0")), "len({filas}) > 0");
    }

    #[test]
    fn linea_entera_y_linea_texto() {
        assert_eq!(normalize_line(&json!(12)), "12");
        assert_eq!(normalize_line(&json!("2.10.5")), "2.10.5");
        assert_eq!(normalize_line(&Value::Null), "?");
    }

    #[test]
    fn detecta_variable_de_escritura_en_las_tres_claves() {
        let n = json!({"father": "setVar", "var": "res", "line": 1, "id": "z"});
        assert_eq!(flatten(&[n])[0].write_var.as_deref(), Some("res"));
    }
}
