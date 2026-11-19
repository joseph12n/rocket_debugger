//! Normalizacion de mensajes de error en firmas estables.
//!
//! La firma ignora los valores variables (nombres, hashes, rutas, puertos, horas)
//! y es idempotente: el mismo defecto da el mismo texto entre ejecuciones.

const UNDEFINED_NAME: &str = "name:{var}:not_defined";

/// Cabecera de la traza de Python: a partir de ahi el mensaje no describe el defecto.
const TRACEBACK: &str = "Traceback (most recent call last):";

/// Modulos de la plataforma que fallan por defecto propio, no por el robot.
const MODULE_DEFECTS: [&str; 4] = ["datatransform", "getbot", "getbot_service", "zdev"];

/// 32 es el tamano de un MD5: por debajo, cualquier numero largo se confundiria con un hash.
const HASH_MIN: usize = 32;

/// De quien es el fallo: la UI agrupa por esto y mezclar ruido del motor con defectos del robot la deja en rojo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Robot,
    EngineNoise,
    Module,
}

/// Firma estable: dos ejecuciones del mismo defecto dan el mismo texto aunque cambien variable, linea, ruta, puerto o hash.
///
/// ```
/// use rocketdebug_core::signature::normalize;
///
/// assert_eq!(
///     normalize("NameError: name 'contador' is not defined"),
///     "NameError:name:{var}:not_defined"
/// );
/// assert_eq!(
///     normalize("EXCEPTION IN (<string> Rocketbot, L_1375 \"\"): Error: Incorrect padding"),
///     "EXCEPTION IN (<string> Rocketbot, L_? \"\"): Error:Incorrect padding"
/// );
/// ```
pub fn normalize(message: &str) -> String {
    // Una traza de Python no describe el defecto: cambia con la version instalada y rompe la agrupacion entre ejecuciones.
    let sin_traza = match message.find(TRACEBACK) {
        Some(at) => &message[..at],
        None => message,
    };
    let s = collapse_ws(sin_traza);
    let s = mask_paths(&s);
    let s = mask_line_numbers(&s);
    let s = mask_hashes(&s);
    let s = mask_ports(&s);
    let s = compact_undefined_name(&s);
    let s = compact_error_colon(&s);
    collapse_ws(&s)
}

/// Ruido conocido del motor (zdev base64, getbot IndexError): no se arregla desde el robot y envenena el informe.
pub fn is_engine_noise(message: &str) -> bool {
    let lower = message.to_lowercase();
    match exception_module(message) {
        Some(module) if module.eq_ignore_ascii_case("zdev") => {
            lower.contains("base64")
                || lower.contains("padding")
                || lower.contains("unicodedecodeerror")
                || lower.contains("invalid continuation byte")
        }
        Some(module) if module.eq_ignore_ascii_case("getbot") => {
            lower.contains("indexerror") || lower.contains("list index out of range")
        }
        None => {
            lower.contains("incorrect padding") || lower.contains("invalid base64-encoded string")
        }
        _ => false,
    }
}

/// Un modulo de plataforma es Module aunque ademas sea ruido conocido; EngineNoise solo para ruido sin modulo concreto.
pub fn classify(message: &str) -> ErrorKind {
    let en_modulo_defectuoso = exception_module(message).is_some_and(|module| {
        MODULE_DEFECTS
            .iter()
            .any(|m| module.eq_ignore_ascii_case(m))
    });
    if en_modulo_defectuoso {
        return ErrorKind::Module;
    }
    if is_engine_noise(message) {
        return ErrorKind::EngineNoise;
    }
    ErrorKind::Robot
}

/// El modulo del `<string> X` del runner: unico dato para decidir si el fallo es del robot o de la plataforma.
fn exception_module(message: &str) -> Option<&str> {
    const MARK: &str = "<string>";
    // `to_ascii_lowercase` conserva longitud y fronteras de caracter: el indice vale sobre el original.
    let lower = message.to_ascii_lowercase();
    let at = lower.find(MARK)?;
    let rest = message.get(at + MARK.len()..)?.trim_start();
    let end = rest.find([',', ')']).unwrap_or(rest.len());
    let module = rest[..end].trim();
    if module.is_empty() {
        None
    } else {
        Some(module)
    }
}

/// Espacios multiples (y saltos de linea) a uno solo, sin extremos.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pendiente = false;
    for c in s.chars() {
        if c.is_whitespace() {
            pendiente = !out.is_empty();
        } else {
            if pendiente {
                out.push(' ');
                pendiente = false;
            }
            out.push(c);
        }
    }
    out
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_path_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'\\' | b'/' | b'.' | b'-' | b'_' | b'~' | b'$' | b'+' | b'@' | b'%'
        )
}

/// Impide que un `/` inicie ruta: `http://host/x` no se parte y `foo/bar` no cuenta.
fn is_posix_path_blocker(b: u8) -> bool {
    is_word_byte(b) || matches!(b, b'-' | b'.' | b'/' | b'\\' | b':' | b'$')
}

/// Sustituye rutas absolutas por `<path>`; se corta en el primer espacio (decision consciente).
fn mask_paths(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        let end = windows_path_end(s, i)
            .or_else(|| unc_path_end(s, i))
            .or_else(|| posix_path_end(s, i));
        match end {
            Some(end) => {
                out.push_str(&s[copied..i]);
                out.push_str("<path>");
                copied = end;
                i = end;
            }
            None => i += 1,
        }
    }
    out.push_str(&s[copied..]);
    out
}

/// `X:\...` o `X:/...`: ruta absoluta de Windows.
fn windows_path_end(s: &str, i: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let b = *bytes.get(i)?;
    if !b.is_ascii_alphabetic() || (i > 0 && is_word_byte(bytes[i - 1])) {
        return None;
    }
    if bytes.get(i + 1) != Some(&b':') {
        return None;
    }
    let sep = *bytes.get(i + 2)?;
    if sep != b'\\' && sep != b'/' {
        return None;
    }
    Some(scan_path(s, i + 2))
}

/// `\\servidor\recurso`: ruta UNC.
fn unc_path_end(s: &str, i: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.get(i) != Some(&b'\\') || bytes.get(i + 1) != Some(&b'\\') {
        return None;
    }
    if i > 0 && is_word_byte(bytes[i - 1]) {
        return None;
    }
    Some(scan_path(s, i + 2))
}

/// Ruta absoluta POSIX; el primer caracter tras `/` debe ser de nombre de fichero.
fn posix_path_end(s: &str, i: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.get(i) != Some(&b'/') {
        return None;
    }
    if i > 0 && is_posix_path_blocker(bytes[i - 1]) {
        return None;
    }
    let next = *bytes.get(i + 1)?;
    if !(next.is_ascii_alphanumeric() || matches!(next, b'_' | b'.' | b'~')) {
        return None;
    }
    Some(scan_path(s, i + 1))
}

fn scan_path(s: &str, start: usize) -> usize {
    let bytes = s.as_bytes();
    let mut i = start;
    while i < bytes.len() && is_path_byte(bytes[i]) {
        i += 1;
    }
    i
}

/// `L_123` -> `L_?`, exigiendo inicio de palabra para no tocar nombres como `ML_06_Legacy`.
fn mask_line_numbers(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut copied = 0;
    let mut i = 0;
    while i + 2 < bytes.len() {
        let es_linea = bytes[i] == b'L'
            && bytes[i + 1] == b'_'
            && bytes[i + 2].is_ascii_digit()
            && (i == 0 || !is_word_byte(bytes[i - 1]));
        if es_linea {
            out.push_str(&s[copied..i]);
            out.push_str("L_?");
            i += 2;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            copied = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&s[copied..]);
    out
}

/// Tiras hexadecimales de 32+ caracteres (MD5, SHA1, SHA256...) -> `<hash>`.
fn mask_hashes(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_hexdigit() && (i == 0 || !is_word_byte(bytes[i - 1])) {
            let mut end = i;
            while end < bytes.len() && bytes[end].is_ascii_hexdigit() {
                end += 1;
            }
            let frontera = end == bytes.len() || !is_word_byte(bytes[end]);
            if end - i >= HASH_MIN && frontera {
                out.push_str(&s[copied..i]);
                out.push_str("<hash>");
                copied = end;
            }
            i = end;
        } else {
            i += 1;
        }
    }
    out.push_str(&s[copied..]);
    out
}

/// Puerto en `host:8080` -> `host:<port>` y en `port 8080` -> `port <port>`; `12:43:47` es una hora.
fn mask_ports(s: &str) -> String {
    let s = mask_colon_ports(s);
    mask_labelled_ports(&s)
}

fn is_host_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_')
}

fn mask_colon_ports(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b':' {
            let mut host_start = i;
            while host_start > 0 && is_host_byte(bytes[host_start - 1]) {
                host_start -= 1;
            }
            let host = &s[host_start..i];
            let host_con_nombre = !host.is_empty() && !host.bytes().all(|b| b.is_ascii_digit());
            let mut end = i + 1;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            let digitos = end - (i + 1);
            let frontera = end == bytes.len() || !bytes[end].is_ascii_digit();
            if host_con_nombre && (2..=5).contains(&digitos) && frontera {
                out.push_str(&s[copied..i + 1]);
                out.push_str("<port>");
                copied = end;
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&s[copied..]);
    out
}

fn mask_labelled_ports(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        let palabra = if starts_with_ci(s, i, "puerto") {
            6
        } else if starts_with_ci(s, i, "port") {
            4
        } else {
            0
        };
        if palabra > 0 && (i == 0 || !is_word_byte(bytes[i - 1])) {
            let tras = i + palabra;
            let fin_palabra = tras >= bytes.len() || !is_word_byte(bytes[tras]);
            if fin_palabra {
                let mut j = tras;
                while j < bytes.len() && bytes[j] == b' ' {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b':' {
                    j += 1;
                }
                while j < bytes.len() && bytes[j] == b' ' {
                    j += 1;
                }
                let inicio = j;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                let digitos = j - inicio;
                let frontera = j == bytes.len() || !bytes[j].is_ascii_digit();
                if (2..=5).contains(&digitos) && frontera {
                    out.push_str(&s[copied..inicio]);
                    out.push_str("<port>");
                    copied = j;
                    i = j;
                    continue;
                }
            }
        }
        i += 1;
    }
    out.push_str(&s[copied..]);
    out
}

fn starts_with_ci(s: &str, at: usize, needle: &str) -> bool {
    s.as_bytes()
        .get(at..at + needle.len())
        .is_some_and(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

/// `name 'contador' is not defined` -> `name:{var}:not_defined`.
fn compact_undefined_name(s: &str) -> String {
    const PHRASE: &str = " is not defined";
    const HEAD: &str = "name ";
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some(rel) = s[i..].find(HEAD) {
        let start = i + rel;
        let tras_head = start + HEAD.len();
        let Some(quote) = s[tras_head..].chars().next() else {
            break;
        };
        if quote != '\'' && quote != '"' {
            out.push_str(&s[i..tras_head]);
            i = tras_head;
            continue;
        }
        let body = &s[tras_head + quote.len_utf8()..];
        let Some(cierre) = body.find(quote) else {
            out.push_str(&s[i..tras_head]);
            i = tras_head;
            continue;
        };
        let ident = &body[..cierre];
        let cola = &body[cierre + quote.len_utf8()..];
        let previo_ok = start == 0 || !is_word_byte(s.as_bytes()[start - 1]);
        let ident_ok = !ident.is_empty() && !ident.chars().any(char::is_whitespace);
        if previo_ok && ident_ok {
            if let Some(resto) = cola.strip_prefix(PHRASE) {
                out.push_str(&s[i..start]);
                out.push_str(UNDEFINED_NAME);
                i = s.len() - resto.len();
                continue;
            }
        }
        out.push_str(&s[i..tras_head]);
        i = tras_head;
    }
    out.push_str(&s[i..]);
    out
}

/// Quita el espacio tras `Error:` / `Exception:`: la firma no depende de como formatee Python.
fn compact_error_colon(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut comiendo = false;
    for c in s.chars() {
        if comiendo {
            if c == ' ' {
                continue;
            }
            comiendo = false;
        }
        out.push(c);
        if c == ':' && (out.ends_with("Error:") || out.ends_with("Exception:")) {
            comiendo = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::parse_text;

    const MUESTRA: &str = include_str!("../../../fixtures/logs/sample.log");

    #[test]
    fn firma_ignora_el_nombre_de_la_variable() {
        let contador = normalize("NameError: name 'contador' is not defined");
        let similitud = normalize("NameError: name 'similitud' is not defined");
        assert_eq!(contador, "NameError:name:{var}:not_defined");
        assert_eq!(contador, similitud);
    }

    #[test]
    fn firma_ignora_la_linea_del_runner() {
        let a = normalize(
            r#"EXCEPTION IN (<string> Rocketbot, L_91 ""): NameError: name 'contador' is not defined"#,
        );
        let b = normalize(
            r#"EXCEPTION IN (<string> Rocketbot, L_2218 ""): NameError: name 'similitud' is not defined"#,
        );
        assert_eq!(a, b);
        assert_eq!(
            a,
            r#"EXCEPTION IN (<string> Rocketbot, L_? ""): NameError:name:{var}:not_defined"#
        );
    }

    #[test]
    fn la_traza_no_entra_en_la_firma() {
        let con_traza = normalize(
            "NameError: name 'contador' is not defined\nTraceback (most recent call last):\n  File \"<string>\", line 91, in run",
        );
        assert_eq!(con_traza, normalize("NameError: name 'x' is not defined"));
    }

    #[test]
    fn firma_ignora_lineas_que_no_son_de_traza() {
        // Un `ML_06_Legacy` no es una linea `L_n` y no se debe tocar.
        assert_eq!(
            normalize("INICIO subrobot ML_06_Legacy"),
            "INICIO subrobot ML_06_Legacy"
        );
    }

    #[test]
    fn firma_ignora_rutas_windows_y_posix() {
        let win = normalize(
            r"OSError: [Errno 22] Invalid argument: 'C:\rocketbot_bots\entrega\resumen.xlsx'",
        );
        let posix = normalize("/home/usuario/app/robot.py no existe");
        assert!(win.contains("<path>"), "{win}");
        assert!(!win.contains("rocketbot_bots"), "{win}");
        assert!(posix.starts_with("<path> "), "{posix}");
        // Dos rutas distintas del mismo error dan la misma firma.
        assert_eq!(
            normalize("/home/a/robot.py roto"),
            normalize("/home/b/robot.py roto")
        );
    }

    #[test]
    fn una_url_no_se_parte_en_mitades() {
        let firma = normalize("GET https://example.com/api/v1/robots fallo");
        assert!(
            firma.contains("https://example.com/api/v1/robots"),
            "{firma}"
        );
    }

    #[test]
    fn firma_ignora_hashes_largos() {
        let md5 = normalize("hash 0123456789abcdef0123456789abcdef distinto");
        let sha256 = normalize(
            "hash 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef distinto",
        );
        assert!(md5.contains("<hash>"), "{md5}");
        assert!(sha256.contains("<hash>"), "{sha256}");
        assert!(!md5.contains("0123456789"));
        // Una tira corta no es un hash.
        assert_eq!(normalize("id deadbeef"), "id deadbeef");
    }

    #[test]
    fn firma_ignora_puertos_pero_no_horas() {
        assert_eq!(
            normalize("conectando a localhost:8080"),
            "conectando a localhost:<port>"
        );
        assert_eq!(
            normalize("conectando a 127.0.0.1:5000"),
            "conectando a 127.0.0.1:<port>"
        );
        assert_eq!(
            normalize("no pude abrir el port 8080"),
            "no pude abrir el port <port>"
        );
        assert_eq!(
            normalize("no pude abrir el puerto 3000"),
            "no pude abrir el puerto <port>"
        );
        assert_eq!(
            normalize("no pude abrir el puerto: 3000"),
            "no pude abrir el puerto: <port>"
        );
        // 12:43:47 es una hora, no un puerto.
        assert_eq!(
            normalize("error a las 12:43:47.586814"),
            "error a las 12:43:47.586814"
        );
    }

    #[test]
    fn colapsa_espacios_y_saltos() {
        assert_eq!(normalize("  a  \n\t b   c  "), "a b c");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn normalize_es_idempotente() {
        let casos = [
            "NameError: name 'contador' is not defined",
            r#"EXCEPTION IN (<string> Rocketbot, L_91 ""): FileNotFoundError: [WinError 3] ruta: 'C:\a\b'"#,
            "conectando a localhost:8080 desde /home/u/app.py con hash 0123456789abcdef0123456789abcdef",
            "port 8080 y puerto 3000",
        ];
        for caso in casos {
            let una = normalize(caso);
            assert_eq!(normalize(&una), una, "no es idempotente: {caso}");
        }
    }

    #[test]
    fn ruido_de_motor_conocido() {
        // Formas reales del entorno: zdev rompiendo base64 y getbot sin subrobot.
        assert!(is_engine_noise(
            r#"EXCEPTION IN (<string> zdev, L_1375 ""): Error: Incorrect padding"#
        ));
        assert!(is_engine_noise(
            r#"EXCEPTION IN (<string> zdev, L_1375 ""): Error: Invalid base64-encoded string: number of data characters (17) cannot be 1 more than a multiple of 4"#
        ));
        assert!(is_engine_noise(
            r#"EXCEPTION IN (<string> zdev, L_1375 ""): UnicodeDecodeError: 'utf-8' codec can't decode byte 0xf1"#
        ));
        assert!(is_engine_noise(
            r#"EXCEPTION IN (<string> getbot, L_1704 ""): IndexError: list index out of range"#
        ));
        // Error del robot: no es ruido.
        assert!(!is_engine_noise(
            r#"EXCEPTION IN (<string> Rocketbot, L_91 ""): NameError: name 'contador' is not defined"#
        ));
        // Defecto de datatransform: es de la plataforma, pero no ruido transitorio.
        assert!(!is_engine_noise(
            r#"EXCEPTION IN (<string> datatransform, L_212 ""): AttributeError: 'NoneType' object has no attribute 'get'"#
        ));
        // Sin marcador de modulo, el propio texto delata el ruido del base64.
        assert!(is_engine_noise("Error: Incorrect padding"));
    }

    #[test]
    fn clasifica_por_responsable() {
        let robot = r#"EXCEPTION IN (<string> Rocketbot, L_91 ""): NameError: name 'contador' is not defined"#;
        assert_eq!(classify(robot), ErrorKind::Robot);
        assert_eq!(
            classify(r#"EXCEPTION IN (<string> system, L_130 ""): Exception: boom"#),
            ErrorKind::Robot
        );
        for modulo in ["datatransform", "getbot", "getbot_service", "zdev"] {
            let msg = format!(r#"EXCEPTION IN (<string> {modulo}, L_1 ""): KeyError: 'x'"#);
            assert_eq!(classify(&msg), ErrorKind::Module, "modulo {modulo}");
        }
        // El ruido sin modulo conocido se queda en ruido de motor.
        assert_eq!(classify("Error: Incorrect padding"), ErrorKind::EngineNoise);
    }

    #[test]
    fn la_muestra_agrupa_los_errores_repetidos() {
        let entradas = parse_text(MUESTRA);
        let errores: Vec<_> = entradas.iter().filter(|e| e.is_error()).collect();
        assert_eq!(errores.len(), 9);
        let mut firmas: Vec<String> = errores.iter().map(|e| normalize(&e.message)).collect();
        firmas.sort();
        firmas.dedup();
        assert_eq!(firmas.len(), 8, "los dos NameError colapsan en una firma");
        assert!(firmas.iter().any(|f| f.contains("name:{var}:not_defined")));
        assert!(firmas
            .iter()
            .all(|f| !f.contains("contador") && !f.contains("similitud")));
        assert!(firmas.iter().any(|f| f.contains("<path>")));
    }
}
