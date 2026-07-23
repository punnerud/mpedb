//! Rust frontend: a small Rust subset parsed with `syn` on the host at
//! define time only. Compiles to the same IR as the Python frontend.
//!
//! # Accepted
//!
//! - exactly one `fn name(a: i64, b: &str, ...) -> i64 { ... }` per source
//!   (parameter/return types are limited to `i64`, `f64`, `bool`, `String`,
//!   `&str` and `()`; procedures are dynamically typed at runtime, the
//!   annotations are arity documentation);
//! - `let` / `let mut` (with shadowing and block scoping), assignment and
//!   compound assignment (`+= -= *= /= %=`) to `mut` variables;
//! - literals: integer (i64), float, string, bool; operators
//!   `+ - * / %` (Rust semantics: `/` on ints truncates, `%` takes the
//!   dividend's sign), `== != < <= > >=`, `&&`/`||` (short-circuit), unary
//!   `-`/`!`;
//! - `if`/`else if`/`else`, `while`, `break`/`continue` (no labels/values),
//!   `return`, plain `{ }` blocks, and a tail expression at the end of the
//!   function body (sugar for `return`);
//! - `x.len()`, indexing `rows[i]` / `row[j]`;
//! - `db.query("SQL", &[args...])` / `db.execute("SQL", &[args...])` with
//!   a **string literal** SQL and an (optionally `&`-referenced) array
//!   literal of arguments;
//! - streaming cursors — one row per pull, O(1) memory in the result size,
//!   read-only procedures only (the IR's v1 cursor rule):
//!   `let c = db.rows("SQL", &[args...]);` opens a cursor,
//!   `db.cursor_next(c)` advances it (returns whether a row is available),
//!   `db.cursor_col(c, i)` reads column `i` of the current row (sugar for
//!   `CursorRow` + `Index`, so negative `i` wraps like any other index).
//!
//! `for`/`loop`, `match`, closures, paths (`a::b`), free-function calls,
//! references outside db-argument lists, bit operations and everything else
//! are compile errors with source location. `for` is intentionally omitted
//! (see the Python frontend note): `while db.cursor_next(c)` covers
//! streaming iteration, `while` + index covers materialized results.
//!
//! Runtime typing note: the IR is dynamically typed; `if 1 { }` is not
//! rejected statically and evaluates via truthiness. For well-typed Rust
//! sources the semantics coincide with Rust. Division differs from the
//! Python frontend **by design**: this frontend emits `IntDiv`/`IntRem`.

use crate::emit::{cerr, CallKind, FuncBuilder, Skeleton, SqlCall};
use crate::ir::Op;
use mpedb_types::{Error, Result, Value};
use syn::spanned::Spanned;

const LANG: &str = "rs";

pub fn compile(src: &str) -> Result<Skeleton> {
    let file: syn::File = syn::parse_str(src).map_err(|e| {
        let lc = e.span().start();
        cerr(LANG, lc.line, lc.column, format!("syntax error: {e}"))
    })?;
    let mut items = file.items.iter();
    let (Some(syn::Item::Fn(f)), None) = (items.next(), items.next()) else {
        return Err(cerr(
            LANG,
            1,
            0,
            "a procedure is exactly one `fn name(...) { ... }` — nothing else at file level",
        ));
    };
    let mut c = Compiler {
        b: FuncBuilder::new(),
        scopes: vec![Vec::new()],
    };
    let (name, argc) = c.compile_function(f)?;
    Ok(Skeleton {
        name,
        argc,
        nlocals: c.b.nlocals(),
        consts: c.b.consts,
        instrs: c.b.instrs,
        calls: c.b.calls,
    })
}

/// (name, slot, mutable) triples; inner scopes shadow outer ones.
type Scope = Vec<(String, u16, bool)>;

struct Compiler {
    b: FuncBuilder,
    scopes: Vec<Scope>,
}

fn serr(node: &impl Spanned, msg: impl AsRef<str>) -> Error {
    let lc = node.span().start();
    cerr(LANG, lc.line, lc.column, msg)
}

fn type_ok(ty: &syn::Type) -> bool {
    match ty {
        syn::Type::Path(p) => {
            p.qself.is_none()
                && p.path.segments.len() == 1
                && matches!(
                    p.path.segments[0].ident.to_string().as_str(),
                    "i64" | "f64" | "bool" | "String"
                )
        }
        syn::Type::Reference(r) => {
            r.mutability.is_none() && matches!(&*r.elem, syn::Type::Path(p)
                if p.path.is_ident("str"))
        }
        syn::Type::Tuple(t) => t.elems.is_empty(),
        syn::Type::Paren(p) => type_ok(&p.elem),
        _ => false,
    }
}

impl Compiler {
    fn compile_function(&mut self, f: &syn::ItemFn) -> Result<(String, u16)> {
        let sig = &f.sig;
        if !f.attrs.is_empty() {
            return Err(serr(f, "attributes are not supported"));
        }
        if sig.asyncness.is_some()
            || sig.unsafety.is_some()
            || sig.constness.is_some()
            || sig.abi.is_some()
            || sig.variadic.is_some()
            || !sig.generics.params.is_empty()
            || sig.generics.where_clause.is_some()
        {
            return Err(serr(
                sig,
                "async/unsafe/const/extern/generic functions are not supported",
            ));
        }
        if let syn::ReturnType::Type(_, ty) = &sig.output {
            if !type_ok(ty) {
                return Err(serr(
                    ty,
                    "unsupported return type (use i64, f64, bool, String, &str or ())",
                ));
            }
        }
        let mut argc = 0u16;
        for input in &sig.inputs {
            let syn::FnArg::Typed(pat) = input else {
                return Err(serr(input, "self parameters are not supported"));
            };
            let syn::Pat::Ident(id) = &*pat.pat else {
                return Err(serr(pat, "parameters must be plain identifiers"));
            };
            if !type_ok(&pat.ty) {
                return Err(serr(
                    &pat.ty,
                    "unsupported parameter type (use i64, f64, bool, String or &str)",
                ));
            }
            let name = id.ident.to_string();
            if self.lookup(&name).is_some() {
                return Err(serr(id, format!("duplicate parameter `{name}`")));
            }
            let slot = self.b.alloc_local().map_err(|e| relocate(e, input))?;
            self.scopes[0].push((name, slot, id.mutability.is_some()));
            argc += 1;
        }
        self.block(&f.block, true)?;
        // Implicit `return ()` — Null stands in for unit.
        let none = self.b.const_idx(Value::Null)?;
        self.b.emit(Op::LoadConst(none))?;
        self.b.emit(Op::Return)?;
        Ok((sig.ident.to_string(), argc))
    }

    fn lookup(&self, name: &str) -> Option<(u16, bool)> {
        for scope in self.scopes.iter().rev() {
            for (n, slot, m) in scope.iter().rev() {
                if n == name {
                    return Some((*slot, *m));
                }
            }
        }
        None
    }

    /// Compile a block. `fn_body`: a trailing expression without `;`
    /// becomes the return value; in nested blocks it is rejected.
    fn block(&mut self, block: &syn::Block, fn_body: bool) -> Result<()> {
        self.scopes.push(Vec::new());
        let last = block.stmts.len().wrapping_sub(1);
        for (i, s) in block.stmts.iter().enumerate() {
            self.stmt(s, fn_body && i == last)?;
        }
        self.scopes.pop();
        Ok(())
    }

    fn stmt(&mut self, s: &syn::Stmt, tail_ok: bool) -> Result<()> {
        match s {
            syn::Stmt::Local(l) => self.let_stmt(l),
            syn::Stmt::Expr(e, semi) => self.expr_stmt(e, semi.is_none(), tail_ok),
            syn::Stmt::Item(i) => Err(serr(i, "nested items are not supported")),
            syn::Stmt::Macro(m) => Err(serr(m, "macros are not supported")),
        }
    }

    fn let_stmt(&mut self, l: &syn::Local) -> Result<()> {
        if !l.attrs.is_empty() {
            return Err(serr(l, "attributes are not supported"));
        }
        // Unwrap `let x: T = ...` typed patterns.
        let (pat, ty) = match &l.pat {
            syn::Pat::Type(t) => (&*t.pat, Some(&*t.ty)),
            p => (p, None),
        };
        if let Some(ty) = ty {
            if !type_ok(ty) {
                return Err(serr(
                    ty,
                    "unsupported type annotation (use i64, f64, bool, String or &str)",
                ));
            }
        }
        let syn::Pat::Ident(id) = pat else {
            return Err(serr(&l.pat, "let patterns must be plain identifiers"));
        };
        if let Some(init) = &l.init {
            if init.diverge.is_some() {
                return Err(serr(l, "let-else is not supported"));
            }
            self.expr(&init.expr)?;
        }
        let slot = self.b.alloc_local().map_err(|e| relocate(e, l))?;
        if l.init.is_some() {
            self.b.emit(Op::StoreLocal(slot))?;
        }
        // Declared after the initializer compiles: `let x = x + 1;`
        // resolves the right-hand `x` to the shadowed outer binding.
        self.scopes
            .last_mut()
            .expect("scope stack never empty")
            .push((id.ident.to_string(), slot, id.mutability.is_some()));
        Ok(())
    }

    /// A statement-position expression. `no_semi` = no trailing `;`.
    fn expr_stmt(&mut self, e: &syn::Expr, no_semi: bool, tail_ok: bool) -> Result<()> {
        match e {
            syn::Expr::If(i) => self.if_stmt(i),
            syn::Expr::While(w) => {
                if w.label.is_some() {
                    return Err(serr(w, "loop labels are not supported"));
                }
                let start = self.b.here();
                self.expr(&w.cond)?;
                let jf = self.b.emit_jump(Op::JumpIfFalse(u32::MAX))?;
                self.b.push_loop(start);
                self.block(&w.body, false)?;
                self.b.emit(Op::Jump(start))?; // the backward jump
                self.b.patch_to_here(jf);
                self.b.pop_loop();
                Ok(())
            }
            syn::Expr::Block(b) => {
                if b.label.is_some() {
                    return Err(serr(b, "block labels are not supported"));
                }
                self.block(&b.block, false)
            }
            syn::Expr::Return(r) => {
                match &r.expr {
                    Some(v) => self.expr(v)?,
                    None => {
                        let none = self.b.const_idx(Value::Null)?;
                        self.b.emit(Op::LoadConst(none))?;
                    }
                }
                self.b.emit(Op::Return)?;
                Ok(())
            }
            syn::Expr::Break(br) => {
                if br.label.is_some() || br.expr.is_some() {
                    return Err(serr(br, "break with label/value is not supported"));
                }
                if !self.b.emit_break()? {
                    return Err(serr(br, "break outside of a loop"));
                }
                Ok(())
            }
            syn::Expr::Continue(c) => {
                if c.label.is_some() {
                    return Err(serr(c, "loop labels are not supported"));
                }
                if !self.b.emit_continue()? {
                    return Err(serr(c, "continue outside of a loop"));
                }
                Ok(())
            }
            syn::Expr::ForLoop(_) | syn::Expr::Loop(_) => Err(serr(
                e,
                "for/loop are not supported; iterate with while + index",
            )),
            syn::Expr::Assign(a) => {
                let (slot, mutable, name) = self.assign_target(&a.left)?;
                self.expr(&a.right)?;
                if !mutable {
                    return Err(serr(
                        a,
                        format!("cannot assign to `{name}`: not declared `mut`"),
                    ));
                }
                self.b.emit(Op::StoreLocal(slot))?;
                Ok(())
            }
            syn::Expr::Binary(b) if assign_op(&b.op).is_some() => {
                let op = assign_op(&b.op).expect("checked");
                let (slot, mutable, name) = self.assign_target(&b.left)?;
                if !mutable {
                    return Err(serr(
                        b,
                        format!("cannot assign to `{name}`: not declared `mut`"),
                    ));
                }
                self.b.emit(Op::LoadLocal(slot))?;
                self.expr(&b.right)?;
                self.b.emit(op)?;
                self.b.emit(Op::StoreLocal(slot))?;
                Ok(())
            }
            // Tail expression of the function body = return value.
            _ if no_semi && tail_ok => {
                self.expr(e)?;
                self.b.emit(Op::Return)?;
                Ok(())
            }
            _ if no_semi => Err(serr(
                e,
                "blocks do not yield values here; use an explicit `return`",
            )),
            _ => {
                self.expr(e)?;
                self.b.emit(Op::Pop)?;
                Ok(())
            }
        }
    }

    fn assign_target(&self, e: &syn::Expr) -> Result<(u16, bool, String)> {
        let syn::Expr::Path(p) = e else {
            return Err(serr(e, "assignment target must be a plain variable"));
        };
        let name = path_ident(p).ok_or_else(|| serr(e, "assignment target must be a plain variable"))?;
        let (slot, mutable) = self
            .lookup(&name)
            .ok_or_else(|| serr(e, format!("undefined variable `{name}`")))?;
        Ok((slot, mutable, name))
    }

    fn if_stmt(&mut self, i: &syn::ExprIf) -> Result<()> {
        if matches!(&*i.cond, syn::Expr::Let(_)) {
            return Err(serr(i, "if-let is not supported"));
        }
        self.expr(&i.cond)?;
        let jf = self.b.emit_jump(Op::JumpIfFalse(u32::MAX))?;
        self.block(&i.then_branch, false)?;
        match &i.else_branch {
            None => self.b.patch_to_here(jf),
            Some((_, els)) => {
                let jend = self.b.emit_jump(Op::Jump(u32::MAX))?;
                self.b.patch_to_here(jf);
                match &**els {
                    syn::Expr::If(nested) => self.if_stmt(nested)?, // else if
                    syn::Expr::Block(b) => self.block(&b.block, false)?,
                    other => return Err(serr(other, "unsupported else form")),
                }
                self.b.patch_to_here(jend);
            }
        }
        Ok(())
    }

    fn expr(&mut self, e: &syn::Expr) -> Result<()> {
        match e {
            syn::Expr::Lit(l) => self.lit(&l.lit),
            syn::Expr::Paren(p) => self.expr(&p.expr),
            syn::Expr::Group(g) => self.expr(&g.expr),
            syn::Expr::Path(p) => {
                let name = path_ident(p)
                    .ok_or_else(|| serr(p, "paths (a::b) are not supported"))?;
                if name == "db" {
                    return Err(serr(
                        p,
                        "`db` may only be used as db.query(\"SQL\", &[...]) or db.execute(...)",
                    ));
                }
                let (slot, _) = self
                    .lookup(&name)
                    .ok_or_else(|| serr(p, format!("undefined variable `{name}`")))?;
                self.b.emit(Op::LoadLocal(slot))?;
                Ok(())
            }
            syn::Expr::Unary(u) => match u.op {
                syn::UnOp::Neg(_) => {
                    // Fold -<int literal> so i64::MIN is writable.
                    if let syn::Expr::Lit(l) = &*u.expr {
                        if let syn::Lit::Int(i) = &l.lit {
                            let digits: i128 = i
                                .base10_parse()
                                .map_err(|_| serr(l, "integer literal out of range"))?;
                            let v = i64::try_from(-digits)
                                .map_err(|_| serr(l, "integer literal out of i64 range"))?;
                            let idx = self.b.const_idx(Value::Int(v))?;
                            self.b.emit(Op::LoadConst(idx))?;
                            return Ok(());
                        }
                    }
                    self.expr(&u.expr)?;
                    self.b.emit(Op::Neg)?;
                    Ok(())
                }
                syn::UnOp::Not(_) => {
                    self.expr(&u.expr)?;
                    self.b.emit(Op::Not)?;
                    Ok(())
                }
                _ => Err(serr(u, "unsupported unary operator")),
            },
            syn::Expr::Binary(b) => {
                if assign_op(&b.op).is_some() {
                    return Err(serr(b, "compound assignment is a statement, not an expression"));
                }
                match &b.op {
                    syn::BinOp::And(_) | syn::BinOp::Or(_) => {
                        // Short-circuit; exact Rust semantics on bools.
                        let jop = if matches!(b.op, syn::BinOp::And(_)) {
                            Op::JumpIfFalse(u32::MAX)
                        } else {
                            Op::JumpIfTrue(u32::MAX)
                        };
                        self.expr(&b.left)?;
                        self.b.emit(Op::Dup)?;
                        let end = self.b.emit_jump(jop)?;
                        self.b.emit(Op::Pop)?;
                        self.expr(&b.right)?;
                        self.b.patch_to_here(end);
                        Ok(())
                    }
                    op => {
                        let ir = match op {
                            syn::BinOp::Add(_) => Op::Add,
                            syn::BinOp::Sub(_) => Op::Sub,
                            syn::BinOp::Mul(_) => Op::Mul,
                            syn::BinOp::Div(_) => Op::IntDiv, // Rust semantics
                            syn::BinOp::Rem(_) => Op::IntRem, // Rust semantics
                            syn::BinOp::Eq(_) => Op::Eq,
                            syn::BinOp::Ne(_) => Op::Ne,
                            syn::BinOp::Lt(_) => Op::Lt,
                            syn::BinOp::Le(_) => Op::Le,
                            syn::BinOp::Gt(_) => Op::Gt,
                            syn::BinOp::Ge(_) => Op::Ge,
                            other => {
                                return Err(serr(
                                    b,
                                    format!(
                                        "operator {} is not supported",
                                        quote_op(other)
                                    ),
                                ))
                            }
                        };
                        self.expr(&b.left)?;
                        self.expr(&b.right)?;
                        self.b.emit(ir)?;
                        Ok(())
                    }
                }
            }
            syn::Expr::MethodCall(m) => self.method_call(m),
            syn::Expr::Index(ix) => {
                self.expr(&ix.expr)?;
                self.expr(&ix.index)?;
                self.b.emit(Op::Index)?;
                Ok(())
            }
            syn::Expr::Call(c) => Err(serr(
                c,
                "free function calls are not supported; \
                 only db.query(...), db.execute(...) and x.len()",
            )),
            syn::Expr::Field(f) => Err(serr(
                f,
                "field access is not allowed (db.query/db.execute are the only dotted forms)",
            )),
            syn::Expr::Reference(r) => Err(serr(
                r,
                "references are only allowed around db argument arrays: db.query(\"...\", &[a])",
            )),
            syn::Expr::If(_) | syn::Expr::Match(_) | syn::Expr::Block(_) => Err(serr(
                e,
                "control flow is statement-only here (no if/match/block expressions)",
            )),
            syn::Expr::ForLoop(_) | syn::Expr::Loop(_) => Err(serr(
                e,
                "for/loop are not supported; iterate with while + index",
            )),
            syn::Expr::Closure(_) => Err(serr(e, "closures are not supported")),
            syn::Expr::Macro(_) => Err(serr(e, "macros are not supported")),
            other => Err(serr(other, "this expression is not supported")),
        }
    }

    fn lit(&mut self, l: &syn::Lit) -> Result<()> {
        let v = match l {
            syn::Lit::Int(i) => Value::Int(
                i.base10_parse::<i64>()
                    .map_err(|_| serr(l, "integer literal out of i64 range"))?,
            ),
            syn::Lit::Float(f) => Value::Float(
                f.base10_parse::<f64>()
                    .map_err(|_| serr(l, "invalid float literal"))?,
            ),
            syn::Lit::Str(s) => Value::Text(s.value()),
            syn::Lit::Bool(b) => Value::Bool(b.value),
            _ => return Err(serr(l, "unsupported literal")),
        };
        let idx = self.b.const_idx(v)?;
        self.b.emit(Op::LoadConst(idx))?;
        Ok(())
    }

    fn method_call(&mut self, m: &syn::ExprMethodCall) -> Result<()> {
        if m.turbofish.is_some() {
            return Err(serr(m, "turbofish is not supported"));
        }
        let method = m.method.to_string();
        let recv_is_db = matches!(&*m.receiver, syn::Expr::Path(p)
            if path_ident(p).as_deref() == Some("db"));
        match (recv_is_db, method.as_str()) {
            (true, "query") => self.db_call(m, CallKind::Query),
            (true, "execute") => self.db_call(m, CallKind::Exec),
            (true, "rows") => self.db_call(m, CallKind::Rows),
            // db.cursor_next(c): advance; pushes bool (row available).
            (true, "cursor_next") => {
                let mut args = m.args.iter();
                let (Some(cur), None) = (args.next(), args.next()) else {
                    return Err(serr(m, "db.cursor_next takes exactly one cursor"));
                };
                self.expr(cur)?;
                self.b.emit(Op::CursorAdvance)?;
                Ok(())
            }
            // db.cursor_col(c, i): current row's column i.
            (true, "cursor_col") => {
                let mut args = m.args.iter();
                let (Some(cur), Some(idx), None) = (args.next(), args.next(), args.next())
                else {
                    return Err(serr(
                        m,
                        "db.cursor_col takes a cursor and a column index",
                    ));
                };
                self.expr(cur)?;
                self.b.emit(Op::CursorRow)?;
                self.expr(idx)?;
                self.b.emit(Op::Index)?;
                Ok(())
            }
            (true, other) => Err(serr(
                m,
                format!(
                    "db.{other} does not exist; use db.query, db.execute, \
                     db.rows, db.cursor_next or db.cursor_col"
                ),
            )),
            (false, "len") => {
                if !m.args.is_empty() {
                    return Err(serr(m, "len() takes no arguments"));
                }
                self.expr(&m.receiver)?;
                self.b.emit(Op::Len)?;
                Ok(())
            }
            (false, other) => Err(serr(
                m,
                format!("method .{other}() is not supported (only .len())"),
            )),
        }
    }

    fn db_call(&mut self, m: &syn::ExprMethodCall, kind: CallKind) -> Result<()> {
        let mut args = m.args.iter();
        let (Some(sql_expr), arr, None) = (args.next(), args.next(), args.next()) else {
            return Err(serr(
                m,
                "db calls take (\"SQL\") or (\"SQL\", &[args...])",
            ));
        };
        let syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(sql),
            ..
        }) = sql_expr
        else {
            return Err(serr(
                sql_expr,
                "SQL must be a string literal — it is compiled once at define time, \
                 never at run time",
            ));
        };
        let mut argc = 0usize;
        if let Some(arr) = arr {
            // Accept both `&[a, b]` and `[a, b]`.
            let inner = match arr {
                syn::Expr::Reference(r) => &*r.expr,
                other => other,
            };
            let syn::Expr::Array(elems) = inner else {
                return Err(serr(
                    arr,
                    "db call arguments must be an array literal: db.query(\"...\", &[a, b])",
                ));
            };
            argc = elems.elems.len();
            for item in &elems.elems {
                self.expr(item)?;
            }
        }
        if argc > u8::MAX as usize {
            return Err(serr(m, "too many SQL parameters"));
        }
        let lc = m.span().start();
        let plan_idx = self
            .b
            .add_call(SqlCall {
                sql: sql.value(),
                kind,
                argc: argc as u8,
                line: lc.line,
                col: lc.column,
            })
            .map_err(|e| relocate(e, m))?;
        self.b.emit(match kind {
            CallKind::Query => Op::DbQuery(plan_idx),
            CallKind::Exec => Op::DbExec(plan_idx),
            CallKind::Rows => Op::CursorOpen(plan_idx),
        })?;
        Ok(())
    }
}

/// Compound-assignment operators map to the arithmetic op they wrap.
fn assign_op(op: &syn::BinOp) -> Option<Op> {
    Some(match op {
        syn::BinOp::AddAssign(_) => Op::Add,
        syn::BinOp::SubAssign(_) => Op::Sub,
        syn::BinOp::MulAssign(_) => Op::Mul,
        syn::BinOp::DivAssign(_) => Op::IntDiv,
        syn::BinOp::RemAssign(_) => Op::IntRem,
        _ => return None,
    })
}

fn quote_op(op: &syn::BinOp) -> &'static str {
    match op {
        syn::BinOp::BitAnd(_) => "& (bitwise)",
        syn::BinOp::BitOr(_) => "| (bitwise)",
        syn::BinOp::BitXor(_) => "^",
        syn::BinOp::Shl(_) => "<<",
        syn::BinOp::Shr(_) => ">>",
        _ => "this operator",
    }
}

fn path_ident(p: &syn::ExprPath) -> Option<String> {
    if p.qself.is_none() && p.path.segments.len() == 1 && p.path.segments[0].arguments.is_none() {
        Some(p.path.segments[0].ident.to_string())
    } else {
        None
    }
}

/// Reattach a source location to a location-less limit error.
fn relocate(e: Error, node: &impl Spanned) -> Error {
    match e {
        Error::Unsupported(m) => serr(node, m),
        other => other,
    }
}
