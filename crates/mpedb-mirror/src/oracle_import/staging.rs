//! Own a private staging directory. Publishing is an atomic no-replace hard
//! link on the destination filesystem; failures remove only this owned stage.
use mpedb_types::{Error, Result};
use std::path::{Path, PathBuf};
pub(super) struct Staging {
    dir: PathBuf,
    dest: PathBuf,
    parent: PathBuf,
}
impl Staging {
    pub fn new(dest: &Path, nonce: [u8; 16]) -> Result<Self> {
        let parent = dest
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        let hex = nonce.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let dir = parent.join(format!(".mpedb-oracle-import-{hex}"));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&dir)?;
        Ok(Self {
            dir,
            dest: dest.into(),
            parent: parent.into(),
        })
    }
    pub fn path(&self) -> PathBuf {
        self.dir.join("data.mpedb")
    }
    pub fn publish(&self) -> Result<()> {
        match std::fs::hard_link(self.path(), &self.dest) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(Error::Config(
                "Oracle import destination appeared before publication; existing file preserved"
                    .into(),
            )),
            Err(e) => return Err(e.into()),
        }
        #[cfg(unix)]
        std::fs::File::open(&self.parent)?.sync_all()?;
        Ok(())
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publishing_never_overwrites_a_late_destination_and_cleans_its_stage() {
        let dir =
            mpedb_testkit::scratch_base().join(format!("oracle-stage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("final.mpedb");
        let _ = std::fs::remove_file(&dest);
        let stage = Staging::new(&dest, [9; 16]).unwrap();
        let owned = stage.dir.clone();
        std::fs::write(stage.path(), b"new").unwrap();
        std::fs::write(&dest, b"existing").unwrap();
        assert!(stage.publish().is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"existing");
        drop(stage);
        assert!(!owned.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
