use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::fmt;
use std::str::FromStr;

/// A SHA-256 content address.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub fn of(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        Hash(out)
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// First 12 hex chars, for display.
    pub fn short(&self) -> String {
        self.to_hex()[..12].to_string()
    }

    pub fn from_hex(s: &str) -> Result<Self, crate::Error> {
        let bytes = hex::decode(s).map_err(|e| crate::Error::Invalid(format!("bad hash {s}: {e}")))?;
        if bytes.len() != 32 {
            return Err(crate::Error::Invalid(format!("bad hash length: {s}")));
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        Ok(Hash(out))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.short())
    }
}

impl FromStr for Hash {
    type Err = crate::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Hash::from_hex(s)
    }
}

impl Serialize for Hash {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Hash::from_hex(&s).map_err(serde::de::Error::custom)
    }
}
