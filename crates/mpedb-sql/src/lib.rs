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
// The PostgreSQL-only surface, addable and removable as ONE piece — see
// `pg/mod.rs` for why it is a module rather than scattered dialect branches.
// Only `pg::api` is public; everything else under it is compiler internals.
pub mod pg;
mod plan;
mod planner;
mod policy;
mod token;
mod trigger;
mod view;

pub use binder::{HostUdfSet, OpSet, SpellFnSet};
pub use planner::sequence;
pub use dbref::{
    inline_temp_views, inline_temp_views_dialect, rename_identifier, rename_identifier_dialect,
    rename_table_in_ddl, rename_table_in_ddl_dialect, rewrite_temp_ddl, rewrite_temp_ddl_dialect,
    mangle as mangle_db_table, parse_attach, parse_attach_dialect, resolve_db_refs,
    resolve_db_refs_dialect, AttachStmt, DbResolution, DbScope,
};
pub use ddl::{
    CreateColumnSpec, CreatePolicySpec, CreateTableSpec, CreateTriggerSpec, CreateVirtualTableSpec,
    DdlStmt, RlsAction, TriggerBodySpec, TriggerEvent, TriggerTiming,
};
pub use trigger::{
    compile_trigger_arg, compile_trigger_body, compile_trigger_when, RowMap, RowSide,
    TriggerRaise, TriggerStmt,
};
pub use plan::{
    AccessPath, AggCall, Aggregation, CompiledPlan, CompoundArm, CompoundPlan, ConflictProbe, Frame,
    FrameBound, FrameExclude, FrameMode, FtsQuery, FtsTerm, GroupKey, InsertSource, Join,
    JoinKind, OrderOver,
    parallel_fold_shape, LimitVal, PlanOnConflict, PlanStmt, PolicyStamp, Projection, DerivedPlan, dual_def,
    RecursiveCtePlan, SelectPlan, SetOp, SortDir, SubBody, SubPlan, SubPlanKind, WinInt,
    WindowFunc, WindowSpec, CTE_TABLE, DUAL_TABLE, SERIES_TABLE, series_def,
};
pub use planner::{
    magnitude, row_prune, secondary_indexes, set_mpee_enabled, CostSource, Mask, RowCountFn,
    RowPrune, NO_ROW_COUNTS,
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
    parse_ddl_dialect(sql, Dialect::Sqlite)
}

/// [`parse_ddl`] under an explicit dialect. The facade passes the session's;
/// the plain wrapper keeps sqlite so every existing caller is unchanged.
pub fn parse_ddl_dialect(sql: &str, dialect: Dialect) -> Result<Option<DdlStmt>> {
    parser::parse_ddl_dialect(sql, dialect)
}

pub use parser::TxnControl;

/// Recognize a transaction/savepoint-control statement (`BEGIN`, `COMMIT`,
/// `ROLLBACK [TO SAVEPOINT n]`, `SAVEPOINT n`, `RELEASE n`) with the real
/// grammar, or `None` for anything else — including any parse trouble, so the
/// caller falls through to the compile road and refusals keep their canonical
/// messages. The facade's write session dispatches these directly (#N2): a
/// savepoint op needs no plan, and compiling one per Django's unique-per-use
/// savepoint names churned the text memo's CLOCK ring and grew the hash cache
/// without bound.
pub fn parse_txn_control(sql: &str) -> Option<TxnControl> {
    parser::parse_txn_control(sql)
}

// Re-export the shared types a plan consumer needs.
pub use mpedb_types::{
    Dialect, Collation, ColumnDef, ColumnType, DefaultExpr, Error, ExprProgram, Footprint,
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
        Dialect::default(),
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
        Dialect::default(),
        &HostUdfSet::default(),
        NO_ROW_COUNTS,
    )
}

/// Like [`prepare_maybe_explain_with_policies`] but also given the view catalog
/// (name → SELECT source) and the GROUP BY strictness dialect (COMPAT.md); a
/// query naming a view is flattened onto the view's base table before planning
/// (design/DESIGN-VIEW.md). `compat` decides whether a bare (non-aggregated,
/// non-grouped) column is accepted (sqlite) or refused (postgres) — the facade
/// passes the database's configured [`Dialect`]; the simpler `prepare*`
/// wrappers default to [`Dialect::Sqlite`].
pub fn prepare_maybe_explain_with_views(
    sql: &str,
    schema: &Schema,
    catalog: &PolicyCatalog,
    views: &ViewCatalog,
    compat: Dialect,
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
    let (mut stmt, is_explain, n_params, ctes) = parser::parse_statement_ctes_dialect(
        sql,
        host_udfs.aggs(),
        host_udfs.window_aggs(),
        &host_udfs.ops,
        compat,
    )?;
    // A `WITH` CTE is a statement-scoped named view. Pass the CTE bodies to
    // `inline_views` in a SECOND catalog kept distinct from the persistent views,
    // so a `FROM cte` reference is spliced by the keep-alias machinery (`cte.col`
    // and `FROM cte AS x` resolve) while stored views keep their strip-name
    // splice unchanged. A CTE shadows a same-named view for this one statement.
    // No planner/plan-bytes/executor change (#CTE).
    // A CTE body is captured as SOURCE and re-parsed where it is spliced, so
    // its parameters are numbered by that second parse. Two consequences, and
    // they are decided HERE because this is the only place that sees every
    // body:
    //
    //   * `$n` is ABSOLUTE, so the body's indices are already the caller's —
    //     but the outer parse never saw them, and the statement's slot count
    //     would be too small for the spliced AST. Raise it.
    //   * `?` is POSITIONAL, and the re-parse numbers the body's from zero —
    //     the same slots the outer statement's own `?` take. Refused by name;
    //     answering it would bind the wrong values, not fail.
    //
    // (The C-API rewrites every `?` to `$K` over the whole statement before
    // mpedb sees it, so a consumer going through the shim is on the first path.)
    let mut n_params = n_params;
    for (name, src) in &ctes {
        if parser::has_question_param(src)? {
            return Err(Error::Bind(format!(
                "CTE `{name}` body uses `?` parameters, which are numbered by \
                 position and would collide with the outer statement's; use \
                 `$1`-style numbering, which is absolute"
            )));
        }
        let (_, _, body_params, _) = parser::parse_statement_ctes(
            src,
            host_udfs.aggs(),
            host_udfs.window_aggs(),
            &host_udfs.ops,
        )?;
        n_params = n_params.max(body_params);
    }
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
        Tok::Ident(s) | Tok::QuotedIdent(s, _) => Some(s.clone()),
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
                (Expr::Col(c, _), Expr::ContextRef(_)) | (Expr::ContextRef(_), Expr::Col(c, _)) => {
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

/// Compile a partial index's `WHERE <expr>` against the finished table.
///
/// Same shape as [`compile_check`] — an expression over the table's own
/// columns, no parameters — but the two are NOT interchangeable at evaluation
/// time and the separate name is the reminder. A CHECK passes on TRUE *or*
/// NULL; index membership is a `WHERE`, so only TRUE is a member (MEASURED
/// against sqlite 3.45.1: two rows may share a partial-UNIQUE key when the
/// predicate is FALSE *and* when it is NULL). The engine applies that rule in
/// `index_predicate_admits`.
pub fn compile_index_predicate(expr_src: &str, table: &TableDef) -> Result<ExprProgram> {
    compile_check(expr_src, table)
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
/// Compile an expression for its VALUE, against `table`'s columns.
///
/// [`compile_check`] binds a PREDICATE (its result is a truth value) and
/// [`compile_generated`] binds an ASSIGNMENT (its result is coerced to a target
/// column). A column DEFAULT is neither: `DEFAULT (3.14159)` must come back as
/// the float, and the DDL applier does the column's affinity and type check
/// afterwards, exactly as it does for a literal default. Compiling it as a check
/// returned `true`.
pub fn compile_value_expr(expr_src: &str, table: &TableDef) -> Result<ExprProgram> {
    let (expr, n_params) = parser::parse_expr_only(expr_src)?;
    if n_params > 0 {
        return Err(Error::Bind("parameters are not allowed here".into()));
    }
    let mut binder = binder::Binder::new(table, 0, false);
    let (bound, _ty) = binder.bind_expr(&expr)?;
    binder::compile_program(&bound)
}

/// `DEFAULT ( <expr> )` — compiled, with the statement instant permitted.
///
/// Returns the program and whether it READS that instant. One that does not is
/// a constant written as arithmetic (`DEFAULT (1 + 2)`), and the caller folds
/// it to a literal exactly as before; one that does is stored and evaluated per
/// INSERT, which is what `DEFAULT (STRFTIME('%Y-%m-%d %H:%M:%f', 'NOW'))` —
/// Django's `auto_now_add` — asks for.
///
/// The instant lands in parameter slot 0 because a default takes no user
/// parameters, which is what lets the executor evaluate the program with a
/// one-element array and no numbering to reconcile.
pub fn compile_default_expr(expr_src: &str, table: &TableDef) -> Result<(ExprProgram, bool)> {
    compile_default_expr_with_udfs(expr_src, table, &HostUdfSet::default())
}

/// [`compile_default_expr`] with the compiling connection's host-registered
/// UDFs in scope.
///
/// A DEFAULT may call one, and sqlite is the reason: it accepts
/// `DEFAULT (f(x))` for an f it has never heard of and resolves the name at
/// INSERT, so a connection that registers f afterwards makes the column work
/// (measured against 3.45.1). Django's sqlite backend registers its whole
/// function set — `django_datetime_extract` among them — when the connection
/// opens, which is before any migration runs, so the narrow rule "resolve
/// against the UDFs this connection has" is enough to accept every DEFAULT
/// Django writes, without adopting sqlite's deferred-name resolution.
///
/// Safe here in a way it is NOT for a generated column, and the difference is
/// worth stating because the two look alike: a generated column must be
/// RECOMPUTABLE by every reader — index membership depends on it — so a value
/// only one connection can produce would corrupt an index. A DEFAULT is
/// evaluated once, at INSERT, and stored as an ordinary value. A writer without
/// the function gets a named error on its own INSERT; nothing already written
/// changes meaning. That is exactly sqlite's bargain.
pub fn compile_default_expr_with_udfs(
    expr_src: &str,
    table: &TableDef,
    host_udfs: &HostUdfSet,
) -> Result<(ExprProgram, bool)> {
    let (expr, n_params) = parser::parse_expr_only(expr_src)?;
    if n_params > 0 {
        return Err(Error::Bind("parameters are not allowed in a DEFAULT".into()));
    }
    let mut binder = binder::Binder::new_default_expr(table);
    binder.set_host_udfs(host_udfs);
    let (bound, _ty) = binder.bind_expr(&expr)?;
    let uses_instant = binder.uses_statement_instant();
    Ok((binder::compile_program(&bound)?, uses_instant))
}

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
