//! git-style short hash-prefix resolution for procedures
//! ([`ProcEngine::resolve`]). Full 32-byte hashes stay the internal anchor;
//! only the human-facing resolution accepts a unique hex prefix.

use mpedb::{Config, Database, Error};
use mpedb_proc::{Lang, ProcEngine};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn test_db(name: &str, size_mb: u64) -> (Database, FileGuard) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-proc-prefix-{name}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = {size_mb}
max_readers = 32

[[table]]
name = "t"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"
"#,
        path.display()
    );
    (
        Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(),
        FileGuard(path),
    )
}

struct FileGuard(PathBuf);
impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A unique hex prefix (and a full hash) resolves; a name still wins over a
/// prefix-looking string; a zero-match hex prefix is "unknown".
#[test]
fn unique_prefix_full_hash_name_precedence_and_zero_match() {
    let (db, _g) = test_db("unique", 16);
    let engine = ProcEngine::new(&db);

    let hash = engine
        .define("def solo():\n    return 111\n", Lang::Python)
        .unwrap();
    let hex = hash.to_string();

    // Full 64-hex hash still works.
    assert_eq!(engine.info(&hex).unwrap().name, "solo");
    // Unique short prefixes of increasing length all resolve to `solo`.
    for len in [4usize, 8, 16, 40] {
        let info = engine.info(&hex[..len]).unwrap();
        assert_eq!(info.name, "solo", "prefix len {len}");
        assert_eq!(info.hash, hash);
    }
    // Upper-case prefix resolves too (hashes render lowercase; we case-fold).
    assert_eq!(engine.info(&hex[..8].to_uppercase()).unwrap().name, "solo");

    // A name that *looks* like a hash prefix takes precedence over prefix
    // resolution: define a proc literally named `abcd` (4 hex chars).
    engine
        .define("def abcd():\n    return 222\n", Lang::Python)
        .unwrap();
    let info = engine.info("abcd").unwrap();
    assert_eq!(info.name, "abcd");
    // ...and calling it runs the named proc, not a hash-prefix match.
    assert_eq!(
        engine.call("abcd", &[]).unwrap().to_string(),
        mpedb::Value::Int(222).to_string()
    );

    // A hex prefix that matches no stored hash (and is not a name) is unknown.
    // Use a prefix guaranteed absent by flipping the first nibble of `solo`.
    let first = u8::from_str_radix(&hex[..1], 16).unwrap();
    let miss = format!("{:x}{}", (first + 1) % 16, &hex[1..4]);
    match engine.info(&miss) {
        Err(Error::Unsupported(m)) => {
            assert!(m.contains("unknown procedure"), "{m}");
        }
        other => panic!("expected unknown-procedure error, got {other:?}"),
    }

    // Too-short (< 4) hex is not treated as a prefix at all — it is just an
    // unknown name.
    assert!(matches!(engine.info(&hex[..2]), Err(Error::Unsupported(_))));
}

/// Two procedures whose content hashes share a 4-hex-char prefix make that
/// prefix ambiguous; the error names the colliding candidates. blake3 is
/// deterministic, so the collision is found at a fixed (non-flaky) point.
#[test]
fn ambiguous_prefix_errors_clearly() {
    let (db, _g) = test_db("ambiguous", 64);
    let engine = ProcEngine::new(&db);

    // Define distinct pure procs (non-hex names so they can never be confused
    // with a hex prefix) until two hashes collide on their first 4 hex chars.
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut ambiguous_prefix: Option<(String, [String; 2])> = None;
    for i in 0..5000u32 {
        let name = format!("gg{i}");
        let src = format!("def {name}():\n    return {i}\n");
        let hash = engine.define(&src, Lang::Python).unwrap().to_string();
        let p4 = hash[..4].to_owned();
        if let Some(prev_hash) = seen.get(&p4) {
            ambiguous_prefix = Some((p4, [prev_hash.clone(), hash]));
            break;
        }
        seen.insert(p4, hash);
    }
    let (prefix, [h1, h2]) =
        ambiguous_prefix.expect("no 4-hex-char collision within 5000 procs (astronomically unlikely)");

    // Resolving the shared prefix is an ambiguity error listing both hashes.
    match engine.info(&prefix) {
        Err(Error::Unsupported(m)) => {
            assert!(m.contains("ambiguous"), "message: {m}");
            assert!(m.contains(&prefix), "message must echo the prefix: {m}");
            // both candidates' short (12-hex) forms are listed
            assert!(m.contains(&h1[..12]), "missing candidate {}: {m}", &h1[..12]);
            assert!(m.contains(&h2[..12]), "missing candidate {}: {m}", &h2[..12]);
        }
        other => panic!("expected ambiguous-prefix error, got {other:?}"),
    }

    // A longer prefix that distinguishes the two (their first differing char)
    // resolves uniquely again.
    let diff = h1
        .char_indices()
        .zip(h2.chars())
        .find(|((_, a), b)| a != b)
        .map(|((idx, _), _)| idx)
        .expect("distinct hashes differ somewhere");
    if diff + 1 >= 4 {
        let uniq1 = &h1[..diff + 1];
        assert_eq!(engine.info(uniq1).unwrap().hash.to_string(), h1);
    }
}
