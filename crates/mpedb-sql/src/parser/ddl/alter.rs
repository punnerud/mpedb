//! ALTER TABLE grammar and ADD COLUMN defaults.

use super::*;

impl Parser<'_> {
    pub(super) fn parse_alter(&mut self) -> Result<DdlStmt> {
        // `ALTER <anything but TABLE>` names the OBJECT KIND, as the CREATE and
        // DROP paths already do. `expect_word("TABLE")` alone reported
        // ``expected `TABLE` `` — 422 statements in PostgreSQL's regress corpus
        // being told what the parser wanted, for a statement whose problem is
        // that mpedb has no `ALTER TYPE`, `ALTER SEQUENCE`, `ALTER ROLE`…
        // Naming the word they wrote is what makes the refusal a fact about
        // mpedb rather than a fact about the parser's cursor.
        if self.word_at_cursor() != "TABLE" {
            let found = self.word_at_cursor();
            return Err(self.err_here(format!(
                "ALTER {found} is not supported — mpedb implements ALTER TABLE                  (RENAME, ADD/DROP COLUMN, and the ROW LEVEL SECURITY forms)"
            )));
        }
        self.expect_word("TABLE")?;
        let table = self.ident("table name")?;
        // RENAME forms (pure schema metadata) branch off before the RLS words.
        if self.eat_word("RENAME") {
            if self.eat_word("TO") {
                let new_name = self.ident("new table name")?;
                return Ok(DdlStmt::AlterRenameTable { table, new_name });
            }
            // `RENAME COLUMN a TO b` or the bare `RENAME a TO b` (sqlite accepts
            // both; COLUMN is optional).
            self.eat_word("COLUMN");
            let column = self.ident("column name")?;
            if !self.eat_word("TO") {
                return Err(self.err_here("expected TO in RENAME COLUMN"));
            }
            let new_name = self.ident("new column name")?;
            return Ok(DdlStmt::AlterRenameColumn {
                table,
                column,
                new_name,
            });
        }
        if self.eat_word("ADD") {
            if self.dialect == mpedb_types::Dialect::Postgres
                && matches!(
                    self.word_at_cursor().as_str(),
                    "CONSTRAINT" | "FOREIGN" | "UNIQUE"
                )
            {
                let name = if self.eat_word("CONSTRAINT") {
                    Some(self.ident("constraint name")?)
                } else {
                    None
                };
                if self.eat_word("UNIQUE") {
                    let columns = self.paren_ident_list()?;
                    return Ok(DdlStmt::AlterAddUnique {
                        table,
                        columns,
                        name,
                    });
                }
                self.expect_word("FOREIGN")?;
                self.expect_word("KEY")?;
                let columns = self.paren_ident_list()?;
                self.expect_word("REFERENCES")?;
                let foreign_key = self.references_clause(columns, name)?;
                return Ok(DdlStmt::AlterAddForeignKey { table, foreign_key });
            }
            // A TABLE CONSTRAINT is not a column, and reading it as one was a
            // WRONG ANSWER — the worst kind, because it succeeded.
            //
            // `ALTER TABLE t ADD CONSTRAINT c CHECK (id > 0)` reported
            // `affected: 0` and left the table with a phantom column literally
            // named `CONSTRAINT`: `SELECT *` showed it, and a violation read
            // ``CHECK violation: t.CONSTRAINT failed``. `COLUMN` is optional in
            // this grammar, so the constraint keyword landed in the slot the
            // column name was read from. The same hole swallows `ADD PRIMARY
            // KEY`, `ADD UNIQUE`, `ADD FOREIGN KEY` and a bare `ADD CHECK`.
            //
            // Found while giving the OTHER `ALTER TABLE` actions their names —
            // `ADD` was the one action that did not reach the message being
            // fixed, because it silently succeeded.
            let w = self.word_at_cursor();
            if matches!(
                w.as_str(),
                "CONSTRAINT" | "PRIMARY" | "UNIQUE" | "FOREIGN" | "CHECK" | "EXCLUDE"
            ) {
                return Err(self.err_here(format!(
                    "ALTER TABLE … ADD {w} adds a TABLE CONSTRAINT, and mpedb's constraints                      are fixed at CREATE TABLE — a rigid schema is what its type checking                      rests on. Recreate the table with the constraint"
                )));
            }
            self.eat_word("COLUMN"); // optional, as in sqlite/PG
            let cname = self.ident("column name")?;
            // The SAME declared-type grammar CREATE TABLE uses — `varchar(100)`
            // must not mean one thing in a CREATE and another in an ADD. Zero
            // type words is the typeless column (`ALTER TABLE t ADD COLUMN c`)
            // → Any, matching sqlite's no-affinity column.
            let (ty, affinity, decl) = self.declared_type()?;
            let mut col = crate::ddl::CreateColumnSpec {
                pg_enum: decl
                    .as_ref()
                    .and_then(|d| self.pg_enums.iter().find(|e| e.matches_decl(d)))
                    .cloned(),
                name: cname,
                ty,
                affinity,
                decl,
                not_null: false,
                unique: false,
                pk: false,
                default: None,
                default_src: None,
                default_text: None,
                autoincrement: false,
                check: None,
                collation: Collation::Binary,
                generated: None,
                references: None,
            };
            loop {
                // A per-column constraint may carry a `CONSTRAINT <name>`
                // prefix — same grammar as the CREATE TABLE column path, and
                // the name is dropped the same way (stock accepts
                // `ADD COLUMN q integer CONSTRAINT qpos CHECK (q >= 0)`;
                // carrying the name into messages is the v19 named-check
                // item, for both paths at once).
                let named = if self.eat_word("CONSTRAINT") {
                    Some(self.ident("constraint name after CONSTRAINT")?)
                } else {
                    None
                };
                if self.eat_kw(Kw::Not) {
                    self.expect_kw(Kw::Null, "NULL")?;
                    col.not_null = true;
                    self.conflict_clause("a NOT NULL constraint")?;
                } else if self.eat_kw(Kw::Null) {
                    col.not_null = false;
                } else if self.eat_word("UNIQUE") {
                    col.unique = true;
                    self.conflict_clause("a UNIQUE constraint")?;
                } else if self.eat_word("PRIMARY") {
                    self.expect_word("KEY")?;
                    let _ = self.eat_kw(Kw::Asc) || self.eat_kw(Kw::Desc);
                    col.pk = true;
                    self.conflict_clause("a PRIMARY KEY constraint")?;
                    if self.eat_word("AUTOINCREMENT") {
                        return Err(self.err_here(AUTOINCREMENT_REFUSAL));
                    }
                } else if self.eat_word("AUTOINCREMENT") {
                    return Err(self.err_here(AUTOINCREMENT_REFUSAL));
                } else if self.eat_word("COLLATE") {
                    col.collation = self.parse_collation_name()?;
                } else if self.eat_word("DEFAULT") {
                    // `ADD COLUMN … DEFAULT <const>` fills existing rows with the
                    // constant (and a `NOT NULL DEFAULT <const>` becomes legal —
                    // the fill value is non-NULL). Only a literal is accepted,
                    // matching sqlite, which refuses a non-constant ADD-COLUMN
                    // default. The facade type-checks the value against `ty`.
                    col.default = Some(self.parse_add_column_default()?);
                } else if self.eat_word("CHECK") {
                    // sqlite ACCEPTS a CHECK on ADD COLUMN — differentially
                    // confirmed on 3.45.1: `ALTER TABLE t ADD COLUMN x integer
                    // CHECK (x >= 0)` succeeds on a populated table, the next
                    // violating write is refused, and the rows already present
                    // are never re-tested (they read the fill value, tested by
                    // nothing — sqlite's rule, kept). Django emits exactly this
                    // for every added PositiveIntegerField. Same clause grammar
                    // as the CREATE TABLE column path: several CHECKs conjoin.
                    let src = self.capture_paren_source()?;
                    col.check = Some(match col.check.take() {
                        Some(prev) => format!("({prev}) AND ({src})"),
                        None => src,
                    });
                } else if self.eat_word("REFERENCES") {
                    col.references = Some(self.references_clause(vec![col.name.clone()], None)?);
                } else if self.eat_word("GENERATED") {
                    self.expect_word("ALWAYS")?;
                    col.generated = Some(self.parse_generated_tail()?);
                } else if matches!(self.peek(), Some(Tok::Kw(Kw::As))) {
                    col.generated = Some(self.parse_generated_tail()?);
                } else {
                    if let Some(n) = named {
                        return Err(self
                            .err_here(format!("CONSTRAINT {n} must be followed by a constraint")));
                    }
                    break;
                }
            }
            if col.generated.is_some() {
                if col.default.is_some() {
                    return Err(self.err_here(format!(
                        "cannot use DEFAULT on generated column `{}`",
                        col.name
                    )));
                }
                if col.pk {
                    return Err(self.err_here(format!(
                        "generated column `{}` cannot be part of the PRIMARY KEY",
                        col.name
                    )));
                }
            }
            return Ok(DdlStmt::AlterAddColumn { table, column: col });
        }
        if self.eat_word("DROP") {
            self.eat_word("COLUMN"); // optional, as in sqlite/PG
            let column = self.ident("column name")?;
            return Ok(DdlStmt::AlterDropColumn { table, column });
        }
        let action = if self.eat_word("ENABLE") {
            self.expect_row_level_security()?;
            RlsAction::Enable { force: false }
        } else if self.eat_word("FORCE") {
            self.expect_row_level_security()?;
            RlsAction::Enable { force: true }
        } else if self.eat_word("DISABLE") {
            self.expect_row_level_security()?;
            RlsAction::Disable
        } else {
            // The partitioning verbs get their own sentence before the
            // catch-all, for the same reason CREATE TABLE does: the generic
            // message names the last thing the parser happened to try
            // (row-level security) rather than the thing that was written.
            if matches!(self.word_at_cursor().as_str(), "ATTACH" | "DETACH") {
                return Err(self.err_here(PARTITIONING));
            }
            // Every other ALTER TABLE action names ITSELF. The old message
            // named row-level security — the last branch the parser happened to
            // try — for statements about constraints, ownership, inheritance
            // and column types. The corpus writes ALTER (266), SET (86), OWNER
            // (35), INHERIT (28), VALIDATE (19), RESET (18) and more, and each
            // of them was told about a feature it had not mentioned.
            let found = self.word_at_cursor();
            if !found.is_empty() && found != "…" {
                return Err(self.err_here(format!(
                    "ALTER TABLE … {found} is not supported — mpedb implements RENAME                      (table and column), ADD/DROP COLUMN, and ENABLE/FORCE/DISABLE ROW                      LEVEL SECURITY"
                )));
            }
            return Err(self.err_here("expected ENABLE, FORCE, or DISABLE ROW LEVEL SECURITY"));
        };
        Ok(DdlStmt::AlterRls { table, action })
    }

    /// Parse the `DEFAULT <const>` value of an `ALTER TABLE ADD COLUMN` clause.
    /// sqlite accepts ONLY a literal constant here — an integer, float, string,
    /// blob, boolean, `NULL`, or a signed number — and refuses anything that
    /// needs evaluation (a parenthesized expression such as `(1+2)`, a function
    /// call, a column reference, or `CURRENT_*`) with "Cannot add a column with
    /// non-constant default". We match that: a non-literal default is a parse
    /// error. The value is folded into a [`DefaultExpr::Const`]; the facade
    /// type-checks it against the column type.
    pub(super) fn parse_add_column_default(&mut self) -> Result<DefaultExpr> {
        // sqlite accepts REDUNDANT parentheses around a literal here — `(5)`,
        // `(-5)`, `('hi')` all work — while refusing anything it would have to
        // evaluate: `(3+4)` is "Cannot add a column with non-constant default"
        // (MEASURED at 3.45.1). The rule is about the VALUE being a literal, not
        // about the punctuation, because ADD COLUMN has to fill the rows already
        // in the table without computing anything. So peel one layer and require
        // a literal inside; a computed one falls through to the refusal below.
        if self.peek() == Some(&Tok::LParen) {
            let save = self.pos;
            self.pos += 1;
            if let Ok(d) = self.parse_add_column_default() {
                if self.eat(&Tok::RParen) {
                    return Ok(d);
                }
            }
            self.pos = save;
        }
        // A leading sign only makes sense before a numeric literal.
        let signed = if self.eat(&Tok::Minus) {
            Some(true)
        } else if self.eat(&Tok::Plus) {
            Some(false)
        } else {
            None
        };
        let non_const = |p: &Self| {
            p.err_here(
                "ADD COLUMN DEFAULT must be a constant literal (a number, string, blob, \
                 boolean, or NULL) — a parenthesized expression, function call, column \
                 reference, or CURRENT_* default is not supported (matches sqlite)",
            )
        };
        let val = match self.advance() {
            Some(Tok::Int(i)) => {
                let i = if signed == Some(true) {
                    i.checked_neg()
                        .ok_or_else(|| self.err_here("integer literal overflows i64"))?
                } else {
                    i
                };
                Value::Int(i)
            }
            Some(Tok::Float(f)) => Value::Float(if signed == Some(true) { -f } else { f }),
            // A sign before a non-numeric literal is a syntax error.
            Some(Tok::Str(s)) if signed.is_none() => Value::Text(s),
            Some(Tok::Blob(b)) if signed.is_none() => Value::Blob(b),
            Some(Tok::Kw(Kw::Null)) if signed.is_none() => Value::Null,
            Some(Tok::Kw(Kw::True)) if signed.is_none() => Value::Bool(true),
            Some(Tok::Kw(Kw::False)) if signed.is_none() => Value::Bool(false),
            _ => return Err(non_const(self)),
        };
        Ok(DefaultExpr::Const(val))
    }
}
