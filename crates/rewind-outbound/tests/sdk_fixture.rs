//! The calls Rewind signs, as the TypeScript SDK's `verifyRewindCall` must accept them
//! (`sdk/typescript/test/fixtures/rewind-signed-calls.json`, read by
//! `sdk/typescript/test/signatures.test.ts`, which checks the stored signatures). Here the
//! fixture is compared with what Rewind signs now, each JWT by its decoded header and claims: the
//! order of a JSON object's keys depends on how `serde_json` is built (`preserve_order`), so the
//! bytes of a JWT may differ between builds while saying the same. `REWIND_OUTBOUND_BLESS=1`
//! writes the fixture again.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rewind_outbound::{CallIdentity, SignedRequest, Signer, jwk, kid_of};
use serde_json::{Value, json};
use url::Url;

const SEED: [u8; 32] = [7; 32];
const ISSUER: &str = "https://api.rewind.codes/organizations/org_01ACME";
const AUDIENCE: &str = "https://api.example.com";
const NOW_S: u64 = 1_800_000_000;
const RUN: &str = "0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e10";

fn public_key(seed: &[u8; 32]) -> [u8; 32] {
    use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
    let pair = Ed25519KeyPair::from_seed_unchecked(seed).expect("an Ed25519 seed");
    pair.public_key().as_ref().try_into().expect("32 bytes")
}

fn call(seq: u64) -> CallIdentity {
    CallIdentity {
        subject: "user:42".into(),
        labels: BTreeMap::from([
            ("env".to_owned(), "prod".to_owned()),
            ("goal".to_owned(), "q3-report".to_owned()),
        ]),
        run: RUN.into(),
        tool: "dax.createMeasure".into(),
        idempotency_key: format!("{RUN}/{seq}"),
        jti: format!("{RUN}/{seq}/1"),
    }
}

fn signed(signer: &Signer, seq: u64, method: &str, url: &str, body: Option<&str>) -> Value {
    let url = Url::parse(url).expect("a URL");
    let token = signer
        .sign(
            ISSUER,
            Some(&call(seq)),
            &SignedRequest {
                audience: AUDIENCE,
                method,
                url: &url,
                body: body.map(str::as_bytes),
            },
            UNIX_EPOCH + Duration::from_secs(NOW_S),
        )
        .expect("signed");
    json!({
        "method": method,
        "url": url.as_str(),
        "body": body,
        "authorization": format!("Bearer {token}"),
    })
}

#[test]
fn the_sdk_fixture_holds_the_calls_rewind_signs() {
    let public = public_key(&SEED);
    let kid = kid_of(&public);
    let signer = Signer::new(&kid, &SEED).expect("a signer");
    let fixture = json!({
        "comment": "Written by crates/rewind-outbound/tests/sdk_fixture.rs (REWIND_OUTBOUND_BLESS=1): JWTs signed by Rewind's Rust signer, checked by verifyRewindCall.",
        "issuer": ISSUER,
        "audience": AUDIENCE,
        "now": NOW_S,
        "jwks": { "keys": [jwk(&kid, &public)] },
        "calls": {
            "post": signed(
                &signer,
                7,
                "post",
                "https://api.example.com/v1/measures?model=sales#ignored",
                Some(r#"{"name":"Revenue","expression":"SUM(Sales[Amount])"}"#),
            ),
            "get": signed(&signer, 8, "GET", "https://api.example.com/v1/measures/m%201?view=full", None),
        },
    });
    let text = format!(
        "{}\n",
        serde_json::to_string_pretty(&fixture).expect("JSON")
    );
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../sdk/typescript/test/fixtures/rewind-signed-calls.json");
    if std::env::var_os("REWIND_OUTBOUND_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the fixtures directory");
        std::fs::write(&path, &text).expect("the fixture written");
        return;
    }
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_default())
        .unwrap_or(Value::Null);
    assert_eq!(
        decoded(stored),
        decoded(fixture),
        "the SDK's fixture differs from what Rewind signs (REWIND_OUTBOUND_BLESS=1 writes it again)"
    );
}

/// The fixture with each call's `authorization` replaced by its JWT's decoded header and claims.
fn decoded(mut fixture: Value) -> Value {
    if let Some(calls) = fixture.get_mut("calls").and_then(Value::as_object_mut) {
        for call in calls.values_mut() {
            let Some(token) = call
                .get("authorization")
                .and_then(Value::as_str)
                .and_then(|value| value.strip_prefix("Bearer "))
            else {
                continue;
            };
            let parts: Vec<Value> = token
                .split('.')
                .take(2)
                .map(|part| {
                    let bytes = URL_SAFE_NO_PAD.decode(part).expect("base64url");
                    serde_json::from_slice(&bytes).expect("JSON")
                })
                .collect();
            call["authorization"] = json!(parts);
        }
    }
    fixture
}
