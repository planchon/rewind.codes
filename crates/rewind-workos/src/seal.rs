//! Sealed cookies (`archi/07-code-architecture.md`, "Sign-in (`/auth`)"): a value serialized to
//! JSON and sealed with the `workos` crate's `seal`, AES-256-GCM keyed by the deployment's
//! `WORKOS_COOKIE_PASSWORD` (`archi/08-decisions.md`). Nothing sealed can be read or changed
//! without the password; a value sealed with another password does not unseal.

use std::fmt;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::ConfigError;

/// Seals and unseals the values of the dashboard's cookies. `Debug` never prints the password.
#[derive(Clone)]
pub struct CookieSeal {
    password: String,
}

/// A value that could not be sealed, or a sealed value that does not unseal: forged, cut, or
/// sealed with another password. It says nothing more, so that nothing leaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SealError {
    #[error("the value cannot be sealed")]
    Seal,
    #[error("the sealed value is invalid, or was sealed with another password")]
    Unseal,
}

impl fmt::Debug for CookieSeal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CookieSeal(<redacted>)")
    }
}

impl CookieSeal {
    /// `password`: 32 bytes in hexadecimal, 64 characters, as `WORKOS_COOKIE_PASSWORD` holds
    /// them.
    pub fn new(password: &str) -> Result<Self, ConfigError> {
        let password = password.trim();
        if password.len() != 64 || !password.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ConfigError::InvalidCookiePassword);
        }
        Ok(Self {
            password: password.to_owned(),
        })
    }

    /// The value as JSON, sealed: base64 of the nonce, the ciphertext and its tag.
    pub fn seal<T: Serialize>(&self, value: &T) -> Result<String, SealError> {
        workos::seal(value, &self.password).map_err(|_| SealError::Seal)
    }

    pub fn unseal<T: DeserializeOwned>(&self, sealed: &str) -> Result<T, SealError> {
        workos::unseal(sealed, &self.password).map_err(|_| SealError::Unseal)
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Session {
        refresh_token: String,
    }

    #[test]
    fn a_sealed_value_unseals_only_with_its_password() {
        let seal = CookieSeal::new(&"ab".repeat(32)).unwrap();
        let session = Session {
            refresh_token: "refresh-secret".into(),
        };
        let sealed = seal.seal(&session).unwrap();
        assert!(!sealed.contains("refresh-secret"));
        assert_ne!(seal.seal(&session).unwrap(), sealed, "a nonce per seal");
        assert_eq!(seal.unseal::<Session>(&sealed).unwrap(), session);

        let other = CookieSeal::new(&"cd".repeat(32)).unwrap();
        assert_eq!(
            other.unseal::<Session>(&sealed).unwrap_err(),
            SealError::Unseal
        );
        let mut cut = sealed.clone();
        cut.truncate(sealed.len() - 4);
        assert_eq!(seal.unseal::<Session>(&cut).unwrap_err(), SealError::Unseal);
        assert_eq!(
            seal.unseal::<Session>("not base64 !").unwrap_err(),
            SealError::Unseal
        );
        assert!(!format!("{seal:?}").contains("abab"));
    }

    #[test]
    fn the_password_is_64_hexadecimal_characters() {
        for invalid in ["", "short", &"zz".repeat(32), &"ab".repeat(33)] {
            assert_eq!(
                CookieSeal::new(invalid).unwrap_err(),
                ConfigError::InvalidCookiePassword,
                "{invalid}"
            );
        }
        assert!(CookieSeal::new(&"0F".repeat(32)).is_ok());
    }
}
