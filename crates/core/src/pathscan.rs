//! Primitivas del escaneo de rutas absolutas en texto libre.
//!
//! Tres consumidores miran el mismo texto con contratos distintos: `rules` marca
//! la primera ruta de unidad de un parametro, `paths` extrae todas las rutas para
//! comprobarlas contra el disco y `redact` decide si un valor lleva una ruta del
//! usuario. Las politicas difieren a proposito (UNC solo cuenta para privacidad,
//! POSIX abierto solo para la migracion), pero el esqueleto era el mismo y estaba
//! copiado: `continues_scheme` tres veces y `path_snippet` dos, con topes que ya
//! habian derivado (60 para el mensaje, 260 para el diagnostico).
//!
//! Aqui vive una sola copia de las primitivas; cada consumidor conserva su
//! politica y su formato. Un cambio de criterio se hace una vez y se ve en los tres.
//!
//! Los indices son offsets de **byte**: quien recorte con ellos tiene que respetar
//! `is_char_boundary` (un texto con acentos tiene posiciones que no son frontera de
//! caracter y cortar ahi entra en panico).

/// Separador de esquema: lo que sigue a `://` es una direccion, no el disco.
const URL_SEP: &str = "://";

/// Tramos que ocupa cada URL (`esquema://resto`) dentro del texto.
///
/// El resto del texto puede ser una ruta aunque comparta cadena con ellas: en
/// `file:///C:/x` el `C:/` cae dentro del tramo y no es una ruta del usuario.
pub fn url_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut from = 0;
    while let Some(sep) = find_from(text, URL_SEP, from) {
        let start = text[..sep]
            .char_indices()
            .rev()
            .take_while(|(_, c)| is_scheme_char(*c))
            .last()
            .map_or(sep, |(i, _)| i);
        let body = sep + URL_SEP.len();
        let end = text[body..]
            .char_indices()
            .find(|(_, c)| is_url_stop(*c))
            .map_or(text.len(), |(i, _)| body + i);
        spans.push((start, end));
        // `body` garantiza avance aunque la URL se corte justo tras `://`.
        from = end.max(body);
    }
    spans
}

/// `true` si la posicion cae dentro de alguno de los tramos (fin exclusivo).
pub fn inside_any(spans: &[(usize, usize)], at: usize) -> bool {
    spans.iter().any(|&(start, end)| (start..end).contains(&at))
}

/// Caracteres que puede llevar un esquema (`https`, `file`, `git+ssh`).
fn is_scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')
}

/// `https:` -> la `s` no es una unidad porque la precede otra letra del esquema.
pub fn continues_scheme(text: &str, at: usize) -> bool {
    text[..at].chars().next_back().is_some_and(is_scheme_char)
}

/// `true` si la posicion abre token: la anterior no es parte de una palabra.
///
/// Lo usa la deteccion de raices POSIX: en `carpeta/home/x` el prefijo aparece,
/// pero la ruta es relativa y se conserva.
pub fn opens_token(text: &str, at: usize) -> bool {
    text[..at]
        .chars()
        .next_back()
        .is_none_or(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
}

/// Un caracter que corta la URL: a partir de ahi empieza otro token del valor.
fn is_url_stop(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';'
        )
}

fn find_from(text: &str, needle: &str, from: usize) -> Option<usize> {
    text.get(from..)?.find(needle).map(|i| from + i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn los_tramos_de_url_acaban_en_el_siguiente_delimitador() {
        let text = r#"url "https://x/a?b=1" y fin"#;
        let spans = url_spans(text);
        assert_eq!(spans.len(), 1);
        assert_eq!(&text[spans[0].0..spans[0].1], "https://x/a?b=1");
    }

    #[test]
    fn una_url_sin_delimitador_llega_al_final() {
        let text = "https://x/a";
        let spans = url_spans(text);
        assert_eq!(&text[spans[0].0..spans[0].1], text);
    }

    #[test]
    fn dos_urls_se_detectan_por_separado() {
        let text = "https://a y https://b";
        assert_eq!(url_spans(text).len(), 2);
    }

    #[test]
    fn el_fin_del_tramo_es_exclusivo() {
        let spans = [(0usize, 3usize)];
        assert!(inside_any(&spans, 0));
        assert!(inside_any(&spans, 2));
        assert!(!inside_any(&spans, 3));
    }

    #[test]
    fn la_letra_de_un_esquema_no_es_una_unidad() {
        assert!(continues_scheme("https:/", 5));
        assert!(!continues_scheme("C:/", 0));
        assert!(!continues_scheme("y C:/", 2));
    }

    #[test]
    fn un_token_abre_tras_espacio_o_simbolo_pero_no_tras_letra() {
        assert!(opens_token("/home/x", 0));
        assert!(opens_token("a /home/x", 2));
        assert!(opens_token(":/home/x", 1));
        assert!(!opens_token("carpeta/home/x", 7));
        assert!(!opens_token("a_b/home/x", 3));
    }

    #[test]
    fn los_indices_respetan_multibyte() {
        let text = "ñandu https://x";
        let spans = url_spans(text);
        assert!(text.is_char_boundary(spans[0].0));
        assert_eq!(&text[spans[0].0..spans[0].1], "https://x");
    }
}
