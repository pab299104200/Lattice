use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::events::kinds::EventPayload;

pub const HASH_ALGORITHM: &str = "sha256";
const HASH_PREFIX: &str = "sha256:";

/// Stable 32-byte payload hash encoded as `sha256:<hex>` on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PayloadHash([u8; 32]);

impl PayloadHash {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        let mut output = String::with_capacity(64);
        for byte in self.0 {
            use std::fmt::Write as _;
            let _ = write!(&mut output, "{byte:02x}");
        }
        output
    }
}

impl fmt::Display for PayloadHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{HASH_PREFIX}{}", self.to_hex())
    }
}

impl Serialize for PayloadHash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PayloadHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PayloadHashVisitor;

        impl<'de> Visitor<'de> for PayloadHashVisitor {
            type Value = PayloadHash;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a sha256 payload hash in sha256:<hex> form")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                parse_payload_hash(value).map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                parse_payload_hash(&value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(PayloadHashVisitor)
    }
}

pub fn hash_payload(payload: &EventPayload) -> Result<PayloadHash, serde_json::Error> {
    let bytes = canonical_json_bytes(payload)?;
    Ok(hash_canonical_payload_bytes(&bytes))
}

pub fn hash_canonical_payload_bytes(bytes: &[u8]) -> PayloadHash {
    let digest = Sha256::digest(bytes);
    let mut hash = [0_u8; 32];
    hash.copy_from_slice(&digest);
    PayloadHash::new(hash)
}

pub fn canonical_json_bytes<T>(value: &T) -> Result<Vec<u8>, serde_json::Error>
where
    T: Serialize,
{
    let value = serde_json::to_value(value)?;
    let canonical = canonicalize_json_value(value);
    serde_json::to_vec(&canonical)
}

pub fn canonicalize_json_value(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(canonicalize_json_value)
                .collect::<Vec<_>>(),
        ),
        Value::Object(map) => Value::Object(canonicalize_json_map(map)),
        other => other,
    }
}

fn canonicalize_json_map(map: Map<String, Value>) -> Map<String, Value> {
    let mut entries = map.into_iter().collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut canonical = Map::with_capacity(entries.len());
    for (key, value) in entries {
        canonical.insert(key, canonicalize_json_value(value));
    }
    canonical
}

fn parse_payload_hash(value: &str) -> Result<PayloadHash, String> {
    let hex = value
        .strip_prefix(HASH_PREFIX)
        .ok_or_else(|| "payload hash must start with `sha256:`".to_string())?;
    if hex.len() != 64 {
        return Err("payload hash must contain 64 lowercase hex characters".to_string());
    }

    let mut bytes = [0_u8; 32];
    for (index, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
        let pair = std::str::from_utf8(chunk)
            .map_err(|_| "payload hash must contain valid UTF-8 hex".to_string())?;
        bytes[index] = u8::from_str_radix(pair, 16)
            .map_err(|_| "payload hash must contain lowercase hex characters".to_string())?;
    }
    Ok(PayloadHash::new(bytes))
}
