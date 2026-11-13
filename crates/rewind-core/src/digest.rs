use std::fmt;

use sha2::{Digest as _, Sha256};

/// SHA-256 digest. Identifies immutable content: payloads (by their canonical JSON), scripts
/// and toolsets.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest([u8; 32]);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid digest {0:?}: expected 64 hex digits")]
pub struct InvalidDigest(pub String);

impl Digest {
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    pub fn from_hex(hex: &str) -> Result<Self, InvalidDigest> {
        let invalid = || InvalidDigest(hex.to_owned());
        if hex.len() != 64 || !hex.is_ascii() {
            return Err(invalid());
        }
        let mut bytes = [0u8; 32];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|_| invalid())?;
        }
        Ok(Self(bytes))
    }

    /// First 12 hex digits, for display.
    pub fn short(&self) -> String {
        self.to_hex()[..12].to_owned()
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", self.short())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let digest = Digest::of(b"null");
        assert_eq!(
            digest.to_hex(),
            "74234e98afe7498fb5daf1f36ac2d78acc339464f950703b8c019892f982b90b"
        );
        assert_eq!(Digest::from_hex(&digest.to_hex()).unwrap(), digest);
        assert!(Digest::from_hex("zz").is_err());
        assert!(Digest::from_hex(&"g".repeat(64)).is_err());
    }
}
