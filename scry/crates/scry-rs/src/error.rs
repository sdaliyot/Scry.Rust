use std::fmt;

/// Why a path or operation failed. [`Error::code`] gives a stable machine-readable code, in the
/// same spirit as Scry.NET's error codes.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    NoSuchRoot(String),
    /// A short root name (`APP`) matches more than one full one (`a::APP`, `b::APP`).
    AmbiguousRoot { name: String, candidates: Vec<String> },
    NoSuchMember { type_name: &'static str, member: String },
    IndexOutOfRange { len: usize, index: usize },
    /// The path continues past a value that has no children (a scalar or an opaque value).
    NotNavigable { type_name: &'static str },
    /// The value is empty (`None`, an unset `OnceLock`) and the path continues into it.
    Empty { type_name: &'static str },
    NotWritable { reason: String },
    TypeMismatch { expected: &'static str, found: String },
    /// A lock on the path stayed held by the app for the whole timeout.
    LockBusy { type_name: &'static str },
    BadPath(String),
    BadCommand(String),
    /// A script did not parse, or failed while running.
    Script(String),
    /// A script ran past its time or size limits and was stopped.
    ScriptLimit(String),
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Error::NoSuchRoot(_) => "no_such_root",
            Error::AmbiguousRoot { .. } => "ambiguous_root",
            Error::NoSuchMember { .. } => "no_such_member",
            Error::IndexOutOfRange { .. } => "index_out_of_range",
            Error::NotNavigable { .. } => "not_navigable",
            Error::Empty { .. } => "empty",
            Error::NotWritable { .. } => "not_writable",
            Error::TypeMismatch { .. } => "type_mismatch",
            Error::LockBusy { .. } => "lock_busy",
            Error::BadPath(_) => "bad_path",
            Error::BadCommand(_) => "bad_command",
            Error::Script(_) => "script_error",
            Error::ScriptLimit(_) => "script_limit",
        }
    }

    pub(crate) fn not_writable(reason: impl Into<String>) -> Self {
        Error::NotWritable { reason: reason.into() }
    }

    pub(crate) fn needs_interior_mutability(type_name: &'static str) -> Self {
        Error::not_writable(format!(
            "nothing on the path to this {type_name} can be written safely from outside - a write \
             needs a Mutex, RwLock, atomic or signal on its way down"
        ))
    }

    pub(crate) fn mismatch(expected: &'static str, found: &serde_json::Value) -> Self {
        Error::TypeMismatch { expected, found: found.to_string() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoSuchRoot(name) => write!(f, "no root named '{name}'"),
            Error::AmbiguousRoot { name, candidates } => {
                write!(f, "'{name}' matches several roots: {}", candidates.join(", "))
            }
            Error::NoSuchMember { type_name, member } => write!(f, "{type_name} has no member '{member}'"),
            Error::IndexOutOfRange { len, index } => write!(f, "index {index} is out of range (length {len})"),
            Error::NotNavigable { type_name } => write!(f, "{type_name} has no members to navigate into"),
            Error::Empty { type_name } => write!(f, "{type_name} is empty"),
            Error::NotWritable { reason } => write!(f, "not writable: {reason}"),
            Error::TypeMismatch { expected, found } => write!(f, "expected {expected}, got {found}"),
            Error::LockBusy { type_name } => write!(f, "{type_name} stayed locked by the application; try again"),
            Error::BadPath(message) => write!(f, "bad path: {message}"),
            Error::BadCommand(message) => write!(f, "bad command: {message}"),
            Error::Script(message) => write!(f, "script error: {message}"),
            Error::ScriptLimit(message) => write!(f, "script stopped: {message}"),
        }
    }
}

impl std::error::Error for Error {}
