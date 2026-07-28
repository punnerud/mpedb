//! `rretl apply`/`revert` — stage 2 of #52 (design/DESIGN-RRETL.md §11, §12.2).
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

    let report = d.rretl_apply("mag", "t", "v").unwrap();
    assert_eq!(report.rows, 6);
    assert_eq!(report.residuals, 6, "one residual per row, keyed (run_id, pk)");
    assert_eq!(col_v(&d), vec![5, 3, 0, 700, 42, 1], "signs destroyed in place");

    // What was lost is IN the database, addressable by run:
    let res = rows(
        d.query(
            "SELECT count(*) FROM rretl_residual WHERE run_id = $1",
            &[Value::Int(report.run_id)],
        )
        .unwrap(),
    );
    assert_eq!(res[0][0], Value::Int(6));

    let log = d.rretl_log().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].outcome, "applied");

    let back = d.rretl_revert(report.run_id).unwrap();
    assert_eq!(back.rows, 6);
    assert_eq!(col_v(&d), original, "revert restores the column exactly");
    assert_eq!(d.rretl_log().unwrap()[0].outcome, "reverted");

    // And the run's residuals are gone — consumed, not leaked.
    let res = rows(
        d.query(
            "SELECT count(*) FROM rretl_residual WHERE run_id = $1",
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

    let report = d.rretl_apply("flip", "t", "v").unwrap();
    assert_eq!(report.residuals, 0, "bijective = residual-free, enforced");
    assert_eq!(col_v(&d), vec![-1, 2, -3]);

    d.rretl_revert(report.run_id).unwrap();
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

    let e = d.rretl_apply("crush", "t", "v").unwrap_err().to_string();
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

    let e = d.rretl_apply("mag", "t", "v").unwrap_err().to_string();
    assert!(e.contains("refuses row"), "{e}");
    assert!(e.contains("aborted"), "{e}");

    // The column is untouched — including the rows that WOULD have transformed.
    let vals = rows(d.query("SELECT v FROM t ORDER BY id", &[]).unwrap());
    assert_eq!(vals[0][0], Value::Int(5));
    assert_eq!(vals[1][0], Value::Int(-3), "no partial transform survived the abort");
    assert_eq!(vals[2][0], Value::Null);

    // Failed runs are first-class lineage (§7), in their own transaction.
    let log = d.rretl_log().unwrap();
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

    let e = d.rretl_apply("promote", "t", "v").unwrap_err().to_string();
    assert!(e.contains("does not fit"), "{e}");
    assert!(e.contains("any"), "the refusal should say what WOULD work: {e}");
    assert_eq!(col_v(&d), vec![0, 100, -40], "refused early, nothing touched");

    // The Any column takes it — and revert brings back the exact ints.
    let report = d.rretl_apply("promote", "t", "w").unwrap();
    let w = rows(d.query("SELECT w FROM t ORDER BY id", &[]).unwrap());
    assert_eq!(w[0][0], Value::Float(0.0));
    assert_eq!(w[2][0], Value::Float(-40.0));
    d.rretl_revert(report.run_id).unwrap();
    let w = rows(d.query("SELECT w FROM t ORDER BY id", &[]).unwrap());
    assert_eq!(w[0][0], Value::Int(0), "revert restores the VALUE AND THE TYPE");
    assert_eq!(w[1][0], Value::Int(100));

    let _ = std::fs::remove_file(&path);
}

/// Runs STACK, and unwind strictly LIFO: reverting a buried run is refused
/// with the topmost run named; unwinding top-down works, and each layer's
/// hash gate still holds because run N+1's source IS run N's output.
#[test]
fn stacked_runs_unwind_lifo_only() {
    let (d, path) = db("stack");
    define_abs_pair(&d);
    def(&d, "def flip(x):\n    return -x\n");
    d.create_lens("flip", "flip", "flip", LensClass::Bijective).unwrap();
    seed(&d, &[-1, 2]);

    let first = d.rretl_apply("mag", "t", "v").unwrap(); // [1, 2]
    let second = d.rretl_apply("flip", "t", "v").unwrap(); // [-1, -2]
    assert!(second.run_id > first.run_id, "run ids are a counter, never reused");
    assert_eq!(col_v(&d), vec![-1, -2]);

    // The buried run is refused BY NAME, for both unwind operations.
    for res in [d.rretl_revert(first.run_id), d.rretl_putback(first.run_id)] {
        let e = res.unwrap_err().to_string();
        assert!(e.contains("buried under"), "{e}");
        assert!(e.contains(&second.run_id.to_string()), "the blocker is named: {e}");
    }

    // LIFO order unwinds cleanly to the original.
    d.rretl_revert(second.run_id).unwrap();
    d.rretl_revert(first.run_id).unwrap();
    assert_eq!(col_v(&d), vec![-1, 2]);
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
    let report = d.rretl_apply("mag", "t", "v").unwrap();

    let mut s = d.begin().unwrap();
    s.query("UPDATE t SET v = 999 WHERE id = 0", &[]).unwrap();
    s.commit().unwrap();

    let e = d.rretl_revert(report.run_id).unwrap_err().to_string();
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
    let report = d.rretl_apply("mag", "t", "v").unwrap();

    let mut s = d.begin().unwrap();
    s.query(
        "DELETE FROM rretl_residual WHERE run_id = $1 AND pk_enc = $2",
        &[Value::Int(report.run_id), Value::Blob(vec![2, 0, 0, 0, 0, 0, 0, 0, 0])],
    )
    .unwrap();
    s.commit().unwrap();

    // The residual GATE catches the deletion before any per-row read: the
    // stored set no longer hashes to what the apply recorded. (The per-row
    // missing-row check still stands behind it, for invariant breaks inside
    // the writing transaction itself.)
    let e = d.rretl_revert(report.run_id).unwrap_err().to_string();
    assert!(e.contains("no longer hash"), "{e}");
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
    let e = d.rretl_revert(42).unwrap_err().to_string();
    assert!(e.contains("no rretl"), "{e}");
    let _ = std::fs::remove_file(&path);
}

/// Reverting twice: the second is refused by outcome, not by accident.
#[test]
fn double_revert_is_refused_by_outcome() {
    let (d, path) = db("double");
    define_abs_pair(&d);
    seed(&d, &[-1]);
    let report = d.rretl_apply("mag", "t", "v").unwrap();
    d.rretl_revert(report.run_id).unwrap();
    let e = d.rretl_revert(report.run_id).unwrap_err().to_string();
    assert!(e.contains("already reverted"), "{e}");
    let _ = std::fs::remove_file(&path);
}

/// The ETL bookkeeping itself is off-limits as a target.
#[test]
fn the_bookkeeping_tables_are_refused_as_targets() {
    let (d, path) = db("meta");
    define_abs_pair(&d);
    seed(&d, &[1]);
    let _ = d.rretl_apply("mag", "t", "v").unwrap(); // creates the tables
    let e = d.rretl_apply("mag", "rretl_lineage", "rows").unwrap_err().to_string();
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

    let report = d.rretl_apply("gray", "t", "v").unwrap();
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
    let e = d.rretl_revert(report.run_id).unwrap_err().to_string();
    assert!(e.contains("changed outside the pipeline"), "{e}");

    let back = d.rretl_putback(report.run_id).unwrap();
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
            "SELECT count(*) FROM rretl_residual WHERE run_id = $1",
            &[Value::Int(report.run_id)],
        )
        .unwrap(),
    );
    assert_eq!(res[0][0], Value::Int(0));
    assert_eq!(d.rretl_log().unwrap()[0].outcome, "putback");
    let e = d.rretl_putback(report.run_id).unwrap_err().to_string();
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
    let report = d.rretl_apply("mag", "t", "v").unwrap();

    let mut s = d.begin().unwrap();
    s.query("INSERT INTO t (id, v, w) VALUES (99, 7, 0)", &[]).unwrap();
    s.commit().unwrap();

    let e = d.rretl_putback(report.run_id).unwrap_err().to_string();
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
    let report = d.rretl_apply("mag", "t", "v").unwrap();

    let mut s = d.begin().unwrap();
    s.query("UPDATE t SET v = $1 WHERE id = 0", &[Value::Int(-9)]).unwrap();
    s.commit().unwrap();

    let e = d.rretl_putback(report.run_id).unwrap_err().to_string();
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
    let report = d.rretl_apply("flip", "t", "v").unwrap();
    assert_eq!(col_v(&d), vec![-1, 2]);

    let mut s = d.begin().unwrap();
    s.query("UPDATE t SET v = $1 WHERE id = 0", &[Value::Int(-100)]).unwrap();
    s.query("INSERT INTO t (id, v, w) VALUES (9, 50, 0)", &[]).unwrap();
    s.commit().unwrap();

    let back = d.rretl_putback(report.run_id).unwrap();
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
    let report = d.rretl_apply("clamp", "t", "v").unwrap();
    assert_eq!(col_v(&d), vec![5, 100, 100, 100], "clamped in place");
    // Every row has a residual ROW; two of them hold the VALUE NULL.
    let nulls = rows(
        d.query(
            "SELECT count(*) FROM rretl_residual WHERE run_id = $1 AND residual IS NULL",
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
    d.rretl_putback(report.run_id).unwrap();
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

    let report = d.rretl_apply("redact", "t", "w").unwrap();
    let redacted = rows(d.query("SELECT w FROM t ORDER BY id", &[]).unwrap());
    for r in &redacted {
        assert_eq!(r[0], Value::Text("[REDACTED]".into()));
    }

    // Editing a redaction has no preimage — PutRes refuses it by name.
    let mut s = d.begin().unwrap();
    s.query("UPDATE t SET w = 'peek' WHERE id = 0", &[]).unwrap();
    s.commit().unwrap();
    let e = d.rretl_putback(report.run_id).unwrap_err().to_string();
    assert!(e.contains("outside the pair's image"), "{e}");

    // Restore the redaction text and revert exactly.
    let mut s = d.begin().unwrap();
    s.query("UPDATE t SET w = '[REDACTED]' WHERE id = 0", &[]).unwrap();
    s.commit().unwrap();
    let back = d.rretl_revert(report.run_id).unwrap();
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
    let report = d.rretl_apply("mag", "t", "v").unwrap();
    assert_eq!((report.rows, report.residuals), (0, 0));
    d.rretl_revert(report.run_id).unwrap();
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
    let report = d.rretl_apply("mag", "t", "v").unwrap();
    let run_id = report.run_id;
    drop(d);

    let d2 = Database::open_from_file(std::path::Path::new(&path)).unwrap();
    let mut s = d2.begin().unwrap();
    s.query("UPDATE t SET v = 9 WHERE id = 1", &[]).unwrap();
    s.commit().unwrap();
    d2.rretl_putback(run_id).unwrap();
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

    let report = d.rretl_apply("mag", "t", "v").unwrap();
    assert_eq!(report.residuals, 3);
    d.rretl_revert(report.run_id).unwrap();
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
    let report = d.rretl_apply("mag", "t", "v").unwrap();
    d.rretl_revert(report.run_id).unwrap();
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
    let report = d.rretl_apply("mag", "t", "v").unwrap();
    assert_eq!(report.rows, 5000);
    let mut s = d.begin().unwrap();
    s.query("UPDATE t SET v = 12345 WHERE id = 100", &[]).unwrap();
    s.commit().unwrap();
    d.rretl_putback(report.run_id).unwrap();
    let v100 = rows(d.query("SELECT v FROM t WHERE id = 100", &[]).unwrap());
    assert_eq!(v100[0][0], Value::Int(-12345), "edit carried back with its sign");
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// The randomized chain — rRETL's gold, model-checked
// ---------------------------------------------------------------------------

/// One pool pair: the PySpell functions plus a RUST MIRROR — the test's
/// oracle. `domain_ok` mirrors the pair's forward domain; the harness only
/// applies a pair when EVERY current value is inside it (a refused row aborts
/// a run by design, and the chain must compose). `edit` yields an in-image
/// edited value, and the harness additionally probes the edit's full
/// carry-down against every lower layer's domain before committing it.
struct PoolPair {
    name: &'static str,
    fwd_src: String,
    rex_src: Option<String>,
    inv_src: String,
    fwd: fn(i64) -> i64,
    rex: fn(i64) -> Option<i64>,
    inv: fn(i64, Option<i64>) -> i64,
    domain_ok: fn(i64) -> bool,
    edit: fn(u64) -> i64,
}

/// The standard domain guard every pool pair carries, distilled from FOUR
/// registration refusals the verifier handed this file while it was written:
/// fractional floats break exact recovery; ±0.0 collide under ordinary
/// arithmetic (0.0 + k == -0.0 + k); subnormals are ABSORBED (2.2e-308 + 7777
/// == 7777.0 exactly, and the inverse returns 0.0); and beyond 2^53 integral
/// floats lose exactness. Integral, nonzero, |x| <= 4e9 kills all four —
/// integral floats in that range ride every pair's arithmetic exactly.
const STD_GUARD: &str = "    if x % 1 != 0:\n        return 1 // 0\n    if x == 0:\n        return 1 // 0\n    if x > 4000000000:\n        return 1 // 0\n    if x < 0 - 4000000000:\n        return 1 // 0\n";

fn std_domain(x: i64) -> bool {
    x != 0 && x.abs() <= 4_000_000_000
}

fn pool() -> Vec<PoolPair> {
    // Per-pair EXTRA guards exclude the one input whose output would be 0 —
    // a zero row would make every later std-guarded pair inadmissible and
    // deadlock the chain (the harness would panic on "no admissible pair").
    vec![
        PoolPair {
            name: "p_neg",
            fwd_src: format!("def p_neg_f(x):\n{STD_GUARD}    return -x\n"),
            rex_src: None,
            inv_src: "def p_neg_i(y):\n    return -y\n".into(),
            fwd: |x| -x,
            rex: |_| None,
            inv: |y, _| -y,
            domain_ok: std_domain,
            edit: |d| nz((d % 100_000) as i64 - 50_000),
        },
        PoolPair {
            name: "p_add7",
            fwd_src: format!(
                "def p_add7_f(x):\n{STD_GUARD}    if x == 0 - 7777:\n        return 1 // 0\n    return x + 7777\n"
            ),
            rex_src: None,
            inv_src: "def p_add7_i(y):\n    return y - 7777\n".into(),
            fwd: |x| x + 7777,
            rex: |_| None,
            inv: |y, _| y - 7777,
            domain_ok: |x| std_domain(x) && x != -7777,
            edit: |d| match (d % 100_000) as i64 - 50_000 {
                7777 | 0 => 42,
                v => v,
            },
        },
        PoolPair {
            name: "p_mul3",
            fwd_src: format!("def p_mul3_f(x):\n{STD_GUARD}    return x * 3\n"),
            rex_src: None,
            inv_src: "def p_mul3_i(y):\n    return y // 3\n".into(),
            fwd: |x| x * 3,
            rex: |_| None,
            inv: |y, _| y.div_euclid(3),
            domain_ok: std_domain,
            edit: |d| nz((d % 60_000) as i64 - 30_000) * 3,
        },
        PoolPair {
            name: "p_inv999",
            fwd_src: format!(
                "def p_inv999_f(x):\n{STD_GUARD}    if x == 999999:\n        return 1 // 0\n    return 999999 - x\n"
            ),
            rex_src: None,
            inv_src: "def p_inv999_i(y):\n    return 999999 - y\n".into(),
            fwd: |x| 999_999 - x,
            rex: |_| None,
            inv: |y, _| 999_999 - y,
            domain_ok: |x| std_domain(x) && x != 999_999,
            edit: |d| match (d % 100_000) as i64 - 50_000 {
                999_999 | 0 => 5,
                v => v,
            },
        },
        PoolPair {
            name: "p_scale2p1",
            fwd_src: format!("def p_scale2p1_f(x):\n{STD_GUARD}    return x * 2 + 1\n"),
            rex_src: None,
            inv_src: "def p_scale2p1_i(y):\n    return (y - 1) // 2\n".into(),
            fwd: |x| x * 2 + 1,
            rex: |_| None,
            inv: |y, _| (y - 1).div_euclid(2),
            domain_ok: std_domain,
            // image = odd numbers; y' = 1 has preimage 0, avoid it
            edit: |d| {
                let k = (d % 30_000) as i64 + 1;
                if d & 1 == 0 { 2 * k + 1 } else { -(2 * k) + 1 }
            },
        },
        PoolPair {
            name: "p_abs",
            fwd_src: format!(
                "def p_abs_f(x):\n{STD_GUARD}    if x < 0:\n        return 0 - x\n    return x\n"
            ),
            rex_src: Some(format!(
                "def p_abs_r(x):\n{STD_GUARD}    if x < 0:\n        return 1\n    return 0\n"
            )),
            inv_src:
                "def p_abs_i(y, s):\n    if s == 1:\n        return 0 - y\n    return y\n"
                    .into(),
            fwd: |x| x.abs(),
            rex: |x| Some(if x < 0 { 1 } else { 0 }),
            inv: |y, r| if r == Some(1) { -y } else { y },
            domain_ok: std_domain,
            edit: |d| (d % 50_000) as i64 + 1,
        },
        PoolPair {
            name: "p_half",
            fwd_src: format!(
                "def p_half_f(x):\n{STD_GUARD}    if x < 2:\n        if x > 0 - 2:\n            return 1 // 0\n    if x < 0:\n        return 0 - ((0 - x) // 2)\n    return x // 2\n"
            ),
            rex_src: Some(format!(
                "def p_half_r(x):\n{STD_GUARD}    if x < 2:\n        if x > 0 - 2:\n            return 1 // 0\n    if x < 0:\n        return (0 - x) % 2\n    return x % 2\n"
            )),
            inv_src: "def p_half_i(y, r):\n    if y < 0:\n        return 0 - ((0 - y) * 2 + r)\n    return y * 2 + r\n".into(),
            fwd: |x| if x < 0 { -((-x) / 2) } else { x / 2 },
            rex: |x| Some(if x < 0 { (-x) % 2 } else { x % 2 }),
            inv: |y, r| {
                let r = r.unwrap();
                if y < 0 { -((-y) * 2 + r) } else { y * 2 + r }
            },
            // |x| >= 2 keeps the halved output nonzero
            domain_ok: |x| std_domain(x) && x.abs() >= 2,
            edit: |d| (d % 50_000) as i64 + 1,
        },
        PoolPair {
            name: "p_mod1000",
            fwd_src: format!(
                "def p_mod1000_f(x):\n{STD_GUARD}    if x < 1:\n        return 1 // 0\n    return x % 1000 + 1\n"
            ),
            rex_src: Some(format!(
                "def p_mod1000_r(x):\n{STD_GUARD}    if x < 1:\n        return 1 // 0\n    return x // 1000\n"
            )),
            inv_src: "def p_mod1000_i(y, r):\n    return r * 1000 + y - 1\n".into(),
            fwd: |x| x % 1000 + 1,
            rex: |x| Some(x / 1000),
            inv: |y, r| r.unwrap() * 1000 + y - 1,
            domain_ok: |x| std_domain(x) && x >= 1,
            // image = 1..=1000; y' with r=0 giving x' = y'-1 = 0 is probed out
            edit: |d| (d % 1000) as i64 + 1,
        },
        PoolPair {
            name: "p_clamp",
            fwd_src: format!(
                "def p_clamp_f(x):\n{STD_GUARD}    if x > 5000:\n        return 5000\n    return x\n"
            ),
            rex_src: Some(format!(
                "def p_clamp_r(x):\n{STD_GUARD}    if x > 5000:\n        return x\n    return None\n"
            )),
            inv_src: "def p_clamp_i(y, r):\n    if r is None:\n        return y\n    return r\n".into(),
            fwd: |x| x.min(5000),
            rex: |x| if x > 5000 { Some(x) } else { None },
            inv: |y, r| r.unwrap_or(y),
            domain_ok: std_domain,
            // legal only for r = None rows; the harness edits those only
            edit: |d| (d % 4999) as i64 + 1,
        },
        PoolPair {
            name: "p_sub_million",
            fwd_src: format!(
                "def p_sub_million_f(x):\n{STD_GUARD}    if x == 1000000:\n        return 1 // 0\n    return x - 1000000\n"
            ),
            rex_src: None,
            inv_src: "def p_sub_million_i(y):\n    return y + 1000000\n".into(),
            fwd: |x| x - 1_000_000,
            rex: |_| None,
            inv: |y, _| y + 1_000_000,
            domain_ok: |x| std_domain(x) && x != 1_000_000,
            edit: |d| match (d % 100_000) as i64 - 50_000 {
                -1_000_000 | 0 => 5,
                v => v,
            },
        },
        PoolPair {
            name: "p_dblneg",
            fwd_src: format!("def p_dblneg_f(x):\n{STD_GUARD}    return 0 - x - x\n"),
            rex_src: None,
            inv_src: "def p_dblneg_i(y):\n    return 0 - (y // 2)\n".into(),
            fwd: |x| -2 * x,
            rex: |_| None,
            inv: |y, _| -(y.div_euclid(2)),
            domain_ok: std_domain,
            // image = nonzero even numbers
            edit: |d| {
                let k = (d % 30_000) as i64 + 1;
                if d & 1 == 0 { 2 * k } else { -2 * k }
            },
        },
    ]
}

fn nz(v: i64) -> i64 {
    if v == 0 { 17 } else { v }
}

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

/// THE chain test, and the reason "reverse that carries edits" is worth more
/// than a round trip: deterministic random data, 12 randomly-chosen transforms
/// (from a pool of 11 distinct pairs) applied output-into-input, the column
/// asserted to CHANGE at every step and to match a Rust-mirror model exactly;
/// then unwound strictly LIFO with an EDIT injected before every putback —
/// each edit must survive the remaining unwinds TRANSFORMED by every inverse
/// below it, and the mirror model is the oracle at every depth. At the bottom:
/// unedited rows are byte-original, edited rows are exactly the composed
/// carry-down of their edits.
#[test]
fn a_random_chain_of_twelve_transforms_unwinds_with_edits_carried() {
    let (d, path) = db("chain");
    let pool = pool();
    assert!(pool.len() >= 10, "the spec says at least 10 different transforms");
    for p in &pool {
        def(&d, &p.fwd_src);
        if let Some(r) = &p.rex_src {
            def(&d, r);
        }
        def(&d, &p.inv_src);
        let f = format!("{}_f", p.name);
        let i = format!("{}_i", p.name);
        match &p.rex_src {
            Some(_) => {
                let r = format!("{}_r", p.name);
                d.create_residual_lens(p.name, &f, &r, &i, ColumnType::Any).unwrap();
            }
            None => {
                d.create_lens(p.name, &f, &i, LensClass::Bijective).unwrap();
            }
        }
    }

    // Random-but-deterministic data (house rule: xorshift, no rand dep).
    let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
    let n_rows = 64usize;
    let mut model: Vec<i64> = (0..n_rows)
        .map(|_| nz((xorshift(&mut rng) % 2000) as i64 - 1000))
        .collect();
    let mut s = d.begin().unwrap();
    for (i, v) in model.iter().enumerate() {
        s.query(
            "INSERT INTO t (id, v, w) VALUES ($1, $2, 0)",
            &[Value::Int(i as i64), Value::Int(*v)],
        )
        .unwrap();
    }
    s.commit().unwrap();
    let original = model.clone();

    // UP: 12 applies. The rng proposes a pair; the harness takes the first
    // ADMISSIBLE one from there (every value in the pair's forward domain,
    // and the transform actually changes something) — a refused row aborts a
    // run by design, so the chain must only pick composable steps.
    let depth = 12usize;
    let mut stack: Vec<(usize, i64, Vec<Option<i64>>)> = Vec::new();
    let mut used: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for step in 0..depth {
        let start = (xorshift(&mut rng) as usize) % pool.len();
        let pick = (0..pool.len())
            .map(|k| (start + k) % pool.len())
            .find(|&pi| {
                let p = &pool[pi];
                model.iter().all(|x| (p.domain_ok)(*x))
                    && model.iter().any(|x| (p.fwd)(*x) != *x)
            })
            .expect("no admissible pair — the pool should always offer one");
        let p = &pool[pick];
        used.insert(pick);
        let before = col_v(&d);
        let report = d.rretl_apply(p.name, "t", "v").unwrap();
        let residuals: Vec<Option<i64>> = model.iter().map(|x| (p.rex)(*x)).collect();
        for x in model.iter_mut() {
            *x = (p.fwd)(*x);
        }
        let after = col_v(&d);
        assert_eq!(after, model, "step {step}: engine and model disagree after {}", p.name);
        assert_ne!(after, before, "step {step}: {} must CHANGE the column", p.name);
        stack.push((pick, report.run_id, residuals));
    }
    assert!(used.len() >= 5, "the rng should exercise pair variety, got {}", used.len());

    // DOWN: before every putback, one legal edit — which must survive the
    // remaining unwinds TRANSFORMED. The model mirrors both. An edit is
    // skipped when its carry-down would leave any lower layer's domain (the
    // model predicts that exactly, so no engine refusal is ever provoked).
    let mut edits_made = 0usize;
    while let Some((pick, run_id, residuals)) = stack.pop() {
        let p = &pool[pick];
        let mut row = (xorshift(&mut rng) as usize) % n_rows;
        if p.name == "p_clamp" {
            let mut tries = 0;
            while residuals[row].is_some() && tries < n_rows {
                row = (row + 1) % n_rows;
                tries += 1;
            }
            if residuals[row].is_some() {
                row = usize::MAX;
            }
        }
        if row != usize::MAX {
            let new_y = (p.edit)(xorshift(&mut rng));
            // Exact PutRes prediction via the mirrors, at EVERY layer the
            // edit will pass through on the way down: the carried value must
            // be in each layer's forward IMAGE for its stored residual
            // (fwd(inv(y, r)) == y and rex(inv) == r), and inside the domain
            // guards. Checking only the domain was this probe's first draft,
            // and an odd value carried into the double-negate layer proved it
            // insufficient — image membership is the real constraint.
            let legal = |lp: &PoolPair, y: i64, r: Option<i64>| -> Option<i64> {
                let x = (lp.inv)(y, r);
                ((lp.domain_ok)(x) && (lp.fwd)(x) == y && (lp.rex)(x) == r).then_some(x)
            };
            let mut ok = true;
            let mut probe = match legal(p, new_y, residuals[row]) {
                Some(x) => x,
                None => {
                    ok = false;
                    0
                }
            };
            if ok {
                for &(lpick, _, ref lres) in stack.iter().rev() {
                    match legal(&pool[lpick], probe, lres[row]) {
                        Some(x) => probe = x,
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
            }
            if ok {
                let mut s = d.begin().unwrap();
                s.query(
                    "UPDATE t SET v = $1 WHERE id = $2",
                    &[Value::Int(new_y), Value::Int(row as i64)],
                )
                .unwrap();
                s.commit().unwrap();
                model[row] = new_y;
                edits_made += 1;
            }
        }
        d.rretl_putback(run_id).unwrap();
        for (x, r) in model.iter_mut().zip(residuals.iter()) {
            *x = (p.inv)(*x, *r);
        }
        assert_eq!(
            col_v(&d),
            model,
            "unwinding {} (run {run_id}): engine and model disagree",
            p.name
        );
    }

    // Fully unwound: unedited rows byte-original, edited rows the composed
    // carry-down — and at least one of each, so the test cannot silently
    // degenerate into a pure round trip.
    let final_col = col_v(&d);
    assert_eq!(final_col, model, "final state matches the model");
    assert!(edits_made >= 3, "the run must actually inject edits, got {edits_made}");
    let changed = final_col.iter().zip(original.iter()).filter(|(a, b)| a != b).count();
    assert!(changed > 0, "at least one edit must survive all the way down");
    assert!(changed < n_rows, "unedited rows must be byte-original");

    let _ = std::fs::remove_file(&path);
}

/// Adversarial-check finding 20, closed: a pre-existing user table that
/// merely SHARES a bookkeeping table's name is refused by name, up front —
/// never written into. Both bookkeeping names are guarded.
#[test]
fn a_user_table_named_like_the_bookkeeping_is_refused() {
    let (d, path) = db("shape");
    define_abs_pair(&d);
    seed(&d, &[-1]);
    let mut s = d.begin().unwrap();
    s.query("CREATE TABLE rretl_lineage (whatever INTEGER PRIMARY KEY, x TEXT)", &[])
        .unwrap();
    s.commit().unwrap();

    let e = d.rretl_apply("mag", "t", "v").unwrap_err().to_string();
    assert!(e.contains("NOT rretl's bookkeeping table"), "{e}");
    assert!(e.contains("whatever"), "the imposter's columns are named: {e}");
    // And nothing was written into the imposter:
    let rows_in = rows(d.query("SELECT count(*) FROM rretl_lineage", &[]).unwrap());
    assert_eq!(rows_in[0][0], Value::Int(0));
    let _ = std::fs::remove_file(&path);
}

/// `rretl_fsck` — verify-at-rest. Clean after an apply; a tampered top-run
/// column and a deleted residual row are each reported (not repaired); a
/// buried run's non-matching column hash is NOT a finding (later runs
/// legitimately transformed it); after revert everything is clean again.
#[test]
fn fsck_reports_tampering_and_missing_residuals_and_nothing_else() {
    let (d, path) = db("fsck");
    define_abs_pair(&d);
    def(&d, "def flip(x):\n    return -x\n");
    d.create_lens("flip", "flip", "flip", LensClass::Bijective).unwrap();
    seed(&d, &[-5, 3]);

    let run1 = d.rretl_apply("mag", "t", "v").unwrap();
    assert!(d.rretl_fsck().unwrap().is_empty(), "clean after apply");

    // Stack a second run: run1 is now buried, and its column-hash mismatch
    // must NOT be reported.
    let run2 = d.rretl_apply("flip", "t", "v").unwrap();
    assert!(d.rretl_fsck().unwrap().is_empty(), "a buried run is not a finding");

    // Tamper with the live column: the TOP run (run2) is flagged.
    let mut s = d.begin().unwrap();
    s.query("UPDATE t SET v = 999 WHERE id = 0", &[]).unwrap();
    s.commit().unwrap();
    let findings = d.rretl_fsck().unwrap();
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(findings[0].contains("edited outside the pipeline"), "{}", findings[0]);
    assert!(findings[0].contains(&format!("run {}", run2.run_id)), "{}", findings[0]);

    // Un-tamper (putback run2 accepts the edit; that empties the stack top).
    d.rretl_putback(run2.run_id).unwrap();

    // Now delete one of run1's residual rows: fsck names the row.
    let mut s = d.begin().unwrap();
    s.query(
        "DELETE FROM rretl_residual WHERE run_id = $1 AND pk_enc = $2",
        &[Value::Int(run1.run_id), Value::Blob(vec![2, 1, 0, 0, 0, 0, 0, 0, 0])],
    )
    .unwrap();
    s.commit().unwrap();
    let findings = d.rretl_fsck().unwrap();
    assert!(
        findings.iter().any(|f| f.contains("NO residual")),
        "the missing residual is reported: {findings:?}"
    );

    let _ = std::fs::remove_file(&path);
}

/// The guide's SIMPLE setup, executed verbatim (PYSPELL-RRETL.md §1): a config
/// with NO `[[table]]` blocks, the working table created live with
/// `CREATE TABLE … ANY`, then the full apply → edit → putback loop on it.
#[test]
fn the_guides_zero_table_setup_carries_a_full_rretl_loop() {
    let path = format!(
        "{}/rretl-simple-{}.mpedb",
        mpedb_testkit::scratch_base_str(),
        std::process::id()
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 32\nmax_readers = 8\n\
         durability = \"none\"\n"
    );
    let d = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();

    d.query("CREATE TABLE pixels (id INTEGER PRIMARY KEY, px ANY)", &[]).unwrap();
    let mut s = d.begin().unwrap();
    for (i, v) in [-7i64, 4, -1].iter().enumerate() {
        s.query(
            "INSERT INTO pixels (id, px) VALUES ($1, $2)",
            &[Value::Int(i as i64), Value::Int(*v)],
        )
        .unwrap();
    }
    s.commit().unwrap();

    define_abs_pair(&d);
    let run = d.rretl_apply("mag", "pixels", "px").unwrap();
    assert_eq!(run.residuals, 3);

    // Edit one magnitude, then putback: the edit rides the stored sign home.
    let mut s = d.begin().unwrap();
    s.query("UPDATE pixels SET px = 9 WHERE id = 0", &[]).unwrap();
    s.commit().unwrap();
    d.rretl_putback(run.run_id).unwrap();

    let got: Vec<i64> = rows(d.query("SELECT px FROM pixels ORDER BY id", &[]).unwrap())
        .into_iter()
        .map(|r| match &r[0] {
            Value::Int(i) => *i,
            other => panic!("expected int, got {other:?}"),
        })
        .collect();
    assert_eq!(got, vec![-9, 4, -1], "edit carried back through the sign residual");
    let _ = std::fs::remove_file(&path);
}

/// A tampered residual VALUE is the putback attack the residual gate exists
/// for: with mag ⇄ sgn, flipping a stored sign bit survives BOTH PutRes
/// halves (forward(inverse(y, r')) == y and rex(x') == r' both hold), so
/// before the gate, putback would silently restore a sign the user never
/// flipped. Now the run's residual set must hash to what the apply wrote.
#[test]
fn a_tampered_residual_is_refused_by_revert_putback_and_named_by_fsck() {
    let (d, path) = db("resgate");
    define_abs_pair(&d);
    seed(&d, &[-5, 3]);
    let run = d.rretl_apply("mag", "t", "v").unwrap();
    assert!(d.rretl_fsck().unwrap().is_empty());

    // Flip every stored sign to 0 — row 0's residual changes 1 -> 0.
    let mut s = d.begin().unwrap();
    s.query(
        "UPDATE rretl_residual SET residual = $1 WHERE run_id = $2",
        &[Value::Int(0), Value::Int(run.run_id)],
    )
    .unwrap();
    s.commit().unwrap();

    let e = d.rretl_putback(run.run_id).unwrap_err().to_string();
    assert!(e.contains("no longer hash") && e.contains("putback"), "{e}");
    let e = d.rretl_revert(run.run_id).unwrap_err().to_string();
    assert!(e.contains("no longer hash") && e.contains("reverting"), "{e}");
    let findings = d.rretl_fsck().unwrap();
    assert!(
        findings.iter().any(|f| f.contains(&format!("run {}", run.run_id))
            && f.contains("no longer hash")),
        "{findings:?}"
    );
    // The column itself is untouched by all three refusals.
    assert_eq!(col_v(&d), vec![5, 3]);
    let _ = std::fs::remove_file(&path);
}

/// A BURIED run's residuals are now verified at rest: fsck re-hashes every
/// standing run's residual set against the chain its apply recorded — the
/// buried run needs no oracle for its (long transformed away) column, but
/// its residuals are still its own.
#[test]
fn fsck_names_a_buried_runs_tampered_residuals() {
    let (d, path) = db("buried");
    define_abs_pair(&d);
    def(&d, "def flip(x):\n    return -x\n");
    d.create_lens("flip", "flip", "flip", LensClass::Bijective).unwrap();
    seed(&d, &[-5, 3]);

    let run1 = d.rretl_apply("mag", "t", "v").unwrap();
    let run2 = d.rretl_apply("flip", "t", "v").unwrap();
    assert!(d.rretl_fsck().unwrap().is_empty(), "buried but untampered = clean");

    let mut s = d.begin().unwrap();
    s.query(
        "UPDATE rretl_residual SET residual = $1 WHERE run_id = $2",
        &[Value::Int(0), Value::Int(run1.run_id)],
    )
    .unwrap();
    s.commit().unwrap();

    let findings = d.rretl_fsck().unwrap();
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(findings[0].contains(&format!("run {}", run1.run_id)), "{}", findings[0]);
    assert!(findings[0].contains("no longer hash"), "{}", findings[0]);

    // The LIFO unwind hits the gate at the right moment too: run2 (bijective,
    // no residuals) reverts fine; run1 then refuses.
    d.rretl_revert(run2.run_id).unwrap();
    let e = d.rretl_revert(run1.run_id).unwrap_err().to_string();
    assert!(e.contains("no longer hash"), "{e}");
    let _ = std::fs::remove_file(&path);
}

/// The streaming scans cross chunk boundaries without skipping, repeating or
/// reordering a row: with the chunk forced tiny, a 100-row apply → edit →
/// putback → revert loop must land byte-exact. Duplicates included — resume
/// is `pk > last` on the PK, not on values.
#[test]
fn scans_cross_chunk_boundaries_exactly() {
    // Chunk 7 makes every pass straddle many uneven boundaries. The variable
    // is read per call, and a different chunk size never changes RESULTS for
    // any concurrently running test — only how many rows each fetch carries.
    std::env::set_var("MPEDB_RRETL_CHUNK", "7");
    let (d, path) = db("chunky");
    define_abs_pair(&d);
    let vals: Vec<i64> = (0..100).map(|i| ((i % 13) - 6) * (1 + (i % 3))).collect();
    seed(&d, &vals);

    let run = d.rretl_apply("mag", "t", "v").unwrap();
    let want: Vec<i64> = vals.iter().map(|v| v.abs()).collect();
    assert_eq!(col_v(&d), want, "forward across boundaries");

    // Edit one magnitude mid-table, putback: the edit rides the sign home.
    let mut s = d.begin().unwrap();
    s.query("UPDATE t SET v = 99 WHERE id = 50", &[]).unwrap();
    s.commit().unwrap();
    d.rretl_putback(run.run_id).unwrap();
    let mut expect = vals.clone();
    expect[50] = if vals[50] < 0 { -99 } else { 99 };
    assert_eq!(col_v(&d), expect, "putback across boundaries, edit carried");

    // A fresh apply + exact revert over the same tiny chunks.
    let run2 = d.rretl_apply("mag", "t", "v").unwrap();
    d.rretl_revert(run2.run_id).unwrap();
    assert_eq!(col_v(&d), expect, "revert across boundaries is exact");
    assert!(d.rretl_fsck().unwrap().is_empty());
    std::env::remove_var("MPEDB_RRETL_CHUNK");
    let _ = std::fs::remove_file(&path);
}

/// Adversarial resume keys: TEXT PKs where one value is a byte-PREFIX of
/// another (`"a"`, `"aa"`, `"ab"`). The chunk resume (`pk > last`) and the
/// residual chain both live on keycode's prefix-freedom per column — with a
/// tiny chunk, a full apply → putback → revert loop must stay byte-exact.
#[test]
fn text_pks_that_prefix_each_other_survive_chunked_scans() {
    std::env::set_var("MPEDB_RRETL_CHUNK", "2");
    let path = format!(
        "{}/rretl-textpk-{}.mpedb",
        mpedb_testkit::scratch_base_str(),
        std::process::id()
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 32\nmax_readers = 8\n\
         durability = \"none\"\n"
    );
    let d = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    d.query("CREATE TABLE t2 (k TEXT PRIMARY KEY, v ANY)", &[]).unwrap();
    let keys = ["a", "aa", "ab", "aaa", "b", "ba"];
    let vals = [-3i64, 4, -1, 7, -9, 2];
    let mut s = d.begin().unwrap();
    for (k, v) in keys.iter().zip(vals.iter()) {
        s.query(
            "INSERT INTO t2 (k, v) VALUES ($1, $2)",
            &[Value::Text((*k).into()), Value::Int(*v)],
        )
        .unwrap();
    }
    s.commit().unwrap();
    define_abs_pair(&d);

    let run = d.rretl_apply("mag", "t2", "v").unwrap();
    let got = |d: &Database| -> Vec<i64> {
        rows(d.query("SELECT v FROM t2 ORDER BY k", &[]).unwrap())
            .into_iter()
            .map(|r| match &r[0] {
                Value::Int(i) => *i,
                other => panic!("{other:?}"),
            })
            .collect()
    };
    // ORDER BY k: a, aa, aaa, ab, b, ba -> |v| in that key order.
    assert_eq!(got(&d), vec![3, 4, 7, 1, 9, 2]);
    d.rretl_revert(run.run_id).unwrap();
    assert_eq!(got(&d), vec![-3, 4, 7, -1, -9, 2]);
    assert!(d.rretl_fsck().unwrap().is_empty());
    std::env::remove_var("MPEDB_RRETL_CHUNK");
    let _ = std::fs::remove_file(&path);
}
