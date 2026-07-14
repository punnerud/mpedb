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

use crate::state::{self, ColumnMap, MapPolicy, SourceKind, TableMap};

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
    /// What it would take to make this value fit (§26.3). `Fine` for column-level
    /// findings — an already-lossy column has no per-row coercion.
    pub fix: crate::adapt::Adaptation,
    /// The mpedb table id and column index, so an `--adapt` pass can write the
    /// coerced value back without re-deriving them from names.
    pub table_id: u32,
    pub col_idx: usize,
}

#[derive(Debug, Default)]
pub struct PreflightReport {
    pub findings: Vec<Finding>,
    pub rows_checked: u64,
}

impl PreflightReport {
    /// Findings a coercion would fix with nothing lost.
    pub fn adaptable_exact(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| matches!(f.fix, crate::adapt::Adaptation::Exact(_)))
            .count()
    }

    /// Findings a coercion would fix only by discarding something.
    pub fn adaptable_lossy(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| matches!(f.fix, crate::adapt::Adaptation::Lossy(..)))
            .count()
    }

    /// Findings no coercion can fix — the ones that end a migration.
    pub fn unfixable(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| matches!(f.fix, crate::adapt::Adaptation::Impossible(_)))
            .count()
    }

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
/// The integer range a declared type actually enforces **in its own dialect**.
///
/// The dialect argument is load-bearing, not decoration. `INTEGER` names a
/// 32-bit column in PostgreSQL and a *64-bit* one in sqlite, and `REAL` is
/// single precision in PostgreSQL and double in sqlite — the same word, a
/// different type. Reading a sqlite `INTEGER` with PG's meaning made preflight
/// reject 5_000_000_000, a value sqlite stores natively and would take back
/// without complaint, and that false rejection then blocked the export of
/// perfectly good data.
fn int_range(declared: &str, kind: SourceKind) -> Option<(i64, i64)> {
    // sqlite enforces no width at all: INTEGER holds a full i64, and a declared
    // type is an affinity rather than a constraint. Nothing to check.
    if kind == SourceKind::Sqlite {
        return None;
    }
    match base_type(declared).to_ascii_lowercase().as_str() {
        "int2" | "smallint" => Some((i16::MIN as i64, i16::MAX as i64)),
        "int4" | "integer" | "int" => Some((i32::MIN as i64, i32::MAX as i64)),
        _ => None,
    }
}

/// Check one value against one recorded source column, in `kind`'s dialect.
fn check_value(c: &ColumnMap, v: &Value, kind: SourceKind) -> Option<(FindingKind, String)> {
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
    if let (Some((lo, hi)), Value::Int(i)) = (int_range(&c.source_type, kind), v) {
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
            // Length and precision are declarations sqlite does not enforce:
            // `VARCHAR(8)` there happily holds 800 characters, and writing one
            // back is legal. Only a strict dialect can reject on these.
            if kind == SourceKind::Sqlite {
                return None;
            }
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

    // Which dialect the recorded declared types are written in. Without this
    // every check silently assumes PostgreSQL, and a sqlite mirror gets judged
    // by rules sqlite does not have.
    let src_kind = match db.sys_record_get(state::MIR_NS, state::KEY_CFG)? {
        Some(raw) => state::MirrorConfig::decode(&raw)?.source_kind,
        None => {
            return Err(Error::Config(
                "not a mirror (no mir/cfg record): preflight has no source schema to \
                 check against"
                    .into(),
            ))
        }
    };

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
                fix: crate::adapt::Adaptation::Fine,
                table_id: tid as u32,
                col_idx: 0,
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
                    fix: crate::adapt::Adaptation::Fine,
                    table_id: tid as u32,
                    col_idx: 0,
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
                if let Some((kind, detail)) = check_value(c, v, src_kind) {
                    report.findings.push(Finding {
                        table: map.source_name.clone(),
                        column: c.source_name.clone(),
                        kind,
                        pk: pk_idx.iter().map(|&i| row[i].clone()).collect(),
                        detail,
                        fix: crate::adapt::adapt(c, v),
                        table_id: tid as u32,
                        col_idx: i,
                    });
                }
            }
        }
    }
    Ok(report)
}

/// How much of a pre-flight's advice to actually act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdaptMode {
    /// Only coercions that lose nothing (`"42"` → `42`).
    ExactOnly,
    /// Also apply coercions that discard something (truncate to `varchar(n)`,
    /// drop a fractional part). Separate on purpose: this one destroys data, so
    /// it cannot ride along on the same flag as the safe ones.
    AllowLossy,
}

#[derive(Debug, Default)]
pub struct AdaptStats {
    pub applied_exact: u64,
    pub applied_lossy: u64,
    /// Left alone: no coercion exists, or it was lossy and not permitted.
    pub left: u64,
}

/// Rewrite the values a pre-flight says can be coerced, in mpedb, in place.
///
/// **Never called implicitly.** `preflight` reports; this acts, and only for the
/// verdicts `mode` permits. Everything else is left exactly as it was so it
/// still shows up in the next report rather than quietly disappearing.
///
/// Each row is updated by PK through the normal write path, so CHECK
/// constraints, RLS and change-capture all apply — an adaptation is an ordinary
/// local write, not a backdoor into the file.
pub fn apply_adaptations(db: &Database, mode: AdaptMode) -> Result<AdaptStats> {
    let report = preflight(db)?;
    let schema = db.schema().clone();
    let mut stats = AdaptStats::default();

    for f in &report.findings {
        let new = match (&f.fix, mode) {
            (crate::adapt::Adaptation::Exact(v), _) => {
                stats.applied_exact += 1;
                v.clone()
            }
            (crate::adapt::Adaptation::Lossy(v, _), AdaptMode::AllowLossy) => {
                stats.applied_lossy += 1;
                v.clone()
            }
            _ => {
                stats.left += 1;
                continue;
            }
        };
        let tdef = schema
            .tables
            .get(f.table_id as usize)
            .ok_or_else(|| Error::Internal("adapt: unknown table id".into()))?;
        let set_col = &tdef.columns[f.col_idx].name;
        let where_sql = tdef
            .primary_key
            .iter()
            .enumerate()
            .map(|(j, &i)| {
                format!(
                    "\"{}\" = ${}",
                    tdef.columns[i as usize].name.replace('"', "\"\""),
                    j + 2
                )
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        let sql = format!(
            "UPDATE {} SET \"{}\" = $1 WHERE {where_sql}",
            tdef.name,
            set_col.replace('"', "\"\"")
        );
        let mut params = vec![new];
        params.extend(f.pk.iter().cloned());
        db.query(&sql, &params)?;
    }
    Ok(stats)
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
        assert_eq!(int_range("int4", SourceKind::Postgres), Some((-2147483648, 2147483647)));
        assert_eq!(int_range("integer", SourceKind::Postgres), Some((-2147483648, 2147483647)));
        assert_eq!(int_range("int2", SourceKind::Postgres), Some((-32768, 32767)));
        // int8 is NOT narrowing — mpedb's Int64 matches it exactly
        assert_eq!(int_range("int8", SourceKind::Postgres), None);
        assert_eq!(int_range("bigint", SourceKind::Postgres), None);
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
        let f = check_value(&c, &Value::Int(2147483648), SourceKind::Postgres);
        assert!(matches!(f, Some((FindingKind::WontFit, _))), "got {f:?}");
        // and the in-range value is silent
        assert!(check_value(&c, &Value::Int(2147483647), SourceKind::Postgres).is_none());
        // int8 never overflows: mpedb's Int64 IS int8
        let big = col("int8", ColumnType::Int64, false);
        assert!(check_value(&big, &Value::Int(i64::MAX), SourceKind::Postgres).is_none());
    }

    #[test]
    fn catches_varchar_length_in_characters_not_bytes() {
        use mpedb_types::ColumnType;
        let c = col("character varying(4)", ColumnType::Text, false);
        assert!(check_value(&c, &Value::Text("abcd".into()), SourceKind::Postgres).is_none());
        assert!(matches!(
            check_value(&c, &Value::Text("abcde".into()), SourceKind::Postgres),
            Some((FindingKind::WontFit, _))
        ));
        // 4 multi-byte chars = 12 bytes but still 4 characters: a byte check
        // would have false-positived here.
        assert!(check_value(&c, &Value::Text("æøå日".into()), SourceKind::Postgres).is_none());
    }

    #[test]
    fn catches_numeric_precision_and_flags_rounding_separately() {
        use mpedb_types::ColumnType;
        let c = col("numeric(5,2)", ColumnType::Text, false);
        assert!(check_value(&c, &Value::Text("123.45".into()), SourceKind::Postgres).is_none());
        // 4 integer digits > (5-2)=3 → the target REJECTS
        let over = check_value(&c, &Value::Text("1234.5".into()), SourceKind::Postgres);
        assert!(matches!(&over, Some((FindingKind::WontFit, d)) if d.contains("before the point")));
        // too many fractional digits → the target ROUNDS; say so, do not claim
        // it will be rejected
        let round = check_value(&c, &Value::Text("1.234".into()), SourceKind::Postgres);
        assert!(matches!(&round, Some((FindingKind::WontFit, d)) if d.contains("ROUND")));
        // leading zeros are not significant digits
        assert!(check_value(&c, &Value::Text("000.12".into()), SourceKind::Postgres).is_none());
    }

    #[test]
    fn catches_null_in_not_null_and_nul_in_text() {
        use mpedb_types::ColumnType;
        let nn = col("text", ColumnType::Text, true);
        assert!(matches!(
            check_value(&nn, &Value::Null, SourceKind::Postgres),
            Some((FindingKind::NullInNotNull, _))
        ));
        let t = col("text", ColumnType::Text, false);
        assert!(matches!(
            check_value(&t, &Value::Text("a\0b".into()), SourceKind::Postgres),
            Some((FindingKind::NulInText, _))
        ));
        // a nullable column with NULL is fine and must not be reported
        assert!(check_value(&t, &Value::Null, SourceKind::Postgres).is_none());
    }

    /// A loose source's whole point: sqlite lets a row hold anything, so the
    /// recorded schema says what SHOULD be there and this is where we find out.
    #[test]
    fn catches_loose_source_type_drift() {
        use mpedb_types::ColumnType;
        let c = col("INTEGER", ColumnType::Int64, false);
        let f = check_value(&c, &Value::Text("oops".into()), SourceKind::Postgres);
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
            fix: crate::adapt::Adaptation::Fine,
            table_id: 0,
            col_idx: 0,
        });
        assert!(!r.would_fail(), "a lossy column is a judgement call, not an error");
        r.findings.push(Finding {
            table: "t".into(),
            column: "c".into(),
            kind: FindingKind::WontFit,
            pk: vec![],
            detail: String::new(),
            fix: crate::adapt::Adaptation::Fine,
            table_id: 0,
            col_idx: 0,
        });
        assert!(r.would_fail());
    }

    /// End to end over a LOOSE sqlite source — and the correction of what this
    /// test used to assert.
    ///
    /// It previously imported a sqlite table declaring `qty INT4` and `code
    /// VARCHAR(4)`, wrote 2_147_483_648 and 'toolong', and demanded preflight
    /// flag both. That is PostgreSQL's rulebook applied to sqlite. sqlite stores
    /// both values happily — a declared type there is an affinity, not a
    /// constraint, and `VARCHAR(4)` enforces no length at all — so writing them
    /// BACK to the source succeeds. Flagging them was a false rejection, and
    /// `would_fail()` blocks an export, so it blocked good data.
    ///
    /// What is actually true for a sqlite source: NOT NULL is enforced, widths
    /// are not. The strict checks belong to a strict dialect, and there is a
    /// unit test below pinning the two apart.
    #[test]
    fn a_loose_sqlite_source_is_judged_by_sqlites_rules_not_postgresqls() {
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

        let r = preflight(&db).unwrap();
        assert!(r.findings.is_empty(), "clean data must be silent: {:?}", r.findings);
        assert_eq!(r.rows_checked, 1);

        // Values that "overflow" the DECLARED types. sqlite takes them without
        // complaint, so preflight must not claim the source would reject them.
        db.query(
            "INSERT INTO t (id, qty, code, note) VALUES ($1, $2, $3, $4)",
            &[
                Value::Int(2),
                Value::Int(2_147_483_648), // > int4, but sqlite INT4 is 64-bit
                Value::Text("toolong".into()), // > VARCHAR(4), unenforced
                Value::Text("x".into()),
            ],
        )
        .unwrap();

        let r = preflight(&db).unwrap();
        assert_eq!(r.rows_checked, 2);
        assert!(
            !r.would_fail(),
            "sqlite enforces neither int width nor varchar length; flagging these \
             blocks an export of data the source would take back verbatim: {:?}",
            r.findings
        );

        // Prove it, rather than trusting the reasoning: sqlite really does
        // accept both back.
        {
            let c = Connection::open(&src).unwrap();
            c.execute(
                "INSERT INTO t VALUES (2, 2147483648, 'toolong', 'x')",
                [],
            )
            .expect("sqlite must accept the very values preflight used to reject");
            let (q, code): (i64, String) = c
                .query_row("SELECT qty, code FROM t WHERE id=2", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .unwrap();
            assert_eq!(q, 2_147_483_648);
            assert_eq!(code, "toolong");
        }

        for p in [src, dest] {
            let _ = std::fs::remove_file(p);
        }
    }

    /// The dialect split, pinned. Same declared type, same value, opposite
    /// verdicts — because `INTEGER` is 32-bit in PostgreSQL and 64-bit in
    /// sqlite. Reading one dialect's schema with the other's rules is what made
    /// preflight reject 5_000_000_000 out of a sqlite INTEGER column.
    #[test]
    fn the_same_declared_type_means_different_things_per_dialect() {
        let c = col("INTEGER", mpedb_types::ColumnType::Int64, false);
        let v = Value::Int(5_000_000_000);
        assert!(
            check_value(&c, &v, SourceKind::Postgres).is_some(),
            "PostgreSQL `integer` is int4: 5e9 does not fit"
        );
        assert!(
            check_value(&c, &v, SourceKind::Sqlite).is_none(),
            "sqlite `INTEGER` is 64-bit: 5e9 fits natively"
        );

        // Same story for declared widths.
        let vc = col("VARCHAR(4)", mpedb_types::ColumnType::Text, false);
        let long = Value::Text("toolong".into());
        assert!(check_value(&vc, &long, SourceKind::Postgres).is_some());
        assert!(check_value(&vc, &long, SourceKind::Sqlite).is_none());

        // NOT NULL is enforced by BOTH — the dialect split must not swallow it.
        let nn = col("TEXT", mpedb_types::ColumnType::Text, true);
        assert!(check_value(&nn, &Value::Null, SourceKind::Sqlite).is_some());
        assert!(check_value(&nn, &Value::Null, SourceKind::Postgres).is_some());
    }

    /// 26.3 end to end — with the premise the first attempt got wrong, twice.
    ///
    /// **(1) Drift cannot exist inside a `.mpedb`.** The schema is rigid, so
    /// INSERTing a Text into an Int64 column is refused outright. sqlite's mess
    /// must therefore be handled where it arrives: at IMPORT.
    ///
    /// **(2) sqlite already fixes the easy half.** Affinity coerces
    /// numeric-LOOKING text on the way in: `'42'` into an INTEGER column is
    /// stored as the integer 42, so the textbook "text in a number column" never
    /// reaches us. What survives affinity is exactly what sqlite could NOT
    /// convert — `'yes'`, `'2023-11-14T22:13:20Z'`, `'007abc'` — i.e. precisely
    /// the cases that need judgement. That is the whole job of this layer.
    #[test]
    fn import_adapts_what_sqlite_affinity_could_not_and_still_refuses_the_cast_trap() {
        use crate::import::{import_sqlite, ImportOptions};
        use rusqlite::Connection;

        let dir = std::env::temp_dir().join("mpedb-mirror-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let pid = std::process::id();
        let mk = |t: &str| dir.join(format!("ad-{t}-{pid}.db"));
        let dst = |t: &str| dir.join(format!("ad-{t}-{pid}.mpedb"));

        // BOOLEAN/DATETIME have NUMERIC affinity, so these stay TEXT in sqlite
        // (verified: 'yes' and the ISO string are typeof()='text').
        let seed = |p: &std::path::Path, b: &str, d: &str| {
            let _ = std::fs::remove_file(p);
            let c = Connection::open(p).unwrap();
            c.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, ok BOOLEAN, at DATETIME);")
                .unwrap();
            c.execute("INSERT INTO t VALUES (1, ?1, ?2)", [b, d]).unwrap();
        };

        // --- default: strict-reject. §4.5's default, and it holds.
        let (s1, d1) = (mk("strict"), dst("strict"));
        seed(&s1, "yes", "2023-11-14T22:13:20Z");
        let r = import_sqlite(
            &mut Connection::open(&s1).unwrap(),
            &d1,
            &ImportOptions::default(),
        )
        .err();
        assert!(
            matches!(&r, Some(mpedb_types::Error::TypeMismatch(m)) if m.contains("strict-reject")),
            "strict-reject is the default: {r:?}"
        );
        let _ = std::fs::remove_file(&d1);

        // --- opted in: both coerce, and the import SAYS what it changed.
        let (s2, d2) = (mk("adapt"), dst("adapt"));
        seed(&s2, "yes", "2023-11-14T22:13:20Z");
        let opts = ImportOptions {
            adapt: Some(AdaptMode::ExactOnly),
            ..Default::default()
        };
        let (db, rep) = import_sqlite(&mut Connection::open(&s2).unwrap(), &d2, &opts).unwrap();
        assert_eq!(rep.adapted.len(), 2, "every coercion is reported: {:?}", rep.adapted);
        match db.query("SELECT ok, at FROM t", &[]).unwrap() {
            ExecResult::Rows { rows, .. } => {
                assert_eq!(rows[0][0], Value::Bool(true), "'yes' -> true");
                assert_eq!(
                    rows[0][1],
                    Value::Timestamp(1_700_000_000_000_000),
                    "ISO-8601 -> micros UTC"
                );
            }
            other => panic!("{other:?}"),
        }
        drop(db);
        let _ = std::fs::remove_file(&d2);

        // --- the line: '007abc' is NOT 7, in any mode. SQL's CAST says 7.
        let (s3, d3) = (mk("trap"), dst("trap"));
        {
            let _ = std::fs::remove_file(&s3);
            let c = Connection::open(&s3).unwrap();
            c.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, qty INTEGER);")
                .unwrap();
            // non-numeric text survives INTEGER affinity as TEXT
            c.execute("INSERT INTO t VALUES (1, ?1)", ["007abc"]).unwrap();
        }
        for mode in [AdaptMode::ExactOnly, AdaptMode::AllowLossy] {
            let _ = std::fs::remove_file(&d3);
            let o = ImportOptions {
                adapt: Some(mode),
                ..Default::default()
            };
            let r = import_sqlite(&mut Connection::open(&s3).unwrap(), &d3, &o).err();
            assert!(
                matches!(&r, Some(mpedb_types::Error::TypeMismatch(m)) if m.contains("data loss")),
                "{mode:?} must refuse '007abc', not coerce it to 7: {r:?}"
            );
        }

        for p in [s1, s2, s3, d3] {
            let _ = std::fs::remove_file(p);
        }
    }
}
