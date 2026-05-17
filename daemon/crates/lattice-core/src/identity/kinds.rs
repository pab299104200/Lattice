use std::fmt;

use serde::{Deserialize, Serialize};

use super::encoding::encode_identity;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FileId {
    pub workspace_id: String,
    pub repo_relative_path: String,
    pub content_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SymbolId {
    pub file: FileId,
    pub qualified_name: String,
    pub byte_offset: usize,
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DocId {
    pub workspace_id: String,
    pub repo_relative_path: String,
    pub content_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SectionId {
    pub doc: DocId,
    pub heading_path: Vec<String>,
    pub byte_offset: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventId {
    pub workspace_id: String,
    pub ulid: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MemoryId {
    pub workspace_id: String,
    pub ulid: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OperatorId {
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContextHandleId {
    pub workspace_id: String,
    pub session_id: String,
    pub ulid: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum IdentityKind {
    File,
    Symbol,
    Doc,
    Section,
    Event,
    Memory,
    ContextHandle,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Identity {
    File(FileId),
    Symbol(SymbolId),
    Doc(DocId),
    Section(SectionId),
    Event(EventId),
    Memory(MemoryId),
    ContextHandle(ContextHandleId),
}

impl Identity {
    pub fn kind(&self) -> IdentityKind {
        match self {
            Identity::File(_) => IdentityKind::File,
            Identity::Symbol(_) => IdentityKind::Symbol,
            Identity::Doc(_) => IdentityKind::Doc,
            Identity::Section(_) => IdentityKind::Section,
            Identity::Event(_) => IdentityKind::Event,
            Identity::Memory(_) => IdentityKind::Memory,
            Identity::ContextHandle(_) => IdentityKind::ContextHandle,
        }
    }
}

impl From<FileId> for Identity {
    fn from(value: FileId) -> Self {
        Identity::File(value)
    }
}

impl From<SymbolId> for Identity {
    fn from(value: SymbolId) -> Self {
        Identity::Symbol(value)
    }
}

impl From<DocId> for Identity {
    fn from(value: DocId) -> Self {
        Identity::Doc(value)
    }
}

impl From<SectionId> for Identity {
    fn from(value: SectionId) -> Self {
        Identity::Section(value)
    }
}

impl From<EventId> for Identity {
    fn from(value: EventId) -> Self {
        Identity::Event(value)
    }
}

impl From<MemoryId> for Identity {
    fn from(value: MemoryId) -> Self {
        Identity::Memory(value)
    }
}

impl From<ContextHandleId> for Identity {
    fn from(value: ContextHandleId) -> Self {
        Identity::ContextHandle(value)
    }
}

impl From<crate::symbols::SymbolId> for SymbolId {
    fn from(value: crate::symbols::SymbolId) -> Self {
        SymbolId {
            file: FileId {
                workspace_id: "legacy".to_string(),
                repo_relative_path: value.file,
                content_hash: "00000000".to_string(),
            },
            qualified_name: value.name,
            byte_offset: value.byte_offset,
            kind: "unknown".to_string(),
        }
    }
}

macro_rules! impl_identity_display {
    ($type_name:ty, $variant:ident) => {
        impl fmt::Display for $type_name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "{}",
                    encode_identity(&Identity::$variant(self.clone()))
                )
            }
        }
    };
}

impl_identity_display!(FileId, File);
impl_identity_display!(SymbolId, Symbol);
impl_identity_display!(DocId, Doc);
impl_identity_display!(SectionId, Section);
impl_identity_display!(EventId, Event);
impl_identity_display!(MemoryId, Memory);
impl_identity_display!(ContextHandleId, ContextHandle);

impl fmt::Display for OperatorId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.value)
    }
}

impl fmt::Display for Identity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", encode_identity(self))
    }
}
