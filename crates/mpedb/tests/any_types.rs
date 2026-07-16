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
    let path = format!("/dev/shm/mpedb-any-{tag}-{}.mpedb", std::process::id());
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

#[test]
fn any_is_refused_in_a_key_and_in_a_unique_index() {
    // A key is memcmp-ordered; `any` has no order across types. Refusing beats
    // inventing whether 5 sorts before "a".
    let pk = r#"[[table]]
name = "t"
primary_key = ["k"]
  [[table.column]]
  name = "k"
  type = "any"
"#;
    let e = match db("pk", pk) {
        Err(e) => e,
        Ok(_) => panic!("an `any` primary key must be refused"),
    };
    assert!(format!("{e}").contains("cannot be `any`"), "got: {e}");

    let uq = r#"[[table]]
name = "t"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "u"
  type = "any"
  unique = true
"#;
    let e = match db("uq", uq) {
        Err(e) => e,
        Ok(_) => panic!("an `any` UNIQUE column must be refused"),
    };
    assert!(
        format!("{e}").contains("`any` and carry an index (UNIQUE)"),
        "got: {e}"
    );
}

/// A NON-unique index over `any` is refused for the same reason — the index
/// is memcmp-ordered and `any` has no order across types. This slipped
/// through once, and the adversarial review showed the consequence: an
/// IndexRange over mixed runtime types returned WRONG rows, and DELETE
/// through it deleted them.
#[test]
fn any_indexed_is_refused() {
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
  indexed = true
"#;
    let e = match db("ix", ix) {
        Err(e) => e,
        Ok(_) => panic!("an `any` indexed column must be refused"),
    };
    assert!(
        format!("{e}").contains("`any` and carry an index (indexed)"),
        "got: {e}"
    );
}
