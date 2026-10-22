use std::fmt;
use std::io;

/// Everything that can go wrong in the engine.
#[derive(Debug)]
pub enum Error {
    /// The operating system refused a read, write or sync.
    Io(io::Error),
    /// The file is not a KvSQL file, or a page failed its checks.
    Corrupt(String),
    /// Another process holds the file open for writing.
    Locked,
    /// A key is empty or longer than [`crate::MAX_KEY_LEN`].
    KeyTooLarge(usize),
    /// A value is longer than [`crate::MAX_VALUE_LEN`].
    ValueTooLarge(usize),
    /// A keyspace name is empty or longer than [`crate::MAX_KEY_LEN`].
    BadKeyspaceName,
    /// An earlier write failed in a way that left this handle unsafe to use.
    /// Reopen the file to recover the last committed state.
    Poisoned,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "i/o error: {e}"),
            Error::Corrupt(m) => write!(f, "corrupt file: {m}"),
            Error::Locked => write!(f, "file is locked by another writer"),
            Error::KeyTooLarge(n) => {
                write!(f, "key length {n} is outside 1..={}", crate::MAX_KEY_LEN)
            }
            Error::ValueTooLarge(n) => {
                write!(f, "value length {n} exceeds {}", crate::MAX_VALUE_LEN)
            }
            Error::BadKeyspaceName => write!(f, "keyspace name is empty or too long"),
            Error::Poisoned => write!(f, "handle is poisoned by an earlier failed write; reopen"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn corrupt<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Corrupt(msg.into()))
}
