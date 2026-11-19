const URL_SEP: &str = "://";

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
        from = end.max(body);
    }
    spans
}

pub fn inside_any(spans: &[(usize, usize)], at: usize) -> bool {
    spans.iter().any(|&(start, end)| (start..end).contains(&at))
}

fn is_scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')
}

pub fn continues_scheme(text: &str, at: usize) -> bool {
    text[..at].chars().next_back().is_some_and(is_scheme_char)
}

pub fn inside_url_token(text: &str, at: usize) -> bool {
    let start = text[..at]
        .char_indices()
        .rev()
        .find(|(_, c)| {
            c.is_whitespace()
                || matches!(
                    c,
                    '"' | '\'' | ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}'
                )
        })
        .map_or(0, |(i, c)| i + c.len_utf8());
    text[start..at].contains(URL_SEP)
}

pub fn opens_token(text: &str, at: usize) -> bool {
    text[..at].chars().next_back().map_or(true, |c| {
        !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    })
}

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
    fn un_token_dentro_de_una_url_lo_sabe_por_la_cola() {
        let text = r#"x "https://ejemplo/C:/a" y"#;
        let at = text.find("C:/").expect("esta");
        assert!(inside_url_token(text, at));
        assert!(!inside_url_token("C:/a", 0));
        assert!(!inside_url_token("url: C:/a", 6));
    }

    #[test]
    fn los_indices_respetan_multibyte() {
        let text = "ñandu https://x";
        let spans = url_spans(text);
        assert!(text.is_char_boundary(spans[0].0));
        assert_eq!(&text[spans[0].0..spans[0].1], "https://x");
    }
}
