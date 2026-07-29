//! mpedbfs — a read-only FUSE view of what an .mpedb file already holds.
//!
//! The database can already hand you these bytes: `mpedb rretl get`,
//! `rretl pack-out`. What it cannot do is hand them to a program that only
//! speaks paths — an image viewer, `grep -r`, a build system, an editor.
//! Mounting is the adapter, and the only thing it adds.
//!
//! ```text
//! /obj/<name>/latest        the newest version's bytes
//! /obj/<name>/v<N>          that version, exactly
//! /archive/<id>-<name>/…    a spliced zip's members, as a real tree
//! ```
//!
//! **Read-only, and that is a design decision, not a stage.** A writable
//! mount means a partial write has to become something: either a blob
//! version the user never asked for, or state held outside the database
//! until close. It also means holding mpedb's single writer lock across a
//! user's `cp`, which turns a slow copy into an outage for every other
//! writer. Every mutating operation returns `EROFS`.
//!
//! **One snapshot per open file.** `open` reads the bytes and the handle
//! keeps them until `release`, so a `cat` sees one consistent version even
//! if a writer commits a new one mid-read. That is what MVCC is for. The
//! cost is honest and stated: the whole object sits in memory for the life
//! of the handle (#43's incremental blob API is the way out, when a blob
//! that does not fit becomes a real complaint).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};

use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, Generation, INodeNo, MountOption,
    OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, Request,
};
use mpedb::{Config, Database, ExecResult, Value};

const TTL: Duration = Duration::from_secs(1);
const ROOT: u64 = 1;
const DIR_OBJ: u64 = 2;
const DIR_ARCHIVE: u64 = 3;
const FIRST_DYNAMIC: u64 = 16;

/// What an inode names. The namespace is rebuilt from the database on
/// demand; inodes are handed out once per path and never reused, so a file
/// that is open cannot have its identity pulled out from under it.
#[derive(Debug, Clone)]
enum Node {
    /// `/obj/<name>` — the versions of one object.
    ObjDir(String),
    /// `/obj/<name>/latest` or `/obj/<name>/v<N>`.
    ObjFile { obj: String, ver: Option<i64> },
    /// `/archive/<id>-<name>`, or a directory inside it.
    ArchiveDir { id: i64, prefix: String },
    /// A member of a spliced archive, named by its full path inside it.
    ArchiveFile { id: i64, member: String },
}

/// fuser 0.18 calls every operation on `&self`, so the namespace bookkeeping
/// lives behind a mutex. Contention is not a concern: the work under the
/// lock is map lookups and, at most, one blob decode.
struct Fs {
    db: Database,
    st: Mutex<State>,
    uid: u32,
    gid: u32,
}

struct State {
    nodes: HashMap<u64, Node>,
    ino_of: HashMap<String, u64>,
    path_of: HashMap<u64, String>,
    next_ino: u64,
    /// path → content length. Sound because a version's CONTENT never
    /// changes once written (its STORAGE does — a newer version rewrites
    /// the previous one as a reverse delta — but the bytes it decodes to
    /// are the same), and archive members are immutable rows.
    len_of: HashMap<String, u64>,
    /// Open handles: the bytes, read once at `open`.
    open: HashMap<u64, Vec<u8>>,
    next_fh: u64,
}

fn rows(r: ExecResult) -> Vec<Vec<Value>> {
    match r {
        ExecResult::Rows { rows, .. } => rows,
        _ => Vec::new(),
    }
}

fn as_int(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        _ => -1,
    }
}

/// A member is stored exactly as the zip had it, so the presented bytes are
/// the INFLATED ones. Method 0 is stored-as-is; 8 is raw deflate (no zlib
/// header). Anything else is refused rather than served: a member this view
/// cannot decode must not look like one it can.
fn inflate(method: i64, raw: &[u8]) -> Option<Vec<u8>> {
    match method {
        0 => Some(raw.to_vec()),
        8 => {
            use std::io::Read;
            let mut out = Vec::new();
            flate2::read::DeflateDecoder::new(raw).read_to_end(&mut out).ok()?;
            Some(out)
        }
        _ => None,
    }
}

/// A name out of the database. A zip member's name is stored as BYTES —
/// the format allows any encoding — so a Blob is decoded lossily rather
/// than debug-printed, which is what turned member names into
/// `Blob([114, 101, …])` the first time this was mounted.
fn text(v: &Value) -> String {
    match v {
        Value::Text(t) => t.clone(),
        Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
        other => format!("{other:?}"),
    }
}

impl Fs {
    fn new(db: Database) -> Fs {
        Fs {
            db,
            st: Mutex::new(State {
                nodes: HashMap::new(),
                ino_of: HashMap::new(),
                path_of: HashMap::new(),
                next_ino: FIRST_DYNAMIC,
                len_of: HashMap::new(),
                open: HashMap::new(),
                next_fh: 1,
            }),
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }

    fn attr(&self, ino: u64, kind: FileType, size: u64) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind,
            // Read-only, and the permission bits say so as well as the mount
            // option: a tool that checks before writing gets the same answer
            // as one that tries.
            perm: if kind == FileType::Directory { 0o555 } else { 0o444 },
            nlink: if kind == FileType::Directory { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    // ------------------------------------------------------------- reading

    fn objects(&self) -> Vec<String> {
        match self.db.query("SELECT DISTINCT obj FROM rretl_versions ORDER BY obj", &[]) {
            Ok(r) => rows(r).iter().map(|r| text(&r[0])).collect(),
            Err(_) => Vec::new(),
        }
    }

    fn archives(&self) -> Vec<(i64, String)> {
        self.db
            .rretl_archives()
            .unwrap_or_default()
            .into_iter()
            .map(|a| (a.archive_id, a.name))
            .collect()
    }

    fn members(&self, id: i64) -> Vec<String> {
        match self.db.query(
            "SELECT name FROM rretl_archive_members WHERE archive_id = $1 ORDER BY member_no",
            &[Value::Int(id)],
        ) {
            Ok(r) => rows(r).iter().map(|r| text(&r[0])).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// The bytes behind one node. The ONLY place that materializes.
    fn bytes(&self, node: &Node) -> Option<Vec<u8>> {
        match node {
            Node::ObjFile { obj, ver } => {
                let v = match ver {
                    Some(v) => *v,
                    None => self.db.rretl_versions(obj).ok()?.last()?.ver,
                };
                self.db.rretl_get_version(obj, v).ok()
            }
            Node::ArchiveFile { id, member } => {
                let r = self
                    .db
                    .query(
                        "SELECT name, method, data FROM rretl_archive_members \
                         WHERE archive_id = $1",
                        &[Value::Int(*id)],
                    )
                    .ok()?;
                // Matched on the DECODED name, because that is the name the
                // directory listing handed out: the column may hold either
                // text or bytes, and one row must not be findable under one
                // spelling and listed under another.
                let row = rows(r).into_iter().find(|row| text(&row[0]) == *member)?;
                let raw = match &row[2] {
                    Value::Blob(b) => b.clone(),
                    Value::Text(t) => t.as_bytes().to_vec(),
                    _ => return None,
                };
                inflate(as_int(&row[1]), &raw)
            }
            _ => None,
        }
    }

    /// A file's size, without opening it. Cached, because `ls -l` on an
    /// object directory would otherwise decode every delta chain in it.
    fn size(&self, st: &mut State, ino: u64) -> u64 {
        let Some(path) = st.path_of.get(&ino).cloned() else { return 0 };
        if let Some(n) = st.len_of.get(&path) {
            return *n;
        }
        let n = st
            .nodes
            .get(&ino)
            .cloned()
            .and_then(|node| self.bytes(&node))
            .map(|b| b.len() as u64)
            .unwrap_or(0);
        st.len_of.insert(path, n);
        n
    }

    /// The entries of one directory, as (name, inode, kind).
    fn list(&self, st: &mut State, ino: u64) -> Vec<(String, u64, FileType)> {
        let mut out = Vec::new();
        match ino {
            ROOT => {
                out.push(("obj".into(), DIR_OBJ, FileType::Directory));
                out.push(("archive".into(), DIR_ARCHIVE, FileType::Directory));
            }
            DIR_OBJ => {
                for name in self.objects() {
                    let p = format!("/obj/{name}");
                    let i = st.ino(&p, Node::ObjDir(name.clone()));
                    out.push((name, i, FileType::Directory));
                }
            }
            DIR_ARCHIVE => {
                for (id, name) in self.archives() {
                    let dir = format!("{id}-{name}");
                    let p = format!("/archive/{dir}");
                    let i = st.ino(&p, Node::ArchiveDir { id, prefix: String::new() });
                    out.push((dir, i, FileType::Directory));
                }
            }
            _ => match st.nodes.get(&ino).cloned() {
                Some(Node::ObjDir(obj)) => {
                    let vers = self.db.rretl_versions(&obj).unwrap_or_default();
                    if !vers.is_empty() {
                        let p = format!("/obj/{obj}/latest");
                        let i = st.ino(&p, Node::ObjFile { obj: obj.clone(), ver: None });
                        out.push(("latest".into(), i, FileType::RegularFile));
                    }
                    for v in vers {
                        let name = format!("v{}", v.ver);
                        let p = format!("/obj/{obj}/{name}");
                        let i = st.ino(&p, Node::ObjFile { obj: obj.clone(), ver: Some(v.ver) });
                        out.push((name, i, FileType::RegularFile));
                    }
                }
                Some(Node::ArchiveDir { id, prefix }) => {
                    let base = st.path_of.get(&ino).cloned().unwrap_or_default();
                    // A member name is a PATH inside the zip, so the tree is
                    // real directories: only the segment at this depth is
                    // listed here, and a name seen twice is one entry.
                    let mut seen: Vec<String> = Vec::new();
                    for m in self.members(id) {
                        let Some(rest) = m.strip_prefix(&prefix) else { continue };
                        let rest = rest.trim_start_matches('/');
                        if rest.is_empty() {
                            continue;
                        }
                        match rest.split_once('/') {
                            Some((seg, _)) => {
                                if seen.iter().any(|s| s == seg) {
                                    continue;
                                }
                                seen.push(seg.to_string());
                                let sub = if prefix.is_empty() {
                                    seg.to_string()
                                } else {
                                    format!("{prefix}/{seg}")
                                };
                                let p = format!("{base}/{seg}");
                                let i = st.ino(&p, Node::ArchiveDir { id, prefix: sub });
                                out.push((seg.to_string(), i, FileType::Directory));
                            }
                            None => {
                                let p = format!("{base}/{rest}");
                                let i = st.ino(&p, Node::ArchiveFile { id, member: m.clone() });
                                out.push((rest.to_string(), i, FileType::RegularFile));
                            }
                        }
                    }
                }
                _ => {}
            },
        }
        out
    }
}

impl State {
    /// One inode per path, for the life of the mount. Never reused, so an
    /// open file cannot have its identity pulled out from under it.
    fn ino(&mut self, path: &str, node: Node) -> u64 {
        if let Some(i) = self.ino_of.get(path) {
            return *i;
        }
        let i = self.next_ino;
        self.next_ino += 1;
        self.ino_of.insert(path.to_string(), i);
        self.path_of.insert(i, path.to_string());
        self.nodes.insert(i, node);
        i
    }

    fn is_file(&self, ino: u64) -> bool {
        matches!(self.nodes.get(&ino), Some(Node::ObjFile { .. } | Node::ArchiveFile { .. }))
    }
}

impl Filesystem for Fs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let want = name.to_string_lossy().to_string();
        let mut st = self.st.lock().expect("mpedbfs state");
        let Some((_, ino, kind)) =
            self.list(&mut st, parent.0).into_iter().find(|(n, _, _)| *n == want)
        else {
            reply.error(Errno::ENOENT);
            return;
        };
        let size = if st.is_file(ino) { self.size(&mut st, ino) } else { 0 };
        reply.entry(&TTL, &self.attr(ino, kind, size), Generation(0));
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let ino = ino.0;
        if ino == ROOT || ino == DIR_OBJ || ino == DIR_ARCHIVE {
            reply.attr(&TTL, &self.attr(ino, FileType::Directory, 0));
            return;
        }
        let mut st = self.st.lock().expect("mpedbfs state");
        if !st.nodes.contains_key(&ino) {
            reply.error(Errno::ENOENT);
            return;
        }
        if st.is_file(ino) {
            let size = self.size(&mut st, ino);
            reply.attr(&TTL, &self.attr(ino, FileType::RegularFile, size));
        } else {
            reply.attr(&TTL, &self.attr(ino, FileType::Directory, 0));
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let mut st = self.st.lock().expect("mpedbfs state");
        let mut entries: Vec<(String, u64, FileType)> = vec![
            (".".into(), ino.0, FileType::Directory),
            ("..".into(), ROOT, FileType::Directory),
        ];
        entries.extend(self.list(&mut st, ino.0));
        for (i, (name, e_ino, kind)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(e_ino), (i + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        // Read-only: refuse the INTENT, not just the write.
        if flags.0 & (libc::O_WRONLY | libc::O_RDWR | libc::O_TRUNC) != 0 {
            reply.error(Errno::EROFS);
            return;
        }
        let mut st = self.st.lock().expect("mpedbfs state");
        let Some(node) = st.nodes.get(&ino.0).cloned() else {
            reply.error(Errno::ENOENT);
            return;
        };
        // ONE read, held for the handle's life: the snapshot a `cat` sees
        // cannot change under it, and the size it was told stays true.
        let Some(bytes) = self.bytes(&node) else {
            reply.error(Errno::EIO);
            return;
        };
        let fh = st.next_fh;
        st.next_fh += 1;
        st.open.insert(fh, bytes);
        reply.opened(FileHandle(fh), fuser::FopenFlags::empty());
    }

    #[allow(clippy::too_many_arguments)]
    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let st = self.st.lock().expect("mpedbfs state");
        let Some(bytes) = st.open.get(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        let from = (offset as usize).min(bytes.len());
        let to = from.saturating_add(size as usize).min(bytes.len());
        reply.data(&bytes[from..to]);
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock: Option<fuser::LockOwner>,
        _flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        self.st.lock().expect("mpedbfs state").open.remove(&fh.0);
        reply.ok();
    }
}

fn usage() -> ! {
    eprintln!(
        "mpedbfs — read-only FUSE view of an .mpedb file\n\n\
         usage: mpedbfs <config.toml> <mountpoint>\n\n\
         presents:\n  \
           /obj/<name>/latest, /obj/<name>/v<N>   versioned blobs (rretl put)\n  \
           /archive/<id>-<name>/...               spliced zip members, as a tree\n\n\
         unmount with:  fusermount3 -u <mountpoint>"
    );
    std::process::exit(2)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 || args.iter().any(|a| a == "-h" || a == "--help") {
        usage();
    }
    let (config, mount) = (&args[0], &args[1]);
    let toml = match std::fs::read_to_string(config) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("mpedbfs: cannot read `{config}`: {e}");
            std::process::exit(1);
        }
    };
    let db = match Config::from_toml_str(&toml).and_then(Database::open_with_config) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("mpedbfs: cannot open the database: {e}");
            std::process::exit(1);
        }
    };
    // `Config` is #[non_exhaustive]: build the default and set what we need.
    let mut cfg = fuser::Config::default();
    cfg.mount_options = vec![
        MountOption::RO,
        MountOption::FSName("mpedbfs".into()),
        MountOption::NoAtime,
    ];
    if let Err(e) = fuser::mount(Fs::new(db), mount, &cfg) {
        eprintln!("mpedbfs: mount failed: {e}");
        std::process::exit(1);
    }
}
