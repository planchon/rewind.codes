//! What the glue sends to the built-in tools, and what it expects back (`archi/04-tools.md`,
//! "Routes"). The dispatcher that implements them (`ptc-mcp`, or the engine for `webhook`) and
//! whoever pins a toolset follow the same shapes, recorded in `archi/08-decisions.md`.
//!
//! **`webhook(name)`** calls the tool `webhook` with `{"name": string}`, and resolves to its
//! result, `{"url": string}`.
//!
//! **`fetch(input, init)`** calls the tool `http` with:
//!
//! ```json
//! { "url": "https://api.example.com/x", "method": "POST",
//!   "headers": [["content-type", "application/json"]],
//!   "body": { "text": "{\"a\":1}" } }
//! ```
//!
//! The URL is absolute (`http:` or `https:`), parsed and serialized by the sandbox's `URL`.
//! The method is a token, upper-cased when it is one of `DELETE`, `GET`, `HEAD`, `OPTIONS`,
//! `POST`, `PUT`. Header names are lower-cased, in the order the script gave them, repeated
//! names kept; a string or `URLSearchParams` body adds the matching `content-type` if the
//! script set none. `body` is absent without a body, `{"text": …}` for a string or
//! `URLSearchParams`, `{"base64": …}` for bytes. It resolves to a `Response` built from the
//! result:
//!
//! ```json
//! { "status": 200, "statusText": "OK", "url": "https://api.example.com/x",
//!   "redirected": false, "headers": [["content-type", "application/json"]],
//!   "body": { "text": "{\"ok\":true}" } }
//! ```
//!
//! Only `status` is required. `body` is `{"text": …}` when the body is UTF-8 and
//! `{"base64": …}` otherwise; `url` is the URL after redirects. A status outside 2xx is a
//! response, not a failure: a request the dispatcher refuses or cannot send fails the call,
//! and the script gets a `ToolError`.

use serde_json::{Value, json};

/// The input schema to pin for the built-in `webhook` tool.
pub fn webhook_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "name": { "type": "string" } },
        "required": ["name"],
        "additionalProperties": false,
    })
}

/// The input schema to pin for the built-in `http` tool: the requests `fetch` sends.
pub fn http_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "url": { "type": "string" },
            "method": { "type": "string" },
            "headers": {
                "type": "array",
                "items": {
                    "type": "array",
                    "prefixItems": [{ "type": "string" }, { "type": "string" }],
                    "minItems": 2,
                    "maxItems": 2,
                },
            },
            "body": {
                "oneOf": [
                    {
                        "type": "object",
                        "properties": { "text": { "type": "string" } },
                        "required": ["text"],
                        "additionalProperties": false,
                    },
                    {
                        "type": "object",
                        "properties": { "base64": { "type": "string" } },
                        "required": ["base64"],
                        "additionalProperties": false,
                    },
                ],
            },
        },
        "required": ["url", "method", "headers"],
        "additionalProperties": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schemas_accept_what_the_glue_sends() {
        let http = jsonschema::validator_for(&http_input_schema()).unwrap();
        assert!(http.is_valid(&json!({
            "url": "https://api.example.com/x",
            "method": "POST",
            "headers": [["content-type", "text/plain;charset=UTF-8"]],
            "body": { "text": "hi" },
        })));
        assert!(
            http.is_valid(&json!({ "url": "https://a.example/", "method": "GET", "headers": [] }))
        );
        assert!(!http.is_valid(
            &json!({ "url": "https://a.example/", "method": "GET", "headers": [["x"]] })
        ));
        assert!(!http.is_valid(&json!({
            "url": "https://a.example/", "method": "PUT", "headers": [], "body": { "text": "a", "base64": "YQ==" },
        })));
        let webhook = jsonschema::validator_for(&webhook_input_schema()).unwrap();
        assert!(webhook.is_valid(&json!({ "name": "pr_merged" })));
        assert!(!webhook.is_valid(&json!({ "name": 1 })));
    }
}
