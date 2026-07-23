//! mpedb SQL front-end: tokenizer, parser, binder, planner, and
//! content-hashed compiled plans.
//!
//! SQL text is compiled **once** by [`prepare`] into a [`CompiledPlan`] — a
//! self-contained, deterministically serializable plan with a blake3 content
//! hash. Other processes execute directly from the serialized form
//! ([`CompiledPlan::decode`]) with no parsing; decode fully re-validates the
//! bytes against the schema, because plan blobs live in shared memory and may
//! be corrupt or hostile.
//!
//! Determinism: two statements that differ only in whitespace, keyword case,
//! or `?` vs `$n` parameter spelling (in the same left-to-right order)
//! compile to identical plans and identical hashes. Identifiers and literals
//! are case-/value-sensitive.
//!
//! No execution happens in this crate; the executor (a later crate) consumes
//! [`PlanStmt`] and the plan's [`mpedb_types::Footprint`].

mod ast;
mod binder;
mod dbref;
mod ddl;
mod parser;
mod plan;
mod planner;
mod policy;
mod token;
mod trigger;
mod view;

pub use binder::{HostUdfSet, OpSet, SpellFnSet};
pub use planner::sequence;
pub use dbref::{
    mangle as mangle_db_table, parse_attach, resolve_db_refs, AttachStmt, DbResolution, DbScope,
};
pub use ddl::{
    CreateColumnSpec, CreatePolicySpec, CreateTableSpec, CreateTriggerSpec, CreateVirtualTableSpec,
    DdlStmt, RlsAction, TriggerEvent, TriggerTiming,
};
pub use trigger::{compile_trigger_body, compile_trigger_when, RowMap, RowSide};
pub use plan::{
    AccessPath, AggCall, Aggregation, CompiledPlan, CompoundArm, CompoundPlan, ConflictProbe, Frame,
    FrameBound, FrameMode, FtsQuery, FtsTerm, GroupKey, InsertSource, Join, JoinKind, OrderOver,
    parallel_fold_shape, PlanOnConflict, PlanStmt, PolicyStamp, Projection, DerivedPlan, dual_def,
    RecursiveCtePlan, SelectPlan, SetOp, SortDir, SubBody, SubPlan, SubPlanKind, WindowFunc,
    WindowSpec, CTE_TABLE, DUAL_TABLE,
};
pub use planner::{
    row_prune, secondary_indexes, set_mpee_enabled, CostSource, Mask, RowCountFn, RowPrune,
    NO_ROW_COUNTS,
};
pub use policy::{table_policy_hash, PolicyCatalog, TablePolicies};
pub use view::ViewCatalog;

/// The reserved session-context key that carries the STATEMENT-START instant —
/// what a literal `'now'` in `date()`/`time()`/`datetime()`/`julianday()`/
/// `strftime()` binds to.
///
/// It is a context key so that the whole reserved-slot mechanism (sizing into
/// `n_params`, plan encoding, one fill per `execute()`) applies unchanged; the
/// facade recognises this ONE key by name and fills it from the clock instead of
/// from the `Session`, and the binder refuses it in `current_setting()` so a
/// caller can neither read it nor shadow it. The leading `@` keeps it outside
/// the identifier-shaped names real settings use.
///
/// One slot per statement is the whole determinism argument: every `'now'` in a
/// statement compiles to a reference to THIS slot, so they all read the same
/// value (sqlite's `iCurrentTime` rule), while the plan bytes carry only a
/// parameter index and never a clock reading.
pub const STATEMENT_INSTANT_KEY: &str = "@statement_instant";

/// Parse a row-level-security DDL statement, or `None` if `sql` is ordinary
/// DML/query text (design/DESIGN-MULTIDB.md §3.1). The facade calls this before
/// compiling, and applies any DDL against the catalog directly.
pub fn parse_ddl(sql: &str) -> Result<Option<DdlStmt>> {
    parser::parse_ddl(sql)
}

// Re-export the shared types a plan consumer needs.
pub use mpedb_types::{
    BareGroupBy, Collation, ColumnDef, ColumnType, DefaultExpr, Error, ExprProgram, Footprint,
    Instr, KeyAccess, KeyBound, KeyPart, PlanHash, PolicyCmd, PolicyDef, Result, Schema, TableDef,
    TableKind, Tokenizer, Value, FORMAT_VERSION,
};

/// Compile SQL against a schema. Deterministic: identical logical statements
/// (modulo whitespace/keyword case) against the same schema produce identical
/// plans and hashes.
///
/// `EXPLAIN <stmt>` compiles the inner statement; use
/// [`prepare_maybe_explain`] to learn whether the source asked for EXPLAIN.
pub fn prepare(sql: &str, schema: &Schema) -> Result<CompiledPlan> {
    prepare_with_policies(sql, schema, &PolicyCatalog::empty())
}

/// Compile with the catalog's per-table row counts available to the MPEE join
/// solver (design/DESIGN-MPEE-SOLVER.md). The plain [`prepare`] passes a zero
/// source, which leaves the solver's structural term (cartesian-step
/// avoidance) intact but blind to table sizes.
pub fn prepare_with_row_counts(
    sql: &str,
    schema: &Schema,
    row_count: RowCountFn<'_>,
) -> Result<CompiledPlan> {
    Ok(prepare_maybe_explain_with_views(
        sql,
        schema,
        &PolicyCatalog::empty(),
        &view::ViewCatalog::new(),
        BareGroupBy::default(),
        &HostUdfSet::default(),
        row_count,
    )?
    .0)
}

/// Like [`prepare`], additionally reporting whether the statement was wrapped
/// in `EXPLAIN` (the returned plan is always the inner statement's plan; the
/// caller renders [`CompiledPlan::explain`] instead of executing).
pub fn prepare_maybe_explain(sql: &str, schema: &Schema) -> Result<(CompiledPlan, bool)> {
    prepare_maybe_explain_with_policies(sql, schema, &PolicyCatalog::empty())
}

/// Compile with row-level-security policies injected (design/DESIGN-MULTIDB.md §3).
/// The planner AND-folds each target table's applicable `USING`/`WITH CHECK`
/// predicates from `catalog` into the statement; an empty catalog is identical
/// to [`prepare`].
pub fn prepare_with_policies(
    sql: &str,
    schema: &Schema,
    catalog: &PolicyCatalog,
) -> Result<CompiledPlan> {
    Ok(prepare_maybe_explain_with_policies(sql, schema, catalog)?.0)
}

pub fn prepare_maybe_explain_with_policies(
    sql: &str,
    schema: &Schema,
    catalog: &PolicyCatalog,
) -> Result<(CompiledPlan, bool)> {
    prepare_maybe_explain_with_views(
        sql,
        schema,
        catalog,
        &view::ViewCatalog::new(),
        BareGroupBy::default(),
        &HostUdfSet::default(),
        NO_ROW_COUNTS,
    )
}

/// Like [`prepare_maybe_explain_with_policies`] but also given the view catalog
/// (name → SELECT source) and the GROUP BY strictness dialect (COMPAT.md); a
/// query naming a view is flattened onto the view's base table before planning
/// (design/DESIGN-VIEW.md). `compat` decides whether a bare (non-aggregated,
/// non-grouped) column is accepted (sqlite) or refused (postgres) — the facade
/// passes the database's configured [`BareGroupBy`]; the simpler `prepare*`
/// wrappers default to [`BareGroupBy::Sqlite`].
pub fn prepare_maybe_explain_with_views(
    sql: &str,
    schema: &Schema,
    catalog: &PolicyCatalog,
    views: &ViewCatalog,
    compat: BareGroupBy,
    // Host-registered scalar UDFs visible to the compiling connection
    // (design/DESIGN-UDF.md). Empty for callers that register none — then
    // function resolution is exactly as before. Threaded alongside `compat`.
    host_udfs: &HostUdfSet,
    row_count: RowCountFn<'_>,
) -> Result<(CompiledPlan, bool)> {
    // The HOST AGGREGATE registrations reach the PARSER, not just the binder:
    // `myagg(DISTINCT x) FILTER (WHERE …)` is aggregate grammar, and the branch
    // has to be chosen before the argument list is read (design/DESIGN-UDF.md
    // stage 2). Host SCALARS still resolve in the binder, unchanged.
    let (mut stmt, is_explain, n_params, ctes) =
        parser::parse_statement_ctes(sql, host_udfs.aggs(), host_udfs.window_aggs(), &host_udfs.ops)?;
    // A `WITH` CTE is a statement-scoped named view. Pass the CTE bodies to
    // `inline_views` in a SECOND catalog kept distinct from the persistent views,
    // so a `FROM cte` reference is spliced by the keep-alias machinery (`cte.col`
    // and `FROM cte AS x` resolve) while stored views keep their strip-name
    // splice unchanged. A CTE shadows a same-named view for this one statement.
    // No planner/plan-bytes/executor change (#CTE).
    if ctes.is_empty() {
        view::inline_views(&mut stmt, views)?;
    } else {
        // A CTE body may reference an EARLIER CTE (resolved by the flat scope);
        // self/forward/cyclic references and duplicate names are refused here.
        view::validate_cte_order(&ctes)?;
        let scope: view::ViewCatalog = ctes.into_iter().collect();
        view::inline_views_with_ctes(&mut stmt, views, &scope)?;
    }
    let plan =
        planner::plan_statement(&stmt, schema, n_params, catalog, compat, host_udfs, row_count)?;
    Ok((plan, is_explain))
}

/// Split an optional leading `alias.` database qualifier off a statement's
/// table reference, for [`Workspace`](mpedb) routing (design/DESIGN-MULTIDB.md §1.3).
/// Returns the alias (if present) and the SQL with the qualifier removed, so
/// the chosen member database compiles an ordinary single-table plan and its
/// content hash is unaffected by which alias addressed it.
///
/// Routing is done on the **token stream**, never by string search: an
/// `alias.` sequence inside a string literal, a number, or the `WHERE` clause
/// can never be mistaken for a table qualifier. Only the statement's single
/// table reference — the identifier after `FROM`/`INTO`, or after `UPDATE` — is
/// considered. Statements with no table (`BEGIN`/`COMMIT`/`ROLLBACK`) return
/// `(None, sql)` unchanged.
pub fn split_db_alias(sql: &str) -> Result<(Option<String>, String)> {
    use token::{Kw, Tok};
    let toks = token::tokenize(sql)?;
    let table_idx = toks
        .iter()
        .position(|t| matches!(t.tok, Tok::Kw(Kw::From) | Tok::Kw(Kw::Into) | Tok::Kw(Kw::Update)))
        .map(|i| i + 1);
    let ti = match table_idx {
        Some(ti) => ti,
        None => return Ok((None, sql.to_string())),
    };
    let ident_of = |t: &Tok| match t {
        Tok::Ident(s) | Tok::QuotedIdent(s) => Some(s.clone()),
        _ => None,
    };
    if let (Some(a), Some(dot), Some(tb)) = (toks.get(ti), toks.get(ti + 1), toks.get(ti + 2)) {
        if dot.tok == Tok::Dot {
            if let (Some(alias), Some(_table)) = (ident_of(&a.tok), ident_of(&tb.tok)) {
                // Drop the bytes [alias.pos, table.pos): the `alias.` qualifier
                // (and any surrounding spaces), leaving the bare table name.
                let mut out = String::with_capacity(sql.len());
                out.push_str(&sql[..a.pos]);
                out.push_str(&sql[tb.pos..]);
                return Ok((Some(alias), out));
            }
        }
    }
    Ok((None, sql.to_string()))
}

/// Validate an RLS policy predicate source (`USING` / `WITH CHECK`) against a
/// table at policy-creation time (design/DESIGN-MULTIDB.md §3): it must parse, type to
/// bool, reference only the table's columns / literals / `current_setting()`,
/// and use no `$`/`?` parameters (policies cannot reference query params).
pub fn validate_policy_expr(src: &str, table: &TableDef) -> Result<()> {
    let (expr, n_params) = parser::parse_expr_only(src)?;
    if n_params > 0 {
        return Err(Error::Bind(
            "RLS policy predicates may not use `$`/`?` parameters; use current_setting()".into(),
        ));
    }
    // allow_params=true enables `current_setting()`; no `$` params can reach it
    // (rejected above). bind_predicate requires the result to be boolean.
    let mut binder = binder::Binder::new(table, 0, true);
    binder.bind_predicate(&expr)?;
    Ok(())
}

/// The columns a policy predicate pins directly to session context — i.e. every
/// `col = current_setting('…')` (either operand order). These are the policy's
/// **discriminators**: the columns that decide which partition of the table a
/// caller can see.
///
/// Only top-level `=` conjuncts count. A discriminator buried under `OR` does not
/// partition the table (the other branch admits rows regardless), and anything
/// richer than equality is not a partition key either, so neither is reported —
/// under-reporting here just means the lint says nothing, which is the safe way
/// to be wrong for a lint.
pub fn policy_discriminators(src: &str, table: &TableDef) -> Vec<u16> {
    let Ok((expr, _)) = parser::parse_expr_only(src) else {
        return Vec::new(); // unparseable: validate_policy_expr reports it properly
    };
    let mut out = Vec::new();
    collect_discriminators(&expr, table, &mut out);
    out.sort_unstable();
    out.dedup();
    out
}

fn collect_discriminators(e: &ast::Expr, table: &TableDef, out: &mut Vec<u16>) {
    use ast::{BinOp, Expr};
    match e {
        // AND: both sides constrain, so descend into both.
        Expr::Binary(BinOp::And, a, b) => {
            collect_discriminators(a, table, out);
            collect_discriminators(b, table, out);
        }
        Expr::Binary(BinOp::Eq, a, b) => {
            let pair = match (a.as_ref(), b.as_ref()) {
                (Expr::Col(c), Expr::ContextRef(_)) | (Expr::ContextRef(_), Expr::Col(c)) => {
                    Some(c)
                }
                _ => None,
            };
            if let Some(name) = pair {
                if let Some(i) = table.column_index(name) {
                    out.push(i);
                }
            }
        }
        _ => {}
    }
}

/// Compile a CHECK-constraint expression against one table at attach time.
/// Parses a single expression (no statement), binds it against the table's
/// columns with **no parameters allowed**, and requires it to type to bool.
pub fn compile_check(expr_src: &str, table: &TableDef) -> Result<ExprProgram> {
    let (expr, n_params) = parser::parse_expr_only(expr_src)?;
    if n_params > 0 {
        return Err(Error::Bind(
            "parameters are not allowed in CHECK expressions".into(),
        ));
    }
    let mut binder = binder::Binder::new(table, 0, false);
    let bound = binder.bind_check(&expr)?;
    binder::compile_program(&bound)
}

/// Compile a `GENERATED ALWAYS AS (<expr>)` body against the finished table,
/// coerced to the generated column's declared type.
///
/// The same shape as [`compile_check`] with one extra step: the result goes
/// through `bind_assign`, so `a + b` into an `INTEGER` column and `lower(name)`
/// into a `TEXT` one are both type-checked at DDL time rather than failing per
/// row. Aggregates, subqueries and window functions are refused by the binder —
/// which is what sqlite refuses too ("misuse of aggregate", "subqueries
/// prohibited in generated columns") — and parameters are refused here, since a
/// generated expression is evaluated per row with no statement to bind from.
///
/// The program is stored in the schema and re-validated by `Schema::validate`
/// (column bounds, no forward reference to another generated column), so a
/// caller cannot smuggle a cyclic or out-of-range expression past this.
pub fn compile_generated(expr_src: &str, table: &TableDef, col: usize) -> Result<ExprProgram> {
    let (expr, n_params) = parser::parse_expr_only(expr_src)?;
    if n_params > 0 {
        return Err(Error::Bind(
            "parameters are not allowed in a generated column expression".into(),
        ));
    }
    let target = table.columns.get(col).ok_or_else(|| {
        Error::Bind(format!("generated column index {col} out of range"))
    })?;
    let mut binder = binder::Binder::new(table, 0, false);
    let bound = binder.bind_assign(&expr, target)?;
    binder::compile_program(&bound)
}

#[cfg(test)]
mod route_tests {
    use super::split_db_alias;

    fn split(sql: &str) -> (Option<String>, String) {
        split_db_alias(sql).unwrap()
    }

    #[test]
    fn strips_qualifier_from_each_statement_shape() {
        assert_eq!(
            split("SELECT * FROM billing.orders WHERE id = $1"),
            (Some("billing".into()), "SELECT * FROM orders WHERE id = $1".into())
        );
        assert_eq!(
            split("INSERT INTO shared.tenants (id) VALUES (1)"),
            (Some("shared".into()), "INSERT INTO tenants (id) VALUES (1)".into())
        );
        assert_eq!(
            split("UPDATE billing.orders SET total = 5 WHERE id = 1"),
            (Some("billing".into()), "UPDATE orders SET total = 5 WHERE id = 1".into())
        );
        assert_eq!(
            split("DELETE FROM billing.orders WHERE id = 1"),
            (Some("billing".into()), "DELETE FROM orders WHERE id = 1".into())
        );
    }

    #[test]
    fn unqualified_and_tableless_pass_through() {
        assert_eq!(split("SELECT * FROM orders"), (None, "SELECT * FROM orders".into()));
        assert_eq!(split("BEGIN"), (None, "BEGIN".into()));
        assert_eq!(split("COMMIT"), (None, "COMMIT".into()));
    }

    #[test]
    fn explain_prefix_is_handled() {
        assert_eq!(
            split("EXPLAIN SELECT * FROM billing.orders"),
            (Some("billing".into()), "EXPLAIN SELECT * FROM orders".into())
        );
    }

    #[test]
    fn dotted_text_inside_a_string_literal_is_not_a_qualifier() {
        // The `x.y` lives in a string literal, not the table reference: the
        // token-level router must leave it untouched.
        let sql = "SELECT * FROM orders WHERE note = 'from a.b to c'";
        assert_eq!(split(sql), (None, sql.to_string()));
    }

    #[test]
    fn quoted_alias_and_table() {
        assert_eq!(
            split("SELECT * FROM \"billing\".\"orders\""),
            (Some("billing".into()), "SELECT * FROM \"orders\"".into())
        );
    }
}
