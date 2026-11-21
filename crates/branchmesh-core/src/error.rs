use crate::merge::Conflict;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("storage error: {0}")]
    Storage(String),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("object not found: {0}")]
    ObjectNotFound(String),
    #[error("branch not found: {0}")]
    BranchNotFound(String),
    #[error("branch already exists: {0}")]
    BranchExists(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("concurrent update of ref {0}; retry")]
    RefConflict(String),
    #[error("merge has {} unresolved conflict(s)", .0.len())]
    MergeConflict(Vec<Conflict>),
    #[error("non-deterministic replay in execution {execution} at seq {seq}: journal has '{recorded}', code requested '{requested}'")]
    NonDeterminism {
        execution: String,
        seq: u64,
        recorded: String,
        requested: String,
    },
    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn other(msg: impl std::fmt::Display) -> Self {
        Error::Other(msg.to_string())
    }
}

macro_rules! storage_from {
    ($($t:ty),* $(,)?) => {
        $(impl From<$t> for Error {
            fn from(e: $t) -> Self { Error::Storage(e.to_string()) }
        })*
    };
}

storage_from!(
    redb::Error,
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
);
