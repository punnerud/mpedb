//! #23.1: `type = "any"` — loose per column, rigid everywhere else.
use mpedb::{params, Config, Database, ExecResult, Value};
use std::ops::Deref;

/// Takes its file with it when it dies. Building a `/dev/shm` path by hand and
/// forgetting this is how 116 leaked test databases (1.9 GB) accumulated there
/// and eventually failed a benchmark run with ENOSPC.
struct Tmp {
    db: Database,
    path: String,
}
impl Deref for Tmp {
    type Target = Database;
    fn deref(&self) -> &Database {
        &self.db
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(format!("{}-wal", self.path));
    }
}

fn db(tag: &str, extra: &str) -> mpedb::Result<Tmp> {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        std::path::PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir
        .join(format!("mpedb-any-{tag}-{}.mpedb", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&path);
    let db = Database::open_with_config(Config::from_toml_str(&format!(
        "[database]\npath = \"{path}\"\nsize_mb = 8\n{extra}"
    ))?)?;
    Ok(Tmp { db, path })
}

const T: &str = r#"[[table]]
name = "t"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "loose"
  type = "any"
  nullable = true
  [[table.column]]
  name = "strict"
  type = "int64"
  nullable = true
"#;

#[test]
fn any_accepts_every_type_while_its_neighbour_stays_rigid() {
    let d = db("mix", T).unwrap();
    let ins = d.prepare("INSERT INTO t (id, loose, strict) VALUES ($1, $2, $3)").unwrap();
    // the loose column takes anything...
    for (i, v) in [
        Value::Int(1),
        Value::Text("two".into()),
        Value::Float(3.5),
        Value::Bool(true),
        Value::Blob(vec![4, 5]),
        Value::Null,
    ]
    .into_iter()
    .enumerate()
    {
        d.execute(&ins, &params![i as i64, v.clone(), Value::Int(0)])
            .unwrap_or_else(|e| panic!("any column rejected {v:?}: {e}"));
    }
    // ...and the rigid one next to it still does not. This is the whole point:
    // `any` must be a per-column opt-out, not a hole in the schema.
    let e = d
        .execute(&ins, &params![99i64, Value::Int(0), Value::Text("nope".into())])
        .unwrap_err();
    assert!(
        matches!(e, mpedb::Error::TypeMismatch(_)),
        "a rigid column next to an `any` one must still reject: got {e:?}"
    );

    // read every value back as what it was put in as
    match d.query("SELECT id, loose FROM t ORDER BY id", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 6);
            assert_eq!(rows[0][1], Value::Int(1));
            assert_eq!(rows[1][1], Value::Text("two".into()));
            assert_eq!(rows[2][1], Value::Float(3.5));
            assert_eq!(rows[3][1], Value::Bool(true));
            assert_eq!(rows[4][1], Value::Blob(vec![4, 5]));
            assert_eq!(rows[5][1], Value::Null);
        }
        o => panic!("{o:?}"),
    }
}

/// An `any` column IS allowed in a key now — see `Schema::ANY_KEY_COLUMNS` and
/// `tests/any_key.rs`, which verifies the semantics against sqlite 3.45.1. What
/// this test holds is the part the schema is responsible for: such a table
/// OPENS, and its key really is keyed by STORAGE CLASS rather than by mpedb
/// type, which is what makes `1`/`1.0` one key and `'1'`/`x'31'` two.
#[test]
fn any_is_allowed_in_a_key_and_keys_by_storage_class() {
    let pk = r#"[[table]]
name = "t"
primary_key = ["k"]
  [[table.column]]
  name = "k"
  type = "any"
  [[table.column]]
  name = "tag"
  type = "text"
"#;
    let d = db("pk", pk).expect("an `any` primary key is allowed");
    let ins = d.prepare("INSERT INTO t (k, tag) VALUES ($1, $2)").unwrap();
    let put = |v: Value, tag: &str| d.execute(&ins, &params![v, Value::Text(tag.into())]);

    put(Value::Int(1), "int1").unwrap();
    // The type-keyed encoder ALIASED these two (identical payload bytes, and the
    // type is not in the encoding); the class-keyed one keeps them apart, which
    // is the difference between two rows and one silently overwritten row.
    put(Value::Text("1".into()), "text1").unwrap();
    put(Value::Blob(b"1".to_vec()), "blob1").unwrap();
    // ...and it SPLIT these, where sqlite has exactly one key.
    assert!(put(Value::Float(1.0), "dup").is_err(), "1.0 must collide with 1");
    put(Value::Float(-0.0), "negzero").unwrap();
    assert!(put(Value::Int(0), "dup0").is_err(), "0 must collide with -0.0");

    // Tree order is sqlite's: NULL < numbers < text < blob.
    match d.query("SELECT tag FROM t ORDER BY k", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => {
            let tags: Vec<String> = rows
                .iter()
                .map(|r| match &r[0] {
                    Value::Text(s) => s.clone(),
                    o => panic!("{o:?}"),
                })
                .collect();
            assert_eq!(tags, ["negzero", "int1", "text1", "blob1"]);
        }
        o => panic!("{o:?}"),
    }
}

/// A UNIQUE index over `any` is allowed too, and collides exactly where
/// sqlite's `=` does. (The full cross-engine battery lives in
/// `tests/any_key.rs`; this pins the config/TOML declaration path.)
#[test]
fn any_unique_index_is_allowed_and_collides_by_class() {
    let uq = r#"[[table]]
name = "t"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "u"
  type = "any"
  nullable = true
  unique = true
"#;
    let d = db("uq", uq).expect("an `any` UNIQUE column is allowed");
    let ins = d.prepare("INSERT INTO t (id, u) VALUES ($1, $2)").unwrap();
    let put = |id: i64, v: Value| d.execute(&ins, &params![id, v]);
    put(1, Value::Int(1)).unwrap();
    put(2, Value::Text("1".into())).unwrap();
    put(3, Value::Blob(b"1".to_vec())).unwrap();
    assert!(put(4, Value::Float(1.0)).is_err(), "1.0 must collide with 1");
    // An any-NULL row has no index entry, so NULLs never collide.
    put(5, Value::Null).unwrap();
    put(6, Value::Null).unwrap();
}

/// A NON-unique index over `any` is allowed as well. It is MAINTAINED and never
/// PROBED (`planner::access` refuses a typeless access path), so its only
/// observable effect must be none at all — pinned differentially in
/// `tests/any_key.rs::a_typeless_index_never_changes_an_answer`.
#[test]
fn any_indexed_is_allowed() {
    let ix = r#"
[[table]]
name = "t"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "u"
  type = "any"
  nullable = true
  indexed = true
"#;
    let d = db("ix", ix).expect("an `any` indexed column is allowed");
    let ins = d.prepare("INSERT INTO t (id, u) VALUES ($1, $2)").unwrap();
    for (i, v) in [
        Value::Int(1),
        Value::Float(1.0),
        Value::Text("1".into()),
        Value::Blob(b"1".to_vec()),
        Value::Null,
    ]
    .into_iter()
    .enumerate()
    {
        d.execute(&ins, &params![i as i64, v]).unwrap();
    }
    match d.query("SELECT count(*) FROM t WHERE u = 1", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Int(2)),
        o => panic!("{o:?}"),
    }
}
