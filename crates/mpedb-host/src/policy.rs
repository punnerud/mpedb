use super::*;
use std::{
    collections::BTreeSet,
    net::IpAddr,
    path::{Component, Path, PathBuf},
};
const MAX_BYTES: u64 = 64 * 1024 * 1024;
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub timeout_ms: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            read_bytes: 1024 * 1024,
            write_bytes: 1024 * 1024,
            timeout_ms: 10_000,
        }
    }
}
impl Limits {
    pub(crate) fn validate(self) -> Result<()> {
        if self.read_bytes > MAX_BYTES
            || self.write_bytes > MAX_BYTES
            || self.timeout_ms == 0
            || self.timeout_ms > 120_000
        {
            return Err(Error::Config("invalid byte/time limits".into()));
        }
        Ok(())
    }
    pub(crate) fn intersect(self, other: Self) -> Self {
        Self {
            read_bytes: self.read_bytes.min(other.read_bytes),
            write_bytes: self.write_bytes.min(other.write_bytes),
            timeout_ms: self.timeout_ms.min(other.timeout_ms),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Directory {
    pub root: PathBuf,
    #[serde(default)]
    pub read: bool,
    #[serde(default)]
    pub write: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub base_url: String,
    /// Explicit runtime addresses: no ambient DNS, proxy or redirect fallback.
    pub connect_ips: Vec<IpAddr>,
    pub methods: BTreeSet<String>,
    #[serde(default)]
    pub headers: BTreeSet<String>,
    #[serde(default)]
    pub allow_http: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Environment {
    pub directories: BTreeMap<String, Directory>,
    pub endpoints: BTreeMap<String, Endpoint>,
    pub limits: Limits,
    /// Outer ceiling on dynamic database-side ACL administration.
    pub allow_acl_changes: bool,
    #[serde(default = "default_in_flight")]
    pub max_in_flight_per_runtime: u32,
}
fn default_in_flight() -> u32 {
    64
}
impl Default for Environment {
    fn default() -> Self {
        Self {
            directories: BTreeMap::new(),
            endpoints: BTreeMap::new(),
            limits: Limits::default(),
            allow_acl_changes: false,
            max_in_flight_per_runtime: default_in_flight(),
        }
    }
}
impl Environment {
    pub fn from_toml(text: &str) -> Result<Self> {
        let policy: Self =
            toml::from_str(text).map_err(|_| Error::Config("invalid host policy TOML".into()))?;
        policy.validate()?;
        Ok(policy)
    }
    pub(crate) fn validate(&self) -> Result<()> {
        self.limits.validate()?;
        if self.max_in_flight_per_runtime == 0 || self.max_in_flight_per_runtime > 65536 {
            return Err(Error::Config("invalid host in-flight budget".into()));
        }
        for (name, dir) in &self.directories {
            valid_name(name)?;
            if !dir.root.is_absolute() {
                return Err(Error::Config(
                    "host directory roots must be absolute".into(),
                ));
            }
        }
        #[cfg(not(feature = "http"))]
        if !self.endpoints.is_empty() {
            return Err(Error::Config(
                "network endpoints require the http feature".into(),
            ));
        }
        #[cfg(feature = "http")]
        for (name, endpoint) in &self.endpoints {
            valid_name(name)?;
            if endpoint.connect_ips.is_empty()
                || endpoint.connect_ips.len() > 64
                || endpoint.methods.is_empty()
                || endpoint.methods.iter().any(|m| !valid_method(m))
                || endpoint.headers.iter().any(|h| {
                    h.is_empty()
                        || h != &h.to_ascii_lowercase()
                        || !h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
            {
                return Err(Error::Config(
                    "invalid host endpoint addresses/methods/headers".into(),
                ));
            }
            #[cfg(feature = "http")]
            crate::http::validate_endpoint(endpoint)?;
        }
        Ok(())
    }
}
/// Mutable database/user/routine grants. The embedding catalog decides who a
/// principal is; it must never take the principal from an untrusted SQL value.
/// Grants may exceed the environment: that NEVER enlarges effective authority.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Grants {
    pub read_directories: BTreeSet<String>,
    pub write_directories: BTreeSet<String>,
    pub endpoints: BTreeMap<String, BTreeSet<String>>,
    pub manage_access: bool,
    pub limits: Limits,
}
pub(crate) fn valid_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
        return Err(Error::Config("invalid host resource/principal name".into()));
    }
    Ok(())
}
#[cfg(feature = "http")]
pub(crate) fn valid_method(method: &str) -> bool {
    !method.is_empty()
        && method.len() <= 32
        && method.bytes().all(|b| b.is_ascii_uppercase() || b == b'-')
}
pub(crate) fn relative_file(path: &str) -> Result<&Path> {
    let p = Path::new(path);
    if path.is_empty()
        || path.ends_with('/')
        || path.len() > 4096
        || path.contains(['\\', '\0'])
        || p.components().any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(Error::Denied(
            "file path must stay below its named directory",
        ));
    }
    Ok(p)
}
