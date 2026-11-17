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

/// Valor listo para el pack. `None` cuando no hay nada que mostrar: el render lo
/// pinta como `(sin valor inicial)`, igual que antes de redactar.
///
/// El cifrado manda sobre el nombre: si el valor esta cifrado se dice que lo
/// esta, aunque la variable se llame `destinatario` y no parezca sensible.
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
    fn la_cadena_vacia_no_tiene_valor() {
        assert_eq!(redacted_value("url", ""), None);
        assert_eq!(redacted_value("url", "   "), None, "solo espacios es vacio");
        assert_eq!(
            redacted_value("password", ""),
            None,
            "sin valor no hay nada que ocultar"
        );
    }
}
