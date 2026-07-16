//! #44: table aliases — self-joins, alias-qualified columns, and the PG rule
//! that an alias shadows the table name.
use mpedb::{params, Config, Database, ExecResult, Value};
use std::ops::Deref;

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

fn db(tag: &str) -> Tmp {
    let path = format!("/dev/shm/mpedb-alias-{tag}-{}.mpedb", std::process::id());
    let _ = std::fs::remove_file(&path);
    let db = Database::open_with_config(
        Config::from_toml_str(&format!(
            "[database]\npath = \"{path}\"\nsize_mb = 8\n\
             [[table]]\nname = \"emp\"\nprimary_key = [\"id\"]\n  \
             [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  \
             [[table.column]]\n  name = \"manager\"\n  type = \"int64\"\n  nullable = true\n  \
             [[table.column]]\n  name = \"name\"\n  type = \"text\"\n  nullable = false"
        ))
        .unwrap(),
    )
    .unwrap();
    Tmp { db, path }
}

fn seed(d: &Database) {
    let ins = d
        .prepare("INSERT INTO emp (id, manager, name) VALUES ($1, $2, $3)")
        .unwrap();
    // 1 = CEO (no manager), 2 and 3 report to 1
    for (id, mgr, name) in [(1i64, None, "Ada"), (2, Some(1i64), "Ben"), (3, Some(1), "Ced")] {
        let m = mgr.map(Value::Int).unwrap_or(Value::Null);
        d.execute(&ins, &params![id, m, name]).unwrap();
    }
}

/// The headline: a table joined to itself, which was refused before #44 for
/// lack of alias syntax. Each employee with a manager, paired to that manager's
/// name.
#[test]
fn self_join_pairs_employee_with_manager() {
    let d = db("selfjoin");
    seed(&d);
    let rows = match d
        .query(
            "SELECT e.name, m.name FROM emp e JOIN emp m ON e.manager = m.id",
            &[],
        )
        .unwrap()
    {
        ExecResult::Rows { rows, .. } => rows,
        o => panic!("{o:?}"),
    };
    // Ben->Ada, Ced->Ada (Ada has no manager, so she is not an outer row here)
    let mut pairs: Vec<(String, String)> = rows
        .iter()
        .map(|r| {
            (
                match &r[0] { Value::Text(s) => s.clone(), v => panic!("{v:?}") },
                match &r[1] { Value::Text(s) => s.clone(), v => panic!("{v:?}") },
            )
        })
        .collect();
    pairs.sort();
    assert_eq!(pairs, vec![("Ben".into(), "Ada".into()), ("Ced".into(), "Ada".into())]);
}

/// `FROM emp e` puts `e` in scope and NOT `emp` — PG's rule. `emp.id` must fail.
#[test]
fn alias_shadows_the_table_name() {
    let d = db("shadow");
    seed(&d);
    assert!(d.query("SELECT e.id FROM emp e", &[]).is_ok());
    let err = d.query("SELECT emp.id FROM emp e", &[]).unwrap_err();
    assert!(
        format!("{err}").contains("emp"),
        "using the shadowed table name should fail: {err}"
    );
}

/// A self-join with no alias is still refused — there is no way to tell the
/// sides apart.
#[test]
fn unaliased_self_join_is_refused() {
    let d = db("unaliased");
    seed(&d);
    let err = d
        .query("SELECT * FROM emp JOIN emp ON emp.manager = emp.id", &[])
        .unwrap_err();
    assert!(format!("{err}").contains("emp") || format!("{err}").to_lowercase().contains("alias"));
}

/// #56: SELECT-item aliases name the OUTPUT — `AS x`, and the bare form
/// `expr name` sqlite/PG also accept. The corpus' second-largest blocker.
#[test]
fn select_item_aliases_name_the_output() {
    let d = db("itemalias");
    seed(&d);
    match d
        .query("SELECT id AS x, id + 1 AS y, name z FROM emp ORDER BY x", &[])
        .unwrap()
    {
        ExecResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["x", "y", "z"]);
            assert_eq!(rows[0][..2], [Value::Int(1), Value::Int(2)]);
        }
        other => panic!("expected rows, got {other:?}"),
    }
    // An aliased aggregate names its output too.
    match d.query("SELECT count(*) AS n FROM emp", &[]).unwrap() {
        ExecResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["n"]);
            assert_eq!(rows, vec![vec![Value::Int(3)]]);
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// ORDER BY resolves an output ALIAS before an input column — the PG rule.
/// `SELECT manager AS id … ORDER BY id` sorts by the OUTPUT (manager), even
/// though the table has its own `id`; NULLS FIRST puts Ada on top.
#[test]
fn order_by_prefers_output_alias_over_input_column() {
    let d = db("aliasorder");
    seed(&d);
    match d
        .query("SELECT name, manager AS id FROM emp ORDER BY id, name", &[])
        .unwrap()
    {
        ExecResult::Rows { rows, .. } => {
            let names: Vec<&Value> = rows.iter().map(|r| &r[0]).collect();
            assert_eq!(
                names,
                [&Value::Text("Ada".into()), &Value::Text("Ben".into()), &Value::Text("Ced".into())],
                "NULL manager first proves the sort used the alias, not emp.id"
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
    // And under DISTINCT the alias counts as selected.
    assert!(d
        .query("SELECT DISTINCT manager AS m FROM emp ORDER BY m", &[])
        .is_ok());
}
