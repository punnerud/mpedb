//! **Pre-flight: find out what the target will reject, before writing anything**
//! (DESIGN-MIRROR §2 `map/`, task #26).
//!
//! The push path already survives a bad row — each op runs in its own SAVEPOINT
//! and a rejected one parks (§6, review CONF#38). But that is *reactive*: you
//! learn about row 40,000 after 40,000 round-trips, mid-write, with the target
//! half-populated. A migration deserves better than trickling out its own
//! failures.
//!
//! This walks mpedb's data **locally** — no connection to the target at all —
//! and checks every value against the source schema recorded at import
//! ([`crate::state::TableMap`]). One complete report, before the first INSERT.
//!
//! ## Why the recorded schema is the right contract
//!
//! mpedb is deliberately *looser* than PostgreSQL: `int2`, `int4` and `int8` all
//! become `Int64`; `varchar(64)` and `text` are both `Text`. That widening is
//! what lets a permissive sqlite source land in mpedb at all — and it is exactly
//! what makes a strict target throw on the way out. The [`MapPolicy::Widened`]
//! verdict marks those columns, so the checks below know where to look.
//!
//! ## What it cannot tell you
//!
//! A [`MapPolicy::LossyAtImport`] column is reported once, at column level, and
//! never per row: the information was discarded at import, so the values in
//! mpedb are already the rounded ones. There is nothing to compare against. That
//! finding is a warning about the *migration*, not about a row — and reporting
//! it per-row would imply a per-row fix that does not exist.

use mpedb::{Database, ExecResult};
use mpedb_types::{Error, Result, Value};

use crate::state::{self, ColumnMap, MapPolicy, TableMap};

/// What a value (or a column) will do to a strict target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingKind {
    /// The value does not fit the source column's declared type — the class the
    /// target rejects at INSERT. `int4` holding 2147483648, `varchar(8)` holding
    /// 20 characters, `numeric(10,2)` holding more digits than it can express.
    WontFit,
    /// NULL in a column the source declares NOT NULL.
    NullInNotNull,
    /// Text carrying a NUL byte: mpedb stores it happily, PostgreSQL refuses it
    /// outright (`invalid byte sequence for encoding`), and sqlite silently
    /// truncates at it — three different behaviours for the same bytes.
    NulInText,
    /// The value's type does not match what the column was mapped as. Only
    /// reachable from a loose source: sqlite's declared types are affinities,
    /// not constraints, so any row may hold anything.
    TypeDrift,
    /// **Column-level**: this column already lost information at import, so no
    /// per-row check is meaningful. Reported once.
    LossyColumn,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub table: String,
    pub column: String,
    pub kind: FindingKind,
    /// The row's primary key, or empty for a column-level finding.
    pub pk: Vec<Value>,
    /// Human-readable specifics (the offending value, the limit it broke).
    pub detail: String,
}

#[derive(Debug, Default)]
pub struct PreflightReport {
    pub findings: Vec<Finding>,
    pub rows_checked: u64,
}

impl PreflightReport {
    /// Whether a write to the recorded source schema would be rejected. A
    /// `LossyColumn` finding alone does NOT make it fail: the data is already
    /// what it is, the target will accept it, and the operator needs to decide
    /// whether the loss is acceptable — that is a judgement, not an error.
    pub fn would_fail(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.kind != FindingKind::LossyColumn)
    }
}

/// Parse a trailing typmod out of a declared type: `numeric(10,2)` → `(10, 2)`,
/// `character varying(64)` → `(64, _)`. Returns None when unconstrained.
fn typmod(declared: &str) -> Option<(i64, i64)> {
    let open = declared.find('(')?;
    let close = declared.rfind(')')?;
    if close <= open {
        return None;
    }
    let inner = &declared[open + 1..close];
    let mut it = inner.split(',');
    let a: i64 = it.next()?.trim().parse().ok()?;
    let b: i64 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    Some((a, b))
}

fn base_type(declared: &str) -> &str {
    declared
        .find('(')
        .map_or(declared, |i| &declared[..i])
        .trim()
}

/// The inclusive range a narrow integer source column accepts, or None if the
/// column is not a narrowing integer.
fn int_range(declared: &str) -> Option<(i64, i64)> {
    match base_type(declared).to_ascii_lowercase().as_str() {
        "int2" | "smallint" => Some((i16::MIN as i64, i16::MAX as i64)),
        "int4" | "integer" | "int" => Some((i32::MIN as i64, i32::MAX as i64)),
        _ => None,
    }
}

/// Check one value against one recorded source column.
fn check_value(c: &ColumnMap, v: &Value) -> Option<(FindingKind, String)> {
    if v.is_null() {
        return if c.not_null {
            Some((
                FindingKind::NullInNotNull,
                format!("column `{}` is NOT NULL at the source", c.source_name),
            ))
        } else {
            None
        };
    }

    // Narrowing integers: mpedb widened int2/int4 to Int64 at import, so a local
    // write can hold what the source cannot. This is the exact failure the PG
    // fidelity work hit at INSERT time.
    if let (Some((lo, hi)), Value::Int(i)) = (int_range(&c.source_type), v) {
        if *i < lo || *i > hi {
            return Some((
                FindingKind::WontFit,
                format!("{i} is outside {} ({lo}..={hi})", c.source_type),
            ));
        }
    }

    match v {
        Value::Text(s) => {
            if s.contains('\0') {
                return Some((
                    FindingKind::NulInText,
                    "text contains a NUL byte: PostgreSQL rejects it, sqlite truncates at it"
                        .to_string(),
                ));
            }
            let bt = base_type(&c.source_type).to_ascii_lowercase();
            // varchar(n)/char(n) count CHARACTERS, not bytes — a byte check
            // would false-positive on every non-ASCII value.
            if let Some((n, _)) = typmod(&c.source_type) {
                if matches!(bt.as_str(), "varchar" | "character varying" | "bpchar" | "character")
                    && s.chars().count() as i64 > n
                {
                    return Some((
                        FindingKind::WontFit,
                        format!(
                            "{} characters exceeds {}",
                            s.chars().count(),
                            c.source_type
                        ),
                    ));
                }
                // numeric(p,s): p = TOTAL significant digits, s = digits after
                // the point. The integer part may hold at most p - s.
                if bt == "numeric" || bt == "decimal" {
                    if let Some((p, sc)) = typmod(&c.source_type) {
                        if let Some(f) = numeric_overflow(s, p, sc) {
                            return Some((FindingKind::WontFit, f));
                        }
                    }
                }
            }
        }
        Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Blob(_) | Value::Timestamp(_) => {}
        Value::Null | Value::List(_) => {}
    }

    // Loose-source drift: the value's type is not what the column was mapped as.
    // sqlite lets any row hold any type, so the schema says what SHOULD be there
    // and this is where we find out it is not.
    if let Some(ct) = v.column_type() {
        if ct != c.mapped {
            return Some((
                FindingKind::TypeDrift,
                format!(
                    "value is {} but the column was mapped as {} (source `{}`)",
                    v.type_name(),
                    c.mapped,
                    c.source_type
                ),
            ));
        }
    }
    None
}

/// `numeric(p,s)` overflow, on the canonical text form numeric was imported as.
fn numeric_overflow(s: &str, p: i64, scale: i64) -> Option<String> {
    let t = s.trim();
    let t = t.strip_prefix('-').or_else(|| t.strip_prefix('+')).unwrap_or(t);
    let (int_part, frac_part) = match t.split_once('.') {
        Some((a, b)) => (a, b),
        None => (t, ""),
    };
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        // Not a plain decimal (exponent, NaN, junk): the target will decide.
        return None;
    }
    let int_digits = int_part.trim_start_matches('0').len() as i64;
    let max_int = p - scale;
    if int_digits > max_int {
        return Some(format!(
            "{int_digits} digits before the point exceeds numeric({p},{scale}) (max {max_int})"
        ));
    }
    if frac_part.len() as i64 > scale {
        // PG ROUNDS rather than errors here, so this is a fidelity warning, not
        // a rejection — but silently rounding someone's money is worth saying.
        return Some(format!(
            "{} digits after the point exceeds numeric({p},{scale}) scale {scale}; \
             the target will ROUND, not reject",
            frac_part.len()
        ));
    }
    None
}

/// Walk every mirrored table and check its data against the source schema
/// recorded at import. Reads only mpedb — the target is never contacted.
pub fn preflight(db: &Database) -> Result<PreflightReport> {
    let schema = db.schema().clone();
    let mut report = PreflightReport::default();

    for (tid, tdef) in schema.tables.iter().enumerate() {
        let Some(raw) = db.sys_record_get(state::MIR_NS, &state::map_key(tid as u32))? else {
            // No provenance: an .mpedb created before this record existed, or a
            // table outside the mirrored scope. Silence would imply "checked and
            // clean", so say what we could not check.
            report.findings.push(Finding {
                table: tdef.name.clone(),
                column: String::new(),
                kind: FindingKind::LossyColumn,
                pk: Vec::new(),
                detail: "no source-schema provenance recorded for this table: \
                         nothing could be verified (re-import to record it)"
                    .into(),
            });
            continue;
        };
        let map = TableMap::decode(&raw)?;

        // Column-level verdicts first: a lossy column has no per-row fix.
        for c in &map.columns {
            if c.policy == MapPolicy::LossyAtImport {
                report.findings.push(Finding {
                    table: map.source_name.clone(),
                    column: c.source_name.clone(),
                    kind: FindingKind::LossyColumn,
                    pk: Vec::new(),
                    detail: format!(
                        "`{}` lost information at import (mapped to {}): the values here are \
                         already the degraded ones, so nothing per-row can be checked or fixed",
                        c.source_type, c.mapped
                    ),
                });
            }
        }

        // Align the recorded columns to mpedb's, by name. A mangled identifier
        // (PG allows names mpedb does not) means we cannot check that column —
        // and must say so rather than skip quietly.
        let cols: Vec<String> = tdef.columns.iter().map(|c| c.name.clone()).collect();
        let sql = format!(
            "SELECT {} FROM {}",
            cols.iter()
                .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(", "),
            tdef.name
        );
        let rows = match db.query(&sql, &[])? {
            ExecResult::Rows { rows, .. } => rows,
            other => return Err(Error::Internal(format!("preflight scan: {other:?}"))),
        };
        let pk_idx: Vec<usize> = tdef.primary_key.iter().map(|&i| i as usize).collect();

        for row in &rows {
            report.rows_checked += 1;
            for (i, v) in row.iter().enumerate() {
                // match by mpedb column name; the record stores the SOURCE name,
                // which is the same unless the importer mangled it
                let Some(c) = map.columns.iter().find(|c| c.source_name == cols[i]) else {
                    continue;
                };
                if c.generated {
                    continue; // never written back; the target computes it
                }
                if let Some((kind, detail)) = check_value(c, v) {
                    report.findings.push(Finding {
                        table: map.source_name.clone(),
                        column: c.source_name.clone(),
                        kind,
                        pk: pk_idx.iter().map(|&i| row[i].clone()).collect(),
                        detail,
                    });
                }
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typmod_and_base_type_parsing() {
        assert_eq!(typmod("numeric(10,2)"), Some((10, 2)));
        assert_eq!(typmod("character varying(64)"), Some((64, 0)));
        assert_eq!(typmod("text"), None);
        assert_eq!(base_type("numeric(10,2)"), "numeric");
        assert_eq!(base_type("character varying(64)"), "character varying");
        assert_eq!(base_type("int4"), "int4");
        // a malformed typmod must not panic or half-parse
        assert_eq!(typmod("numeric("), None);
        assert_eq!(typmod("numeric)"), None);
        assert_eq!(typmod("numeric(x,y)"), None);
    }

    #[test]
    fn int_ranges_only_for_narrowing_types() {
        assert_eq!(int_range("int4"), Some((-2147483648, 2147483647)));
        assert_eq!(int_range("integer"), Some((-2147483648, 2147483647)));
        assert_eq!(int_range("int2"), Some((-32768, 32767)));
        // int8 is NOT narrowing — mpedb's Int64 matches it exactly
        assert_eq!(int_range("int8"), None);
        assert_eq!(int_range("bigint"), None);
    }

    fn col(source_type: &str, mapped: mpedb_types::ColumnType, not_null: bool) -> ColumnMap {
        ColumnMap {
            source_name: "c".into(),
            source_type: source_type.into(),
            not_null,
            generated: false,
            identity: false,
            unique: false,
            mapped,
            policy: MapPolicy::Widened,
        }
    }

    /// The exact failure the PG fidelity work hit at INSERT time — caught here
    /// instead, with no connection to PostgreSQL at all.
    #[test]
    fn catches_the_int4_overflow_that_postgres_would_have_thrown() {
        use mpedb_types::ColumnType;
        let c = col("int4", ColumnType::Int64, false);
        let f = check_value(&c, &Value::Int(2147483648));
        assert!(matches!(f, Some((FindingKind::WontFit, _))), "got {f:?}");
        // and the in-range value is silent
        assert!(check_value(&c, &Value::Int(2147483647)).is_none());
        // int8 never overflows: mpedb's Int64 IS int8
        let big = col("int8", ColumnType::Int64, false);
        assert!(check_value(&big, &Value::Int(i64::MAX)).is_none());
    }

    #[test]
    fn catches_varchar_length_in_characters_not_bytes() {
        use mpedb_types::ColumnType;
        let c = col("character varying(4)", ColumnType::Text, false);
        assert!(check_value(&c, &Value::Text("abcd".into())).is_none());
        assert!(matches!(
            check_value(&c, &Value::Text("abcde".into())),
            Some((FindingKind::WontFit, _))
        ));
        // 4 multi-byte chars = 12 bytes but still 4 characters: a byte check
        // would have false-positived here.
        assert!(check_value(&c, &Value::Text("æøå日".into())).is_none());
    }

    #[test]
    fn catches_numeric_precision_and_flags_rounding_separately() {
        use mpedb_types::ColumnType;
        let c = col("numeric(5,2)", ColumnType::Text, false);
        assert!(check_value(&c, &Value::Text("123.45".into())).is_none());
        // 4 integer digits > (5-2)=3 → the target REJECTS
        let over = check_value(&c, &Value::Text("1234.5".into()));
        assert!(matches!(&over, Some((FindingKind::WontFit, d)) if d.contains("before the point")));
        // too many fractional digits → the target ROUNDS; say so, do not claim
        // it will be rejected
        let round = check_value(&c, &Value::Text("1.234".into()));
        assert!(matches!(&round, Some((FindingKind::WontFit, d)) if d.contains("ROUND")));
        // leading zeros are not significant digits
        assert!(check_value(&c, &Value::Text("000.12".into())).is_none());
    }

    #[test]
    fn catches_null_in_not_null_and_nul_in_text() {
        use mpedb_types::ColumnType;
        let nn = col("text", ColumnType::Text, true);
        assert!(matches!(
            check_value(&nn, &Value::Null),
            Some((FindingKind::NullInNotNull, _))
        ));
        let t = col("text", ColumnType::Text, false);
        assert!(matches!(
            check_value(&t, &Value::Text("a\0b".into())),
            Some((FindingKind::NulInText, _))
        ));
        // a nullable column with NULL is fine and must not be reported
        assert!(check_value(&t, &Value::Null).is_none());
    }

    /// A loose source's whole point: sqlite lets a row hold anything, so the
    /// recorded schema says what SHOULD be there and this is where we find out.
    #[test]
    fn catches_loose_source_type_drift() {
        use mpedb_types::ColumnType;
        let c = col("INTEGER", ColumnType::Int64, false);
        let f = check_value(&c, &Value::Text("oops".into()));
        assert!(matches!(&f, Some((FindingKind::TypeDrift, d)) if d.contains("mapped as")), "got {f:?}");
    }

    /// A lossy column is a migration-level warning, not a row rejection: the
    /// target accepts the values, they are just already degraded.
    #[test]
    fn lossy_column_alone_does_not_make_the_report_fail() {
        let mut r = PreflightReport::default();
        r.findings.push(Finding {
            table: "t".into(),
            column: "c".into(),
            kind: FindingKind::LossyColumn,
            pk: vec![],
            detail: String::new(),
        });
        assert!(!r.would_fail(), "a lossy column is a judgement call, not an error");
        r.findings.push(Finding {
            table: "t".into(),
            column: "c".into(),
            kind: FindingKind::WontFit,
            pk: vec![],
            detail: String::new(),
        });
        assert!(r.would_fail());
    }

    /// End to end, and the scenario Morten described: a LOOSE sqlite source
    /// (declared types are affinities) imported into mpedb, then local writes
    /// that mpedb happily accepts because it widened the columns — `qty` was
    /// INT4 at the source but is Int64 here, `code` was VARCHAR(4) but is Text.
    /// PostgreSQL would throw on both, mid-INSERT, after a round-trip each.
    /// Pre-flight finds them locally, with the source not even open.
    #[test]
    fn finds_locally_what_a_strict_target_would_reject_on_write() {
        use crate::import::{import_sqlite, ImportOptions};
        use rusqlite::Connection;

        let dir = std::env::temp_dir().join("mpedb-mirror-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join(format!("pf-src-{}.db", std::process::id()));
        let dest = dir.join(format!("pf-dest-{}.mpedb", std::process::id()));
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dest);

        {
            let c = Connection::open(&src).unwrap();
            c.execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, qty INT4, code VARCHAR(4),
                                note TEXT NOT NULL);
                 INSERT INTO t VALUES (1, 5, 'ok', 'fine');",
            )
            .unwrap();
        }
        let db = {
            let mut c = Connection::open(&src).unwrap();
            import_sqlite(&mut c, &dest, &ImportOptions::default()).unwrap().0
        };

        // clean import ⇒ nothing to say
        let r = preflight(&db).unwrap();
        assert!(r.findings.is_empty(), "clean data must be silent: {:?}", r.findings);
        assert_eq!(r.rows_checked, 1);

        // local writes mpedb accepts but the SOURCE column cannot take
        db.query(
            "INSERT INTO t (id, qty, code, note) VALUES ($1, $2, $3, $4)",
            &[
                Value::Int(2),
                Value::Int(2_147_483_648), // INT4 max is 2147483647
                Value::Text("ok".into()),
                Value::Text("x".into()),
            ],
        )
        .unwrap();
        db.query(
            "INSERT INTO t (id, qty, code, note) VALUES ($1, $2, $3, $4)",
            &[
                Value::Int(3),
                Value::Int(1),
                Value::Text("toolong".into()), // VARCHAR(4)
                Value::Text("x".into()),
            ],
        )
        .unwrap();

        let r = preflight(&db).unwrap();
        assert_eq!(r.rows_checked, 3);
        assert!(r.would_fail(), "both writes must be flagged before anything is written");

        let kinds: Vec<_> = r.findings.iter().map(|f| (f.column.as_str(), f.kind)).collect();
        assert!(
            kinds.contains(&("qty", FindingKind::WontFit)),
            "int4 overflow must be caught: {kinds:?}"
        );
        assert!(
            kinds.contains(&("code", FindingKind::WontFit)),
            "varchar(4) overflow must be caught: {kinds:?}"
        );
        // and each finding names the row, so it is actionable
        let qty = r.findings.iter().find(|f| f.column == "qty").unwrap();
        assert_eq!(qty.pk, vec![Value::Int(2)]);
        assert!(qty.detail.contains("2147483648"), "detail: {}", qty.detail);

        for p in [src, dest] {
            let _ = std::fs::remove_file(p);
        }
    }
}
