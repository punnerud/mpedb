use super::*;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    RwLock,
};
/// Unforgeable authorization envelope. Only Runtime constructs this after
/// checking BOTH layers; providers must enforce the limits and pinned targets.
pub struct Authorized<'a> {
    pub(crate) principal: &'a str,
    pub(crate) request: &'a Request,
    pub(crate) limits: Limits,
    pub(crate) environment: &'a Environment,
}
impl Authorized<'_> {
    pub fn principal(&self) -> &str {
        self.principal
    }
    pub fn request(&self) -> &Request {
        self.request
    }
    pub fn limits(&self) -> Limits {
        self.limits
    }
    pub fn environment(&self) -> &Environment {
        self.environment
    }
}
/// Trusted host-side provider, interchangeable across database adapters.
pub trait Backend: Send + Sync {
    fn execute(&self, authorized: Authorized<'_>) -> Result<Response>;
}
pub struct Runtime<B: Backend> {
    environment: Environment,
    grants: RwLock<BTreeMap<String, Grants>>,
    backend: B,
    in_flight: AtomicU32,
}
impl<B: Backend> Runtime<B> {
    /// Trusted host bootstrap: SQL/PLSQL cannot replace this environment.
    pub fn new(
        environment: Environment,
        grants: BTreeMap<String, Grants>,
        backend: B,
    ) -> Result<Self> {
        environment.validate()?;
        for (principal, g) in &grants {
            policy::valid_name(principal)?;
            g.limits.validate()?;
        }
        Ok(Self {
            environment,
            grants: RwLock::new(grants),
            backend,
            in_flight: AtomicU32::new(0),
        })
    }
    /// Database-side administration is allowed when BOTH the environment and
    /// the authenticated actor permit it. It changes only the INNER layer.
    pub fn set_grants(&self, actor: &str, subject: &str, grants: Grants) -> Result<()> {
        policy::valid_name(subject)?;
        grants.limits.validate()?;
        let mut acl = self
            .grants
            .write()
            .map_err(|_| Error::Transport("host ACL lock poisoned"))?;
        if !self.environment.allow_acl_changes || !acl.get(actor).is_some_and(|g| g.manage_access) {
            return Err(Error::Denied(
                "ACL administration requires environment and database authority",
            ));
        }
        acl.insert(subject.into(), grants);
        Ok(())
    }
    pub fn execute(&self, principal: &str, request: &Request) -> Result<Response> {
        let limits = {
            let acl = self
                .grants
                .read()
                .map_err(|_| Error::Transport("host ACL lock poisoned"))?;
            let grant = acl
                .get(principal)
                .ok_or(Error::Denied("principal has no host grants"))?;
            match request {
                Request::ReadFile { directory, path } => {
                    policy::relative_file(path)?;
                    if !self
                        .environment
                        .directories
                        .get(directory)
                        .is_some_and(|d| d.read)
                        || !grant.read_directories.contains(directory)
                    {
                        return Err(Error::Denied("file read requires both grants"));
                    }
                }
                Request::WriteFile {
                    directory,
                    path,
                    bytes,
                } => {
                    policy::relative_file(path)?;
                    if !self
                        .environment
                        .directories
                        .get(directory)
                        .is_some_and(|d| d.write)
                        || !grant.write_directories.contains(directory)
                    {
                        return Err(Error::Denied("file write requires both grants"));
                    }
                    if bytes.len() as u64
                        > self.environment.limits.intersect(grant.limits).write_bytes
                    {
                        return Err(Error::Limit("file write byte budget exceeded"));
                    }
                }
                Request::Http {
                    endpoint,
                    method,
                    path,
                    headers,
                    body,
                } => {
                    let outer = self
                        .environment
                        .endpoints
                        .get(endpoint)
                        .ok_or(Error::Denied("endpoint is outside the environment"))?;
                    if !outer.methods.contains(method)
                        || !grant
                            .endpoints
                            .get(endpoint)
                            .is_some_and(|methods| methods.contains(method))
                    {
                        return Err(Error::Denied("network method requires both grants"));
                    }
                    if path.len() > 8192
                        || headers.len() > 128
                        || headers
                            .iter()
                            .map(|(k, v)| k.len().saturating_add(v.len()))
                            .sum::<usize>()
                            > 65536
                    {
                        return Err(Error::Limit("network path/header budget exceeded"));
                    }
                    if body.len() as u64
                        > self.environment.limits.intersect(grant.limits).write_bytes
                    {
                        return Err(Error::Limit("network request byte budget exceeded"));
                    }
                    #[cfg(feature = "http")]
                    crate::http::validate_request(outer, path, headers)?;
                    #[cfg(not(feature = "http"))]
                    return Err(Error::Denied("http feature is disabled"));
                }
            }
            self.environment.limits.intersect(grant.limits)
        };
        // Release ACL lock before I/O. Revocation applies to subsequent calls.
        self.in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.environment.max_in_flight_per_runtime).then_some(n + 1)
            })
            .map_err(|_| Error::Limit("host in-flight budget exhausted"))?;
        struct Permit<'a>(&'a AtomicU32);
        impl Drop for Permit<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Release);
            }
        }
        let _permit = Permit(&self.in_flight);
        self.backend.execute(Authorized {
            principal,
            request,
            limits,
            environment: &self.environment,
        })
    }
}
impl Runtime<NativeBackend> {
    pub fn native(environment: Environment, grants: BTreeMap<String, Grants>) -> Result<Self> {
        environment.validate()?;
        let backend = NativeBackend::new(&environment)?;
        Self::new(environment, grants, backend)
    }
}
