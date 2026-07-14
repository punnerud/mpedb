//! The source credential channel (DESIGN-MIRROR §12).
//!
//! A DSN carries a password. §12 forbids putting it in `argv` — `ps` shows the
//! full command line of every process on the host to every user, so `mirror pull
//! --dsn 'host=… password=hunter2'` leaks the database password to anyone with a
//! shell, and it lands in `~/.bash_history` besides. Instead the secret lives in
//! a `0600` file and the CLI names that file by *path*; the path is not a secret
//! and may appear in `argv` and in `mir/src`.
//!
//! Storing the secret in a 0600 file is only half the property: a file this
//! process *created* 0600 can have been `chmod`ed since, and a file it did not
//! create may have been planted. [`load`] therefore re-checks ownership and mode
//! at every read and refuses a secret that is readable by anyone but its owner —
//! a loud failure beats silently consuming a world-readable password.
//!
//! [`SourceSpec`]'s `Debug` is hand-written to redact the DSN: a derived one
//! would spill the password into any `unwrap()` panic or `{:?}` log line, which
//! is exactly the leak this module exists to prevent.

use std::fmt;
use std::path::Path;

use mpedb_types::{Error, Result};

use crate::state::SourceKind;

/// Where a mirror's source lives, including any secret needed to reach it.
#[derive(Clone, PartialEq, Eq)]
pub enum SourceSpec {
    Sqlite { path: String },
    Postgres { dsn: String },
}

impl SourceSpec {
    pub fn kind(&self) -> SourceKind {
        match self {
            SourceSpec::Sqlite { .. } => SourceKind::Sqlite,
            SourceSpec::Postgres { .. } => SourceKind::Postgres,
        }
    }

    /// A human-safe rendering: a sqlite path in full, a PG DSN with every
    /// value stripped. Use this in anything a user or a log will ever see.
    pub fn redacted(&self) -> String {
        match self {
            SourceSpec::Sqlite { path } => format!("sqlite:{path}"),
            SourceSpec::Postgres { dsn } => format!("postgres:{}", redact_dsn(dsn)),
        }
    }
}

impl fmt::Debug for SourceSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // NOT derived, on purpose — see the module header.
        write!(f, "SourceSpec({})", self.redacted())
    }
}

/// Keep a DSN's shape (host/db, so an error is diagnosable) but never its
/// values. Handles both `key=value …` and URL forms conservatively: anything
/// that is not a recognised non-secret key is replaced wholesale rather than
/// pattern-matched, so a novel form fails closed.
fn redact_dsn(dsn: &str) -> String {
    if dsn.contains("://") {
        // URL form: postgres://user:pass@host:port/db → postgres://…@host/db
        let (scheme, rest) = dsn.split_once("://").unwrap();
        let hostpart = rest.rsplit_once('@').map(|(_, h)| h).unwrap_or(rest);
        return format!("{scheme}://<redacted>@{hostpart}");
    }
    dsn.split_whitespace()
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) if matches!(k, "host" | "port" | "dbname" | "user") => {
                format!("{k}={v}")
            }
            Some((k, _)) => format!("{k}=<redacted>"),
            None => "<redacted>".to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Read a source-config file, enforcing §12's confidentiality at read time.
///
/// Refuses the file unless it is owned by the effective uid and its mode grants
/// nothing to group or other.
pub fn load(path: &Path) -> Result<SourceSpec> {
    check_perms(path)?;
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Config(format!("read source-config `{}`: {e}", path.display())))?;
    parse(&text)
}

#[cfg(unix)]
fn check_perms(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(path)
        .map_err(|e| Error::Config(format!("stat source-config `{}`: {e}", path.display())))?;
    let mode = md.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(Error::Config(format!(
            "source-config `{}` is mode {mode:04o}: it holds a password and must not be \
             readable by group or other (chmod 600 it). Refusing to read it.",
            path.display()
        )));
    }
    let me = unsafe { libc::geteuid() };
    if md.uid() != me {
        return Err(Error::Config(format!(
            "source-config `{}` is owned by uid {}, not by uid {me}: refusing to read \
             credentials from a file this user does not own.",
            path.display(),
            md.uid()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_perms(_path: &Path) -> Result<()> {
    Ok(())
}

/// Parse the config text. Split out from [`load`] so the codec is testable
/// without a filesystem.
pub fn parse(text: &str) -> Result<SourceSpec> {
    let val: toml::Value =
        toml::from_str(text).map_err(|e| Error::Config(format!("source-config: {e}")))?;
    let t = val
        .as_table()
        .ok_or_else(|| Error::Config("source-config: expected a TOML table".into()))?;
    let kind = t
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Config("source-config: `kind` must be \"sqlite\" or \"postgres\"".into()))?;
    let get = |k: &str| -> Result<String> {
        t.get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| Error::Config(format!("source-config: `{k}` (string) is required for kind=\"{kind}\"")))
    };
    match kind {
        "sqlite" => Ok(SourceSpec::Sqlite { path: get("path")? }),
        "postgres" | "postgresql" | "pg" => Ok(SourceSpec::Postgres { dsn: get("dsn")? }),
        other => Err(Error::Config(format!(
            "source-config: unknown kind `{other}` (want \"sqlite\" or \"postgres\")"
        ))),
    }
}

/// Write a source-config, born 0600 and never widened.
///
/// Creates with `O_EXCL` so this never truncates a file that already holds a
/// different mirror's credentials, and sets the mode at open time rather than
/// with a later `chmod` — the gap between create and chmod is a window where the
/// password sits on disk world-readable.
pub fn write_new(path: &Path, spec: &SourceSpec) -> Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .map_err(|e| Error::Config(format!("create source-config `{}`: {e}", path.display())))?;
    let body = match spec {
        SourceSpec::Sqlite { path } => format!("kind = \"sqlite\"\npath = {}\n", toml_str(path)),
        SourceSpec::Postgres { dsn } => format!("kind = \"postgres\"\ndsn = {}\n", toml_str(dsn)),
    };
    f.write_all(body.as_bytes())
        .map_err(|e| Error::Config(format!("write source-config: {e}")))?;
    Ok(())
}

fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_kinds() {
        let s = parse("kind = \"sqlite\"\npath = \"/tmp/a.db\"\n").unwrap();
        assert_eq!(
            s,
            SourceSpec::Sqlite {
                path: "/tmp/a.db".into()
            }
        );
        let s = parse("kind = \"postgres\"\ndsn = \"host=h user=u password=p\"\n").unwrap();
        assert_eq!(s.kind(), SourceKind::Postgres);
    }

    #[test]
    fn missing_field_is_an_error_not_a_panic() {
        assert!(parse("kind = \"postgres\"\n").is_err());
        assert!(parse("kind = \"sqlite\"\n").is_err());
        assert!(parse("kind = \"mysql\"\n").is_err());
        assert!(parse("").is_err());
        assert!(parse("not toml at all {{{").is_err());
        assert!(parse("kind = 7\n").is_err());
    }

    /// The whole point of the module: a password must not reach a log line.
    #[test]
    fn debug_and_redacted_never_leak_the_password() {
        for dsn in [
            "host=db.example.com port=5432 dbname=app user=app password=hunter2",
            "postgres://app:hunter2@db.example.com:5432/app",
            "postgresql://app:hunter2@db/app?sslmode=require",
        ] {
            let s = SourceSpec::Postgres { dsn: dsn.into() };
            let shown = format!("{s:?} {}", s.redacted());
            assert!(!shown.contains("hunter2"), "leaked password: {shown}");
        }
    }

    #[test]
    fn redaction_keeps_enough_to_diagnose() {
        let s = SourceSpec::Postgres {
            dsn: "host=db.example.com dbname=app user=app password=hunter2".into(),
        };
        let r = s.redacted();
        assert!(r.contains("host=db.example.com"), "{r}");
        assert!(r.contains("dbname=app"), "{r}");
        assert!(r.contains("password=<redacted>"), "{r}");
    }

    /// An unrecognised keyword must fail closed (redact), not fall through.
    #[test]
    fn unknown_dsn_keyword_is_redacted() {
        let s = SourceSpec::Postgres {
            dsn: "host=h sslkey=/secret/path.pem".into(),
        };
        assert!(s.redacted().contains("sslkey=<redacted>"));
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_group_or_world_readable_secret() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mpedb-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("src.toml");
        let _ = std::fs::remove_file(&p);
        write_new(
            &p,
            &SourceSpec::Postgres {
                dsn: "host=h password=p".into(),
            },
        )
        .unwrap();
        // born 0600 → accepted
        assert!(load(&p).is_ok());
        // widened → refused, and the message must say why
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = load(&p).unwrap_err().to_string();
        assert!(e.contains("0644"), "{e}");
        // and refusing must not itself leak the secret
        assert!(!e.contains("password=p"), "{e}");
        std::fs::remove_file(&p).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn write_new_refuses_to_clobber() {
        let dir = std::env::temp_dir().join(format!("mpedb-cfg2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("src.toml");
        let _ = std::fs::remove_file(&p);
        let spec = SourceSpec::Sqlite { path: "/a".into() };
        write_new(&p, &spec).unwrap();
        assert!(write_new(&p, &spec).is_err(), "must not overwrite");
        std::fs::remove_file(&p).unwrap();
    }

    /// Round-trip through the file, including a DSN with TOML metacharacters.
    #[test]
    fn write_then_load_round_trips_a_hostile_dsn() {
        let dir = std::env::temp_dir().join(format!("mpedb-cfg3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("src.toml");
        let _ = std::fs::remove_file(&p);
        let spec = SourceSpec::Postgres {
            dsn: "host=h password=a\"b\\c".into(),
        };
        write_new(&p, &spec).unwrap();
        assert_eq!(load(&p).unwrap(), spec);
        std::fs::remove_file(&p).unwrap();
    }
}
