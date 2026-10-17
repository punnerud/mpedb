//! Database-independent host I/O. Every call intersects an immutable outer
//! environment policy with mutable inner ACLs. No database dependency, ambient
//! SQL permissions, shell execution or implicit network/file authority.
//! External effects are NOT rolled back with a database transaction. Embedders
//! must bind principals from authenticated execution context and use a durable
//! job/outbox for effects that must follow commit or survive retry.
#[cfg(feature = "http")]
mod gateway;
#[cfg(feature = "http")]
mod http;
mod native;
mod policy;
mod runtime;
#[cfg(feature = "http")]
pub use gateway::{GatewayBackend, GatewayEnvelope};
pub use native::NativeBackend;
pub use policy::*;
pub use runtime::{Authorized, Backend, Runtime};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    ReadFile {
        directory: String,
        path: String,
    },
    WriteFile {
        directory: String,
        path: String,
        bytes: Vec<u8>,
    },
    Http {
        endpoint: String,
        method: String,
        path: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        #[serde(default)]
        body: Vec<u8>,
    },
}
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    File {
        bytes: Vec<u8>,
    },
    Written {
        bytes: u64,
    },
    Http {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
}
#[derive(Debug)]
pub enum Error {
    Denied(&'static str),
    Config(String),
    Limit(&'static str),
    Io(std::io::Error),
    Transport(&'static str),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Denied(m) => write!(f, "host access denied: {m}"),
            Self::Config(m) => write!(f, "host configuration: {m}"),
            Self::Limit(m) => write!(f, "host limit: {m}"),
            Self::Io(e) => write!(f, "host I/O: {e}"),
            Self::Transport(m) => write!(f, "host transport: {m}"),
        }
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
pub type Result<T> = std::result::Result<T, Error>;
