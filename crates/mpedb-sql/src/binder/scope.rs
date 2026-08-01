//! Name resolution: the tables a statement addresses and how a bare or
//! qualified column resolves (split from binder.rs; see mod.rs).

use super::*;

/// See [`Scope::binds`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NameBinding {
    Unique,
    Ambiguous,
    Unknown,
}


/// The tables a statement can name, and how a column reference resolves to a
/// slot in the row the expression will see.
///
/// **Why this exists as a type instead of a `&TableDef` field.** The binder held
/// exactly one table, so "which table is this column in" was never a question —
/// and every layer above inherited that assumption without stating it. The
/// footprint never did: `tables_read`/`tables_written` are bitmaps over
/// `MAX_TABLES` and `conflicts_with` is a bitmap AND, so a multi-table access set
/// has always been *representable*. The binder is what made it unreachable.
///
/// Today a scope holds one table and this is a pure refactor — same resolution,
/// same errors, no new SQL. It exists so the next step (a second table) changes
/// this type rather than 45 call sites, and so the rule that matters is written
/// down in ONE place: **a column resolves to an offset into the tuple the
/// expression is evaluated over.** For a single table that is the row itself. For
/// `ON CONFLICT DO UPDATE` it is already `[existing ‖ proposed]`, which is why
/// `excluded.<c>` binds to `Col(n + i)`. For a join it will be the concatenation
/// of the joined rows. Same rule, wider tuple.
pub(crate) struct Scope<'a> {
    /// Tables in tuple order. The slot base of table `k` is the sum of the widths
    /// before it.
    pub(super) tables: Vec<&'a TableDef>,
    /// The name each table is ADDRESSED by — its alias if the query gave one,
    /// else its own name. Parallel to `tables`. Qualified resolution matches
    /// against this, which is what implements PG's rule that `FROM orders o`
    /// puts `o` in scope and NOT `orders`, and what lets a table join itself
    /// under two different names.
    pub(super) names: Vec<String>,
}

impl<'a> Scope<'a> {
    pub fn single(t: &'a TableDef) -> Scope<'a> {
        Scope { names: vec![t.name.clone()], tables: vec![t] }
    }

    /// Single table addressed by `name` (an alias). `FROM orders o WHERE o.id`.
    pub fn single_named(name: String, t: &'a TableDef) -> Scope<'a> {
        Scope { names: vec![name], tables: vec![t] }
    }

    /// A join's scope, each table addressed by an explicit (possibly aliased)
    /// name. Tuple order IS the order given: the outer table's columns come
    /// first, so its slots are its own column indices — which is what lets an
    /// outer-only predicate be handed to the single-table access extractor
    /// unchanged.
    pub fn joined_named(named: Vec<(String, &'a TableDef)>) -> Result<Scope<'a>> {
        // Two tables addressed by the SAME name make `x.c` ambiguous with no way
        // to say which side. That is a self-join with no (or duplicate) aliases;
        // refuse it, but a self-join with two distinct aliases is now fine.
        for (i, (a, _)) in named.iter().enumerate() {
            for (b, _) in &named[i + 1..] {
                if a.eq_ignore_ascii_case(b) {
                    return Err(bind_err(format!(
                        "`{a}` is used for two tables in this statement: give each side of a \
                         self-join a distinct alias (`FROM t a JOIN t b ON …`)"
                    )));
                }
            }
        }
        let (names, tables) = named.into_iter().unzip();
        Ok(Scope { names, tables })
    }


    /// The only table, for the paths that are still single-table by
    /// construction (INSERT's target, RLS policy binding, `excluded.`).
    /// Panics if the scope is wider — a caller that reaches for "the" table of a
    /// join has a bug that must not be papered over with an arbitrary choice.
    /// The single table's NAME when there is one, else empty. Unlike
    /// [`Self::only`] this never asserts — it exists for ERROR paths, where
    /// panicking on a shape the caller did not expect would replace a
    /// diagnosable message with a crash.
    pub fn sole_table_name(&self) -> String {
        match self.tables.as_slice() {
            [t] => t.name.clone(),
            _ => String::new(),
        }
    }

    pub fn only(&self) -> &'a TableDef {
        assert_eq!(
            self.tables.len(),
            1,
            "Scope::only() on a {}-table scope: this path has not been taught about joins",
            self.tables.len()
        );
        self.tables[0]
    }

    /// Slot offset of table `k`'s first column in the evaluated tuple.
    fn base(&self, k: usize) -> usize {
        self.tables[..k].iter().map(|t| t.columns.len()).sum()
    }

    /// Total tuple width.
    pub fn width(&self) -> usize {
        self.base(self.tables.len())
    }

    /// Resolve an UNQUALIFIED column name. Ambiguity is an error, never a
    /// silent pick: with one table it cannot happen, and the day it can, a
    /// wrong guess is a wrong-table read.
    /// The NAME this scope addresses the table owning bare column `name` by —
    /// its alias when the query gave one, else the table's own name.
    ///
    /// Exists so a rewrite that SPLICES an outer expression into an inner
    /// query's scope can qualify it first. An unqualified column moved into a
    /// subquery is resolved there, and if the inner query happens to have a
    /// column of that name the outer reference is silently CAPTURED — which is
    /// a wrong answer, not an error (see `row_value_in_subquery`).
    ///
    /// `None` for a name this scope does not bind, or binds ambiguously: both
    /// are cases the caller must not paper over with a guess.
    pub fn owner_name(&self, name: &str) -> Option<&str> {
        let mut found: Option<&str> = None;
        for (k, t) in self.tables.iter().enumerate() {
            if t.column_index(name).or_else(|| t.rowid_name_col(name)).is_some() {
                if found.is_some() {
                    return None;
                }
                found = Some(self.names[k].as_str());
            }
        }
        found
    }

    /// Does this scope bind `name` at all — uniquely, ambiguously, or not?
    /// The three-way answer exists for sqlite's DQS misfeature: a DOUBLE-quoted
    /// name that resolves to NOTHING becomes a string literal, but an
    /// AMBIGUOUS one is still an error (measured, 3.45.1) — and `resolve`'s two
    /// failures are both `Err`, which the caller must not tell apart by
    /// matching message text.
    pub fn binds(&self, name: &str) -> NameBinding {
        let mut hits = 0;
        for t in self.tables.iter() {
            if t.column_index(name).or_else(|| t.rowid_name_col(name)).is_some() {
                hits += 1;
            }
        }
        match hits {
            0 => NameBinding::Unknown,
            1 => NameBinding::Unique,
            _ => NameBinding::Ambiguous,
        }
    }

    pub fn resolve(&self, name: &str) -> Result<(u16, ColumnType)> {
        let mut found: Option<(u16, ColumnType)> = None;
        for (k, t) in self.tables.iter().enumerate() {
            // A real column wins; only then does an implicit-rowid table's
            // `rowid`/`_rowid_`/`oid` alias resolve to the hidden column (#94),
            // matching sqlite's shadowing rule. `column_index` already finds the
            // literal `rowid` column, so this fallback covers the other spellings
            // and case variants without changing explicit-PK name resolution.
            if let Some(i) = t.column_index(name).or_else(|| t.rowid_name_col(name)) {
                let slot = (self.base(k) + i as usize) as u16;
                if found.is_some() {
                    return Err(bind_err(format!(
                        "column `{name}` is ambiguous: qualify it with a table name"
                    )));
                }
                found = Some((slot, t.columns[i as usize].ty));
            }
        }
        found.ok_or_else(|| {
            bind_err(format!(
                "unknown column `{name}` in {}",
                self.describe()
            ))
        })
    }

    /// Name a slot for humans: bare with one table, `<table>.<column>` with
    /// more — because `did` alone would not say which side it came from, and
    /// both sides usually have one.
    ///
    /// The single place that answers "what is slot N called", so EXPLAIN, the
    /// output header and an error message cannot drift apart.
    /// Column types of the whole tuple, in slot order — the concatenation of
    /// the scoped tables' columns.
    pub fn slot_types(&self) -> Vec<ColumnType> {
        self.tables
            .iter()
            .flat_map(|t| t.columns.iter().map(|c| c.ty))
            .collect()
    }

    /// The `(addressing-name, table)` pairs, in tuple order. Lets a caller
    /// rebuild an EXTENDED scope (base tables ‖ a synthetic tuple) without
    /// knowing whether the base is one table or a join — the window planner
    /// appends its `__w{k}` result table this way (design/DESIGN-WINDOW.md §3.3).
    pub fn named(&self) -> Vec<(String, &'a TableDef)> {
        self.names
            .iter()
            .cloned()
            .zip(self.tables.iter().copied())
            .collect()
    }

    pub fn slot_name(&self, c: u16) -> String {
        let mut base = 0usize;
        for t in &self.tables {
            if (c as usize) < base + t.columns.len() {
                let col = &t.columns[c as usize - base].name;
                return if self.tables.len() == 1 {
                    col.clone()
                } else {
                    format!("{}.{}", t.name, col)
                };
            }
            base += t.columns.len();
        }
        format!("col#{c}")
    }

    /// The DECLARED collating sequence of the column at tuple slot `c` — sqlite's
    /// comparison/ORDER-BY precedence rung "if the operand is a column, use the
    /// column's collation". [`Collation::Binary`] for an out-of-range slot (a
    /// synthetic tuple with no such column), which degrades to the default.
    pub fn column_collation(&self, c: u16) -> Collation {
        let mut base = 0usize;
        for t in &self.tables {
            if (c as usize) < base + t.columns.len() {
                return t.columns[c as usize - base].collation;
            }
            base += t.columns.len();
        }
        Collation::Binary
    }

    /// The `(type, affinity)` of the column at tuple slot `c` — what
    /// `sqlite3ExprAffinity` reads off a column reference, plus the storage type
    /// that says whether the column is the typeless one. `None` for a slot that
    /// names no column (a synthetic tuple), which the caller reads as "no
    /// affinity", exactly as sqlite reads a non-column expression.
    pub fn column_shape(&self, c: u16) -> Option<(ColumnType, Affinity)> {
        let mut base = 0usize;
        for t in &self.tables {
            if (c as usize) < base + t.columns.len() {
                let col = &t.columns[c as usize - base];
                return Some((col.ty, col.affinity));
            }
            base += t.columns.len();
        }
        None
    }

    /// Resolve a QUALIFIED `<table>.<column>`. The qualifier is checked rather
    /// than dropped: accepting `nonsense.id` as `id` turns a typo into a
    /// wrong-table read the moment a scope holds more than one table.
    pub fn resolve_qualified(&self, qual: &str, name: &str) -> Result<(u16, ColumnType)> {
        for (k, t) in self.tables.iter().enumerate() {
            if self.names[k].eq_ignore_ascii_case(qual) {
                let i = t.column_index(name).or_else(|| t.rowid_name_col(name)).ok_or_else(|| {
                    bind_err(format!("unknown column `{qual}.{name}`"))
                })?;
                return Ok((
                    (self.base(k) + i as usize) as u16,
                    t.columns[i as usize].ty,
                ));
            }
        }
        Err(bind_err(format!(
            "no table named `{qual}` in this statement ({})",
            self.describe()
        )))
    }

    fn describe(&self) -> String {
        // Report the names the query addresses tables by (aliases), so an
        // "unknown table `x`" points at what the user actually wrote.
        match self.names.len() {
            1 => format!("table `{}`", self.names[0]),
            _ => format!(
                "tables {}",
                self.names
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}
