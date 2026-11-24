//! Where a secret used goes in a request (`archi/04-tools.md`, "Authenticating calls"): a
//! header, `format` with `{secret}` replaced by the value written as `encoding` says, or the
//! value alone; or a variable of a stdio server's process. Never in a URL.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::{HeaderName, HeaderValue};
use rewind_core::{SECRET_PLACEHOLDER, SecretEncoding, SecretPlacement};

/// A secret put where its server's `auth` says. Never printed: `Debug` shows where only.
#[derive(Clone, PartialEq, Eq)]
pub enum Placed {
    /// An HTTP header, its value marked sensitive.
    Header {
        name: HeaderName,
        value: HeaderValue,
    },
    /// A variable of a stdio server's process.
    Env { variable: String, value: String },
}

impl fmt::Debug for Placed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Placed::Header { name, .. } => write!(f, "Placed::Header({name})"),
            Placed::Env { variable, .. } => write!(f, "Placed::Env({variable})"),
        }
    }
}

/// Why a secret cannot be put where its `auth` says. The message never holds the value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct PlacementError(pub String);

/// Puts `secret` where `placement` says.
pub fn place(placement: &SecretPlacement, secret: &str) -> Result<Placed, PlacementError> {
    match placement {
        SecretPlacement::Header {
            name,
            format,
            encoding,
        } => {
            let header = HeaderName::try_from(name.as_str()).map_err(|_| {
                PlacementError(format!("{name:?} is not the name of an HTTP header"))
            })?;
            let encoded = match encoding {
                SecretEncoding::Plain => secret.to_owned(),
                SecretEncoding::Base64 => STANDARD.encode(secret),
            };
            let text = match format {
                Some(format) => format.replace(SECRET_PLACEHOLDER, &encoded),
                None => encoded,
            };
            let mut value = HeaderValue::from_str(&text).map_err(|_| {
                PlacementError(format!(
                    "the secret does not fit in the header {name}: it holds a character an HTTP \
                     header cannot carry, such as a line break"
                ))
            })?;
            value.set_sensitive(true);
            Ok(Placed::Header {
                name: header,
                value,
            })
        }
        SecretPlacement::Env { variable } => {
            if secret.contains('\0') {
                return Err(PlacementError(format!(
                    "the secret does not fit in the variable {variable}: it holds a NUL character"
                )));
            }
            Ok(Placed::Env {
                variable: variable.clone(),
                value: secret.to_owned(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(placed: Placed) -> (String, String, bool) {
        match placed {
            Placed::Header { name, value } => (
                name.to_string(),
                value.to_str().unwrap().to_owned(),
                value.is_sensitive(),
            ),
            Placed::Env { .. } => panic!("a header"),
        }
    }

    #[test]
    fn a_secret_goes_into_its_header_with_its_format_and_encoding() {
        assert_eq!(
            header(place(&SecretPlacement::bearer(), "ghp_1").unwrap()),
            ("authorization".into(), "Bearer ghp_1".into(), true)
        );
        assert_eq!(
            header(place(&SecretPlacement::basic(), "ada:s3cret").unwrap()),
            (
                "authorization".into(),
                "Basic YWRhOnMzY3JldA==".into(),
                true
            )
        );
        assert_eq!(
            header(place(&SecretPlacement::header("X-Api-Key"), "k-9").unwrap()),
            ("x-api-key".into(), "k-9".into(), true)
        );
        let refused = place(&SecretPlacement::bearer(), "ghp\n_9").unwrap_err();
        assert!(!refused.0.contains("ghp"), "{refused}");
        let env = place(
            &SecretPlacement::Env {
                variable: "GITHUB_TOKEN".into(),
            },
            "ghp_2",
        )
        .unwrap();
        assert_eq!(
            env,
            Placed::Env {
                variable: "GITHUB_TOKEN".into(),
                value: "ghp_2".into()
            }
        );
        assert_eq!(format!("{env:?}"), "Placed::Env(GITHUB_TOKEN)");
    }
}
