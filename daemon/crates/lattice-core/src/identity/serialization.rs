use serde::de::{self, DeserializeOwned};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use super::ambiguity::ResolveOutcome;
use super::encoding::{decode_identity, encode_identity};
use super::kinds::{
    ContextHandleId, DocId, EventId, FileId, Identity, MemoryId, SectionId, SymbolId,
};
use super::resolver::ResolveError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityPayload {
    pub id: String,
    pub fields: Identity,
    pub legacy_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityAmbiguityPayload {
    pub status: String,
    pub query: String,
    pub disambiguation_hint: String,
    pub candidates: Vec<IdentityPayload>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum IdentityPayloadOrAmbiguity {
    Resolved {
        payload: IdentityPayload,
    },
    Ambiguous {
        query: String,
        disambiguation_hint: String,
        candidates: Vec<IdentityPayload>,
    },
    NotFound {
        kind: String,
        query: String,
        reason: String,
    },
}

impl IdentityPayload {
    pub fn new(identity: Identity, legacy_name: Option<String>) -> Self {
        Self {
            id: encode_identity(&identity),
            fields: identity,
            legacy_name,
        }
    }
}

impl Serialize for IdentityPayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("IdentityPayload", 3)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("fields", &identity_fields_value(&self.fields))?;
        if let Some(legacy_name) = &self.legacy_name {
            state.serialize_field("legacy_name", legacy_name)?;
        }
        state.end()
    }
}

impl<'de> Deserialize<'de> for IdentityPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawIdentityPayload {
            id: String,
            fields: Option<Value>,
            legacy_name: Option<String>,
        }

        let raw = RawIdentityPayload::deserialize(deserializer)?;
        let decoded = decode_identity(&raw.id).map_err(de::Error::custom)?;
        let fields = if let Some(value) = raw.fields {
            decode_identity_fields(&decoded, value).map_err(de::Error::custom)?
        } else {
            decoded
        };

        Ok(Self {
            id: raw.id,
            fields,
            legacy_name: raw.legacy_name,
        })
    }
}

pub fn serialize_outcome<T>(
    outcome: ResolveOutcome<T>,
    legacy_name: Option<&str>,
) -> IdentityPayloadOrAmbiguity
where
    T: Into<Identity>,
{
    match outcome {
        ResolveOutcome::Unique(identity) => IdentityPayloadOrAmbiguity::Resolved {
            payload: IdentityPayload::new(identity.into(), legacy_name.map(str::to_string)),
        },
        ResolveOutcome::Ambiguous(report) => IdentityPayloadOrAmbiguity::Ambiguous {
            query: report.query,
            disambiguation_hint: report.disambiguation_hint,
            candidates: report
                .candidates
                .into_iter()
                .map(|candidate| {
                    IdentityPayload::new(candidate.into(), legacy_name.map(str::to_string))
                })
                .collect(),
        },
        ResolveOutcome::NotFound(error) => IdentityPayloadOrAmbiguity::NotFound {
            kind: not_found_kind(&error).to_string(),
            query: not_found_query(&error),
            reason: error.to_string(),
        },
    }
}

fn identity_fields_value(identity: &Identity) -> Value {
    match identity {
        Identity::File(value) => {
            serde_json::to_value(value).expect("file identity fields serialize")
        }
        Identity::Symbol(value) => {
            serde_json::to_value(value).expect("symbol identity fields serialize")
        }
        Identity::Doc(value) => serde_json::to_value(value).expect("doc identity fields serialize"),
        Identity::Section(value) => {
            serde_json::to_value(value).expect("section identity fields serialize")
        }
        Identity::Event(value) => {
            serde_json::to_value(value).expect("event identity fields serialize")
        }
        Identity::Memory(value) => {
            serde_json::to_value(value).expect("memory identity fields serialize")
        }
        Identity::ContextHandle(value) => {
            serde_json::to_value(value).expect("context handle identity fields serialize")
        }
    }
}

fn decode_identity_fields(expected: &Identity, value: Value) -> Result<Identity, String> {
    if let Value::String(compact) = &value {
        return decode_identity(compact).map_err(|error| error.to_string());
    }

    match expected {
        Identity::File(_) => decode_struct::<FileId>(value).map(Identity::File),
        Identity::Symbol(_) => decode_struct::<SymbolId>(value).map(Identity::Symbol),
        Identity::Doc(_) => decode_struct::<DocId>(value).map(Identity::Doc),
        Identity::Section(_) => decode_struct::<SectionId>(value).map(Identity::Section),
        Identity::Event(_) => decode_struct::<EventId>(value).map(Identity::Event),
        Identity::Memory(_) => decode_struct::<MemoryId>(value).map(Identity::Memory),
        Identity::ContextHandle(_) => {
            decode_struct::<ContextHandleId>(value).map(Identity::ContextHandle)
        }
    }
}

fn decode_struct<T>(value: Value) -> Result<T, String>
where
    T: DeserializeOwned,
{
    serde_json::from_value(value).map_err(|error| error.to_string())
}

fn not_found_kind(error: &ResolveError) -> &'static str {
    match error {
        ResolveError::WorkspaceNotIndexed { .. } => "workspace",
        ResolveError::Malformed { kind, .. } => kind,
        ResolveError::NotFound { kind, .. } => kind,
        ResolveError::IndexLagBehind { .. } => "event",
    }
}

fn not_found_query(error: &ResolveError) -> String {
    match error {
        ResolveError::WorkspaceNotIndexed { workspace_id } => workspace_id.clone(),
        ResolveError::Malformed { query, .. } => query.clone(),
        ResolveError::NotFound { query, .. } => query.clone(),
        ResolveError::IndexLagBehind { latest_event_id } => latest_event_id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{serialize_outcome, IdentityPayload, IdentityPayloadOrAmbiguity};
    use crate::identity::ambiguity::{AmbiguityReport, ResolveOutcome};
    use crate::identity::kinds::{FileId, Identity, SymbolId};
    use crate::identity::resolver::ResolveError;

    fn symbol_identity() -> Identity {
        Identity::Symbol(SymbolId {
            file: FileId {
                workspace_id: "workspace-main".to_string(),
                repo_relative_path: "src/auth.ts".to_string(),
                content_hash: "abcdef12".to_string(),
            },
            qualified_name: "loginUser".to_string(),
            byte_offset: 128,
            kind: "function".to_string(),
        })
    }

    #[test]
    fn compact_form_round_trips() {
        let payload = IdentityPayload::new(symbol_identity(), Some("loginUser".to_string()));

        let serialized = serde_json::to_value(&payload).expect("serialize payload");
        let deserialized: IdentityPayload =
            serde_json::from_value(serialized.clone()).expect("deserialize payload");

        assert_eq!(deserialized, payload);
        assert_eq!(serialized["legacy_name"].as_str(), Some("loginUser"));
        assert_eq!(serialized["id"].as_str(), Some(payload.id.as_str()));
    }

    #[test]
    fn structured_form_is_accepted() {
        let payload = IdentityPayload::new(symbol_identity(), Some("loginUser".to_string()));
        let parsed: IdentityPayload = serde_json::from_value(json!({
            "id": payload.id,
            "fields": {
                "file": {
                    "workspace_id": "workspace-main",
                    "repo_relative_path": "src/auth.ts",
                    "content_hash": "abcdef12"
                },
                "qualified_name": "loginUser",
                "byte_offset": 128,
                "kind": "function"
            },
            "legacy_name": "loginUser"
        }))
        .expect("structured payload should deserialize");

        assert_eq!(parsed.fields, symbol_identity());
        assert_eq!(parsed.legacy_name.as_deref(), Some("loginUser"));
    }

    #[test]
    fn legacy_name_is_preserved() {
        let outcome = ResolveOutcome::Unique(SymbolId {
            file: FileId {
                workspace_id: "workspace-main".to_string(),
                repo_relative_path: "src/auth.ts".to_string(),
                content_hash: "abcdef12".to_string(),
            },
            qualified_name: "loginUser".to_string(),
            byte_offset: 128,
            kind: "function".to_string(),
        });

        let serialized = serialize_outcome(outcome, Some("loginUser"));

        match serialized {
            IdentityPayloadOrAmbiguity::Resolved { payload } => {
                assert_eq!(payload.legacy_name.as_deref(), Some("loginUser"));
            }
            other => panic!("expected resolved payload, got {other:?}"),
        }
    }

    #[test]
    fn ambiguity_payload_shape_is_structured() {
        let candidate = SymbolId {
            file: FileId {
                workspace_id: "workspace-main".to_string(),
                repo_relative_path: "src/auth.ts".to_string(),
                content_hash: "abcdef12".to_string(),
            },
            qualified_name: "loginUser".to_string(),
            byte_offset: 128,
            kind: "function".to_string(),
        };
        let ambiguous = ResolveOutcome::Ambiguous(AmbiguityReport::new(
            "loginUser",
            vec![candidate.clone(), candidate],
            "Add the parent module or file path, or use the stable symbol identity.",
        ));

        let serialized = serde_json::to_value(serialize_outcome(ambiguous, Some("loginUser")))
            .expect("serialize ambiguity");

        assert_eq!(serialized["status"].as_str(), Some("ambiguous"));
        assert_eq!(serialized["query"].as_str(), Some("loginUser"));
        assert_eq!(serialized["candidates"].as_array().map(Vec::len), Some(2));
        assert_eq!(
            serialized["candidates"][0]["legacy_name"].as_str(),
            Some("loginUser")
        );
    }

    #[test]
    fn not_found_outcome_is_truthful() {
        let serialized = serde_json::to_value(serialize_outcome(
            ResolveOutcome::<SymbolId>::NotFound(ResolveError::NotFound {
                kind: "symbol",
                query: "missing".to_string(),
            }),
            Some("missing"),
        ))
        .expect("serialize not found");

        assert_eq!(serialized["status"].as_str(), Some("not_found"));
        assert_eq!(serialized["kind"].as_str(), Some("symbol"));
        assert!(serialized["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("no symbol identity found")));
    }
}
