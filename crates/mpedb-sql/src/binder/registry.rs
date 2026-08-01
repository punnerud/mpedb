//! Host-registered UDF/aggregate/operator sets and stored-spell sets — what
//! the BINDER may resolve a name against (split from binder.rs; see mod.rs).

use super::*;

/// The names + arities of the HOST-registered UDFs visible to the connection
/// compiling this statement (the C-API `create_function` path,
/// design/DESIGN-UDF.md). Threaded into the binder exactly as the compat dialect
/// is (`set_dialect`/`set_host_udfs`): a function call that matches no native
/// scalar/aggregate but DOES match a registered `(name, argc)` (or a variadic
/// `(name, -1)`) compiles to a [`BExpr::HostCall`]. Empty for every connection
/// that registered none — then function resolution is exactly as before.
///
/// Stage 2 adds `aggs`, the `xStep`/`xFinal` registrations. Those are resolved
/// EARLIER than scalars — in the PARSER, because `myagg(DISTINCT x) FILTER
/// (WHERE …)` is aggregate GRAMMAR and the parser must know to take that branch
/// before it reads the argument list. The two namespaces are checked in the
/// order native aggregate → host aggregate → native scalar → host scalar, so a
/// name registered as both an aggregate and a scalar is read as the aggregate.
/// The STORED function catalog visible to this compile (stage M2): name →
/// (content hash, arity). Loaded by the facade from the sys-keyspace at
/// prepare time, exactly as views are; empty for callers with no database.
/// Unlike [`HostUdfSet`], these definitions live in the FILE — a plan calling
/// one carries the hash, stays deterministic across processes, and may enter
/// the shared registry.
#[derive(Debug, Clone, Default)]
pub struct SpellFnSet {
    fns: Vec<(String, [u8; 32], u16)>,
}

impl SpellFnSet {
    pub fn insert(&mut self, name: String, hash: [u8; 32], argc: u16) {
        self.fns.retain(|(n, _, _)| n != &name);
        self.fns.push((name, hash, argc));
    }
    pub fn is_empty(&self) -> bool {
        self.fns.is_empty()
    }
    /// The registered (hash, arity) for `name`, case-insensitive like every
    /// SQL function name.
    pub fn resolve(&self, name: &str) -> Option<([u8; 32], u16)> {
        self.fns
            .iter()
            .find(|(n, _, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, h, a)| (*h, *a))
    }
}

/// The custom-operator catalog visible to this compile (stage M3,
/// SQL-EXTENSIONS.md): symbol → fixity bits, plus the EXPANDER — a callback
/// the facade backs with the operator's stored PySpell macro. The parser
/// consults fixity to know WHERE a `:sym:` token parses, captures the operand
/// SOURCE TEXT, and splices the expansion's parse in place; the binder then
/// binds the expansion like any hand-written expression, so a macro cannot
/// smuggle anything past a refusal.
/// The macro callback: `(symbol, operand source texts) → SQL fragment`.
pub type OpExpander = std::sync::Arc<dyn Fn(&str, &[&str]) -> Result<String> + Send + Sync>;

#[derive(Clone, Default)]
pub struct OpSet {
    /// `(symbol, fixity)`; fixity bits: 2 = takes a LEFT operand, 1 = RIGHT.
    /// So 3 = infix, 2 = postfix, 1 = prefix, 0 = niladic.
    defs: Vec<(String, u8)>,
    expander: Option<OpExpander>,
}

impl std::fmt::Debug for OpSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpSet").field("defs", &self.defs).finish_non_exhaustive()
    }
}

impl OpSet {
    pub fn insert(&mut self, symbol: String, fixity: u8) {
        self.defs.retain(|(s, _)| s != &symbol);
        self.defs.push((symbol, fixity));
    }
    pub fn set_expander(&mut self, f: OpExpander) {
        self.expander = Some(f);
    }
    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
    pub(crate) fn fixity(&self, symbol: &str) -> Option<u8> {
        self.defs.iter().find(|(s, _)| s == symbol).map(|(_, f)| *f)
    }
    pub(crate) fn expand(&self, symbol: &str, operands: &[&str]) -> Result<String> {
        let f = self.expander.as_ref().ok_or_else(|| {
            Error::Unsupported(format!(
                "operator :{symbol}: cannot expand in this context (no catalog)"
            ))
        })?;
        f(symbol, operands)
    }
}

#[derive(Debug, Clone, Default)]
pub struct HostUdfSet {
    fns: Vec<(String, i32)>,
    /// STORED PySpell functions ([`SpellFnSet`]) — carried here because this
    /// struct is "the function catalogs visible to this compile" and is
    /// already threaded to every binder; a stored function is one more
    /// namespace in that resolution order (native → stored → host).
    pub spells: SpellFnSet,
    /// Custom operators ([`OpSet`]) — same reasoning: the parser needs their
    /// fixity and the expander, and this struct already reaches it.
    pub ops: OpSet,
    aggs: Vec<(String, i32)>,
    /// HOST collating-sequence names (`sqlite3_create_collation`). Names only:
    /// a collation has no arity, and the comparator itself never leaves the
    /// connection's registry — the plan carries the NAME and the executor
    /// resolves it (design/DESIGN-UDF.md stage 3).
    colls: Vec<String>,
    /// Host aggregates registered with sqlite's WINDOW protocol
    /// (`create_window_function` — `xValue`/`xInverse` on top of
    /// `xStep`/`xFinal`). A NAME subset of `aggs`: an entry here is also in
    /// `aggs`, and only an entry here may take an `OVER` clause.
    window_aggs: Vec<String>,
}

impl HostUdfSet {
    /// Build from `(name, n_arg)` pairs; `n_arg == -1` is sqlite's variadic
    /// "any arity" registration.
    pub fn new(fns: Vec<(String, i32)>) -> HostUdfSet {
        HostUdfSet { fns, ..Default::default() }
    }

    /// Build from the scalar AND aggregate registrations.
    pub fn with_aggs(fns: Vec<(String, i32)>, aggs: Vec<(String, i32)>) -> HostUdfSet {
        HostUdfSet { fns, aggs, ..Default::default() }
    }

    pub fn is_empty(&self) -> bool {
        self.fns.is_empty() && self.aggs.is_empty() && self.colls.is_empty()
    }

    /// The registered host AGGREGATE names, for the parser's grammar decision.
    /// Name-only on purpose: the parser must choose the aggregate branch BEFORE
    /// it has parsed the arguments, so arity is checked afterwards
    /// ([`host_agg_arity_ok`](Self::host_agg_arity_ok)).
    /// The registered host COLLATION names, for the ORDER-BY peel. Empty for
    /// every caller that registered none, so collation resolution is exactly as
    /// before for them.
    pub fn colls(&self) -> &[String] {
        &self.colls
    }

    /// Replace the host COLLATION names (the shim registers them per
    /// connection, alongside the scalar/aggregate registries).
    pub fn set_colls(&mut self, colls: Vec<String>) {
        self.colls = colls;
    }

    /// The host aggregates that may be used as WINDOW functions.
    pub fn window_aggs(&self) -> &[String] {
        &self.window_aggs
    }

    /// Replace the window-capable subset (the shim registers these per
    /// connection alongside the plain aggregates).
    pub fn set_window_aggs(&mut self, names: Vec<String>) {
        self.window_aggs = names;
    }

    pub fn agg_names(&self) -> Vec<String> {
        self.aggs.iter().map(|(n, _)| n.clone()).collect()
    }

    /// The `(name, n_arg)` pairs of the registered host aggregates.
    pub fn aggs(&self) -> &[(String, i32)] {
        &self.aggs
    }

    /// Is `name` registered as a host aggregate accepting `argc` arguments?
    /// Exact arity or a variadic `-1`, the same rule scalars use.
    pub fn host_agg_arity_ok(&self, name: &str, argc: usize) -> bool {
        let argc = argc as i32;
        self.aggs
            .iter()
            .any(|(n, a)| n == name && (*a == argc || *a == -1))
    }

    /// Does a call `name(<argc args>)` match a registered host UDF? An exact
    /// `(name, argc)` wins; otherwise a variadic `(name, -1)` also matches.
    pub(super) fn resolves(&self, name: &str, argc: usize) -> bool {
        let argc = argc as i32;
        self.fns
            .iter()
            .any(|(n, a)| n == name && (*a == argc || *a == -1))
    }
}

