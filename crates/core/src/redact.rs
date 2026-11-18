//! Redaccion de secretos antes de entregar un context pack.
//!
//! Un pack esta pensado para compartirse: lo lee un agente, viaja dentro de una
//! incidencia y lo pinta la UI. Pero `project.vars` de un robot real guarda
//! credenciales: Rocketbot las cifra en formato Fernet (prefijo `gAAAAA`) y el
//! usuario las bautiza con nombres que ya dicen lo que son (`db_password`,
//! `api_key`). Sin filtrarlas, el render publica el secreto tal cual.
//!
//! La redaccion se aplica **al construir el pack**, no en el render: todos los
//! consumidores (markdown, JSON, MCP) salen del mismo dato ya limpio y no queda
//! una segunda ruta por la que el secreto pueda escaparse.
//!
//! # Por que por nombre y no por valor
//!
//! El valor de un secreto es indistinguible de un texto normal; el nombre es la
//! unica senal disponible. Tiene un coste: un falso positivo borra informacion
//! que hace falta para entender el flujo, asi que solo entran los terminos que de
//! verdad nombran credenciales. `usuario`, `user`, `contador` o `url` se quedan
//! fuera a proposito: saber a quien apunta una peticion es parte del diagnostico.
//!
//! # Por que tambien las rutas
//!
//! `docs/CONTEXT_PACK.md` promete que el pack no lleva rutas absolutas del
//! usuario. En las variables una ruta local no ayuda a diagnosticar —el agente
//! razona sobre el flujo, no sobre el disco de quien lo ejecuto— y ademas
//! identifica a la persona, asi que se sustituye entera por `(ruta)`: un
//! `chrome_profile = C:/Users/<usuario>/Downloads/perfil_chrome` se publica como
//! `(ruta)`. La deteccion es estrecha a proposito: una URL o una ruta relativa se
//! conservan porque ahi si hay informacion util.
//!
//! # Enmascarar el usuario y no la ruta entera
//!
//! En los parametros de los comandos y en los mensajes de los hallazgos la ruta
//! **si** es informacion de diagnostico: saber que el robot escribe en
//! `OneDrive\Documentos` o que abre un perfil de Chrome concreto explica el flujo,
//! y borrarla entera lo dejaria mas pobre que el JSON crudo. Lo que identifica a
//! la persona es el tramo del usuario, asi que [`mask_user_paths`] substituye solo
//! ese tramo por un marcador estable (`%USERPROFILE%` o `~`) y conserva la cola.
//!
//! El enmascarado se aplica en el render y no al construir el pack: el pack
//! conserva el parametro fiel para el resto de consumidores y la garantia queda
//! concentrada en un solo punto de salida; ver [`crate::render`].

/// Firma de Fernet: Rocketbot cifra asi lo que guarda en `project.vars`.
const FERNET_PREFIX: &str = "gAAAAA";

/// Un token Fernet util ronda el centenar de caracteres. Por debajo de este
/// umbral es mas probable que sea un texto casual que empieza igual.
const MIN_ENCRYPTED_LEN: usize = 40;

/// Terminos que nombran credenciales. `contrasena` y `contraseña` conviven porque
/// el mismo usuario escribe las dos formas segun el teclado que tenga a mano.
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

/// Marcador de un valor que llega cifrado. Dice que hay secreto sin publicarlo y
/// sin fingir que la variable esta vacia.
pub const ENCRYPTED_MARK: &str = "(valor cifrado)";

/// Marcador de un valor oculto por el nombre de su variable.
pub const HIDDEN_MARK: &str = "(oculto)";

/// Marcador de un valor que es una ruta absoluta del equipo del usuario.
pub const PATH_MARK: &str = "(ruta)";

/// Detecta un blob Fernet sin validarlo de verdad: basta con que sea plausible.
/// Si un texto normal empieza por `gAAAAA` y sigue en el alfabeto base64,
/// ocultarlo no pierde informacion util.
pub fn is_encrypted(value: &str) -> bool {
    let v = value.trim();
    if v.len() < MIN_ENCRYPTED_LEN || !v.starts_with(FERNET_PREFIX) {
        return false;
    }
    // Fernet usa base64 URL-safe (`-`, `_`), pero se acepta tambien el estandar
    // (`+`, `/`) y el relleno `=`: el objetivo es no dejar pasar el blob, no
    // auditar como lo codifico Rocketbot.
    v.bytes().all(is_base64_byte)
}

fn is_base64_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'-' | b'_' | b'=')
}

/// El nombre declara una credencial, comparando sin distinguir mayusculas y sin
/// que estorben los separadores (`db_password`, `DB-PASSWORD`, `api_key`).
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
    // Token exacto: `db_pass` -> `pass`.
    if lower.split(is_separator).any(|token| token == term) {
        return true;
    }
    // Pegado: `mipassword` no es un token, y `api_key` no puede serlo porque el
    // propio termino lleva separador. Se comparan ambos sin separadores para que
    // `api_key`, `apikey` y `API-KEY` sean el mismo nombre.
    //
    // La subcadena puede marcar de mas en un nombre raro (`compass` contiene
    // `pass`), pero ocultar un valor inocuo cuesta menos que publicar una
    // credencial, y el dano queda acotado al valor de esa variable.
    flat.contains(strip_separators(term).as_str())
}

fn is_separator(c: char) -> bool {
    matches!(c, '_' | '-' | '.')
}

fn strip_separators(s: &str) -> String {
    s.chars().filter(|c| !is_separator(*c)).collect()
}

/// Raices POSIX que en la practica solo aparecen como ruta absoluta del usuario:
/// nadie escribe `/home/...` para referirse a una carpeta relativa del proyecto.
const POSIX_ROOTS: [&str; 6] = ["/home/", "/Users/", "/var/", "/tmp/", "/etc/", "/opt/"];

/// Separador de esquema URL: lo que sigue a `://` es una direccion, no el disco.
const URL_SEP: &str = "://";

/// UNC de Windows: `\\servidor\recurso`.
const UNC_PREFIX: &str = r"\\";

/// `true` si el valor lleva una ruta absoluta del sistema. Las URLs y las rutas
/// relativas quedan fuera a proposito: `https://x` y `reporte.xlsx` son datos de
/// diagnostico, no ubicaciones de la maquina de quien ejecuto el robot.
pub fn is_absolute_path(value: &str) -> bool {
    let v = value.trim();
    if v.is_empty() {
        return false;
    }
    // Los tramos de URL se calculan una sola vez: `file:///C:/x` contiene `C:/`
    // y aun asi no es una ruta del usuario.
    let urls = url_spans(v);
    has_windows_drive(v, &urls) || has_unc(v) || has_posix_root(v, &urls)
}

/// Tramos que ocupa cada URL (`esquema://resto`) dentro del valor. El resto del
/// texto puede ser una ruta aunque comparta cadena con ellas.
fn url_spans(text: &str) -> Vec<(usize, usize)> {
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

fn find_from(text: &str, needle: &str, from: usize) -> Option<usize> {
    text.get(from..)?.find(needle).map(|i| from + i)
}

/// Caracteres que puede llevar un esquema (`https`, `file`, `git+ssh`).
fn is_scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')
}

/// Un caracter que corta la URL: a partir de ahi empieza otro token del valor.
fn is_url_stop(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';'
        )
}

fn inside_any(spans: &[(usize, usize)], at: usize) -> bool {
    spans.iter().any(|&(start, end)| (start..end).contains(&at))
}

/// Unidad Windows (`C:\`, `C:/`). La letra no puede continuar un esquema: en
/// `https://` la `s` va delante de `:` y el patron la confundiria con una unidad,
/// que es justo el falso positivo que hay que evitar. Tampoco cuenta si cae
/// dentro de una URL ya abierta (`file:///C:/x`).
fn has_windows_drive(text: &str, urls: &[(usize, usize)]) -> bool {
    text.as_bytes().windows(3).enumerate().any(|(i, w)| {
        w[0].is_ascii_alphabetic()
            && w[1] == b':'
            && matches!(w[2], b'\\' | b'/')
            && !continues_scheme(text, i)
            && !inside_any(urls, i)
    })
}

fn continues_scheme(text: &str, at: usize) -> bool {
    text[..at].chars().next_back().is_some_and(is_scheme_char)
}

/// UNC (`\\servidor\recurso`), al principio del valor o embebida en una
/// expresion (`copia en \\srv\share\datos`). Se exige que el servidor no este
/// vacio y que vuelva a aparecer un separador: `\\` suelto es un escape, no una
/// ruta.
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

/// Raiz POSIX conocida. Se exige que abra token: en `carpeta/home/x` el prefijo
/// aparece, pero la ruta es relativa y se conserva.
fn has_posix_root(text: &str, urls: &[(usize, usize)]) -> bool {
    POSIX_ROOTS.iter().any(|root| {
        text.match_indices(root)
            .any(|(at, _)| !inside_any(urls, at) && opens_token(text, at))
    })
}

fn opens_token(text: &str, at: usize) -> bool {
    let prev = text[..at].chars().next_back();
    prev.is_none()
        || prev.is_some_and(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
}

/// Marcador del tramo del usuario en una ruta Windows.
const USERPROFILE_MARK: &str = "%USERPROFILE%";

/// Marcador del tramo del usuario en una ruta POSIX (y en la forma macOS).
const HOME_MARK: &str = "~";

/// Tramo de una ruta que hay que substituir: hasta donde llega (byte) y por que.
struct MaskedPrefix {
    end: usize,
    mark: &'static str,
}

/// Sustituye el tramo del usuario de las rutas absolutas por un marcador estable.
///
/// No se borra la ruta entera a proposito: en un parametro, `OneDrive\Documentos`
/// es justo lo que explica donde escribe el robot. Solo se tapa el nombre del
/// usuario, que es el dato que identifica a la persona:
///
/// - `C:\Users\<x>\...` y `C:/Users/<x>/...` -> `%USERPROFILE%\...` /
///   `%USERPROFILE%/...`, conservando el separador original.
/// - `\home\<x>\...`, `/home/<x>/...`, `\Users\<x>\...` y `/Users/<x>/...` ->
///   `~/...`, la forma en que un shell nombra el home.
///
/// `<x>` es todo el tramo hasta el siguiente separador, la comilla que cierra el
/// parametro o el final de la cadena, asi que un nombre con espacios entra
/// entero: `C:/Users/John Doe/proyectos/x.txt` -> `%USERPROFILE%/proyectos/x.txt`.
///
/// Se enmascara **solo el primer tramo**: en `.../<usuario>/resto` el resto de la
/// ruta se conserva (en POSIX se normaliza a `/` el separador de la marca). La
/// funcion es idempotente: aplicar dos veces da el mismo resultado, porque los
/// marcadores ya no casan con ningun patron.
///
/// # Lo que no se toca
///
/// Las URLs, porque `https://ejemplo.com/Users/algo` es una direccion, no el
/// disco de nadie, y el texto normal, porque sin un patron de usuario completo se
/// devuelve la cadena intacta. La deteccion es tan estrecha como la de
/// [`is_absolute_path`]: un falso positivo aqui cambiaria el parametro que el
/// agente usa para entender el flujo.
pub fn mask_user_paths(text: &str) -> String {
    let urls = url_spans(text);
    // `None` hasta el primer reemplazo: cuando no hay nada que enmascarar se
    // devuelve el original sin asignar ni una cadena nueva.
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((at, _)) = chars.next() {
        // Una ruta dentro de una URL (incluido `file:///C:/Users/...`) no es una
        // ruta del disco para efectos de privacidad: se deja como esta.
        if inside_any(&urls, at) {
            continue;
        }
        let Some(prefix) = masked_prefix_at(text, at) else {
            continue;
        };
        let buf = out.get_or_insert_with(|| String::with_capacity(text.len()));
        buf.push_str(&text[copied..at]);
        buf.push_str(prefix.mark);
        copied = prefix.end;
        // El tramo ya se sustituyo entero: no se vuelve a mirar dentro de el.
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

/// Patron de ruta con usuario que empieza en `at`, si lo hay.
///
/// Windows primero: `C:/Users/x` tambien contiene `/Users/x`, y si se evaluara la
/// forma POSIX primero el resultado seria `C:~/x`.
fn masked_prefix_at(text: &str, at: usize) -> Option<MaskedPrefix> {
    windows_users_at(text, at).or_else(|| posix_home_at(text, at))
}

/// `C:\Users\<x>` / `C:/Users/<x>`: se conserva el separador que sigue al usuario.
///
/// Tambien reconoce la forma escapada de un parametro JSON
/// (`"C:\\Users\\<x>\\x"`): `tree::params_text` re-serializa los objetos y una
/// ruta de Windows llega al render con las barras dobladas, que es otro punto por
/// el que se escaparia el usuario.
fn windows_users_at(text: &str, at: usize) -> Option<MaskedPrefix> {
    let bytes = text.as_bytes();
    if !bytes.get(at).is_some_and(u8::is_ascii_alphabetic)
        || bytes.get(at + 1) != Some(&b':')
        // La letra no puede continuar un esquema: en `https://` la `s` va detras
        // de una letra y el patron la confundiria con una unidad.
        || !opens_token(text, at)
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

/// `\home\<x>`, `/home/<x>`, `\Users\<x>` y `/Users/<x>`: la marca es `~/`.
///
/// El separador que sigue al usuario se consume y se normaliza a `/` para que la
/// salida sea siempre `~/resto`, que es como se lee una ruta de home.
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

/// `~/`: la marca de home cuando aun queda cola detras.
const HOME_MARK_WITH_SEP: &str = "~/";

/// Apertura de token para una raiz POSIX.
///
/// Ademas de no continuar una palabra, se rechaza `:`, `~` y `%` delante:
/// - `C:/home/x` es una carpeta `home` de la unidad C, no el home de un usuario;
/// - `~/home/x` y `%USERPROFILE%/home/x` ya llevan el usuario enmascarado, y
///   volver a enmascararlos romperia la idempotencia.
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

/// Compara sin distinguir mayusculas (`users`, `Users` y `USERS` son la misma
/// carpeta en Windows y en macOS).
fn starts_with_ic(text: &str, at: usize, needle: &str) -> bool {
    text.get(at..at + needle.len())
        .is_some_and(|s| s.eq_ignore_ascii_case(needle))
}

/// Fin del tramo del usuario: desde `start` hasta el separador que abre la cola
/// de la ruta, una comilla de cierre o el final de la cadena.
///
/// El tramo **no** se corta en el primer espacio: la cuenta real de Windows y de
/// macOS lleva espacios de verdad (`John Doe`, `juan perez`), y cortar ahi dejaba
/// `%USERPROFILE% Doe\proyectos` con medio nombre a la vista (issue #18). Los
/// espacios de dentro del tramo se comen con el nombre porque lo que lo delimita
/// es el separador de la ruta, no el espacio.
///
/// El apostrofo es parte del nombre (`O'Brien`) y no un delimitador, salvo cuando
/// lo sigue un espacio o el final: ahi si es la comilla que cierra un parametro
/// entrecomillado. La comilla doble siempre cierra un JSON.
///
/// Devolver `None` cuando el tramo no tiene ni un caracter visible evita
/// inventarse un usuario en `C:/Users/`, en `C:/Users/ /x` o en `/home//x`, que
/// son textos rotos y no rutas.
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
                // Cierra una comilla de parametro, no el nombre.
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

/// Caracter que cierra el tramo del usuario.
///
/// El separador es el limite natural de un nombre de cuenta dentro de una ruta.
/// La comilla doble cierra un parametro JSON y el salto de linea cierra la linea:
/// un nombre de usuario no cruza de linea, y comerselo arrastraria a la marca el
/// texto de la linea siguiente. El apostrofo **no** esta aqui: se decide en
/// [`user_end`] porque puede ser parte del nombre (`O'Brien`).
fn is_user_stop(c: char) -> bool {
    matches!(c, '/' | '\\' | '"' | '\n' | '\r')
}

/// Valor listo para el pack. `None` cuando no hay nada que mostrar: el render lo
/// pinta como `(sin valor inicial)`, igual que antes de redactar.
///
/// El orden de las reglas es el contrato. El cifrado manda sobre todo lo demas:
/// un blob no se explica por su nombre ni por su forma. El nombre manda sobre la
/// ruta: si la variable se llama `db_password`, decir `(ruta)` solo confirmaria
/// que la credencial apunta a una ruta. Y el valor normal se conserva: redactar
/// no puede cegar el diagnostico.
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
    // Se devuelve el original sin recortar: el render ya normaliza y corta.
    Some(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Token Fernet con la forma exacta que Rocketbot deja en `project.vars`
    /// (URL-safe base64, empieza por `gAAAAA`).
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
        // Un falso positivo aqui rompe la comprension del robot: el agente deja de
        // ver a quien apunta la peticion o cuanto vale un contador.
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
            // El hallazgo real de `reporteMercadolibre`.
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
        // Una expresion puede empezar por la ruta y seguir con mas codigo.
        assert!(is_absolute_path(r#""C:/Users/josep/a" + fecha"#));
        // UNC embebida tras otro texto.
        assert!(is_absolute_path(r"copia en \\srv\share\datos"));
    }

    #[test]
    fn no_confunde_urls_ni_rutas_relativas_con_una_ruta() {
        for value in [
            "https://mi-sitio.com/x",
            "http://ejemplo.test/api",
            "https://x",
            "ftp://host/x",
            // Contiene `C:/`, pero dentro de un esquema: no es una ruta.
            "file:///C:/Users/josep/x",
            // El prefijo POSIX aparece, pero la ruta es relativa.
            "carpeta/sub/x.txt",
            "carpeta/home/x.txt",
            // Una URL puede llevar `/home/` sin ser un path del disco.
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
        // El orden de las reglas es el contrato: con `(ruta)` el pack diria que
        // la credencial es una ruta, y eso es informacion de mas.
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
        // La unidad no tiene por que ser C, y la carpeta no distingue mayusculas.
        assert_eq!(mask_user_paths(r"D:\USERS\Josep\x"), r"%USERPROFILE%\x");
        // Embebida en una expresion: todo lo que rodea la ruta se conserva.
        assert_eq!(
            mask_user_paths(r#""C:/Users/josep/x" + fecha"#),
            r#""%USERPROFILE%/x" + fecha"#
        );
        // Sin cola no se inventa un separador que no estaba.
        assert_eq!(mask_user_paths("C:/Users/josep"), "%USERPROFILE%");
        // Los parametros JSON llegan con las barras escapadas (`tree::params_text`
        // re-serializa el objeto); el usuario se enmascara igual.
        assert_eq!(
            mask_user_paths(r#""C:\\Users\\josep\\reporte.xlsx""#),
            r#""%USERPROFILE%\\reporte.xlsx""#
        );
    }

    #[test]
    fn enmascara_el_usuario_con_espacios() {
        // Issue #18: la cuenta de Windows y la de macOS llevan espacios, y el
        // corte en el primero dejaba `%USERPROFILE% Doe\...` con medio nombre
        // publicado. El tramo termina en el separador de la ruta, no en el espacio.
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
        // Sin cola, el tramo llega al final de la cadena.
        assert_eq!(mask_user_paths("C:/Users/John Doe"), "%USERPROFILE%");
        assert_eq!(mask_user_paths("/home/juan perez"), "~");
        // La comilla cierra el parametro, no el nombre: el espacio de dentro del
        // tramo no puede confundirse con el final del valor.
        assert_eq!(
            mask_user_paths(r#""C:/Users/John Doe/x" + fecha"#),
            r#""%USERPROFILE%/x" + fecha"#
        );
        // El apostrofo forma parte del nombre: cortar en el dejaba `s/x` a la
        // vista (issue #18).
        assert_eq!(mask_user_paths("C:/Users/O'Brien/x"), "%USERPROFILE%/x");
        assert_eq!(mask_user_paths("/home/O'Brien/x"), "~/x");
        assert_eq!(mask_user_paths("C:/Users/John'/x"), "%USERPROFILE%/x");
        // Con espacio detras si es la comilla de cierre de un parametro.
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
        // Un tramo sin nada visible sigue sin enmascararse: `C:/Users/ ` es texto
        // roto, no un usuario en blanco.
        for roto in ["C:/Users/", "C:/Users/ /x", "/home//x"] {
            assert_eq!(mask_user_paths(roto), roto, "se invento un usuario: {roto}");
        }
    }

    /// Un texto normal con espacios se queda quieto: sin un patron de ruta no hay
    /// tramo de usuario que esconder, y enmascarar de mas cambiaria el parametro
    /// que el agente usa para entender el flujo.
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
        // Solo el primer tramo: una carpeta `home` de dentro no es el home.
        assert_eq!(mask_user_paths("/home/josep/home/otro"), "~/home/otro");
        // Un nombre de usuario no tiene por que ser ASCII.
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
            // Una URL `file:` tambien es una direccion, aunque lleve una ruta.
            "file:///C:/Users/josep/x",
            // Aunque la URL lleve un nombre con espacios, sigue siendo una
            // direccion: el tramo de usuario de dentro no se enmascara.
            "https://ejemplo.test/Users/John Doe",
        ] {
            assert_eq!(mask_user_paths(url), url, "se toco una URL: {url}");
        }
        // La URL se queda; la ruta que va al lado si se enmascara.
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
            // Un nombre con espacios tampoco puede volver a enmascararse.
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
