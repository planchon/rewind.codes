//! End users' tokens (`archi/06-storage-and-scale.md`, "Who calls"): the opaque ones the API
//! mints, and the keys a customer registers to sign its own as JWTs.
//!
//! - An opaque token is `rewind_nt_` and 43 base-62 characters (256 bits); the store keeps its
//!   SHA-256, which is its id.
//! - A token key is a public JWK: Ed25519 (`OKP`, EdDSA), P-256 (`EC`, ES256) or RSA of at
//!   least 2048 bits (RS256), without a private part. Rewind names it `rk_` and 24 base-62
//!   characters, the `kid` of the tokens it signs, and keeps its public parameters only.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::jwk::{AlgorithmParameters, EllipticCurve, Jwk};
use jsonwebtoken::{Algorithm, DecodingKey};
use rewind_api::headers::{TOKEN_KEY_PREFIX, TOKEN_PREFIX};
use rewind_api::namespaces::TokenAlgorithm;
use serde_json::{Value, json};

use crate::error::ApiError;

const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// `len` base-62 characters from the operating system's CSPRNG.
fn random_base62(len: usize) -> Result<String, ApiError> {
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes).map_err(ApiError::internal)?;
    // The bias of `% 62` on a byte is below 2 %: 43 characters still hold more than 255 bits.
    Ok(bytes
        .iter()
        .map(|byte| char::from(BASE62[usize::from(*byte) % BASE62.len()]))
        .collect())
}

/// A new opaque token: `rewind_nt_` and 43 base-62 characters.
pub(crate) fn new_token() -> Result<String, ApiError> {
    Ok(format!("{TOKEN_PREFIX}{}", random_base62(43)?))
}

/// A new token key's `kid`: `rk_` and 24 base-62 characters.
pub(crate) fn new_kid() -> Result<String, ApiError> {
    Ok(format!("{TOKEN_KEY_PREFIX}{}", random_base62(24)?))
}

/// The members of a JWK that hold a private key (RFC 7518, section 6): a token key is public.
const PRIVATE_MEMBERS: [&str; 7] = ["d", "p", "q", "dp", "dq", "qi", "oth"];

/// The fewest bits of an RSA modulus.
const MIN_RSA_BITS: usize = 2048;

/// A public key a customer registered, read from its JWK.
#[derive(Debug, Clone)]
pub(crate) struct PublicKey {
    pub(crate) algorithm: TokenAlgorithm,
    /// Its public parameters only, as Rewind keeps and answers them.
    pub(crate) jwk: Value,
}

impl PublicKey {
    /// Reads a JWK given to `POST /v1/organization/token-keys`: refused (400) unless it is a
    /// public Ed25519, P-256 or RSA (2048 bits at least) key.
    pub(crate) fn parse(jwk: &Value) -> Result<Self, ApiError> {
        let invalid = |why: &str| {
            ApiError::invalid(format!(
                "jwk: {why}. Expected a public JWK (RFC 7517): {{\"kty\": \"OKP\", \"crv\": \
                 \"Ed25519\", \"x\": …}}, {{\"kty\": \"EC\", \"crv\": \"P-256\", \"x\": …, \"y\": \
                 …}}, or {{\"kty\": \"RSA\", \"n\": …, \"e\": …}} of 2048 bits at least"
            ))
        };
        let Some(members) = jwk.as_object() else {
            return Err(invalid("it is not a JSON object"));
        };
        if let Some(private) = PRIVATE_MEMBERS
            .iter()
            .find(|member| members.contains_key(**member))
        {
            return Err(invalid(&format!(
                "it holds a private part ({private:?}): register the public key only, and keep \
                 the private key with the service that signs the tokens"
            )));
        }
        let parsed: Jwk = serde_json::from_value(jwk.clone())
            .map_err(|_| invalid("its kty, crv and parameters do not make a known key"))?;
        let decoded = |text: &str, what: &str| {
            URL_SAFE_NO_PAD
                .decode(text)
                .map_err(|_| invalid(&format!("{what} is not base64url without padding")))
        };
        match &parsed.algorithm {
            AlgorithmParameters::OctetKeyPair(params) if params.curve == EllipticCurve::Ed25519 => {
                if decoded(&params.x, "x")?.len() != 32 {
                    return Err(invalid("an Ed25519 key's x is 32 bytes"));
                }
                Ok(Self {
                    algorithm: TokenAlgorithm::EdDsa,
                    jwk: json!({ "kty": "OKP", "crv": "Ed25519", "x": params.x }),
                })
            }
            AlgorithmParameters::EllipticCurve(params) if params.curve == EllipticCurve::P256 => {
                if decoded(&params.x, "x")?.len() != 32 || decoded(&params.y, "y")?.len() != 32 {
                    return Err(invalid("a P-256 key's x and y are 32 bytes each"));
                }
                Ok(Self {
                    algorithm: TokenAlgorithm::Es256,
                    jwk: json!({ "kty": "EC", "crv": "P-256", "x": params.x, "y": params.y }),
                })
            }
            AlgorithmParameters::RSA(params) => {
                let modulus = decoded(&params.n, "n")?;
                let bits = modulus
                    .iter()
                    .position(|byte| *byte != 0)
                    .map_or(0, |first| {
                        (modulus.len() - first) * 8 - modulus[first].leading_zeros() as usize
                    });
                if bits < MIN_RSA_BITS {
                    return Err(invalid(&format!(
                        "this RSA key has {bits} bits, fewer than {MIN_RSA_BITS}"
                    )));
                }
                if decoded(&params.e, "e")?.is_empty() {
                    return Err(invalid("e is empty"));
                }
                Ok(Self {
                    algorithm: TokenAlgorithm::Rs256,
                    jwk: json!({ "kty": "RSA", "n": params.n, "e": params.e }),
                })
            }
            _ => Err(invalid(
                "only Ed25519 (OKP), P-256 (EC) and RSA keys sign end users' tokens",
            )),
        }
    }

    /// A key as the store keeps it: the JSON text of its public JWK.
    pub(crate) fn stored(text: &str) -> Option<Self> {
        let jwk: Value = serde_json::from_str(text).ok()?;
        Self::parse(&jwk).ok()
    }

    /// The JWT algorithm of its tokens.
    pub(crate) fn jwt_algorithm(&self) -> Algorithm {
        match self.algorithm {
            TokenAlgorithm::EdDsa => Algorithm::EdDSA,
            TokenAlgorithm::Es256 => Algorithm::ES256,
            TokenAlgorithm::Rs256 => Algorithm::RS256,
        }
    }

    /// The key that checks its tokens' signatures.
    pub(crate) fn decoding_key(&self) -> Option<DecodingKey> {
        let jwk: Jwk = serde_json::from_value(self.jwk.clone()).ok()?;
        DecodingKey::from_jwk(&jwk).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_and_kids_are_long_and_prefixed() {
        let token = new_token().unwrap_or_default();
        assert!(token.starts_with("rewind_nt_"));
        assert_eq!(token.len(), "rewind_nt_".len() + 43);
        assert_ne!(new_token().unwrap_or_default(), token);
        let kid = new_kid().unwrap_or_default();
        assert!(kid.starts_with("rk_"));
        assert_eq!(kid.len(), 3 + 24);
    }

    fn b64(bytes: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(bytes)
    }

    #[test]
    fn only_public_ed25519_p256_and_large_rsa_keys_are_kept() {
        let ed = PublicKey::parse(&json!({
            "kty": "OKP", "crv": "Ed25519", "x": b64(&[7; 32]), "kid": "theirs", "use": "sig"
        }));
        let ed = ed.ok().map(|key| (key.algorithm, key.jwk));
        assert_eq!(
            ed,
            Some((
                TokenAlgorithm::EdDsa,
                json!({ "kty": "OKP", "crv": "Ed25519", "x": b64(&[7; 32]) })
            )),
            "the public parameters only"
        );
        let ec = PublicKey::parse(&json!({
            "kty": "EC", "crv": "P-256", "x": b64(&[1; 32]), "y": b64(&[2; 32])
        }));
        assert_eq!(
            ec.ok().map(|key| key.algorithm),
            Some(TokenAlgorithm::Es256)
        );
        let mut modulus = vec![0xc5u8; 256];
        modulus[0] = 0x80;
        let rsa = PublicKey::parse(&json!({ "kty": "RSA", "n": b64(&modulus), "e": "AQAB" }));
        assert_eq!(
            rsa.ok().map(|key| key.algorithm),
            Some(TokenAlgorithm::Rs256)
        );

        let refused = |jwk: Value| {
            PublicKey::parse(&jwk)
                .err()
                .map(|error| error.0.detail)
                .unwrap_or_default()
        };
        let private = refused(json!({
            "kty": "OKP", "crv": "Ed25519", "x": b64(&[7; 32]), "d": b64(&[9; 32])
        }));
        assert!(private.contains("private part"), "{private}");
        let small = refused(json!({ "kty": "RSA", "n": b64(&[0xff; 128]), "e": "AQAB" }));
        assert!(small.contains("1024 bits"), "{small}");
        let p384 = refused(json!({
            "kty": "EC", "crv": "P-384", "x": b64(&[1; 48]), "y": b64(&[2; 48])
        }));
        assert!(p384.contains("only Ed25519"), "{p384}");
        let secret = refused(json!({ "kty": "oct", "k": b64(b"secret") }));
        assert!(!secret.is_empty());
        assert!(!refused(json!("a key")).is_empty());
        assert!(!refused(json!({ "kty": "OKP", "crv": "Ed25519", "x": b64(&[7; 31]) })).is_empty());
    }
}
