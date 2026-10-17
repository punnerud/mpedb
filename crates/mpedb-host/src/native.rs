use super::*;
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use std::{
    io::{Read, Write},
    path::PathBuf,
};
struct Root {
    configured: PathBuf,
    dir: Dir,
}
pub struct NativeBackend {
    roots: BTreeMap<String, Root>,
    #[cfg(feature = "http")]
    clients: BTreeMap<String, crate::http::Transport>,
}
impl NativeBackend {
    pub fn new(environment: &Environment) -> Result<Self> {
        environment.validate()?;
        let mut roots = BTreeMap::new();
        for (name, config) in &environment.directories {
            if config.read || config.write {
                roots.insert(
                    name.clone(),
                    Root {
                        configured: config.root.clone(),
                        dir: Dir::open_ambient_dir(&config.root, ambient_authority())?,
                    },
                );
            }
        }
        #[cfg(feature = "http")]
        let clients = environment
            .endpoints
            .iter()
            .map(|(name, target)| Ok((name.clone(), crate::http::Transport::new(target.clone())?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        Ok(Self {
            roots,
            #[cfg(feature = "http")]
            clients,
        })
    }
    fn root(&self, name: &str, environment: &Environment) -> Result<&Dir> {
        let root = self
            .roots
            .get(name)
            .ok_or(Error::Denied("directory was not opened by host bootstrap"))?;
        if !environment
            .directories
            .get(name)
            .is_some_and(|d| d.root == root.configured)
        {
            return Err(Error::Denied(
                "provider directory does not match environment",
            ));
        }
        Ok(&root.dir)
    }
}
impl Backend for NativeBackend {
    fn execute(&self, authorized: Authorized<'_>) -> Result<Response> {
        match authorized.request {
            Request::ReadFile { directory, path } => {
                let dir = self.root(directory, authorized.environment)?;
                let mut options = OpenOptions::new();
                options.read(true);
                #[cfg(unix)]
                {
                    use cap_std::fs::OpenOptionsExt;
                    options.custom_flags(libc::O_NONBLOCK);
                }
                let file = dir.open_with(path, &options)?;
                if !file.metadata()?.is_file() {
                    return Err(Error::Denied("host file reads require regular files"));
                }
                let mut bytes = Vec::new();
                file.take(authorized.limits.read_bytes + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() as u64 > authorized.limits.read_bytes {
                    return Err(Error::Limit("file read byte budget exceeded"));
                }
                Ok(Response::File { bytes })
            }
            Request::WriteFile {
                directory,
                path,
                bytes,
            } => {
                let dir = self.root(directory, authorized.environment)?;
                let path = policy::relative_file(path)?;
                let parent = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(std::path::Path::new("."));
                let parent = dir.open_dir(parent)?;
                let name = path.file_name().ok_or(Error::Denied("missing file name"))?;
                let mut nonce = [0u8; 16];
                getrandom::fill(&mut nonce)
                    .map_err(|_| Error::Transport("host file nonce unavailable"))?;
                let temporary = format!(
                    ".mpedb-host-{}",
                    nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
                );
                let mut options = OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use cap_std::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = parent.open_with(&temporary, &options)?;
                let outcome = (|| {
                    file.write_all(bytes)?;
                    file.sync_all()?;
                    drop(file);
                    // Replace the directory entry, never truncate through a
                    // symlink or mutate another hard-link to an existing file.
                    parent.rename(&temporary, &parent, name)?;
                    // cap-std directory capabilities may use O_PATH, which
                    // cannot be fsynced. Open the pinned directory read-only.
                    #[cfg(unix)]
                    parent.open(".")?.sync_all()?;
                    Ok(Response::Written {
                        bytes: bytes.len() as u64,
                    })
                })();
                if outcome.is_err() {
                    let _ = parent.remove_file(&temporary);
                }
                outcome
            }
            Request::Http {
                endpoint,
                method,
                path,
                headers,
                body,
            } => {
                #[cfg(feature = "http")]
                {
                    let target = authorized
                        .environment
                        .endpoints
                        .get(endpoint)
                        .ok_or(Error::Denied("endpoint missing"))?;
                    let transport = self
                        .clients
                        .get(endpoint)
                        .ok_or(Error::Denied("endpoint was not prepared by host bootstrap"))?;
                    if &transport.target != target {
                        return Err(Error::Denied(
                            "provider endpoint does not match environment",
                        ));
                    }
                    transport.request(method, path, headers, body, authorized.limits)
                }
                #[cfg(not(feature = "http"))]
                {
                    let _ = (endpoint, method, path, headers, body);
                    Err(Error::Denied("http feature is disabled"))
                }
            }
        }
    }
}
