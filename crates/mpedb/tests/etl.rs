//! `etl apply`/`revert` — stage 2 of #52 (design/DESIGN-ETL.md §11, §12.2).
//!
//! The contract under test: an in-place column transform keeps what was lost
//! (per-row residuals, keyed by run), verifies 100% of rows against the source
//! hash BEFORE the commit that destroys the source, and can put everything
//! back — or refuses, loudly, with the reason named. Every test here maps to
//! a §12.2 attack.

use mpedb::lens::LensClass;
use mpedb::spellfn::SpellLang;
use mpedb::{ColumnType, Config, Database, ExecResult, Value};

fn db(tag: &str) -> (Database, String) {
    let path = format!(
        "{}/etl-{tag}-{}.mpedb",
        mpedb_testkit::scratch_base_str(),
        std::process::id()
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{path}"
size_mb = 32
max_readers = 8
durability = "none"

[[table]]
name = "t"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "v"
  type = "int64"
  [[table.column]]
  name = "w"
  type = "any"
"#
    );
    (Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(), path)
}

fn def(d: &Database, src: &str) {
    d.create_function(SpellLang::Python, src).unwrap();
}

/// The abs ⇄ sign triple from A2 — non-injective forward, branch-choice residual.
fn define_abs_pair(d: &Database) {
    def(d, "def mag(x):\n    if x < 0:\n        return 0 - x\n    return x\n");
    def(d, "def sgn(x):\n    if x < 0:\n        return 1\n    return 0\n");
    def(d, "def unmag(y, s):\n    if s == 1:\n        return 0 - y\n    return y\n");
    d.create_residual_lens("mag", "mag", "sgn", "unmag", ColumnType::Int64).unwrap();
}

fn rows(r: ExecResult) -> Vec<Vec<Value>> {
    match r {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn col_v(d: &Database) -> Vec<i64> {
    rows(d.query("SELECT v FROM t ORDER BY id", &[]).unwrap())
        .into_iter()
        .map(|r| match &r[0] {
            Value::Int(i) => *i,
            other => panic!("expected int, got {other:?}"),
        })
        .collect()
}

fn seed(d: &Database, vals: &[i64]) {
    let mut s = d.begin().unwrap();
    for (i, v) in vals.iter().enumerate() {
        s.query(
            "INSERT INTO t (id, v, w) VALUES ($1, $2, $3)",
            &[Value::Int(i as i64), Value::Int(*v), Value::Int(*v)],
        )
        .unwrap();
    }
    s.commit().unwrap();
}

/// The headline: apply destroys the signs, the residuals keep them, revert
/// restores the column EXACTLY — and the whole loop is hash-verified twice.
#[test]
fn apply_then_revert_round_trips_exactly() {
    let (d, path) = db("roundtrip");
    define_abs_pair(&d);
    let original = vec![-5i64, 3, 0, -700, 42, -1];
    seed(&d, &original);

    let report = d.etl_apply("mag", "t", "v").unwrap();
    assert_eq!(report.rows, 6);
    assert_eq!(report.residuals, 6, "one residual per row, keyed (run_id, pk)");
    assert_eq!(col_v(&d), vec![5, 3, 0, 700, 42, 1], "signs destroyed in place");

    // What was lost is IN the database, addressable by run:
    let res = rows(
        d.query(
            "SELECT count(*) FROM etl_residual WHERE run_id = $1",
            &[Value::Int(report.run_id)],
        )
        .unwrap(),
    );
    assert_eq!(res[0][0], Value::Int(6));

    let log = d.etl_log().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].outcome, "applied");

    let back = d.etl_revert(report.run_id).unwrap();
    assert_eq!(back.rows, 6);
    assert_eq!(col_v(&d), original, "revert restores the column exactly");
    assert_eq!(d.etl_log().unwrap()[0].outcome, "reverted");

    // And the run's residuals are gone — consumed, not leaked.
    let res = rows(
        d.query(
            "SELECT count(*) FROM etl_residual WHERE run_id = $1",
            &[Value::Int(report.run_id)],
        )
        .unwrap(),
    );
    assert_eq!(res[0][0], Value::Int(0));

    let _ = std::fs::remove_file(&path);
}

/// A bijective pair writes NO residual rows — and its apply still runs the
/// total verification (the Hermes zero-check in database form).
#[test]
fn a_bijective_apply_keeps_no_residuals_and_still_reverts() {
    let (d, path) = db("bijective");
    // `-x`, NOT `0 - x`: subtraction from zero maps BOTH signed zeros to
    // +0.0 — a collision the verifier catches (it did, in this test's first
    // draft). Unary minus is bit-negation and IS bijective.
    def(&d, "def flip(x):\n    return -x\n");
    d.create_lens("flip", "flip", "flip", LensClass::Bijective).unwrap();
    seed(&d, &[1, -2, 3]);

    let report = d.etl_apply("flip", "t", "v").unwrap();
    assert_eq!(report.residuals, 0, "bijective = residual-free, enforced");
    assert_eq!(col_v(&d), vec![-1, 2, -3]);

    d.etl_revert(report.run_id).unwrap();
    assert_eq!(col_v(&d), vec![1, -2, 3]);
    let _ = std::fs::remove_file(&path);
}

/// Commitment 2: in-place transform IS source deletion, so a lossy pair is
/// refused by name.
#[test]
fn a_lossy_pair_is_refused_by_name() {
    let (d, path) = db("lossy");
    def(&d, "def crush(x):\n    return 0\n");
    def(&d, "def uncrush(y):\n    return y\n");
    d.create_lens("crush", "crush", "uncrush", LensClass::Lossy).unwrap();
    seed(&d, &[1, 2]);

    let e = d.etl_apply("crush", "t", "v").unwrap_err().to_string();
    assert!(e.contains("lossy"), "{e}");
    assert!(e.contains("keep the source"), "{e}");
    assert_eq!(col_v(&d), vec![1, 2], "nothing was touched");
    let _ = std::fs::remove_file(&path);
}

/// §12.2 attack 5: a row the pair refuses ABORTS the whole run — no partial
/// column — and the failure is recorded as first-class lineage.
#[test]
fn a_refused_row_aborts_the_whole_run_and_is_lineage() {
    let (d, path) = db("abort");
    define_abs_pair(&d);
    seed(&d, &[5, -3]);
    // NULL is outside the abs pair's domain (`x < 0` on NULL refuses).
    let mut s = d.begin().unwrap();
    s.query("INSERT INTO t (id, v, w) VALUES (99, NULL, 0)", &[]).unwrap();
    s.commit().unwrap();

    let e = d.etl_apply("mag", "t", "v").unwrap_err().to_string();
    assert!(e.contains("refuses row"), "{e}");
    assert!(e.contains("aborted"), "{e}");

    // The column is untouched — including the rows that WOULD have transformed.
    let vals = rows(d.query("SELECT v FROM t ORDER BY id", &[]).unwrap());
    assert_eq!(vals[0][0], Value::Int(5));
    assert_eq!(vals[1][0], Value::Int(-3), "no partial transform survived the abort");
    assert_eq!(vals[2][0], Value::Null);

    // Failed runs are first-class lineage (§7), in their own transaction.
    let log = d.etl_log().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].outcome, "failed");
    assert!(log[0].error.contains("refuses row"), "{}", log[0].error);

    let _ = std::fs::remove_file(&path);
}

/// The rigid-schema gate: a type-changing pair needs an `any` column. Same
/// pair, same data — refused into the int64 column, applied into the Any one.
#[test]
fn a_type_changing_pair_needs_an_any_column() {
    let (d, path) = db("typegate");
    // int → float promotion with the FULL SOURCE as the residual — the honest
    // maximal-residual pair (commitment 6: a stage that drops everything keeps
    // everything; H(X|Y) is just H(X) here). The first draft used the celsius
    // pair, and the verifier refused to register it: over the MIXED corpus its
    // GetPut fails on Int inputs (Int(0) → Float back) — the same type-domain
    // fact stage 1 discovered, enforced at registration exactly as designed.
    def(&d, "def promote(x):\n    return x * 1.0\n");
    def(&d, "def keep_src(x):\n    return x\n");
    def(&d, "def restore(y, r):\n    return r\n");
    d.create_residual_lens("promote", "promote", "keep_src", "restore", ColumnType::Any)
        .unwrap();
    seed(&d, &[0, 100, -40]);

    let e = d.etl_apply("promote", "t", "v").unwrap_err().to_string();
    assert!(e.contains("does not fit"), "{e}");
    assert!(e.contains("any"), "the refusal should say what WOULD work: {e}");
    assert_eq!(col_v(&d), vec![0, 100, -40], "refused early, nothing touched");

    // The Any column takes it — and revert brings back the exact ints.
    let report = d.etl_apply("promote", "t", "w").unwrap();
    let w = rows(d.query("SELECT w FROM t ORDER BY id", &[]).unwrap());
    assert_eq!(w[0][0], Value::Float(0.0));
    assert_eq!(w[2][0], Value::Float(-40.0));
    d.etl_revert(report.run_id).unwrap();
    let w = rows(d.query("SELECT w FROM t ORDER BY id", &[]).unwrap());
    assert_eq!(w[0][0], Value::Int(0), "revert restores the VALUE AND THE TYPE");
    assert_eq!(w[1][0], Value::Int(100));

    let _ = std::fs::remove_file(&path);
}

/// §12.2 attack 6: a second apply on an unreverted column is refused, and the
/// refusal names the run that blocks it. Revert frees the column.
#[test]
fn stacking_is_refused_until_reverted() {
    let (d, path) = db("stack");
    define_abs_pair(&d);
    seed(&d, &[-1, 2]);

    let first = d.etl_apply("mag", "t", "v").unwrap();
    let e = d.etl_apply("mag", "t", "v").unwrap_err().to_string();
    assert!(e.contains("unreverted"), "{e}");
    assert!(e.contains(&first.run_id.to_string()), "the blocking run is named: {e}");

    d.etl_revert(first.run_id).unwrap();
    let second = d.etl_apply("mag", "t", "v").unwrap();
    assert!(second.run_id > first.run_id, "run ids are a counter, never reused");
    let _ = std::fs::remove_file(&path);
}

/// Commitment 8: revert hash-gates the column. A value changed outside the
/// pipeline means the stored residuals describe data that no longer exists —
/// explicit refusal, never silently wrong input.
#[test]
fn revert_refuses_a_column_changed_outside_the_pipeline() {
    let (d, path) = db("tamper");
    define_abs_pair(&d);
    seed(&d, &[-5, 3]);
    let report = d.etl_apply("mag", "t", "v").unwrap();

    let mut s = d.begin().unwrap();
    s.query("UPDATE t SET v = 999 WHERE id = 0", &[]).unwrap();
    s.commit().unwrap();

    let e = d.etl_revert(report.run_id).unwrap_err().to_string();
    assert!(e.contains("changed outside the pipeline"), "{e}");
    let _ = std::fs::remove_file(&path);
}

/// §12.2 attack 3, the missing-row half: a MISSING residual row is a hard
/// error naming the run and the row — what was lost is gone, and reverting
/// without it would fabricate data. (An earlier version of this comment
/// claimed the NULL-VALUE half was untestable because PySpell could not
/// produce NULL — wrong: `None` is a literal in the subset, and
/// `a_null_residual_value_round_trips_and_is_distinct_from_missing` now
/// exercises it for real.)
#[test]
fn a_missing_residual_row_is_a_hard_error() {
    let (d, path) = db("gone");
    define_abs_pair(&d);
    seed(&d, &[-5, 3]);
    let report = d.etl_apply("mag", "t", "v").unwrap();

    let mut s = d.begin().unwrap();
    s.query(
        "DELETE FROM etl_residual WHERE run_id = $1 AND pk_enc = $2",
        &[Value::Int(report.run_id), Value::Blob(vec![2, 0, 0, 0, 0, 0, 0, 0, 0])],
    )
    .unwrap();
    s.commit().unwrap();

    let e = d.etl_revert(report.run_id).unwrap_err().to_string();
    assert!(e.contains("residual row missing"), "{e}");
    assert!(e.contains("fabricate"), "{e}");
    // And nothing was half-reverted: the abort rolled the whole txn back.
    assert_eq!(col_v(&d), vec![5, 3]);
    let _ = std::fs::remove_file(&path);
}

/// A run id that never existed: refused via the lineage row, which is the
/// residuals' meaning (§8.2 amendment).
#[test]
fn reverting_an_unknown_run_is_refused() {
    let (d, path) = db("norun");
    define_abs_pair(&d);
    seed(&d, &[1]);
    let e = d.etl_revert(42).unwrap_err().to_string();
    assert!(e.contains("no etl"), "{e}");
    let _ = std::fs::remove_file(&path);
}

/// Reverting twice: the second is refused by outcome, not by accident.
#[test]
fn double_revert_is_refused_by_outcome() {
    let (d, path) = db("double");
    define_abs_pair(&d);
    seed(&d, &[-1]);
    let report = d.etl_apply("mag", "t", "v").unwrap();
    d.etl_revert(report.run_id).unwrap();
    let e = d.etl_revert(report.run_id).unwrap_err().to_string();
    assert!(e.contains("already reverted"), "{e}");
    let _ = std::fs::remove_file(&path);
}

/// The ETL bookkeeping itself is off-limits as a target.
#[test]
fn the_bookkeeping_tables_are_refused_as_targets() {
    let (d, path) = db("meta");
    define_abs_pair(&d);
    seed(&d, &[1]);
    let _ = d.etl_apply("mag", "t", "v").unwrap(); // creates the tables
    let e = d.etl_apply("mag", "etl_lineage", "rows").unwrap_err().to_string();
    assert!(e.contains("bookkeeping"), "{e}");
    let _ = std::fs::remove_file(&path);
}

/// THE goal scenario, engine-level: putback — reverse that CARRIES EDITS BACK.
/// The image story on rows: a packed-RGB "pixel" column is stripped to
/// grayscale (the chroma offsets are the residual), the user RETOUCHES the
/// grayscale and CROPS the image (deletes rows), and putback re-attaches the
/// colour to the retouched pixels — edits kept, lost half restored, cropped
/// pixels stay gone.
#[test]
fn putback_carries_edits_back_and_keeps_the_crop() {
    let (d, path) = db("putback");
    // px = r*65536 + g*256 + b. forward: luma y = (r+g+b)//3.
    // rex: the chroma offsets (r-y, g-y, b-y), each shifted +255 into 0..511
    // and packed base-512 — exact recovery, so x ↦ (y, rex) is injective.
    // Domain guards (`1 // 0` = a deterministic refusal in PySpell): a pixel
    // is 1..=0xFFFFFF. Every guard here was DEMANDED by the verifier over the
    // probe corpus, one refusal at a time: i64-sized ints overflow the
    // base-512 chroma packing (GetPut fails, value named); fractional floats
    // break exact recovery (`px % 1 != 0`); and the lower bound is 1, not 0,
    // because Float(0.0) and Float(-0.0) produce IDENTICAL (y, chroma) — a
    // GENUINE collision the verifier refused — so pure black is the one pixel
    // value outside the pair's domain.
    let guard = "    if px < 1:\n        return 1 // 0\n    if px > 16777215:\n        return 1 // 0\n    if px % 1 != 0:\n        return 1 // 0\n";
    def(
        &d,
        &format!(
            "def to_gray(px):\n{guard}    r = px // 65536\n    g = (px // 256) % 256\n    b = px % 256\n    return (r + g + b) // 3\n"
        ),
    );
    def(
        &d,
        &format!(
            "def chroma(px):\n{guard}    r = px // 65536\n    g = (px // 256) % 256\n    b = px % 256\n    y = (r + g + b) // 3\n    return ((r - y + 255) * 512 + (g - y + 255)) * 512 + (b - y + 255)\n"
        ),
    );
    def(
        &d,
        "def recolor(y, c):\n    b = c % 512 - 255 + y\n    g = (c // 512) % 512 - 255 + y\n    r = (c // 512 // 512) - 255 + y\n    return r * 65536 + g * 256 + b\n",
    );
    d.create_residual_lens("gray", "to_gray", "chroma", "recolor", ColumnType::Any)
        .unwrap();

    // Four pixels: red-ish, green-ish, blue-ish, gray.
    let px = |r: i64, g: i64, b: i64| r * 65536 + g * 256 + b;
    let original = [px(200, 40, 40), px(30, 180, 60), px(20, 60, 200), px(90, 90, 90)];
    let mut s = d.begin().unwrap();
    for (i, p) in original.iter().enumerate() {
        s.query(
            "INSERT INTO t (id, v, w) VALUES ($1, $2, $3)",
            &[Value::Int(i as i64), Value::Int(*p), Value::Int(0)],
        )
        .unwrap();
    }
    s.commit().unwrap();

    let report = d.etl_apply("gray", "t", "v").unwrap();
    let gray = col_v(&d);
    assert_eq!(gray, vec![93, 90, 93, 90], "luma only — colour is gone from the column");

    // The user works in grayscale: darken pixel 0 by 20, brighten pixel 1 by
    // 10, and CROP pixel 3 away entirely.
    let mut s = d.begin().unwrap();
    s.query("UPDATE t SET v = $1 WHERE id = 0", &[Value::Int(93 - 20)]).unwrap();
    s.query("UPDATE t SET v = $1 WHERE id = 1", &[Value::Int(90 + 10)]).unwrap();
    s.query("DELETE FROM t WHERE id = 3", &[]).unwrap();
    s.commit().unwrap();

    // revert must REFUSE this column — it changed outside the pipeline —
    // and putback is exactly the operation that accepts it.
    let e = d.etl_revert(report.run_id).unwrap_err().to_string();
    assert!(e.contains("changed outside the pipeline"), "{e}");

    let back = d.etl_putback(report.run_id).unwrap();
    assert_eq!(back.rows, 3, "the cropped pixel is not resurrected");

    let after = col_v(&d);
    // Pixel 0: same chroma offsets re-attached to luma 73 — the whole pixel
    // darkened by 20 per channel, colour preserved.
    assert_eq!(after[0], px(200 - 20, 40 - 20, 40 - 20), "darkened WITH its colour back");
    // Pixel 1: brightened by 10 per channel.
    assert_eq!(after[1], px(30 + 10, 180 + 10, 60 + 10), "brightened WITH its colour back");
    // Pixel 2: untouched in gray, so putback restores it exactly.
    assert_eq!(after[2], original[2], "unedited pixel round-trips exactly");

    // The run is consumed: residuals gone, outcome recorded, second putback refused.
    let res = rows(
        d.query(
            "SELECT count(*) FROM etl_residual WHERE run_id = $1",
            &[Value::Int(report.run_id)],
        )
        .unwrap(),
    );
    assert_eq!(res[0][0], Value::Int(0));
    assert_eq!(d.etl_log().unwrap()[0].outcome, "putback");
    let e = d.etl_putback(report.run_id).unwrap_err().to_string();
    assert!(e.contains("putback"), "{e}");

    let _ = std::fs::remove_file(&path);
}

/// A row INSERTED after the apply has no residual: for a residual pair,
/// inverting it is the refused creation path (§4) — named, with the fix.
#[test]
fn putback_refuses_rows_inserted_after_the_apply() {
    let (d, path) = db("putback-new");
    define_abs_pair(&d);
    seed(&d, &[-5, 3]);
    let report = d.etl_apply("mag", "t", "v").unwrap();

    let mut s = d.begin().unwrap();
    s.query("INSERT INTO t (id, v, w) VALUES (99, 7, 0)", &[]).unwrap();
    s.commit().unwrap();

    let e = d.etl_putback(report.run_id).unwrap_err().to_string();
    assert!(e.contains("no residual"), "{e}");
    assert!(e.contains("inserted after the apply"), "{e}");
    // Nothing half-inverted:
    let vals = rows(d.query("SELECT v FROM t ORDER BY id", &[]).unwrap());
    assert_eq!(vals[0][0], Value::Int(5), "aborted atomically");
    let _ = std::fs::remove_file(&path);
}

/// An edit the pair cannot carry back — outside forward's image — fails the
/// per-row PutRes check with the row named. abs/sign: a NEGATIVE value edited
/// into the magnitude column has no preimage under (|x|, sign).
#[test]
fn putback_refuses_an_edit_outside_the_pairs_image() {
    let (d, path) = db("putback-bad");
    define_abs_pair(&d);
    seed(&d, &[-5, 3]);
    let report = d.etl_apply("mag", "t", "v").unwrap();

    let mut s = d.begin().unwrap();
    s.query("UPDATE t SET v = $1 WHERE id = 0", &[Value::Int(-9)]).unwrap();
    s.commit().unwrap();

    let e = d.etl_putback(report.run_id).unwrap_err().to_string();
    assert!(e.contains("outside the pair's image"), "{e}");
    assert!(e.contains("Int(-9)"), "the offending edit is named: {e}");
    let _ = std::fs::remove_file(&path);
}

/// Bijective putback: the creation path is total by construction, so rows
/// inserted after the apply invert like every other row — and edits flow back
/// through the inverse.
#[test]
fn bijective_putback_carries_edits_and_new_rows() {
    let (d, path) = db("putback-bij");
    def(&d, "def flip(x):\n    return -x\n");
    d.create_lens("flip", "flip", "flip", LensClass::Bijective).unwrap();
    seed(&d, &[1, -2]);
    let report = d.etl_apply("flip", "t", "v").unwrap();
    assert_eq!(col_v(&d), vec![-1, 2]);

    let mut s = d.begin().unwrap();
    s.query("UPDATE t SET v = $1 WHERE id = 0", &[Value::Int(-100)]).unwrap();
    s.query("INSERT INTO t (id, v, w) VALUES (9, 50, 0)", &[]).unwrap();
    s.commit().unwrap();

    let back = d.etl_putback(report.run_id).unwrap();
    assert_eq!(back.rows, 3);
    assert_eq!(col_v(&d), vec![100, -2, -50], "edit and new row both inverted");
    let _ = std::fs::remove_file(&path);
}

/// NULL as a residual VALUE, end to end — and the correction of a wrong
/// claim: an earlier comment here said no PySpell pair can produce NULL, but
/// `None` is a literal in the subset. The clamp pair returns None for rows
/// that were NOT clamped (nothing was lost) and the original for rows that
/// were — so the residual column legitimately holds NULL, and the distinction
/// from a MISSING row (hard error) is exercised for real, not argued.
#[test]
fn a_null_residual_value_round_trips_and_is_distinct_from_missing() {
    let (d, path) = db("nullres");
    def(&d, "def clamp(x):\n    if x > 100:\n        return 100\n    return x\n");
    def(
        &d,
        "def clamp_rex(x):\n    if x > 100:\n        return x\n    return None\n",
    );
    def(
        &d,
        "def unclamp(y, r):\n    if r is None:\n        return y\n    return r\n",
    );
    // `any`, not int64: the probe corpus routes Float(inf) through the
    // clamped branch, and rex then returns a float — the same
    // integral-floats-shadow-ints fact every pair here keeps meeting.
    d.create_residual_lens("clamp", "clamp", "clamp_rex", "unclamp", ColumnType::Any)
        .unwrap();

    let original = vec![5i64, 250, 100, 101];
    seed(&d, &original);
    let report = d.etl_apply("clamp", "t", "v").unwrap();
    assert_eq!(col_v(&d), vec![5, 100, 100, 100], "clamped in place");
    // Every row has a residual ROW; two of them hold the VALUE NULL.
    let nulls = rows(
        d.query(
            "SELECT count(*) FROM etl_residual WHERE run_id = $1 AND residual IS NULL",
            &[Value::Int(report.run_id)],
        )
        .unwrap(),
    );
    assert_eq!(nulls[0][0], Value::Int(2), "unclamped rows lost nothing: residual = NULL");

    // Putback carries an edit back through a NULL residual (r is None ⇒
    // x' = y'), and revert-after would be refused; do the full putback.
    let mut s = d.begin().unwrap();
    s.query("UPDATE t SET v = 7 WHERE id = 0", &[]).unwrap();
    s.commit().unwrap();
    d.etl_putback(report.run_id).unwrap();
    assert_eq!(col_v(&d), vec![7, 250, 100, 101], "edit kept; clamped values restored");

    let _ = std::fs::remove_file(&path);
}

/// Text values through the whole loop: redaction with recovery. The forward
/// is CONSTANT — maximally non-injective — and registers as a residual pair
/// because x ↦ ("[REDACTED]", x) is injective via the residual alone. Revert
/// restores the exact originals. (Putback of an edited redaction is
/// meaningless and PutRes refuses it — also asserted.)
#[test]
fn text_redaction_applies_and_reverts_exactly() {
    let (d, path) = db("redact");
    def(&d, "def redact(s):\n    return \"x\" + s + \"\" and \"[REDACTED]\"\n");
    def(&d, "def redact_rex(s):\n    return s\n");
    def(&d, "def unredact(y, r):\n    return r\n");
    d.create_residual_lens("redact", "redact", "redact_rex", "unredact", ColumnType::Any)
        .unwrap();

    let originals = ["password=hunter2", "", "æøå"];
    let mut s = d.begin().unwrap();
    for (i, t) in originals.iter().enumerate() {
        s.query(
            "INSERT INTO t (id, v, w) VALUES ($1, 0, $2)",
            &[Value::Int(i as i64), Value::Text(t.to_string())],
        )
        .unwrap();
    }
    s.commit().unwrap();

    let report = d.etl_apply("redact", "t", "w").unwrap();
    let redacted = rows(d.query("SELECT w FROM t ORDER BY id", &[]).unwrap());
    for r in &redacted {
        assert_eq!(r[0], Value::Text("[REDACTED]".into()));
    }

    // Editing a redaction has no preimage — PutRes refuses it by name.
    let mut s = d.begin().unwrap();
    s.query("UPDATE t SET w = 'peek' WHERE id = 0", &[]).unwrap();
    s.commit().unwrap();
    let e = d.etl_putback(report.run_id).unwrap_err().to_string();
    assert!(e.contains("outside the pair's image"), "{e}");

    // Restore the redaction text and revert exactly.
    let mut s = d.begin().unwrap();
    s.query("UPDATE t SET w = '[REDACTED]' WHERE id = 0", &[]).unwrap();
    s.commit().unwrap();
    let back = d.etl_revert(report.run_id).unwrap();
    assert_eq!(back.rows, 3);
    let after = rows(d.query("SELECT w FROM t ORDER BY id", &[]).unwrap());
    for (r, want) in after.iter().zip(originals.iter()) {
        assert_eq!(r[0], Value::Text(want.to_string()));
    }
    let _ = std::fs::remove_file(&path);
}

/// An empty table is a legal, boring run: 0 rows, 0 residuals, revert works.
/// The edge exists because both hash chains are the empty chain — they must
/// agree rather than trip the verifier.
#[test]
fn an_empty_table_applies_and_reverts_without_drama() {
    let (d, path) = db("empty-tbl");
    define_abs_pair(&d);
    let report = d.etl_apply("mag", "t", "v").unwrap();
    assert_eq!((report.rows, report.residuals), (0, 0));
    d.etl_revert(report.run_id).unwrap();
    let _ = std::fs::remove_file(&path);
}

/// The residuals and lineage live in the FILE, not the handle: apply with one
/// Database, drop it, reopen the same file, and putback with the second
/// handle — the run must be fully resumable across process lifetimes.
#[test]
fn a_run_survives_reopen_and_puts_back_from_a_fresh_handle() {
    let (d, path) = db("reopen");
    define_abs_pair(&d);
    seed(&d, &[-5, 3, -700]);
    let report = d.etl_apply("mag", "t", "v").unwrap();
    let run_id = report.run_id;
    drop(d);

    let d2 = Database::open_from_file(std::path::Path::new(&path)).unwrap();
    let mut s = d2.begin().unwrap();
    s.query("UPDATE t SET v = 9 WHERE id = 1", &[]).unwrap();
    s.commit().unwrap();
    d2.etl_putback(run_id).unwrap();
    assert_eq!(
        rows(d2.query("SELECT v FROM t ORDER BY id", &[]).unwrap())
            .into_iter()
            .map(|r| r[0].clone())
            .collect::<Vec<_>>(),
        vec![Value::Int(-5), Value::Int(9), Value::Int(-700)],
        "sign restored where unedited; the edit (3 -> 9, positive stays) kept"
    );
    let _ = std::fs::remove_file(&path);
}

/// A TEXT primary key: pk_enc is canonical value bits, not an integer
/// assumption. Apply + revert keyed by text pks.
#[test]
fn a_text_primary_key_carries_residuals() {
    let path = format!(
        "{}/etl-textpk-{}.mpedb",
        mpedb_testkit::scratch_base_str(),
        std::process::id()
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{path}"
size_mb = 32
max_readers = 8
durability = "none"

[[table]]
name = "t"
primary_key = ["k"]
  [[table.column]]
  name = "k"
  type = "text"
  [[table.column]]
  name = "v"
  type = "int64"
"#
    );
    let d = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    def(&d, "def mag(x):\n    if x < 0:\n        return 0 - x\n    return x\n");
    def(&d, "def sgn(x):\n    if x < 0:\n        return 1\n    return 0\n");
    def(&d, "def unmag(y, s):\n    if s == 1:\n        return 0 - y\n    return y\n");
    d.create_residual_lens("mag", "mag", "sgn", "unmag", ColumnType::Int64).unwrap();

    let mut s = d.begin().unwrap();
    for (k, v) in [("alpha", -5i64), ("æøå", 3), ("", -9)] {
        s.query(
            "INSERT INTO t (k, v) VALUES ($1, $2)",
            &[Value::Text(k.into()), Value::Int(v)],
        )
        .unwrap();
    }
    s.commit().unwrap();

    let report = d.etl_apply("mag", "t", "v").unwrap();
    assert_eq!(report.residuals, 3);
    d.etl_revert(report.run_id).unwrap();
    let vals = rows(d.query("SELECT v FROM t ORDER BY k", &[]).unwrap());
    assert_eq!(
        vals.into_iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
        vec![Value::Int(-9), Value::Int(-5), Value::Int(3)],
        "exact restore keyed by text pks (empty and non-ASCII included)"
    );
    let _ = std::fs::remove_file(&path);
}

/// The whole loop under `durability = "wal"` — the ring/group-commit path,
/// not the `none` shortcut every other test here uses. One smoke: the ETL
/// transaction shape must be durable-mode-agnostic.
#[test]
fn etl_works_under_wal_durability() {
    let path = format!(
        "{}/etl-wal-{}.mpedb",
        mpedb_testkit::scratch_base_str(),
        std::process::id()
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{path}"
size_mb = 32
max_readers = 8
durability = "wal"

[[table]]
name = "t"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "v"
  type = "int64"
"#
    );
    let d = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    def(&d, "def mag(x):\n    if x < 0:\n        return 0 - x\n    return x\n");
    def(&d, "def sgn(x):\n    if x < 0:\n        return 1\n    return 0\n");
    def(&d, "def unmag(y, s):\n    if s == 1:\n        return 0 - y\n    return y\n");
    d.create_residual_lens("mag", "mag", "sgn", "unmag", ColumnType::Int64).unwrap();
    let mut s = d.begin().unwrap();
    for i in 0..50i64 {
        s.query(
            "INSERT INTO t (id, v) VALUES ($1, $2)",
            &[Value::Int(i), Value::Int(-i)],
        )
        .unwrap();
    }
    s.commit().unwrap();
    let report = d.etl_apply("mag", "t", "v").unwrap();
    d.etl_revert(report.run_id).unwrap();
    let neg = rows(d.query("SELECT count(*) FROM t WHERE v < 0", &[]).unwrap());
    assert_eq!(neg[0][0], Value::Int(49), "restored under wal durability");
    let _ = std::fs::remove_file(&path);
}

/// Scale smoke, not a benchmark: 5 000 rows through apply -> edit -> putback.
/// Exists so a super-linear regression in the per-row loops shows up as a
/// timeout rather than a user report. The residual probe is verified to plan
/// as PkPoint(run_id, pk_enc), so the ~65 s this takes is debug-build
/// per-statement overhead, linear — hence #[ignore], per the house rule.
#[test]
#[ignore = "slow in debug (~65 s, linear): run with --ignored"]
fn five_thousand_rows_apply_and_put_back() {
    let (d, path) = db("scale");
    define_abs_pair(&d);
    let mut s = d.begin().unwrap();
    for i in 0..5000i64 {
        s.query(
            "INSERT INTO t (id, v, w) VALUES ($1, $2, 0)",
            &[Value::Int(i), Value::Int(if i % 2 == 0 { -i } else { i })],
        )
        .unwrap();
    }
    s.commit().unwrap();
    let report = d.etl_apply("mag", "t", "v").unwrap();
    assert_eq!(report.rows, 5000);
    let mut s = d.begin().unwrap();
    s.query("UPDATE t SET v = 12345 WHERE id = 100", &[]).unwrap();
    s.commit().unwrap();
    d.etl_putback(report.run_id).unwrap();
    let v100 = rows(d.query("SELECT v FROM t WHERE id = 100", &[]).unwrap());
    assert_eq!(v100[0][0], Value::Int(-12345), "edit carried back with its sign");
    let _ = std::fs::remove_file(&path);
}
