//! The OpenAPI document committed at `api/openapi.json` is the one the DTOs generate.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

/// Set to write `api/openapi.json` instead of checking it: review the diff, then commit it.
const WRITE: &str = "REWIND_OPENAPI_WRITE";

fn committed_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../api/openapi.json")
}

#[test]
fn the_committed_openapi_document_is_up_to_date() {
    let generated = rewind_api::openapi::to_json();
    let path = committed_path();
    if std::env::var_os(WRITE).is_some() {
        std::fs::write(&path, &generated).expect("api/openapi.json is writable");
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    if committed != generated {
        let line = committed
            .lines()
            .zip(generated.lines())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| committed.lines().count().min(generated.lines().count()));
        panic!(
            "api/openapi.json is stale (first difference at line {}). Write it again with \
             `{WRITE}=1 cargo test -p rewind-api --test openapi`, review the diff, and commit it.",
            line + 1
        );
    }
}

#[test]
fn the_document_is_valid_json_and_openapi_3_1() {
    let document: serde_json::Value =
        serde_json::from_str(&rewind_api::openapi::to_json()).unwrap();
    assert_eq!(document["openapi"], "3.1.0");
    assert!(document["components"]["schemas"]["Problem"].is_object());
    assert!(document["components"]["schemas"]["StreamEvent"].is_object());
    assert!(document["components"]["schemas"]["WebhookEvent"].is_object());
}
