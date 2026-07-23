//! Custom SQL operators — `:sym:` macros backed by stored PySpell (stage M3,
//! SQL-EXTENSIONS.md).
//!
//! An operator is a **bind-time macro over operand SOURCE TEXT**: the parser
//! captures each operand's text (which is why `SELECT * FROM orders WHERE
//! TIME :>: now` works with `TIME` and `now` undefined — they never reach the
//! binder), hands it to the operator's stored spell, and parses the returned
//! SQL fragment in place. The expansion then binds like any hand-written
//! expression — every refusal and type rule applies to it — and the PLAN
//! contains only the expansion, so plan hashing and the shared registry are
//! untouched by the mechanism.
//!
//! Fixity is a two-bit registration (the user's 11/10/01/00): bit 2 = takes a
//! LEFT operand, bit 1 = RIGHT. The spell's arity must equal the operand
//! count — checked at create, so a definition cannot silently disagree with
//! its own grammar.
//!
//! Storage rides the M2 machinery: the spell blob lives content-addressed in
//! `funch/`, the operator record in `op/<symbol>`, and both create and drop
//! bump `schema_gen` — an operator change re-binds every process's next
//! prepare, exactly like a view or function change.

use std::sync::Arc;

use mpedb_spell::ir::Proc;
use mpedb_types::{Error, Result, Value};

use crate::spellfn::SpellLang;

pub const NS_OP: &str = "op";

/// The fixity bitmask, named. Bit 2 = left operand, bit 1 = right operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpFixity {
    /// `a :op: b` — 11.
    Infix,
    /// `a :op:` — 10.
    Postfix,
    /// `:op: a` — 01.
    Prefix,
    /// `:op:` — 00: no operand input; still code that expands to an
    /// expression, and useful for exactly that.
    Niladic,
}

impl OpFixity {
    pub fn bits(self) -> u8 {
        match self {
            OpFixity::Infix => 3,
            OpFixity::Postfix => 2,
            OpFixity::Prefix => 1,
            OpFixity::Niladic => 0,
        }
    }
    pub fn from_bits(b: u8) -> Option<OpFixity> {
        Some(match b {
            3 => OpFixity::Infix,
            2 => OpFixity::Postfix,
            1 => OpFixity::Prefix,
            0 => OpFixity::Niladic,
            _ => return None,
        })
    }
    pub fn operand_count(self) -> u16 {
        (self.bits() & 1) as u16 + ((self.bits() >> 1) & 1) as u16
    }
    pub fn name(self) -> &'static str {
        match self {
            OpFixity::Infix => "infix",
            OpFixity::Postfix => "postfix",
            OpFixity::Prefix => "prefix",
            OpFixity::Niladic => "niladic",
        }
    }
}

/// One registered operator, as `list_operators` reports it.
#[derive(Debug, Clone)]
pub struct OpInfo {
    pub symbol: String,
    pub fixity: OpFixity,
    pub spell_hash_hex: String,
    pub doc: String,
}

/// A scanned `op/` record: `(symbol, fixity, spell hash, doc)`.
type OpRecord = (String, OpFixity, [u8; 32], String);

const OP_RECORD_VERSION: u8 = 1;

fn encode_op_record(fixity: OpFixity, hash: &[u8; 32], doc: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(34 + doc.len());
    v.push(OP_RECORD_VERSION);
    v.push(fixity.bits());
    v.extend_from_slice(hash);
    v.extend_from_slice(doc.as_bytes());
    v
}

fn decode_op_record(bytes: &[u8]) -> Option<(OpFixity, [u8; 32], String)> {
    if bytes.len() < 34 || bytes[0] != OP_RECORD_VERSION {
        return None;
    }
    let fixity = OpFixity::from_bits(bytes[1])?;
    let hash: [u8; 32] = bytes[2..34].try_into().ok()?;
    let doc = String::from_utf8_lossy(&bytes[34..]).into_owned();
    Some((fixity, hash, doc))
}

fn validate_symbol(symbol: &str) -> Result<()> {
    if symbol.is_empty() || symbol.len() > 16 {
        return Err(Error::Unsupported(
            "an operator symbol is 1..=16 bytes between colons".into(),
        ));
    }
    if symbol.chars().any(|c| c == ':' || c.is_whitespace()) {
        return Err(Error::Unsupported(
            "an operator symbol cannot contain `:` or whitespace".into(),
        ));
    }
    Ok(())
}

impl crate::Database {
    /// Define (or redefine) a custom operator: a symbol, its fixity, and the
    /// PySpell macro that receives the operands' SOURCE TEXT and returns the
    /// SQL fragment to splice. The spell's arity must equal the fixity's
    /// operand count.
    pub fn create_operator(
        &self,
        symbol: &str,
        fixity: OpFixity,
        lang: SpellLang,
        source: &str,
        doc: &str,
    ) -> Result<String> {
        validate_symbol(symbol)?;
        let skeleton = match lang {
            SpellLang::Python => mpedb_spell::py::compile(source)?,
            SpellLang::Rust => mpedb_spell::rs::compile(source)?,
        };
        if !skeleton.calls.is_empty() {
            return Err(Error::Unsupported(
                "an operator macro cannot run SQL — it RETURNS SQL text".into(),
            ));
        }
        if skeleton.argc != fixity.operand_count() {
            return Err(Error::Unsupported(format!(
                "a {} operator takes {} operand(s), but the macro takes {} argument(s)",
                fixity.name(),
                fixity.operand_count(),
                skeleton.argc
            )));
        }
        let proc = Proc::new(
            skeleton.name.clone(),
            skeleton.argc,
            skeleton.nlocals,
            Vec::new(),
            skeleton.consts,
            skeleton.instrs,
        )?;
        let blob = proc.encode();
        let hash = proc.hash();

        let record = encode_op_record(fixity, &hash.0, doc);
        let blob_key = crate::sys_record_subkey(crate::spellfn::NS_FUNC_HASH, &hash.0)?;
        let op_key = crate::sys_record_subkey(NS_OP, symbol.as_bytes())?;
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        let res = (|| {
            w.sys_put(&blob_key, &blob)?;
            w.sys_put(&op_key, &record)?;
            w.bump_schema_gen();
            Ok(())
        })();
        match res {
            Ok(()) => w.commit()?,
            Err(e) => {
                w.abort();
                return Err(e);
            }
        }
        self.cache.write().expect(crate::POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(hash.to_string())
    }

    pub fn drop_operator(&self, symbol: &str) -> Result<bool> {
        let op_key = crate::sys_record_subkey(NS_OP, symbol.as_bytes())?;
        let mut w = self.engine.begin_write_deadline(self.busy_deadline())?;
        let existed = match w.sys_delete(&op_key) {
            Ok(existed) => existed,
            Err(e) => {
                w.abort();
                return Err(e);
            }
        };
        if !existed {
            w.abort();
            return Ok(false);
        }
        w.bump_schema_gen();
        w.commit()?;
        self.cache.write().expect(crate::POISON).clear();
        let _ = self.engine.reload_schema_from_catalog();
        Ok(true)
    }

    /// Every registered operator — also the data behind `mpedb op list`. (The
    /// SQL-queryable `mpedb_operators` system table is deferred until the
    /// synthetic-table seam exists; the CLI and this API are the v1 windows.)
    pub fn list_operators(&self) -> Result<Vec<OpInfo>> {
        let r = self.engine.begin_read()?;
        let out = self.scan_op_records(&r)?;
        r.finish()?;
        Ok(out
            .into_iter()
            .map(|(symbol, fixity, hash, doc)| OpInfo {
                symbol,
                fixity,
                spell_hash_hex: mpedb_spell::ProcHash(hash).to_string(),
                doc,
            })
            .collect())
    }

    fn scan_op_records(
        &self,
        r: &mpedb_core::engine::ReadTxn<'_>,
    ) -> Result<Vec<OpRecord>> {
        let mut lo = NS_OP.as_bytes().to_vec();
        lo.push(0);
        let mut hi = NS_OP.as_bytes().to_vec();
        hi.push(1);
        let mut out = Vec::new();
        for (k, v) in r.sys_scan_range(&lo, &hi)? {
            let symbol = String::from_utf8_lossy(&k[lo.len()..]).into_owned();
            let Some((fixity, hash, doc)) = decode_op_record(&v) else {
                continue; // advisory: a corrupt record = "that symbol is not defined"
            };
            out.push((symbol, fixity, hash, doc));
        }
        Ok(out)
    }

    /// The operator catalog for one compile: fixity map + an expander closure
    /// running each operator's stored macro under the M2 budget.
    pub(crate) fn load_ops(
        &self,
        r: &mpedb_core::engine::ReadTxn<'_>,
    ) -> Result<mpedb_sql::OpSet> {
        let mut set = mpedb_sql::OpSet::default();
        let records = self.scan_op_records(r)?;
        if records.is_empty() {
            return Ok(set);
        }
        let mut resolved: Vec<(String, Arc<Proc>)> = Vec::with_capacity(records.len());
        for (symbol, fixity, hash, _doc) in &records {
            set.insert(symbol.clone(), fixity.bits());
            let proc = self.load_proc_by_hash(r, hash)?.ok_or_else(|| {
                Error::Corrupt(format!(":{symbol}: names a spell blob that is missing"))
            })?;
            resolved.push((symbol.clone(), proc));
        }
        set.set_expander(Arc::new(move |symbol: &str, operands: &[&str]| {
            let proc = resolved
                .iter()
                .find(|(s, _)| s == symbol)
                .map(|(_, p)| p)
                .ok_or_else(|| {
                    Error::Unsupported(format!(":{symbol}: is not in this catalog"))
                })?;
            let args: Vec<Value> =
                operands.iter().map(|o| Value::Text((*o).to_string())).collect();
            match crate::spellfn::call_spell_fn(proc, &args)? {
                Value::Text(s) => Ok(s),
                other => Err(Error::TypeMismatch(format!(
                    ":{symbol}: macro returned {} — an operator macro must return SQL text",
                    other.type_name()
                ))),
            }
        }));
        Ok(set)
    }

    /// Install the model-driven operators (SQL-EXTENSIONS.md): the M1 model's
    /// ROLES are what tell the sugar which tables it means.
    ///
    /// - `role = "edge"` with a two-column `traverse` declaration `[src, dst]`
    ///   installs `:->:` — `a :->: b` expands to an EXISTS over that edge
    ///   table ("there is an edge from a to b").
    /// - `role = "embedding"` with a `knn` declaration installs `:~:` —
    ///   `emb :~: $q` expands to `vec_l2(emb, $q)`, composing with
    ///   `ORDER BY … LIMIT k` into exactly the exact-kNN fast path's shape.
    ///
    /// Returns the installed symbols. Refusals name what the model must
    /// declare — the operator is only ever as good as the declaration.
    pub fn install_model_operators(&self) -> Result<Vec<String>> {
        use mpedb_types::model::{AccessKind, TableRole};
        let Some(model) = self.model()? else {
            return Err(Error::Unsupported(
                "no model stored — `mpedb model set` first; roles are what tell \
                 the operators which tables they mean"
                    .into(),
            ));
        };
        let mut installed = Vec::new();
        for t in &model.tables {
            match t.role {
                Some(TableRole::Edge) => {
                    let Some(tr) = t.access.iter().find(|a| a.kind == AccessKind::Traverse)
                    else {
                        continue;
                    };
                    if tr.columns.len() != 2 {
                        return Err(Error::Unsupported(format!(
                            "edge table `{}`: a traverse declaration needs [source, \
                             destination] columns for `:->:` to know both ends",
                            t.name
                        )));
                    }
                    let (table, src, dst) = (&t.name, &tr.columns[0], &tr.columns[1]);
                    let body = format!(
                        "def op_edge(l, r):\n    return \"EXISTS (SELECT 1 FROM {table} \
                         WHERE {table}.{src} = (\" + l + \") AND {table}.{dst} = (\" + r + \"))\"\n"
                    );
                    self.create_operator(
                        "->",
                        OpFixity::Infix,
                        SpellLang::Python,
                        &body,
                        &format!("edge step over `{table}` ({src} → {dst}); a :->: b = an edge exists"),
                    )?;
                    installed.push("->".to_string());
                }
                Some(TableRole::Embedding) => {
                    if !t.access.iter().any(|a| a.kind == AccessKind::Knn) {
                        continue;
                    }
                    let body = "def op_near(l, r):\n    return \"vec_l2((\" + l + \"), (\" + r + \"))\"\n";
                    self.create_operator(
                        "~",
                        OpFixity::Infix,
                        SpellLang::Python,
                        body,
                        "vector distance: a :~: b = vec_l2(a, b); ORDER BY emb :~: $q LIMIT k is exact kNN",
                    )?;
                    installed.push("~".to_string());
                }
                _ => {}
            }
        }
        if installed.is_empty() {
            return Err(Error::Unsupported(
                "the model declares no edge/embedding roles with traverse/knn access — \
                 nothing to install"
                    .into(),
            ));
        }
        Ok(installed)
    }

    /// Load + decode + verify one content-addressed spell blob (shared by the
    /// function table and the operator catalog).
    pub(crate) fn load_proc_by_hash(
        &self,
        r: &mpedb_core::engine::ReadTxn<'_>,
        hash: &[u8; 32],
    ) -> Result<Option<Arc<Proc>>> {
        if let Some(p) = self.spell_cache.read().expect(crate::POISON).get(hash) {
            return Ok(Some(p.clone()));
        }
        let blob_key = crate::sys_record_subkey(crate::spellfn::NS_FUNC_HASH, hash)?;
        let Some(blob) = r.sys_get(&blob_key)? else {
            return Ok(None);
        };
        if *blake3::hash(&blob).as_bytes() != *hash {
            return Err(Error::Corrupt("stored spell blob does not match its hash".into()));
        }
        let proc = Arc::new(Proc::decode(&blob)?);
        if proc.has_db_ops() {
            return Err(Error::Corrupt("stored spell blob contains database operations".into()));
        }
        self.spell_cache.write().expect(crate::POISON).insert(*hash, proc.clone());
        Ok(Some(proc))
    }
}
