use crate::pathscan;

const FERNET_PREFIX: &str = "gAAAAA";

const MIN_ENCRYPTED_LEN: usize = 40;

const SENSITIVE_TERMS: [&str; 14] = [
    "pass",
    "password",
    "passwd",
    "clave",
    "contrasena",
    "contraseña",
    "secret",
    "token",
    "apikey",
    "api_key",
    "serial",
    "credencial",
    "credential",
    "auth",
];

pub const ENCRYPTED_MARK: &str = "(valor cifrado)";

pub const HIDDEN_MARK: &str = "(oculto)";

pub const PATH_MARK: &str = "(ruta)";

pub fn is_encrypted(value: &str) -> bool {
    let v = value.trim();
    if v.len() < MIN_ENCRYPTED_LEN || !v.starts_with(FERNET_PREFIX) {
        return false;
    }
    v.bytes().all(is_base64_byte)
}

fn is_base64_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'-' | b'_' | b'=')
}

pub fn is_sensitive_name(name: &str) -> bool {
    let lower = name.trim().to_lowercase();
    if lower.is_empty() {
        return false;
    }
    let flat = strip_separators(&lower);
    SENSITIVE_TERMS
        .iter()
        .any(|term| name_matches(&lower, &flat, term))
}

fn name_matches(lower: &str, flat: &str, term: &str) -> bool {
    if lower.split(is_separator).any(|token| token == term) {
        return true;
    }
    flat.contains(strip_separators(term).as_str())
}

fn is_separator(c: char) -> bool {
    matches!(c, '_' | '-' | '.')
}

fn strip_separators(s: &str) -> String {
    s.chars().filter(|c| !is_separator(*c)).collect()
}

const POSIX_ROOTS: [&str; 6] = ["/home/", "/Users/", "/var/", "/tmp/", "/etc/", "/opt/"];

const UNC_PREFIX: &str = r"\\";

pub fn is_absolute_path(value: &str) -> bool {
    let v = value.trim();
    if v.is_empty() {
        return false;
    }
    let urls = pathscan::url_spans(v);
    has_windows_drive(v, &urls) || has_unc(v) || has_posix_root(v, &urls)
}

fn has_windows_drive(text: &str, urls: &[(usize, usize)]) -> bool {
    text.as_bytes().windows(3).enumerate().any(|(i, w)| {
        w[0].is_ascii_alphabetic()
            && w[1] == b':'
            && matches!(w[2], b'\\' | b'/')
            && !pathscan::continues_scheme(text, i)
            && !pathscan::inside_any(urls, i)
    })
}

fn has_unc(text: &str) -> bool {
    if text.starts_with(UNC_PREFIX) {
        return true;
    }
    text.match_indices(UNC_PREFIX).any(|(at, _)| {
        let rest = &text[at + UNC_PREFIX.len()..];
        rest.find('\\').is_some_and(|sep| {
            sep > 0
                && !rest[..sep]
                    .chars()
                    .any(|c| c.is_whitespace() || matches!(c, '"' | '\''))
        })
    })
}

fn has_posix_root(text: &str, urls: &[(usize, usize)]) -> bool {
    POSIX_ROOTS.iter().any(|root| {
        text.match_indices(root)
            .any(|(at, _)| !pathscan::inside_any(urls, at) && pathscan::opens_token(text, at))
    })
}

const USERPROFILE_MARK: &str = "%USERPROFILE%";

const HOME_MARK: &str = "~";

struct MaskedPrefix {
    end: usize,
    mark: &'static str,
}

pub fn mask_user_paths(text: &str) -> String {
    let urls = pathscan::url_spans(text);
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((at, _)) = chars.next() {
        if pathscan::inside_any(&urls, at) {
            continue;
        }
        let Some(prefix) = masked_prefix_at(text, at) else {
            continue;
        };
        let buf = out.get_or_insert_with(|| String::with_capacity(text.len()));
        buf.push_str(&text[copied..at]);
        buf.push_str(prefix.mark);
        copied = prefix.end;
        while chars.peek().is_some_and(|(next, _)| *next < prefix.end) {
            chars.next();
        }
    }
    match out {
        Some(mut buf) => {
            buf.push_str(&text[copied..]);
            buf
        }
        None => text.to_string(),
    }
}

fn masked_prefix_at(text: &str, at: usize) -> Option<MaskedPrefix> {
    windows_users_at(text, at).or_else(|| posix_home_at(text, at))
}

pub fn mask_emails(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut copied = 0;
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'@' {
            at += 1;
            continue;
        }
        let mut start = at;
        while start > 0 && is_email_local(bytes[start - 1]) {
            start -= 1;
        }
        if start == at {
            at += 1;
            continue;
        }
        let mut end = at + 1;
        while end < bytes.len() && is_email_domain(bytes[end]) {
            end += 1;
        }
        let domain = &text[at + 1..end];
        if !is_valid_email_domain(domain) {
            at += 1;
            continue;
        }
        let local = &text[start..at];
        out.push_str(&text[copied..start]);
        out.push(local.chars().next().unwrap_or('*'));
        out.push_str("***@");
        out.push_str(domain);
        copied = end;
        at = end;
    }
    out.push_str(&text[copied..]);
    out
}

fn is_email_local(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'%' | b'+' | b'-')
}

fn is_email_domain(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')
}

fn is_valid_email_domain(domain: &str) -> bool {
    if domain.len() > 253 || domain.contains("..") {
        return false;
    }
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    for label in &labels {
        if label.is_empty()
            || label.starts_with('-')
            || label.ends_with('-')
            || label.len() > 63
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return false;
        }
    }
    let tld = labels.last().copied().unwrap_or_default();
    tld.len() >= 2 && tld.bytes().all(|byte| byte.is_ascii_alphabetic())
}

fn windows_users_at(text: &str, at: usize) -> Option<MaskedPrefix> {
    let bytes = text.as_bytes();
    if !bytes.get(at).is_some_and(u8::is_ascii_alphabetic)
        || bytes.get(at + 1) != Some(&b':')
        || !pathscan::opens_token(text, at)
    {
        return None;
    }
    let users_at = match (bytes.get(at + 2), bytes.get(at + 3)) {
        (Some(b'\\'), Some(b'\\')) => at + 4,
        (Some(sep), _) if is_sep(*sep) => at + 3,
        _ => return None,
    };
    if !starts_with_ic(text, users_at, "Users") {
        return None;
    }
    let after = users_at + 5;
    let user_start = match (bytes.get(after), bytes.get(after + 1)) {
        (Some(b'\\'), Some(b'\\')) => after + 2,
        (Some(sep), _) if is_sep(*sep) => after + 1,
        _ => return None,
    };
    let end = user_end(text, user_start)?;
    Some(MaskedPrefix {
        end,
        mark: USERPROFILE_MARK,
    })
}

fn posix_home_at(text: &str, at: usize) -> Option<MaskedPrefix> {
    let bytes = text.as_bytes();
    if !bytes.get(at).is_some_and(|c| is_sep(*c)) || !opens_home_token(text, at) {
        return None;
    }
    let after = if starts_with_ic(text, at + 1, "Users") {
        at + 6
    } else if starts_with_ic(text, at + 1, "home") {
        at + 5
    } else {
        return None;
    };
    if !bytes.get(after).is_some_and(|c| is_sep(*c)) {
        return None;
    }
    let end = user_end(text, after + 1)?;
    if bytes.get(end).is_some_and(|c| is_sep(*c)) {
        Some(MaskedPrefix {
            end: end + 1,
            mark: HOME_MARK_WITH_SEP,
        })
    } else {
        Some(MaskedPrefix {
            end,
            mark: HOME_MARK,
        })
    }
}

const HOME_MARK_WITH_SEP: &str = "~/";

fn opens_home_token(text: &str, at: usize) -> bool {
    let prev = text[..at].chars().next_back();
    prev.is_none() || prev.is_some_and(|c| !is_word_char(c) && !matches!(c, ':' | '~' | '%'))
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
}

fn is_sep(b: u8) -> bool {
    matches!(b, b'\\' | b'/')
}

fn starts_with_ic(text: &str, at: usize, needle: &str) -> bool {
    text.get(at..at + needle.len())
        .is_some_and(|s| s.eq_ignore_ascii_case(needle))
}

fn user_end(text: &str, start: usize) -> Option<usize> {
    let mut end = start;
    let mut visible = false;
    for (off, c) in text.get(start..)?.char_indices() {
        if is_user_stop(c) {
            break;
        }
        if c == '\'' {
            let after = start + off + c.len_utf8();
            match text.get(after..).and_then(|rest| rest.chars().next()) {
                None => break,
                Some(next)
                    if next.is_whitespace()
                        || matches!(next, ',' | ';' | ')' | ']' | '}' | ':') =>
                {
                    break
                }
                Some(_) => {}
            }
        }
        end = start + off + c.len_utf8();
        visible |= !c.is_whitespace();
    }
    visible.then_some(end)
}

fn is_user_stop(c: char) -> bool {
    matches!(c, '/' | '\\' | '"' | '\n' | '\r')
}

pub fn redacted_value(name: &str, value: &str) -> Option<String> {
    if value.trim().is_empty() {
        return None;
    }
    if is_encrypted(value) {
        return Some(ENCRYPTED_MARK.to_string());
    }
    if is_sensitive_name(name) {
        return Some(HIDDEN_MARK.to_string());
    }
    if is_absolute_path(value) {
        return Some(PATH_MARK.to_string());
    }
    Some(mask_emails(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CIFRADO: &str = "gAAAAABq7LyRAghP1xNXRUIBkRfDi6ioMMqj_-oPnZ5xvkmSw_Jb1B41VjB_\
                          0Yq-KLSS9s_mV8pv3cJ8R2Q==";

    #[test]
    fn detecta_el_blob_cifrado_real() {
        assert!(is_encrypted(CIFRADO));
        assert!(
            is_encrypted(&format!("  {CIFRADO}  ")),
            "el valor puede venir con espacios del editor"
        );
    }

    #[test]
    fn no_confunde_texto_normal_con_un_blob() {
        assert!(!is_encrypted("https://x"));
        assert!(!is_encrypted(""));
        assert!(!is_encrypted("gAAAAA"), "demasiado corto para un token");
        assert!(
            !is_encrypted(&format!("gAAAAA{}", "!".repeat(40))),
            "el resto debe parecer base64"
        );
        assert!(!is_encrypted(&format!("x{}", "A".repeat(60))));
    }

    #[test]
    fn reconoce_los_nombres_sensibles() {
        for name in [
            "password",
            "PassWord",
            "db_password",
            "pass",
            "db_pass",
            "passwd",
            "clave",
            "contrasena",
            "contraseña",
            "client_secret",
            "token",
            "api_key",
            "apikey",
            "mi-API-KEY",
            "serial",
            "credencial",
            "credentials",
            "auth",
            "auth_header",
        ] {
            assert!(is_sensitive_name(name), "deberia ser sensible: {name}");
        }
    }

    #[test]
    fn no_redacta_nombres_del_flujo() {
        for name in ["user", "usuario", "url", "saludo", "contador", "fallas", ""] {
            assert!(!is_sensitive_name(name), "no deberia ser sensible: {name}");
        }
    }

    #[test]
    fn el_valor_cifrado_se_marca_aunque_el_nombre_no_lo_sea() {
        assert_eq!(
            redacted_value("destinatario", CIFRADO).as_deref(),
            Some("(valor cifrado)")
        );
    }

    #[test]
    fn el_cifrado_gana_al_nombre_sensible() {
        assert_eq!(
            redacted_value("db_password", CIFRADO).as_deref(),
            Some("(valor cifrado)"),
            "primero se dice que esta cifrado; el nombre no aporta nada nuevo"
        );
    }

    #[test]
    fn el_nombre_sensible_oculta_un_valor_visible() {
        assert_eq!(
            redacted_value("db_password", "hunter2").as_deref(),
            Some("(oculto)")
        );
        assert_eq!(
            redacted_value("API_KEY", "abc123").as_deref(),
            Some("(oculto)")
        );
    }

    #[test]
    fn el_valor_normal_se_conserva() {
        assert_eq!(
            redacted_value("url", "https://x").as_deref(),
            Some("https://x")
        );
    }

    #[test]
    fn detecta_las_rutas_absolutas_del_usuario() {
        for value in [
            "C:/Users/josep/Downloads/perfil_chrome",
            r"C:\Users\josep\Downloads\perfil_chrome",
            r"C:\Users\josep\x",
            "C:/Users/josep/x",
            r"\\srv\share",
            r"\\srv\share\sub\datos",
            "/home/user/x",
            "/Users/user/x",
            "/var/log/rocketbot.log",
            "/tmp/x",
            "/etc/hosts",
            "/opt/app/bin/x",
        ] {
            assert!(is_absolute_path(value), "deberia ser ruta: {value}");
        }
        assert!(is_absolute_path(r#""C:/Users/josep/a" + fecha"#));
        assert!(is_absolute_path(r"copia en \\srv\share\datos"));
    }

    #[test]
    fn no_confunde_urls_ni_rutas_relativas_con_una_ruta() {
        for value in [
            "https://mi-sitio.com/x",
            "http://ejemplo.test/api",
            "https://x",
            "ftp://host/x",
            "file:///C:/Users/josep/x",
            "carpeta/sub/x.txt",
            "carpeta/home/x.txt",
            "https://ejemplo.test/home/usuario",
            "reporte_ml.xlsx",
            "operador1",
            "",
            "   ",
        ] {
            assert!(!is_absolute_path(value), "no deberia ser ruta: {value}");
        }
    }

    #[test]
    fn la_ruta_absoluta_se_marca_y_lo_demas_se_conserva() {
        assert_eq!(
            redacted_value("chrome_profile", "C:/Users/josep/Downloads/perfil_chrome").as_deref(),
            Some("(ruta)")
        );
        assert_eq!(
            redacted_value("carpeta_salida", r"C:\Users\josep\Downloads").as_deref(),
            Some("(ruta)")
        );
        assert_eq!(
            redacted_value("ruta_salida", r#""C:/Users/josep/a" + fecha"#).as_deref(),
            Some("(ruta)"),
            "una expresion que empieza por una ruta tambien la filtra"
        );
        assert_eq!(
            redacted_value("reporte", "reporte_ml.xlsx").as_deref(),
            Some("reporte_ml.xlsx"),
            "una ruta relativa es parte del flujo y se conserva"
        );
        assert_eq!(
            redacted_value("url", "https://x").as_deref(),
            Some("https://x"),
            "una URL no identifica al usuario y se conserva"
        );
    }

    #[test]
    fn el_nombre_sensible_gana_a_la_ruta() {
        assert_eq!(
            redacted_value("db_password", "C:/Users/josep/x").as_deref(),
            Some("(oculto)")
        );
    }

    #[test]
    fn el_cifrado_gana_a_la_ruta() {
        assert_eq!(
            redacted_value("ruta_respaldo", CIFRADO).as_deref(),
            Some("(valor cifrado)")
        );
    }

    #[test]
    fn la_cadena_vacia_no_tiene_valor() {
        assert_eq!(redacted_value("url", ""), None);
        assert_eq!(redacted_value("url", "   "), None, "solo espacios es vacio");
        assert_eq!(
            redacted_value("password", ""),
            None,
            "sin valor no hay nada que ocultar"
        );
    }

    #[test]
    fn enmascara_el_usuario_en_rutas_windows() {
        assert_eq!(
            mask_user_paths("C:/Users/josep/OneDrive/Documentos/mercadolibre_"),
            "%USERPROFILE%/OneDrive/Documentos/mercadolibre_"
        );
        assert_eq!(
            mask_user_paths(r"C:\Users\josep\Downloads\perfil_chrome"),
            r"%USERPROFILE%\Downloads\perfil_chrome",
            "el separador original se conserva"
        );
        assert_eq!(mask_user_paths(r"D:\USERS\Josep\x"), r"%USERPROFILE%\x");
        assert_eq!(
            mask_user_paths(r#""C:/Users/josep/x" + fecha"#),
            r#""%USERPROFILE%/x" + fecha"#
        );
        assert_eq!(mask_user_paths("C:/Users/josep"), "%USERPROFILE%");
        assert_eq!(
            mask_user_paths(r#""C:\\Users\\josep\\reporte.xlsx""#),
            r#""%USERPROFILE%\\reporte.xlsx""#
        );
    }

    #[test]
    fn enmascara_el_usuario_con_espacios() {
        assert_eq!(
            mask_user_paths("C:/Users/John Doe/proyectos/x.txt"),
            "%USERPROFILE%/proyectos/x.txt"
        );
        assert_eq!(
            mask_user_paths(r"C:\Users\Jane Roe\AppData\x"),
            r"%USERPROFILE%\AppData\x",
            "el separador original se conserva"
        );
        assert_eq!(mask_user_paths("/home/juan perez/x"), "~/x");
        assert_eq!(
            mask_user_paths("/Users/John Doe/Documents/x"),
            "~/Documents/x"
        );
        assert_eq!(mask_user_paths("C:/Users/John Doe"), "%USERPROFILE%");
        assert_eq!(mask_user_paths("/home/juan perez"), "~");
        assert_eq!(
            mask_user_paths(r#""C:/Users/John Doe/x" + fecha"#),
            r#""%USERPROFILE%/x" + fecha"#
        );
        assert_eq!(mask_user_paths("C:/Users/O'Brien/x"), "%USERPROFILE%/x");
        assert_eq!(mask_user_paths("/home/O'Brien/x"), "~/x");
        assert_eq!(mask_user_paths("C:/Users/John'/x"), "%USERPROFILE%/x");
        assert_eq!(
            mask_user_paths("C:/Users/John' + fecha"),
            "%USERPROFILE%' + fecha"
        );
        let con_apostrofo = mask_user_paths("C:/Users/O'Brien/x");
        assert_eq!(
            mask_user_paths(&con_apostrofo),
            con_apostrofo,
            "enmascarar dos veces no cambia el resultado"
        );
        for roto in ["C:/Users/", "C:/Users/ /x", "/home//x"] {
            assert_eq!(mask_user_paths(roto), roto, "se invento un usuario: {roto}");
        }
    }

    #[test]
    fn enmascara_correos_y_conserva_el_dominio() {
        assert_eq!(
            mask_emails("escribe a juan.perez+tag@ejemplo.com"),
            "escribe a j***@ejemplo.com"
        );
        assert_eq!(
            mask_emails("to=<ana@b.co>, cc=<luis@sub.dominio.org>"),
            "to=<a***@b.co>, cc=<l***@sub.dominio.org>"
        );
        assert_eq!(mask_emails("j***@ejemplo.com"), "j***@ejemplo.com");
    }

    #[test]
    fn no_confunde_una_arroba_cualquiera_con_un_correo() {
        for text in [
            "foo@bar",
            "@ejemplo.com",
            "a@b.c",
            "a@-b.com",
            "a@b..com",
            "usuario@localhost",
            "const f = decorador@clase;",
        ] {
            assert_eq!(mask_emails(text), text, "se toco: {text}");
        }
    }

    #[test]
    fn un_correo_en_una_variable_se_redacta() {
        assert_eq!(
            redacted_value("from_name", "juan@example.com").as_deref(),
            Some("j***@example.com")
        );
        assert_eq!(
            redacted_value("url", "https://ejemplo.com").as_deref(),
            Some("https://ejemplo.com"),
            "una URL normal no se toca"
        );
    }

    #[test]
    fn el_texto_con_espacios_normales_no_se_toca() {
        for text in [
            "el usuario Juan Perez escribio",
            "Juan Perez envio el reporte a las 10:00",
            "contrasena del usuario y nombre completo",
        ] {
            assert_eq!(mask_user_paths(text), text, "se toco el texto: {text}");
        }
    }

    #[test]
    fn enmascara_el_usuario_en_rutas_posix_y_macos() {
        assert_eq!(mask_user_paths("/home/josep/proyectos/x"), "~/proyectos/x");
        assert_eq!(
            mask_user_paths(r"\home\josep\proyectos"),
            "~/proyectos",
            "la forma de shell se normaliza a `~/`"
        );
        assert_eq!(mask_user_paths("/Users/josep/Documents/x"), "~/Documents/x");
        assert_eq!(mask_user_paths(r"\Users\josep\Documents"), "~/Documents");
        assert_eq!(mask_user_paths("/home/josep"), "~");
        assert_eq!(mask_user_paths("/home/josep/home/otro"), "~/home/otro");
        assert_eq!(mask_user_paths("/home/josé/x"), "~/x");
    }

    #[test]
    fn enmascara_todas_las_rutas_del_texto() {
        assert_eq!(
            mask_user_paths(r"de C:\Users\ana\a a /home/luis/b"),
            r"de %USERPROFILE%\a a ~/b"
        );
    }

    #[test]
    fn no_toca_las_urls() {
        for url in [
            "https://ejemplo.com/Users/algo",
            "https://ejemplo.com/home/usuario",
            "https://ejemplo.test/api",
            "file:///C:/Users/josep/x",
            "https://ejemplo.test/Users/John Doe",
        ] {
            assert_eq!(mask_user_paths(url), url, "se toco una URL: {url}");
        }
        assert_eq!(
            mask_user_paths("mira https://ejemplo.com/Users/algo y C:/Users/josep/x"),
            "mira https://ejemplo.com/Users/algo y %USERPROFILE%/x"
        );
    }

    #[test]
    fn el_texto_normal_se_devuelve_intacto() {
        for text in [
            "El robot abre el navegador, escribe en Documentos y sube el reporte.",
            "Users home usuarios /usuarios",
            "reporte_ml.xlsx",
            "C:reporte_sin_separador",
            "https://ejemplo.test/api?home=/home/x",
            "",
        ] {
            assert_eq!(mask_user_paths(text), text, "se toco el texto: {text}");
        }
    }

    #[test]
    fn las_rutas_relativas_se_conservan() {
        for text in [
            "reporte_ml.xlsx",
            "carpeta/home/josep/x.txt",
            "carpeta/Users/josep/x",
            "Users/josep",
        ] {
            assert_eq!(
                mask_user_paths(text),
                text,
                "se toco una ruta relativa: {text}"
            );
        }
    }

    #[test]
    fn enmascarar_dos_veces_da_lo_mismo() {
        for text in [
            r"C:\Users\josep\OneDrive\Documentos",
            "C:/Users/josep/OneDrive/Documentos/mercadolibre_",
            "/home/josep/proyectos/x",
            r"\home\josep\proyectos",
            "/home/josep/home/otro",
            "/Users/josep/Documents/x",
            "https://ejemplo.com/Users/algo",
            r#""C:/Users/josep/a" + "C:\Users\josep\b""#,
            r#""C:\\Users\\josep\\x""#,
            "C:/Users/John Doe/proyectos/x.txt",
            r"C:\Users\Jane Roe\AppData\x",
            "/home/juan perez/x",
            r#""https://ejemplo.test/Users/John Doe""#,
        ] {
            let una = mask_user_paths(text);
            let dos = mask_user_paths(&una);
            assert_eq!(una, dos, "no es idempotente: {text}");
            assert!(
                !dos.contains("%USERPROFILE%%USERPROFILE%"),
                "marca duplicada: {dos}"
            );
        }
    }
}
