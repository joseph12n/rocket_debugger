//! Protocolo JSON-RPC 2.0 delimitado por lineas: un documento JSON por linea.
//!
//! Invariante: una notificacion (sin `id`) no se responde nunca, aunque este mal
//! formada; responderla descoloca a los clientes que emparejan por `id`. Un fallo
//! de negocio no es error de protocolo: viaja como `isError` en el resultado.

use serde_json::{json, Map, Value};

use crate::tools::{self, CallError};
use crate::Settings;

pub const JSONRPC_VERSION: &str = "2.0";

/// Version MCP fija: el cliente incompatible falla en `initialize`, no a mitad de sesion.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

// Codigos estandar de JSON-RPC 2.0.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// Respuesta de ultimo recurso si `serde_json` no puede serializar un `Value`:
/// dejar al cliente sin respuesta lo deja colgado.
const FALLBACK_INTERNAL: &str = concat!(
    r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"#,
    r#""message":"fallo interno: no pude serializar la respuesta"}}"#
);

/// Una linea del cliente ya interpretada.
enum Line {
    Ignore,
    Invalid { id: Value, error: ProtocolError },
    Request(Request),
}

struct Request {
    id: Option<Value>,
    method: String,
    params: Value,
}

#[derive(Debug, Clone)]
struct ProtocolError {
    code: i64,
    message: String,
}

impl ProtocolError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Despacha una linea: `Some(respuesta)` o `None` si no toca responder.
pub fn handle(line: &str, settings: &Settings) -> Option<String> {
    match parse(line) {
        Line::Ignore => None,
        Line::Invalid { id, error } => Some(error_response(&id, &error)),
        Line::Request(request) => {
            // Toda notificacion se corta aqui, incluidas las mal formadas: no hay respuesta.
            let id = request.id.as_ref()?;
            Some(match dispatch(&request, settings) {
                Ok(result) => result_response(id, result),
                Err(error) => error_response(id, &error),
            })
        }
    }
}

fn parse(line: &str) -> Line {
    // El cliente de Windows puede dejar CR; JSON no lo admite fuera del documento.
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Line::Ignore;
    }

    let value: Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(source) => {
            return Line::Invalid {
                id: Value::Null,
                error: ProtocolError::new(PARSE_ERROR, format!("JSON invalido: {source}")),
            }
        }
    };

    let Value::Object(object) = value else {
        return Line::Invalid {
            id: Value::Null,
            error: ProtocolError::new(INVALID_REQUEST, "la peticion tiene que ser un objeto JSON"),
        };
    };

    let id = object.get("id").cloned();
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        // Una notificacion mal formada sigue siendo una notificacion: silencio.
        if id.is_none() {
            return Line::Ignore;
        }
        return Line::Invalid {
            // Se responde con el `id` del cliente: el error tambien tiene que emparejarse.
            id: id.unwrap_or(Value::Null),
            error: ProtocolError::new(INVALID_REQUEST, "falta `method` en la peticion"),
        };
    };

    Line::Request(Request {
        id,
        method: method.to_string(),
        params: object.get("params").cloned().unwrap_or(Value::Null),
    })
}

fn dispatch(request: &Request, settings: &Settings) -> Result<Value, ProtocolError> {
    match request.method.as_str() {
        "initialize" => {
            let _ = params_object(&request.params)?;
            Ok(initialize_result())
        }
        "tools/list" => {
            let _ = params_object(&request.params)?;
            Ok(tools::descriptors())
        }
        "ping" => {
            let _ = params_object(&request.params)?;
            Ok(json!({}))
        }
        "tools/call" => call_tool(&request.params, settings),
        "notifications/initialized" => Ok(json!({})),
        unknown => Err(ProtocolError::new(
            METHOD_NOT_FOUND,
            format!("metodo desconocido: {unknown}"),
        )),
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {"tools": {}},
        "serverInfo": {
            "name": "rocket_debugger",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

fn call_tool(params: &Value, settings: &Settings) -> Result<Value, ProtocolError> {
    let Some(params) = params_object(params)? else {
        return Err(ProtocolError::new(
            INVALID_PARAMS,
            "tools/call necesita `params` con `name`",
        ));
    };
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Err(ProtocolError::new(
            INVALID_PARAMS,
            "falta `name` en tools/call",
        ));
    };
    // `arguments` ausente equivale a `{}`: cada tool valida sus propios argumentos.
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);

    tools::call(name, &arguments, settings)
        .map(|output| output.to_json())
        .map_err(tool_error)
}

/// Un argumento mal formado es `-32602`; una tool que se rompe es `-32603`.
/// El error de negocio no pasa por aqui: viaja como `isError`.
fn tool_error(error: CallError) -> ProtocolError {
    match error {
        CallError::InvalidParams(message) => ProtocolError::new(INVALID_PARAMS, message),
        CallError::Internal(message) => ProtocolError::new(INTERNAL_ERROR, message),
    }
}

/// `params` puede faltar (`Null`) o ser objeto; cualquier otra cosa es `-32602`.
fn params_object(params: &Value) -> Result<Option<&Map<String, Value>>, ProtocolError> {
    match params {
        Value::Null => Ok(None),
        Value::Object(map) => Ok(Some(map)),
        _ => Err(ProtocolError::new(
            INVALID_PARAMS,
            "`params` tiene que ser un objeto JSON",
        )),
    }
}

fn result_response(id: &Value, result: Value) -> String {
    encode(json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": result,
    }))
}

fn error_response(id: &Value, error: &ProtocolError) -> String {
    encode(json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": {"code": error.code, "message": error.message},
    }))
}

fn encode(message: Value) -> String {
    serde_json::to_string(&message).unwrap_or_else(|_| FALLBACK_INTERNAL.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_respuesta_de_ultimo_recurso_sigue_siendo_json_rpc_valido() {
        let value: Value = serde_json::from_str(FALLBACK_INTERNAL).expect("el literal es JSON");
        assert_eq!(value["jsonrpc"], JSONRPC_VERSION);
        assert_eq!(value["error"]["code"], INTERNAL_ERROR);
        assert!(value["id"].is_null());
    }

    #[test]
    fn el_mapeo_de_errores_de_tool_usa_32602_y_32603() {
        assert_eq!(
            tool_error(CallError::InvalidParams("falta robot".to_string())).code,
            INVALID_PARAMS
        );
        assert_eq!(
            tool_error(CallError::Internal("panic".to_string())).code,
            INTERNAL_ERROR
        );
    }
}
