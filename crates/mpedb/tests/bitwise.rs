//! Task #74 item 2 — the bitwise operators `& | << >> ~`.
//!
//! sqlite defines all five in terms of ONE coercion (`sqlite3VdbeIntValue`) and
//! the result is always an integer. Every case below was read off the real
//! `sqlite3` 3.45.1 binary before it was implemented, because each of these is
//! a silent wrong answer if guessed:
//!
//! * precedence — one tier, left-associative, between comparison and `+`/`-`;
//! * a NEGATIVE shift count shifts the other way, and `-64` clamps;
//! * a count ≥ 64 clears the value, EXCEPT `>>` of a negative (arithmetic → -1);
//! * `<<` WRAPS rather than raising, unlike every other integer op in mpedb;
//! * reals TRUNCATE toward zero and CLAMP; text takes an integer-PREFIX parse
//!   that stops at `e`, unlike `CAST(… AS INTEGER)`… except it does not, and
//!   that agreement is asserted here rather than assumed;
//! * NULL propagates through all five.
//!
//! The non-integer coercions are reachable only through an `any` (typeless)
//! column — a statically-typed real/text/blob operand is a bind error naming
//! `CAST`, which is the same discipline item 1 applies to parameters. mpedb's
//! `any` is compared against sqlite **STRICT**'s `ANY`, which is the column
//! that also keeps a value's class instead of applying NUMERIC affinity.

use mpedb::{Config, Database, ExecResult, Value};
use std::io::Write;
use std::ops::Deref;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

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
    }
}

/// `a` is mpedb's typeless column; in sqlite it must be a STRICT `ANY`, which
/// is the only sqlite column that likewise stores a value in its own class.
const DDL: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, i INTEGER, r REAL, s TEXT, a ANY)";
const SQLITE_DDL: &str =
    "CREATE TABLE t (id INTEGER PRIMARY KEY, i INT, r REAL, s TEXT, a ANY) STRICT";
const ROWS: &[&str] = &[
    "INSERT INTO t (id, i, r, s) VALUES (1, 5, 2.5, 'abc')",
    "INSERT INTO t (id, i, r, s) VALUES (2, -8, -3.5, '3')",
    "INSERT INTO t (id, i, r, s) VALUES (3, NULL, NULL, NULL)",
];

fn open() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" };
    let path = format!(
        "{dir}/mpedb-bitwise-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    let t = Tmp { db, path };
    t.db.query(DDL, &[]).unwrap();
    for r in ROWS {
        t.db.query(r, &[]).unwrap();
    }
    t
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => {
            if f.fract() == 0.0 && f.is_finite() {
                format!("{f:.1}")
            } else {
                f.to_string()
            }
        }
        Value::Text(s) => s.clone(),
        Value::Bool(b) => (*b as i32).to_string(),
        other => panic!("unexpected value: {other:?}"),
    }
}

fn mpedb_rows(db: &Database, sql: &str, params: &[Value]) -> Vec<Vec<String>> {
    match db.query(sql, params) {
        Ok(ExecResult::Rows { rows, .. }) => {
            rows.iter().map(|r| r.iter().map(render).collect()).collect()
        }
        Ok(other) => panic!("expected rows from `{sql}`, got {other:?}"),
        Err(e) => panic!("mpedb `{sql}` failed: {e}"),
    }
}

fn sqlite_rows(extra: &[String], query: &str) -> Vec<Vec<String>> {
    let mut script = format!(".nullvalue NULL\n{SQLITE_DDL};\n");
    for r in ROWS.iter().map(|s| s.to_string()).chain(extra.iter().cloned()) {
        script.push_str(&r);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");
    let mut child = Command::new("sqlite3")
        .arg(":memory:")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the sqlite3 CLI must be on PATH for this cross-check");
    child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(out.status.success() && stderr.is_empty(), "sqlite3 failed: {stderr}\n{script}");
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// The same query, over the same rows, must give the same answer in both.
fn same(query: &str) {
    let t = open();
    assert_eq!(
        mpedb_rows(&t.db, query, &[]),
        sqlite_rows(&[], query),
        "mpedb vs sqlite3 diverged for:\n  {query}"
    );
}

/// With one extra row whose typeless column holds `lit`.
fn same_with_any(lit: &str, query: &str) {
    let t = open();
    let ins = format!("INSERT INTO t (id, a) VALUES (9, {lit})");
    t.db.query(&ins, &[]).unwrap();
    assert_eq!(
        mpedb_rows(&t.db, query, &[]),
        sqlite_rows(std::slice::from_ref(&ins), query),
        "mpedb vs sqlite3 diverged for a = {lit}:\n  {query}"
    );
}

// ---- the operators themselves --------------------------------------------

#[test]
fn the_five_operators_agree_with_sqlite() {
    same("SELECT 3 | 1, 3 & 1, 1 << 2, 8 >> 2, ~5");
    same("SELECT 0 | 0, 0 & 0, ~0, ~-1, ~1");
    same("SELECT 9223372036854775807 | 1, -1 | 0, -1 & 5, -1 & -1");
    // Over columns, including the all-NULL row.
    same("SELECT id, i | 1, i & 3, i << 1, i >> 1, ~i FROM t ORDER BY id");
    same("SELECT id FROM t WHERE i & 1 = 1 ORDER BY id");
    // Django's shape: a bit test in a WHERE, and the XOR emulation.
    same("SELECT id FROM t WHERE (i | 2) & 2 = 2 ORDER BY id");
    same("SELECT id, (i & 1) | (i >> 1 & 1) FROM t ORDER BY id");
}

#[test]
fn precedence_is_sqlites() {
    // One tier, left-associative: `1 | 2 & 3` is `(1|2) & 3`, NOT C's binding.
    same("SELECT 1 | 2 & 3, (1 | 2) & 3, 1 & 2 | 3");
    same("SELECT 2 | 1 << 2, 1 << 2 | 2, 4 >> 1 << 1");
    // Looser than `+ - * / %`.
    same("SELECT 1 + 2 | 4, 1 | 2 + 4, 3 * 2 | 1, 8 / 2 | 1, 7 % 4 | 8");
    // Tighter than a comparison, so `x = a | b` is `x = (a|b)`.
    same("SELECT 3 = 1 | 2, 1 | 2 = 3, 4 > 1 | 2");
    // …and than BETWEEN / IN / LIKE, whose operands parse at the bitwise tier.
    same("SELECT 3 BETWEEN 1 | 1 AND 2 | 2, 3 IN (1 | 2, 9)");
    // `~` binds tighter than every infix operator, and nests with unary minus.
    same("SELECT ~-5, -~5, ~5 + 1, ~(5 + 1), ~~5, - ~ - 5");
    // `||` still lexes as concatenation, not two `|` — the one-byte lookahead
    // in the tokenizer is the whole of that, and getting it wrong would turn
    // every concatenation in the corpus into a bitwise OR.
    same("SELECT 'a' || 'b', 'a' || 1, 1 || 2");
    // `||` binds tighter than `|` in sqlite, and mpedb parses this the same
    // way (`1 | (2 || 'x')`) even though it puts `||` in the additive tier;
    // the difference is invisible here. The RESULT differs only because the
    // concatenation's text is a statically-typed text operand, which is the
    // documented refusal — not a precedence divergence.
    let t = open();
    let e = t.db.query("SELECT 1 | 2 || 'x'", &[]).expect_err("text operand").to_string();
    assert!(e.contains("requires int64 operands, got text"), "{e}");
}

#[test]
fn shift_corners_agree_with_sqlite() {
    // A count of 64 or more clears the value…
    same("SELECT 1 << 64, 1 << 100, 8 >> 64, 8 >> 1000");
    // …except `>>` is ARITHMETIC, so a negative value saturates at -1.
    same("SELECT -8 >> 64, -1 >> 63, -1 >> 64, -1 >> 1000");
    // A NEGATIVE count shifts the other way.
    same("SELECT 1 << -1, 1 >> -1, 5 << -2, -5 >> -2");
    // …and -64 or below clamps to a count of 64 rather than negating.
    same("SELECT 1 << -64, 1 << -100, -1 >> -64, -1 << -64");
    // Zero counts, and the sign bit.
    same("SELECT -8 >> 0, -8 << 0, 8 >> 0, 1 << 62, 1 << 63");
    // `<<` WRAPS. This is the one place mpedb does not raise on integer
    // overflow, and it has to be pinned: `+` on the same operands errors.
    same("SELECT 9223372036854775807 << 1, 9223372036854775807 << 2");
    same("SELECT (-9223372036854775807 - 1) << 1, (-9223372036854775807 - 1) >> 1");
}

#[test]
fn null_propagates_through_every_bitwise_operator() {
    same("SELECT NULL | 1, 1 | NULL, NULL | NULL");
    same("SELECT NULL & 1, 1 & NULL, NULL & NULL");
    same("SELECT NULL << 1, 1 << NULL, NULL >> 1, 1 >> NULL");
    same("SELECT ~NULL");
    // 0 is not NULL: the operators must not confuse "no bits" with "unknown".
    same("SELECT 0 & 1, 0 | 0, ~0 IS NULL, NULL & 0 IS NULL");
}

// ---- the coercion, through `any` (sqlite STRICT's ANY) --------------------

#[test]
fn typeless_operands_take_sqlites_full_coercion() {
    let q = "SELECT a | 0, a & 255, a << 1, a >> 1, ~a FROM t WHERE id = 9";
    // Reals truncate toward zero (they do not round) and clamp at the ends.
    for lit in ["3.7", "-3.7", "2.5", "-2.5", "1e300", "-1e300", "9.3e18"] {
        same_with_any(lit, q);
    }
    // Text takes an integer-PREFIX parse: sign, digits, stop at the first
    // non-digit — so `'1e3'` is 1 and `'3.9'` is 3.
    for lit in [
        "'3'", "'abc'", "''", "'1e3'", "'3.9'", "'12abc'", "' -5'", "'+5'", "'--5'", "'5 '", "'9x'",
        "'0x10'", "'  +0009 '",
    ] {
        same_with_any(lit, q);
    }
    // Overflow in the text parse CLAMPS to the end of the range; it does not
    // wrap (20 digits would wrap a u64 accumulator).
    for lit in [
        "'99999999999999999999'",
        "'-99999999999999999999'",
        "'9223372036854775808'",
        "'-9223372036854775809'",
        "'18446744073709551617'",
        "'000000000000000000000000000000009223372036854775807'",
    ] {
        same_with_any(lit, q);
    }
    // Blobs take the same parse over their bytes.
    for lit in ["x'41'", "x'3132'", "x''", "x'2d35'"] {
        same_with_any(lit, q);
    }
    // Integers and NULL through the same column.
    for lit in ["7", "0", "-1", "NULL"] {
        same_with_any(lit, q);
    }
}

// ---- the refusals, each named --------------------------------------------

#[test]
fn statically_typed_non_integers_are_refused_by_name() {
    let t = open();
    for sql in [
        "SELECT r | 1 FROM t",
        "SELECT 1 & r FROM t",
        "SELECT r << 1 FROM t",
        "SELECT ~r FROM t",
        "SELECT s | 1 FROM t",
        "SELECT 'abc' | 1",
        "SELECT x'41' | 1",
        "SELECT 2.5 | 1",
    ] {
        let e = t.db.query(sql, &[]).expect_err(sql).to_string();
        assert!(
            e.contains("requires int64 operands") && e.contains("CAST(x AS INTEGER)"),
            "`{sql}` must refuse by name, got: {e}"
        );
    }
    // …and the CAST the message names produces sqlite's own answer.
    same("SELECT CAST(2.5 AS INTEGER) | 1, CAST('1e3' AS INTEGER) | 1, CAST('abc' AS INTEGER) | 1");
    same("SELECT CAST(r AS INTEGER) | 1, CAST(s AS INTEGER) & 7 FROM t ORDER BY id");
}

#[test]
fn bool_operands_are_the_integer_0_and_1() {
    // sqlite has no boolean type — it IS the integer 0/1, which is the mapping
    // the binder already uses elsewhere, so a comparison result is a legal
    // bitwise operand and gives sqlite's answer.
    same("SELECT (1 = 1) | 2, (1 = 2) | 2, ~(1 = 1), (1 = 1) << 3");
    same("SELECT id, (i > 0) | (i < 0) FROM t ORDER BY id");
}

// ---- parameters ----------------------------------------------------------

#[test]
fn a_bare_parameter_binds_as_int64() {
    let t = open();
    for (n, expect) in [(3i64, "3"), (0, "1"), (-1, "-1")] {
        let got = mpedb_rows(&t.db, "SELECT ? | 1", &[Value::Int(n)]);
        assert_eq!(got, vec![vec![expect.to_string()]], "for ? = {n}");
    }
    assert_eq!(
        mpedb_rows(&t.db, "SELECT ~?", &[Value::Int(5)]),
        vec![vec!["-6".to_string()]]
    );
    // A NULL parameter still propagates.
    assert_eq!(
        mpedb_rows(&t.db, "SELECT ? | 1", &[Value::Null]),
        vec![vec!["NULL".to_string()]]
    );
    // A text parameter in that slot is refused like any other type mismatch.
    let e = t
        .db
        .query("SELECT ? | 1", &[Value::Text("3".into())])
        .expect_err("text must not bind to a bitwise operand")
        .to_string();
    assert!(e.contains("statement requires int64"), "{e}");
}
