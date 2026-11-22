//! Signed calls (`archi/04-tools.md`, "Signed calls"): one JWT per HTTP request, signed with
//! the tenant's Ed25519 key (`alg` `EdDSA`, the key's `kid`), in `Authorization: Bearer`.
//!
//! | Claim | Value | From |
//! | --- | --- | --- |
//! | `iss` | `{public-url}/organizations/{tenant}` | The engine |
//! | `aud` | The API's `baseUrl`, or the MCP server's URL | The dispatcher |
//! | `sub`, `labels`, `run`, `tool`, `idempotency_key` | The run's namespace and labels, the run, the tool, `{run_id}/{seq}` | The engine, for the request of a call only |
//! | `jti` | `{run_id}/{seq}/{attempt}` for the request of a call; a random id for the other requests of an MCP session | The engine; the signer |
//! | `htm`, `htu`, `body` | The method, the URL (without a fragment), the SHA-256 of the body in base64url (absent without a body) | The request |
//! | `iat`, `exp` | Now, and [`TOKEN_LIFETIME`] later, in seconds, by the wall clock | The signer |
//!
//! The private key is the 32-byte seed the `SecretStore` keeps (RFC 8032), loaded through
//! `aws-lc-rs` as a PKCS#8 v1 document, which `jsonwebtoken` signs with.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair as _};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};
use url::Url;

/// How long a JWT is valid: `exp` is `iat` plus this.
pub const TOKEN_LIFETIME: Duration = Duration::from_secs(60);

/// Why a request could not be signed. The message never holds the key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Rewind could not sign the request: {0}")]
pub struct SigningError(pub String);

/// The claims of the request of a call: who it acts for and what it is (`sub`, `labels`, `run`,
/// `tool`, `idempotency_key`, `jti`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallIdentity {
    /// `sub`: the run's namespace, the end user.
    pub subject: String,
    /// `labels`: the run's labels.
    pub labels: BTreeMap<String, String>,
    /// `run`: the run's id.
    pub run: String,
    /// `tool`: the tool's name, as scripts call it.
    pub tool: String,
    /// `idempotency_key`: `{run_id}/{seq}`.
    pub idempotency_key: String,
    /// `jti`: `{run_id}/{seq}/{attempt}`.
    pub jti: String,
}

/// A request to sign: where it goes and what it carries.
#[derive(Debug, Clone, Copy)]
pub struct SignedRequest<'a> {
    /// `aud`: the API's `baseUrl`, or the MCP server's URL.
    pub audience: &'a str,
    /// `htm`: `GET`, `POST`…
    pub method: &'a str,
    /// `htu`: the URL, its fragment left out.
    pub url: &'a Url,
    /// The bytes of the body, if it has one: `body` is their SHA-256.
    pub body: Option<&'a [u8]>,
}

/// Signs requests with one tenant's key. Never printed: `Debug` shows the `kid` only.
#[derive(Clone)]
pub struct Signer {
    kid: String,
    public_key: [u8; 32],
    key: EncodingKey,
}

impl fmt::Debug for Signer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Signer")
            .field("kid", &self.kid)
            .finish_non_exhaustive()
    }
}

impl Signer {
    /// The signer of the key whose seed is `seed` and whose `kid` is `kid`, the first 8 bytes
    /// of the SHA-256 of its public key in hex: refused when they do not match.
    pub fn new(kid: &str, seed: &[u8; 32]) -> Result<Self, SigningError> {
        let pair = Ed25519KeyPair::from_seed_unchecked(seed)
            .map_err(|_| SigningError("the signing key is not an Ed25519 seed".into()))?;
        let public_key: [u8; 32] =
            pair.public_key().as_ref().try_into().map_err(|_| {
                SigningError("the signing key's public part is not 32 bytes".into())
            })?;
        if kid_of(&public_key) != kid {
            return Err(SigningError(format!(
                "the signing key {kid} does not match its public key"
            )));
        }
        let document = pair
            .to_pkcs8v1()
            .map_err(|_| SigningError("the signing key cannot be encoded".into()))?;
        Ok(Self {
            kid: kid.to_owned(),
            public_key,
            key: EncodingKey::from_ed_der(document.as_ref()),
        })
    }

    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// The Ed25519 public key, 32 bytes.
    pub fn public_key(&self) -> &[u8; 32] {
        &self.public_key
    }

    /// The JWT of `request`, issued by `issuer` at `now`: with the claims of `call` for the
    /// request of a call, without them (and a random `jti`) for the other requests of an MCP
    /// session.
    pub fn sign(
        &self,
        issuer: &str,
        call: Option<&CallIdentity>,
        request: &SignedRequest<'_>,
        now: SystemTime,
    ) -> Result<String, SigningError> {
        let iat = now
            .duration_since(UNIX_EPOCH)
            .map_err(|_| SigningError("the clock is before 1970".into()))?
            .as_secs();
        let mut url = request.url.clone();
        url.set_fragment(None);
        let mut claims = Map::new();
        claims.insert("iss".into(), json!(issuer));
        claims.insert("aud".into(), json!(request.audience));
        claims.insert("htm".into(), json!(request.method.to_ascii_uppercase()));
        claims.insert("htu".into(), json!(url.as_str()));
        if let Some(body) = request.body {
            claims.insert("body".into(), json!(body_digest(body)));
        }
        claims.insert("iat".into(), json!(iat));
        claims.insert("exp".into(), json!(iat + TOKEN_LIFETIME.as_secs()));
        match call {
            Some(call) => {
                claims.insert("sub".into(), json!(call.subject));
                claims.insert("labels".into(), json!(call.labels));
                claims.insert("run".into(), json!(call.run));
                claims.insert("tool".into(), json!(call.tool));
                claims.insert("idempotency_key".into(), json!(call.idempotency_key));
                claims.insert("jti".into(), json!(call.jti));
            }
            None => {
                claims.insert("jti".into(), json!(random_jti()?));
            }
        }
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.kid.clone());
        jsonwebtoken::encode(&header, &Value::Object(claims), &self.key)
            .map_err(|error| SigningError(error.to_string()))
    }
}

/// The `kid` of a public key: the first 8 bytes of its SHA-256, in lowercase hex.
pub fn kid_of(public_key: &[u8; 32]) -> String {
    hex(&Sha256::digest(public_key)[..8])
}

/// The `body` claim: the SHA-256 of the body's bytes, in base64url without padding.
pub fn body_digest(body: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(body))
}

/// The JWK of a public key, as `{iss}/jwks.json` publishes it: `kty` `OKP`, `crv` `Ed25519`,
/// `alg` `EdDSA`, `use` `sig`, its `kid`, and `x` the key in base64url.
pub fn jwk(kid: &str, public_key: &[u8; 32]) -> Value {
    json!({
        "kty": "OKP",
        "crv": "Ed25519",
        "alg": "EdDSA",
        "use": "sig",
        "kid": kid,
        "x": URL_SAFE_NO_PAD.encode(public_key),
    })
}

/// What a receiving server expects of a signed request: its issuer, its own audience, and the
/// request as it received it.
#[derive(Debug, Clone, Copy)]
pub struct Expected<'a> {
    pub issuer: &'a str,
    pub audience: &'a str,
    pub method: &'a str,
    /// The URL the client called.
    pub url: &'a str,
    /// The raw bytes of the body; empty for none.
    pub body: &'a [u8],
}

/// The checks a server does on a signed request (`archi/04-tools.md`, "What the server
/// checks"), as the SDK's `verifyRewindCall` does them: the signature with the key of `jwks`
/// (`{"keys": [...]}`) its `kid` names, `iss`, `aud`, `exp` and `iat` with 5 s of leeway, `htm`,
/// `htu` and `body`. The claims, or the check that failed. For Rewind's own tests, and Rust
/// servers that receive its calls.
pub fn verify(token: &str, jwks: &Value, expected: &Expected<'_>) -> Result<Value, String> {
    let header = jsonwebtoken::decode_header(token).map_err(|error| format!("header: {error}"))?;
    if header.alg != Algorithm::EdDSA {
        return Err(format!("alg: {:?}, not EdDSA", header.alg));
    }
    let kid = header.kid.ok_or("kid: absent")?;
    let jwks: jsonwebtoken::jwk::JwkSet =
        serde_json::from_value(jwks.clone()).map_err(|error| format!("jwks: {error}"))?;
    let jwk = jwks
        .find(&kid)
        .ok_or_else(|| format!("kid: {kid} is not in the JWKS"))?;
    let key = jsonwebtoken::DecodingKey::from_jwk(jwk).map_err(|error| format!("jwk: {error}"))?;
    let mut validation = jsonwebtoken::Validation::new(Algorithm::EdDSA);
    validation.leeway = 5;
    validation.set_issuer(&[expected.issuer]);
    validation.set_audience(&[expected.audience]);
    validation.set_required_spec_claims(&["exp", "iat", "iss", "aud"]);
    let claims = jsonwebtoken::decode::<Value>(token, &key, &validation)
        .map_err(|error| format!("signature or claims: {error}"))?
        .claims;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "the clock is before 1970".to_owned())?
        .as_secs();
    if claims["iat"].as_u64().is_none_or(|iat| iat > now + 5) {
        return Err("iat: in the future".into());
    }
    if claims["htm"].as_str() != Some(&expected.method.to_ascii_uppercase()) {
        return Err(format!("htm: {} for {}", claims["htm"], expected.method));
    }
    let mut url = Url::parse(expected.url).map_err(|error| format!("url: {error}"))?;
    url.set_fragment(None);
    if claims["htu"].as_str() != Some(url.as_str()) {
        return Err(format!("htu: {} for {url}", claims["htu"]));
    }
    let body = (!expected.body.is_empty()).then(|| body_digest(expected.body));
    if claims.get("body").and_then(Value::as_str) != body.as_deref() {
        return Err(format!("body: {:?} for {body:?}", claims.get("body")));
    }
    Ok(claims)
}

/// A random `jti`, 128 bits in hex: for the requests of an MCP session that belong to no call.
fn random_jti() -> Result<String, SigningError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| SigningError(format!("no random bytes for its jti: {error}")))?;
    Ok(hex(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use jsonwebtoken::jwk::{Jwk, JwkSet};
    use jsonwebtoken::{DecodingKey, Validation, decode, decode_header};

    use super::*;

    fn signer() -> Signer {
        let seed = [7u8; 32];
        let public = Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
        let public: [u8; 32] = public.public_key().as_ref().try_into().unwrap();
        Signer::new(&kid_of(&public), &seed).unwrap()
    }

    fn verify(token: &str, signer: &Signer, audience: &str) -> Value {
        let jwks: JwkSet =
            serde_json::from_value(json!({ "keys": [jwk(signer.kid(), signer.public_key())] }))
                .unwrap();
        let header = decode_header(token).unwrap();
        assert_eq!(header.alg, Algorithm::EdDSA);
        let kid = header.kid.unwrap();
        let key: &Jwk = jwks.find(&kid).unwrap();
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_audience(&[audience]);
        validation.set_issuer(&["https://rewind.example/organizations/acme"]);
        decode::<Value>(token, &DecodingKey::from_jwk(key).unwrap(), &validation)
            .unwrap()
            .claims
    }

    #[test]
    fn a_call_s_request_carries_every_claim_and_checks_against_the_jwks() {
        let signer = signer();
        let url = Url::parse("https://api.example.com/v1/measures?x=1#frag").unwrap();
        let call = CallIdentity {
            subject: "alice".into(),
            labels: BTreeMap::from([("env".into(), "prod".into())]),
            run: "run-1".into(),
            tool: "dax.createMeasure".into(),
            idempotency_key: "run-1/3".into(),
            jti: "run-1/3/2".into(),
        };
        let token = signer
            .sign(
                "https://rewind.example/organizations/acme",
                Some(&call),
                &SignedRequest {
                    audience: "https://api.example.com/v1",
                    method: "post",
                    url: &url,
                    body: Some(b"{\"a\":1}"),
                },
                SystemTime::now(),
            )
            .unwrap();
        let claims = verify(&token, &signer, "https://api.example.com/v1");
        assert_eq!(claims["sub"], "alice");
        assert_eq!(claims["labels"], json!({ "env": "prod" }));
        assert_eq!(claims["run"], "run-1");
        assert_eq!(claims["tool"], "dax.createMeasure");
        assert_eq!(claims["idempotency_key"], "run-1/3");
        assert_eq!(claims["jti"], "run-1/3/2");
        assert_eq!(claims["htm"], "POST");
        assert_eq!(claims["htu"], "https://api.example.com/v1/measures?x=1");
        assert_eq!(claims["body"], body_digest(b"{\"a\":1}"));
        assert_eq!(
            claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap(),
            60
        );
    }

    #[test]
    fn a_session_s_request_carries_the_reduced_set() {
        let signer = signer();
        let url = Url::parse("https://mcp.example.com/mcp").unwrap();
        let request = SignedRequest {
            audience: "https://mcp.example.com/mcp",
            method: "GET",
            url: &url,
            body: None,
        };
        let issuer = "https://rewind.example/organizations/acme";
        let token = signer
            .sign(issuer, None, &request, SystemTime::now())
            .unwrap();
        let claims = verify(&token, &signer, "https://mcp.example.com/mcp");
        let mut keys: Vec<&str> = claims
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["aud", "exp", "htm", "htu", "iat", "iss", "jti"]);
        let other = signer
            .sign(issuer, None, &request, SystemTime::now())
            .unwrap();
        assert_ne!(
            verify(&other, &signer, "https://mcp.example.com/mcp")["jti"],
            claims["jti"],
            "one jti per request"
        );
    }

    #[test]
    fn a_key_whose_kid_does_not_match_is_refused() {
        let error = Signer::new("0011223344556677", &[7u8; 32]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Rewind could not sign the request: the signing key 0011223344556677 does not \
             match its public key"
        );
        assert!(!format!("{:?}", signer()).contains("key:"));
    }

    #[test]
    fn an_expired_token_is_refused_by_a_checking_server() {
        let signer = signer();
        let url = Url::parse("https://mcp.example.com/mcp").unwrap();
        let token = signer
            .sign(
                "https://rewind.example/organizations/acme",
                None,
                &SignedRequest {
                    audience: "https://mcp.example.com/mcp",
                    method: "POST",
                    url: &url,
                    body: Some(b"{}"),
                },
                SystemTime::now() - Duration::from_secs(600),
            )
            .unwrap();
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_audience(&["https://mcp.example.com/mcp"]);
        let jwk: Jwk = serde_json::from_value(jwk(signer.kid(), signer.public_key())).unwrap();
        let error = decode::<Value>(&token, &DecodingKey::from_jwk(&jwk).unwrap(), &validation)
            .unwrap_err();
        assert_eq!(
            *error.kind(),
            jsonwebtoken::errors::ErrorKind::ExpiredSignature
        );
    }
}
