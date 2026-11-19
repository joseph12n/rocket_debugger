use std::collections::{HashMap, VecDeque};

use crate::tree::FlatNode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Removed,
    Changed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeChange {
    pub kind: ChangeKind,
    pub id: String,
    pub line: String,
    pub command: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

pub fn diff_nodes(before: &[FlatNode], after: &[FlatNode]) -> Vec<NodeChange> {
    let mut pendientes: HashMap<&str, VecDeque<usize>> = HashMap::new();
    for (i, node) in before.iter().enumerate() {
        pendientes.entry(node.id.as_str()).or_default().push_back(i);
    }

    let mut cambios = Vec::new();
    for node in after {
        let previo = pendientes
            .get_mut(node.id.as_str())
            .and_then(VecDeque::pop_front)
            .map(|i| &before[i]);
        let Some(previo) = previo else {
            cambios.push(agregado(node));
            continue;
        };

        if previo.params == node.params && previo.cmd == node.cmd {
            continue;
        }
        cambios.push(NodeChange {
            kind: ChangeKind::Changed,
            id: node.id.clone(),
            line: node.line.clone(),
            command: node.cmd.clone(),
            before: Some(previo.params.clone()),
            after: Some(node.params.clone()),
        });
    }

    let mut sobrantes: Vec<usize> = pendientes.into_values().flatten().collect();
    sobrantes.sort_unstable();
    for i in sobrantes {
        cambios.push(removido(&before[i]));
    }
    cambios
}

fn agregado(node: &FlatNode) -> NodeChange {
    NodeChange {
        kind: ChangeKind::Added,
        id: node.id.clone(),
        line: node.line.clone(),
        command: node.cmd.clone(),
        before: None,
        after: Some(node.params.clone()),
    }
}

fn removido(node: &FlatNode) -> NodeChange {
    NodeChange {
        kind: ChangeKind::Removed,
        id: node.id.clone(),
        line: node.line.clone(),
        command: node.cmd.clone(),
        before: Some(node.params.clone()),
        after: None,
    }
}

pub fn summarize(changes: &[NodeChange]) -> String {
    if changes.is_empty() {
        return "sin cambios".to_string();
    }

    let mut partes = Vec::new();
    for (kind, singular, plural) in [
        (ChangeKind::Added, "anadido", "anadidos"),
        (ChangeKind::Removed, "eliminado", "eliminados"),
        (ChangeKind::Changed, "cambiado", "cambiados"),
    ] {
        let n = changes.iter().filter(|c| c.kind == kind).count();
        if n == 1 {
            partes.push(format!("1 {singular}"));
        } else if n > 1 {
            partes.push(format!("{n} {plural}"));
        }
    }
    partes.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Branch;

    fn nodo(id: &str, line: &str, cmd: &str, params: &str) -> FlatNode {
        FlatNode {
            line: line.to_string(),
            id: id.to_string(),
            cmd: cmd.to_string(),
            group: None,
            option: None,
            desc: None,
            params: params.to_string(),
            write_var: None,
            branch: Branch::Main,
            depth: 0,
        }
    }

    fn kinds(changes: &[NodeChange]) -> Vec<(ChangeKind, &str)> {
        changes.iter().map(|c| (c.kind, c.id.as_str())).collect()
    }

    #[test]
    fn added_removed_y_changed_en_el_mismo_diff() {
        let before = vec![
            nodo("a", "2.1", "log_message", "inicio"),
            nodo("b", "2.2", "setVar", "{res} = 1"),
            nodo("c", "2.4", "close", "cerrar()"),
        ];
        let after = vec![
            nodo("a", "2.1", "log_message", "inicio"),
            nodo("b", "2.3", "setVar", "{res} = 2"),
            nodo("d", "2.4", "evaluateIf", "{x} > 0"),
        ];

        let cambios = diff_nodes(&before, &after);
        assert_eq!(
            kinds(&cambios),
            vec![
                (ChangeKind::Changed, "b"),
                (ChangeKind::Added, "d"),
                (ChangeKind::Removed, "c"),
            ],
            "orden: `after` primero, `Removed` al final"
        );

        let cambiado = &cambios[0];
        assert_eq!(cambiado.id, "b", "el id es la clave, no la linea");
        assert_eq!(
            cambiado.line, "2.3",
            "la linea que se muestra es la de la version `to`"
        );
        assert_eq!(cambiado.command, "setVar");
        assert_eq!(cambiado.before.as_deref(), Some("{res} = 1"));
        assert_eq!(cambiado.after.as_deref(), Some("{res} = 2"));

        let anadido = &cambios[1];
        assert_eq!(anadido.before, None, "un `Added` no tiene lado anterior");
        assert_eq!(anadido.line, "2.4");
        assert_eq!(anadido.command, "evaluateIf");
        assert_eq!(anadido.after.as_deref(), Some("{x} > 0"));

        let retirado = &cambios[2];
        assert_eq!(
            retirado.line, "2.4",
            "un `Removed` conserva la linea de la version `from`"
        );
        assert_eq!(retirado.command, "close");
        assert_eq!(retirado.before.as_deref(), Some("cerrar()"));
        assert_eq!(retirado.after, None, "un `Removed` no tiene lado posterior");
    }

    #[test]
    fn renumeracion_sin_cambio_de_params_no_es_un_cambio() {
        let before = vec![
            nodo("a", "2.1", "log_message", "inicio"),
            nodo("b", "2.2", "setVar", "{res} = 1"),
            nodo("c", "2.3", "close", "cerrar()"),
        ];
        let after = vec![
            nodo("a", "3.1", "log_message", "inicio"),
            nodo("b", "3.2", "setVar", "{res} = 1"),
            nodo("c", "3.3", "close", "cerrar()"),
        ];

        assert!(diff_nodes(&before, &after).is_empty());
        assert!(diff_nodes(&[], &[]).is_empty(), "sin nodos no hay diff");
        assert_eq!(summarize(&[]), "sin cambios");
    }

    #[test]
    fn el_fix_que_inserta_un_nodo_antes_solo_muestra_un_added() {
        let v26 = vec![
            nodo("log", "2.1", "log_message", "Paso Legacy..."),
            nodo("guarda", "2.2", "savexlsx", "guardar_filas()"),
            nodo("contador", "2.3", "evaluateIf", "{contador} < 10"),
        ];
        let v27 = vec![
            nodo("log", "2.1", "log_message", "Paso Legacy..."),
            nodo("init", "2.2", "setVar", "contador = 0"),
            nodo("guarda", "2.3", "savexlsx", "guardar_filas()"),
            nodo("contador", "2.4", "evaluateIf", "{contador} < 10"),
        ];

        let cambios = diff_nodes(&v26, &v27);
        assert_eq!(kinds(&cambios), vec![(ChangeKind::Added, "init")]);
        assert_eq!(summarize(&cambios), "1 anadido");
    }

    #[test]
    fn mismo_params_con_otro_cmd_es_un_cambio() {
        let before = vec![nodo("n", "1.1", "log_message", "reporte_final()")];
        let after = vec![nodo("n", "1.1", "raise_exception", "reporte_final()")];

        let cambios = diff_nodes(&before, &after);
        assert_eq!(kinds(&cambios), vec![(ChangeKind::Changed, "n")]);
        assert_eq!(
            cambios[0].command, "raise_exception",
            "`command` es el de la version `to`"
        );
        assert_eq!(
            cambios[0].before, cambios[0].after,
            "los parametros no cambiaron; lo que cambio es el comando"
        );
        assert_eq!(summarize(&cambios), "1 cambiado");
    }

    #[test]
    fn el_orden_es_estable_y_determinista() {
        let before = vec![
            nodo("z", "1.1", "log_message", "uno"),
            nodo("y", "1.2", "log_message", "dos"),
            nodo("x", "1.3", "log_message", "tres"),
            nodo("w", "1.4", "log_message", "cuatro"),
        ];
        let after = vec![
            nodo("b", "1.1", "log_message", "nuevo-b"),
            nodo("z", "1.2", "log_message", "uno-cambiado"),
            nodo("a", "1.3", "log_message", "nuevo-a"),
        ];

        let primera = diff_nodes(&before, &after);
        let segunda = diff_nodes(&before, &after);
        assert_eq!(
            primera, segunda,
            "misma entrada, misma salida: el orden no sale de un mapa"
        );

        assert_eq!(
            kinds(&primera),
            vec![
                (ChangeKind::Added, "b"),
                (ChangeKind::Changed, "z"),
                (ChangeKind::Added, "a"),
                (ChangeKind::Removed, "y"),
                (ChangeKind::Removed, "x"),
                (ChangeKind::Removed, "w"),
            ]
        );
    }

    #[test]
    fn summarize_singular_plural_y_vacio() {
        let cambio = |kind: ChangeKind, id: &str| NodeChange {
            kind,
            id: id.to_string(),
            line: "1.1".to_string(),
            command: "log_message".to_string(),
            before: Some("a".to_string()),
            after: Some("b".to_string()),
        };

        assert_eq!(summarize(&[]), "sin cambios");
        assert_eq!(
            summarize(&[cambio(ChangeKind::Added, "a")]),
            "1 anadido",
            "singular"
        );
        assert_eq!(
            summarize(&[
                cambio(ChangeKind::Added, "a"),
                cambio(ChangeKind::Added, "b"),
                cambio(ChangeKind::Removed, "c"),
                cambio(ChangeKind::Changed, "d"),
                cambio(ChangeKind::Changed, "e"),
                cambio(ChangeKind::Changed, "f"),
            ]),
            "2 anadidos, 1 eliminado, 3 cambiados",
            "plural por categoria y sin las categorias vacias"
        );
        assert_eq!(
            summarize(&[
                cambio(ChangeKind::Changed, "a"),
                cambio(ChangeKind::Changed, "b"),
                cambio(ChangeKind::Changed, "c"),
            ]),
            "3 cambiados",
            "solo se nombran las categorias con cambios"
        );
    }

    #[test]
    fn ids_repetidos_se_emparejan_por_ocurrencia_y_no_se_pierde_ninguno() {
        let dos_iguales = vec![
            nodo("dup", "1.1", "log_message", "primera"),
            nodo("dup", "1.2", "log_message", "segunda"),
        ];
        let cambiada = vec![
            nodo("dup", "1.1", "log_message", "primera"),
            nodo("dup", "1.2", "log_message", "segunda-cambiada"),
        ];

        let cambios = diff_nodes(&dos_iguales, &cambiada);
        assert_eq!(
            kinds(&cambios),
            vec![(ChangeKind::Changed, "dup")],
            "la primera ocurrencia coincide y la segunda cambio"
        );
        assert_eq!(cambios[0].before.as_deref(), Some("segunda"));
        assert_eq!(cambios[0].after.as_deref(), Some("segunda-cambiada"));

        let sobra_en_to = diff_nodes(
            &[nodo("dup", "1.1", "log_message", "uno")],
            &[
                nodo("dup", "1.1", "log_message", "uno"),
                nodo("dup", "1.2", "log_message", "dos"),
            ],
        );
        assert_eq!(kinds(&sobra_en_to), vec![(ChangeKind::Added, "dup")]);
        assert_eq!(sobra_en_to[0].after.as_deref(), Some("dos"));

        let sobra_en_from = diff_nodes(
            &[
                nodo("dup", "1.1", "log_message", "uno"),
                nodo("dup", "1.2", "log_message", "dos"),
            ],
            &[nodo("dup", "1.1", "log_message", "uno")],
        );
        assert_eq!(kinds(&sobra_en_from), vec![(ChangeKind::Removed, "dup")]);
        assert_eq!(sobra_en_from[0].before.as_deref(), Some("dos"));
        assert_eq!(sobra_en_from[0].line, "1.2");
    }

    #[test]
    fn un_cambio_en_la_rama_o_en_la_etiqueta_no_es_un_cambio() {
        let before = vec![nodo("n", "2.1", "setVar", "{res} = 1")];
        let mut after = before.clone();
        after[0].branch = Branch::Else;
        after[0].depth = 3;
        after[0].desc = Some("otra descripcion".to_string());

        assert!(diff_nodes(&before, &after).is_empty());
    }
}
