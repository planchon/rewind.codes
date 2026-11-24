//! The args and the result of the `http` tool: what the runtime's `fetch` sends and reads.
//!
//! Args:
//!
//! ```json
//! { "url": "https://api.example.com/v1/items", "method": "POST",
//!   "headers": [["content-type", "application/json"]], "body": "{\"a\":1}", "bodyEncoding": "utf8" }
//! ```
//!
//! Only `url` is required. `method` is `GET` by default; `delete`, `get`, `head`, `options`,
//! `post` and `put` are uppercased, as `fetch` does. `body` is text, or bytes in base64 with
//! `"bodyEncoding": "base64"`.
//!
//! Result:
//!
//! ```json
//! { "status": 200, "statusText": "OK", "url": "https://api.example.com/v1/items",
//!   "redirected": false, "headers": [["content-type", "application/json"]], "body": "[]" }
//! ```
//!
//! `url` is the URL after redirects. Header names are lowercase, in the order received. A
//! body that is not UTF-8 is in base64, with `"bodyEncoding": "base64"`.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode};
use serde_json::{Map, Value, json};
use url::Url;

/// A request, as the script sent it.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    pub(crate) url: String,
    pub(crate) method: Method,
    pub(crate) headers: Vec<(HeaderName, HeaderValue)>,
    pub(crate) body: Option<Vec<u8>>,
}

/// Headers a script cannot set: the transport's own, and those that would let a request pass
/// for another (`Host`).
const FORBIDDEN_HEADERS: &[&str] = &[
    "connection",
    "content-length",
    "expect",
    "host",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The request of a call's args, or why they are not one (written for the model).
pub(crate) fn request(args: &Value) -> Result<Request, String> {
    let Value::Object(args) = args else {
        return Err("the http tool expects an object: { url, method?, headers?, body? }".into());
    };
    if let Some(unknown) = args.keys().find(|key| {
        !matches!(
            key.as_str(),
            "url" | "method" | "headers" | "body" | "bodyEncoding"
        )
    }) {
        return Err(format!(
            "the http tool does not know the argument {unknown:?}"
        ));
    }
    let url = match args.get("url") {
        Some(Value::String(url)) => url.clone(),
        _ => return Err("the http tool needs a url, as a string".into()),
    };

    let method = match args.get("method") {
        None | Some(Value::Null) => Method::GET,
        Some(Value::String(method)) => {
            let normalized = match method.to_ascii_uppercase().as_str() {
                upper @ ("DELETE" | "GET" | "HEAD" | "OPTIONS" | "POST" | "PUT") => {
                    upper.to_owned()
                }
                _ => method.clone(),
            };
            if matches!(
                normalized.to_ascii_uppercase().as_str(),
                "CONNECT" | "TRACE" | "TRACK"
            ) {
                return Err(format!("the method {method} is not allowed"));
            }
            Method::from_bytes(normalized.as_bytes())
                .map_err(|_| format!("{method:?} is not an HTTP method"))?
        }
        Some(_) => return Err("the method must be a string".into()),
    };

    let mut headers = Vec::new();
    match args.get("headers") {
        None | Some(Value::Null) => {}
        Some(Value::Array(pairs)) => {
            for pair in pairs {
                let (Some(name), Some(value)) = (
                    pair.get(0).and_then(Value::as_str),
                    pair.get(1).and_then(Value::as_str),
                ) else {
                    return Err("headers must be a list of [name, value] pairs of strings".into());
                };
                let header = HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| format!("{name:?} is not a valid header name"))?;
                if FORBIDDEN_HEADERS.contains(&header.as_str()) {
                    return Err(format!("the header {name} cannot be set"));
                }
                let value = HeaderValue::from_str(value)
                    .map_err(|_| format!("the value of the header {name} is not valid"))?;
                headers.push((header, value));
            }
        }
        Some(_) => return Err("headers must be a list of [name, value] pairs of strings".into()),
    }

    let body = match (args.get("body"), args.get("bodyEncoding")) {
        (None | Some(Value::Null), _) => None,
        (Some(Value::String(body)), None | Some(Value::Null)) => Some(body.as_bytes().to_vec()),
        (Some(Value::String(body)), Some(Value::String(encoding))) => match encoding.as_str() {
            "utf8" => Some(body.as_bytes().to_vec()),
            "base64" => Some(
                STANDARD
                    .decode(body)
                    .map_err(|_| "the body is not valid base64".to_owned())?,
            ),
            other => return Err(format!("unknown body encoding {other:?}: utf8 or base64")),
        },
        _ => return Err("the body must be a string".into()),
    };
    if body.is_some() && matches!(method, Method::GET | Method::HEAD) {
        return Err(format!("a {method} request cannot have a body"));
    }
    Ok(Request {
        url,
        method,
        headers,
        body,
    })
}

/// The result of a request.
pub(crate) fn response(
    status: StatusCode,
    url: &Url,
    redirected: bool,
    headers: &HeaderMap,
    body: &[u8],
) -> Value {
    let headers: Vec<Value> = headers
        .iter()
        .map(|(name, value)| json!([name.as_str(), latin1(value.as_bytes())]))
        .collect();
    let mut result = Map::new();
    result.insert("status".into(), json!(status.as_u16()));
    result.insert(
        "statusText".into(),
        json!(status.canonical_reason().unwrap_or_default()),
    );
    result.insert("url".into(), json!(url.as_str()));
    result.insert("redirected".into(), json!(redirected));
    result.insert("headers".into(), Value::Array(headers));
    match std::str::from_utf8(body) {
        Ok(text) => {
            result.insert("body".into(), json!(text));
        }
        Err(_) => {
            result.insert("body".into(), json!(STANDARD.encode(body)));
            result.insert("bodyEncoding".into(), json!("base64"));
        }
    }
    Value::Object(result)
}

/// A header value's bytes as a string, one character per byte, as `fetch` reads them.
fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&byte| char::from(byte)).collect()
}

fn header_pairs() -> Value {
    json!({
        "type": "array",
        "items": {
            "type": "array",
            "prefixItems": [{ "type": "string" }, { "type": "string" }],
            "minItems": 2,
            "maxItems": 2,
            "items": false
        }
    })
}

/// The input schema of the `http` tool, as an action set pins it.
pub fn http_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "url": { "type": "string", "description": "An absolute http or https URL." },
            "method": { "type": "string", "description": "GET by default." },
            "headers": header_pairs(),
            "body": { "type": "string" },
            "bodyEncoding": { "enum": ["utf8", "base64"], "description": "utf8 by default." }
        },
        "required": ["url"],
        "additionalProperties": false
    })
}

/// The output schema of the `http` tool, as an action set pins it.
pub fn http_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "status": { "type": "integer" },
            "statusText": { "type": "string" },
            "url": { "type": "string", "description": "The URL after redirects." },
            "redirected": { "type": "boolean" },
            "headers": header_pairs(),
            "body": { "type": "string" },
            "bodyEncoding": { "enum": ["base64"], "description": "Present when the body is not UTF-8." }
        },
        "required": ["status", "statusText", "url", "redirected", "headers", "body"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_request() {
        let request = request(&json!({
            "url": "https://api.example.com/x",
            "method": "post",
            "headers": [["Content-Type", "application/json"]],
            "body": "{}"
        }))
        .unwrap();
        assert_eq!(request.method, Method::POST);
        assert_eq!(request.headers[0].0.as_str(), "content-type");
        assert_eq!(request.body.as_deref(), Some(&b"{}"[..]));
        let bytes = super::request(&json!({
            "url": "https://a", "method": "PUT", "body": "AAH/", "bodyEncoding": "base64"
        }))
        .unwrap();
        assert_eq!(bytes.body.as_deref(), Some(&[0u8, 1, 255][..]));
        assert_eq!(
            super::request(&json!({ "url": "https://a", "method": "patch" }))
                .unwrap()
                .method
                .as_str(),
            "patch"
        );
    }

    #[test]
    fn refuses_what_fetch_refuses() {
        let refused = |args: Value| super::request(&args).unwrap_err();
        assert_eq!(
            refused(json!({ "url": "https://a", "headers": [["Host", "internal"]] })),
            "the header Host cannot be set"
        );
        assert_eq!(
            refused(json!({ "url": "https://a", "method": "CONNECT" })),
            "the method CONNECT is not allowed"
        );
        assert_eq!(
            refused(json!({ "url": "https://a", "body": "x" })),
            "a GET request cannot have a body"
        );
        assert_eq!(
            refused(json!({ "url": "https://a", "redirect": "manual" })),
            "the http tool does not know the argument \"redirect\""
        );
        assert_eq!(refused(json!({})), "the http tool needs a url, as a string");
    }

    #[test]
    fn writes_a_response() {
        let mut headers = HeaderMap::new();
        headers.append("content-type", HeaderValue::from_static("text/plain"));
        headers.append("x-raw", HeaderValue::from_bytes(&[0xe9]).unwrap());
        let url = Url::parse("https://api.example.com/final").unwrap();
        assert_eq!(
            response(StatusCode::NOT_FOUND, &url, true, &headers, b"missing"),
            json!({
                "status": 404, "statusText": "Not Found", "url": "https://api.example.com/final",
                "redirected": true, "headers": [["content-type", "text/plain"], ["x-raw", "é"]],
                "body": "missing"
            })
        );
        assert_eq!(
            response(
                StatusCode::OK,
                &url,
                false,
                &HeaderMap::new(),
                &[0xff, 0x00]
            )["bodyEncoding"],
            "base64"
        );
    }
}
