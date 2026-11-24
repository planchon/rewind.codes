//! What the glue sends to the built-in actions, and what it expects back (`archi/04-tools.md`,
//! "Routes"). The dispatcher that implements them (`rewind-mcp` for `http`, the engine for
//! `webhook`) and whoever pins an action set follow the same shapes, recorded in
//! `archi/08-decisions.md`.
//!
//! **`webhook(name)`** calls the action `webhook` with `{"name": string}`, and resolves to its
//! result, `{"url": string}`. [`webhook_input_schema`] is the schema to pin.
//!
//! **`fetch(input, init)`** calls the action `http` in the format of the action `rewind-mcp`
//! implements, whose schemas `rewind_mcp::http_action` pins (one format: the runtime cannot depend
//! on that adapter, so the schemas live there only). Its args:
//!
//! ```json
//! { "url": "https://api.example.com/x", "method": "POST",
//!   "headers": [["content-type", "application/json"]],
//!   "body": "{\"a\":1}" }
//! ```
//!
//! The URL is absolute (`http:` or `https:`), parsed and serialized by the sandbox's `URL`.
//! The method is a token, upper-cased when it is one of `DELETE`, `GET`, `HEAD`, `OPTIONS`,
//! `POST`, `PUT`. Header names are lower-cased, in the order the code gave them, repeated
//! names kept; a string or `URLSearchParams` body adds the matching `content-type` if the
//! code set none. `body` is absent without a body, the text for a string or
//! `URLSearchParams`, and the bytes in base64 with `"bodyEncoding": "base64"` for an
//! `ArrayBuffer` or a typed array. It resolves to a `Response` built from the result:
//!
//! ```json
//! { "status": 200, "statusText": "OK", "url": "https://api.example.com/x",
//!   "redirected": false, "headers": [["content-type", "application/json"]],
//!   "body": "{\"ok\":true}" }
//! ```
//!
//! `body` is the text when the body is UTF-8, and its bytes in base64 with
//! `"bodyEncoding": "base64"` otherwise; `url` is the URL after redirects. Reading a result,
//! only `status` is required: a mock may leave out the rest, which defaults to empty. A status
//! outside 2xx is a response, not a failure: a request the dispatcher refuses or cannot send
//! fails the call, and the code gets an `ActionError`.

use serde_json::{Value, json};

/// The input schema to pin for the built-in `webhook` action.
pub fn webhook_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "name": { "type": "string" } },
        "required": ["name"],
        "additionalProperties": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_accepts_what_the_glue_sends() {
        let webhook = jsonschema::validator_for(&webhook_input_schema()).unwrap();
        assert!(webhook.is_valid(&json!({ "name": "pr_merged" })));
        assert!(!webhook.is_valid(&json!({ "name": 1 })));
    }
}
