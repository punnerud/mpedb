//! Python frontend: a deliberately small subset of Python, parsed with
//! `rustpython-parser` **on the host at define time only**. The runtime
//! never sees Python source — that is the PySpell security boundary.
//!
//! # Accepted
//!
//! - exactly one `def name(a, b, ...):` per source (no defaults, no
//!   `*args`/`**kwargs`, no keyword-only/positional-only markers, no
//!   annotations, no decorators);
//! - literals: int (i64 range), float, str, `True`/`False`/`None`;
//! - operators: `+ - * / // %`, unary `-`/`not`, comparisons
//!   `== != < <= > >=`, `is None` / `is not None`, `and`/`or` (value-
//!   preserving short circuit), augmented assignment (`+=` etc.);
//! - statements: assignment to a name, `if`/`elif`/`else`, `while`,
//!   `break`/`continue`, `return`, `pass`, bare docstrings;
//! - `len(x)`, indexing `rows[i]` / `row[j]` (negative indices wrap);
//! - `db.query("SQL", [args...])` and `db.execute("SQL", [args...])` where
//!   the SQL is a **string literal** and the optional second argument is a
//!   **list literal** of expressions;
//! - `for row in db.rows("SQL", [args...]):` — the ONLY `for` form: a
//!   streaming cursor pulling one row per iteration (O(1) memory in the
//!   result size), each row bound as a tuple. Compiles to
//!   `CursorOpen`/`CursorAdvance`/`CursorRow`; allowed only in read-only
//!   procedures (the IR's v1 cursor rule). `break`/`continue` work; a
//!   `break` leaves the cursor open until the call ends (at most
//!   `interp::MAX_CURSORS` cursors may be open at once).
//!
//! General `for` loops stay intentionally omitted: `while` with an index
//! covers iteration over materialized query results, and keeping the
//! statement set minimal keeps the IR and its validator small.
//!
//! Python semantics notes: `/` on two ints yields a float, `//` floors,
//! `%` takes the divisor's sign, int overflow is an error (no bigints).
//! Everything else — imports, attribute access (other than `db.query` /
//! `db.execute` heads), comprehensions, f-strings, classes, lambdas,
//! chained comparisons, ... — is a compile error with source location.

use crate::emit::{cerr, line_col, CallKind, FuncBuilder, Skeleton, SqlCall};
use mpedb_types::{Error, Result, Value};
use rustpython_parser::ast::{self, Ranged};
use rustpython_parser::{parse, Mode};
use std::collections::HashMap;

const LANG: &str = "py";

pub fn compile(src: &str) -> Result<Skeleton> {
    let module = parse(src, Mode::Module, "<proc>").map_err(|e| {
        let (l, c) = line_col(src, e.offset.to_usize());
        cerr(LANG, l, c, format!("syntax error: {}", e.error))
    })?;
    let ast::Mod::Module(m) = module else {
        return Err(cerr(LANG, 1, 0, "expected a module"));
    };
    let mut stmts = m.body.iter();
    let (Some(ast::Stmt::FunctionDef(f)), None) = (stmts.next(), stmts.next()) else {
        return Err(cerr(
            LANG,
            1,
            0,
            "a procedure is exactly one `def name(...):` — nothing else at module level",
        ));
    };
    let mut c = Compiler {
        src,
        b: FuncBuilder::new(),
        locals: HashMap::new(),
    };
    c.compile_function(f)?;
    let argc = f.args.args.len() as u16;
    Ok(Skeleton {
        name: f.name.to_string(),
        argc,
        nlocals: c.b.nlocals(),
        consts: c.b.consts,
        instrs: c.b.instrs,
        calls: c.b.calls,
    })
}

struct Compiler<'a> {
    src: &'a str,
    b: FuncBuilder,
    locals: HashMap<String, u16>,
}

use crate::ir::Op;

impl Compiler<'_> {
    fn err(&self, node: &impl Ranged, msg: impl AsRef<str>) -> Error {
        let (l, c) = line_col(self.src, node.range().start().to_usize());
        cerr(LANG, l, c, msg)
    }

    fn compile_function(&mut self, f: &ast::StmtFunctionDef) -> Result<()> {
        if !f.decorator_list.is_empty() {
            return Err(self.err(f, "decorators are not supported"));
        }
        if !f.type_params.is_empty() {
            return Err(self.err(f, "type parameters are not supported"));
        }
        if f.returns.is_some() {
            return Err(self.err(f, "return annotations are not supported"));
        }
        let a = &*f.args;
        if a.vararg.is_some() || a.kwarg.is_some() {
            return Err(self.err(f, "*args/**kwargs are not supported"));
        }
        if !a.posonlyargs.is_empty() || !a.kwonlyargs.is_empty() {
            return Err(self.err(f, "positional-only/keyword-only markers are not supported"));
        }
        for arg in &a.args {
            if arg.default.is_some() {
                return Err(self.err(f, "parameter defaults are not supported"));
            }
            if arg.def.annotation.is_some() {
                return Err(self.err(f, "parameter annotations are not supported"));
            }
            let name = arg.def.arg.to_string();
            let slot = self.b.alloc_local().map_err(|e| self.werr(f, e))?;
            if self.locals.insert(name.clone(), slot).is_some() {
                return Err(self.err(f, format!("duplicate parameter `{name}`")));
            }
        }
        // Pre-scan: every name assigned anywhere in the body is a local
        // (Python function scoping). Loads of anything else are compile
        // errors; loads before the first dynamic store are runtime errors.
        let mut assigned = Vec::new();
        collect_assigned(&f.body, &mut assigned);
        for name in assigned {
            if !self.locals.contains_key(&name) {
                let slot = self.b.alloc_local().map_err(|e| self.werr(f, e))?;
                self.locals.insert(name, slot);
            }
        }
        self.stmts(&f.body)?;
        // Implicit `return None` (unreachable if every path returns).
        let none = self.b.const_idx(Value::Null)?;
        self.b.emit(Op::LoadConst(none))?;
        self.b.emit(Op::Return)?;
        Ok(())
    }

    /// Wrap a location-less limit error with a node position.
    fn werr(&self, node: &impl Ranged, e: Error) -> Error {
        match e {
            Error::Unsupported(m) => self.err(node, m),
            other => other,
        }
    }

    fn stmts(&mut self, body: &[ast::Stmt]) -> Result<()> {
        for s in body {
            self.stmt(s)?;
        }
        Ok(())
    }

    fn stmt(&mut self, s: &ast::Stmt) -> Result<()> {
        match s {
            ast::Stmt::Pass(_) => Ok(()),
            ast::Stmt::Return(r) => {
                match &r.value {
                    Some(v) => self.expr(v)?,
                    None => {
                        let none = self.b.const_idx(Value::Null)?;
                        self.b.emit(Op::LoadConst(none))?;
                    }
                }
                self.b.emit(Op::Return)?;
                Ok(())
            }
            ast::Stmt::Assign(a) => {
                let [target] = &a.targets[..] else {
                    return Err(self.err(a, "chained assignment (a = b = ...) is not supported"));
                };
                let ast::Expr::Name(n) = target else {
                    return Err(self.err(
                        a,
                        "only plain variables can be assigned (no tuple unpacking or subscripts)",
                    ));
                };
                self.expr(&a.value)?;
                let slot = self.locals[n.id.as_str()]; // prescan guarantees presence
                self.b.emit(Op::StoreLocal(slot))?;
                Ok(())
            }
            ast::Stmt::AugAssign(a) => {
                let ast::Expr::Name(n) = &*a.target else {
                    return Err(self.err(a, "augmented assignment target must be a variable"));
                };
                let slot = self.locals[n.id.as_str()];
                self.b.emit(Op::LoadLocal(slot))?;
                self.expr(&a.value)?;
                self.b.emit(self.binop(a, &a.op)?)?;
                self.b.emit(Op::StoreLocal(slot))?;
                Ok(())
            }
            ast::Stmt::If(i) => {
                self.expr(&i.test)?;
                let jf = self.b.emit_jump(Op::JumpIfFalse(u32::MAX))?;
                self.stmts(&i.body)?;
                if i.orelse.is_empty() {
                    self.b.patch_to_here(jf);
                } else {
                    let jend = self.b.emit_jump(Op::Jump(u32::MAX))?;
                    self.b.patch_to_here(jf);
                    self.stmts(&i.orelse)?; // elif = nested If in orelse
                    self.b.patch_to_here(jend);
                }
                Ok(())
            }
            ast::Stmt::While(w) => {
                if !w.orelse.is_empty() {
                    return Err(self.err(w, "while/else is not supported"));
                }
                let start = self.b.here();
                self.expr(&w.test)?;
                let jf = self.b.emit_jump(Op::JumpIfFalse(u32::MAX))?;
                self.b.push_loop(start);
                self.stmts(&w.body)?;
                self.b.emit(Op::Jump(start))?; // the backward jump
                self.b.patch_to_here(jf);
                self.b.pop_loop();
                Ok(())
            }
            ast::Stmt::For(f) => self.for_rows(f),
            ast::Stmt::Break(x) => {
                if !self.b.emit_break()? {
                    return Err(self.err(x, "break outside of a loop"));
                }
                Ok(())
            }
            ast::Stmt::Continue(x) => {
                if !self.b.emit_continue()? {
                    return Err(self.err(x, "continue outside of a loop"));
                }
                Ok(())
            }
            ast::Stmt::Expr(e) => {
                // Bare constants (docstrings) compile to nothing.
                if matches!(&*e.value, ast::Expr::Constant(_)) {
                    return Ok(());
                }
                self.expr(&e.value)?;
                self.b.emit(Op::Pop)?;
                Ok(())
            }
            other => Err(self.err(other, format!("{} is not supported", stmt_name(other)))),
        }
    }

    /// The ONLY `for` form: `for <name> in db.rows("SQL", [args...]):` — a
    /// streaming cursor. General iteration stays rejected. Compiled shape:
    ///
    /// ```text
    ///   <args...>; CursorOpen(p); StoreLocal(cur)     ; cur = hidden temp
    /// start:
    ///   LoadLocal(cur); CursorAdvance; JumpIfFalse(end)
    ///   LoadLocal(cur); CursorRow; StoreLocal(target)
    ///   <body>
    ///   Jump(start)
    /// end:
    /// ```
    ///
    /// `continue` jumps to `start` (re-advance); `break` jumps to `end`.
    fn for_rows(&mut self, f: &ast::StmtFor) -> Result<()> {
        if !f.orelse.is_empty() {
            return Err(self.err(f, "for/else is not supported"));
        }
        let ast::Expr::Name(target) = &*f.target else {
            return Err(self.err(
                f,
                "the for target must be a plain variable (no tuple unpacking)",
            ));
        };
        let is_db_rows = |call: &ast::ExprCall| -> bool {
            matches!(&*call.func, ast::Expr::Attribute(a)
                if a.attr.as_str() == "rows"
                    && matches!(&*a.value, ast::Expr::Name(n) if n.id.as_str() == "db"))
        };
        let ast::Expr::Call(call) = &*f.iter else {
            return Err(self.err(
                &*f.iter,
                "for is only supported over db.rows(\"SQL\", [args...]) — \
                 general iteration does not exist here",
            ));
        };
        if !is_db_rows(call) {
            return Err(self.err(
                &*f.iter,
                "for is only supported over db.rows(\"SQL\", [args...]) — \
                 general iteration does not exist here",
            ));
        }
        // <args...>; CursorOpen — via the shared db-call path.
        self.db_call(call, CallKind::Rows)?;
        // Hidden temp holding the cursor handle; anonymous, so it can never
        // collide with a user variable.
        let cur = self.b.alloc_local().map_err(|e| self.werr(f, e))?;
        self.b.emit(Op::StoreLocal(cur))?;
        let target_slot = self.locals[target.id.as_str()]; // prescan guarantees presence
        let start = self.b.here();
        self.b.emit(Op::LoadLocal(cur))?;
        self.b.emit(Op::CursorAdvance)?;
        let jf = self.b.emit_jump(Op::JumpIfFalse(u32::MAX))?;
        self.b.emit(Op::LoadLocal(cur))?;
        self.b.emit(Op::CursorRow)?;
        self.b.emit(Op::StoreLocal(target_slot))?;
        self.b.push_loop(start);
        self.stmts(&f.body)?;
        self.b.emit(Op::Jump(start))?; // the backward jump
        self.b.patch_to_here(jf);
        self.b.pop_loop();
        Ok(())
    }

    fn binop(&self, node: &impl Ranged, op: &ast::Operator) -> Result<Op> {
        Ok(match op {
            ast::Operator::Add => Op::Add,
            ast::Operator::Sub => Op::Sub,
            ast::Operator::Mult => Op::Mul,
            ast::Operator::Div => Op::TrueDiv,
            ast::Operator::FloorDiv => Op::FloorDiv,
            ast::Operator::Mod => Op::PyMod,
            other => {
                return Err(self.err(
                    node,
                    format!("operator {other:?} is not supported (only + - * / // %)"),
                ))
            }
        })
    }

    fn expr(&mut self, e: &ast::Expr) -> Result<()> {
        match e {
            ast::Expr::Constant(c) => self.constant(c),
            ast::Expr::Name(n) => match self.locals.get(n.id.as_str()) {
                Some(&slot) => {
                    self.b.emit(Op::LoadLocal(slot))?;
                    Ok(())
                }
                None if n.id.as_str() == "db" => Err(self.err(
                    n,
                    "`db` may only be used as db.query(\"SQL\", [...]) or db.execute(\"SQL\", [...])",
                )),
                None => Err(self.err(n, format!("undefined variable `{}`", n.id))),
            },
            ast::Expr::BinOp(b) => {
                self.expr(&b.left)?;
                self.expr(&b.right)?;
                let op = self.binop(b, &b.op)?;
                self.b.emit(op)?;
                Ok(())
            }
            ast::Expr::UnaryOp(u) => match u.op {
                ast::UnaryOp::USub => {
                    // Fold -<int literal> so i64::MIN is writable.
                    if let ast::Expr::Constant(c) = &*u.operand {
                        if let ast::Constant::Int(big) = &c.value {
                            let v = i64::try_from(&-big.clone()).map_err(|_| {
                                self.err(u, "integer literal out of i64 range")
                            })?;
                            let idx = self.b.const_idx(Value::Int(v))?;
                            self.b.emit(Op::LoadConst(idx))?;
                            return Ok(());
                        }
                    }
                    self.expr(&u.operand)?;
                    self.b.emit(Op::Neg)?;
                    Ok(())
                }
                ast::UnaryOp::Not => {
                    self.expr(&u.operand)?;
                    self.b.emit(Op::Not)?;
                    Ok(())
                }
                ast::UnaryOp::UAdd => self.expr(&u.operand),
                ast::UnaryOp::Invert => Err(self.err(u, "bitwise ~ is not supported")),
            },
            ast::Expr::BoolOp(b) => {
                // Python value-preserving short circuit:
                //   a and b  ==>  a if falsey else b
                let jump = |op: &ast::BoolOp| match op {
                    ast::BoolOp::And => Op::JumpIfFalse(u32::MAX),
                    ast::BoolOp::Or => Op::JumpIfTrue(u32::MAX),
                };
                let mut ends = Vec::new();
                let (first, rest) = b.values.split_first().expect("parser: >= 2 values");
                self.expr(first)?;
                for v in rest {
                    self.b.emit(Op::Dup)?;
                    ends.push(self.b.emit_jump(jump(&b.op))?);
                    self.b.emit(Op::Pop)?;
                    self.expr(v)?;
                }
                for at in ends {
                    self.b.patch_to_here(at);
                }
                Ok(())
            }
            ast::Expr::Compare(c) => {
                let ([op], [right]) = (&c.ops[..], &c.comparators[..]) else {
                    return Err(self.err(
                        c,
                        "chained comparisons are not supported; write `a < b and b < c`",
                    ));
                };
                let is_none =
                    |e: &ast::Expr| matches!(e, ast::Expr::Constant(k) if k.value.is_none());
                let ir = match op {
                    ast::CmpOp::Eq => Op::Eq,
                    ast::CmpOp::NotEq => Op::Ne,
                    ast::CmpOp::Lt => Op::Lt,
                    ast::CmpOp::LtE => Op::Le,
                    ast::CmpOp::Gt => Op::Gt,
                    ast::CmpOp::GtE => Op::Ge,
                    ast::CmpOp::Is | ast::CmpOp::IsNot
                        if is_none(&c.left) || is_none(right) =>
                    {
                        if matches!(op, ast::CmpOp::Is) {
                            Op::Eq
                        } else {
                            Op::Ne
                        }
                    }
                    ast::CmpOp::Is | ast::CmpOp::IsNot => {
                        return Err(self.err(
                            c,
                            "`is` is only supported against None (identity does not exist here)",
                        ))
                    }
                    ast::CmpOp::In | ast::CmpOp::NotIn => {
                        return Err(self.err(c, "`in` is not supported"))
                    }
                };
                self.expr(&c.left)?;
                self.expr(right)?;
                self.b.emit(ir)?;
                Ok(())
            }
            ast::Expr::Call(call) => self.call(call),
            ast::Expr::Subscript(s) => {
                if matches!(&*s.slice, ast::Expr::Slice(_)) {
                    return Err(self.err(s, "slicing is not supported, only single indexing"));
                }
                self.expr(&s.value)?;
                self.expr(&s.slice)?;
                self.b.emit(Op::Index)?;
                Ok(())
            }
            ast::Expr::Attribute(a) => Err(self.err(
                a,
                "attribute access is not allowed (db.query/db.execute are the only dotted forms)",
            )),
            ast::Expr::List(l) => Err(self.err(
                l,
                "list literals are only allowed as the argument list of db.query/db.execute",
            )),
            other => Err(self.err(other, format!("{} is not supported", expr_name(other)))),
        }
    }

    fn constant(&mut self, c: &ast::ExprConstant) -> Result<()> {
        let v = match &c.value {
            ast::Constant::None => Value::Null,
            ast::Constant::Bool(b) => Value::Bool(*b),
            ast::Constant::Int(big) => Value::Int(
                i64::try_from(big)
                    .map_err(|_| self.err(c, "integer literal out of i64 range"))?,
            ),
            ast::Constant::Float(f) => Value::Float(*f),
            ast::Constant::Str(s) => Value::Text(s.clone()),
            ast::Constant::Bytes(_) => {
                return Err(self.err(c, "bytes literals are not supported"))
            }
            other => {
                return Err(self.err(c, format!("literal {other:?} is not supported")))
            }
        };
        let idx = self.b.const_idx(v)?;
        self.b.emit(Op::LoadConst(idx))?;
        Ok(())
    }

    /// The only three callables: `len(x)`, `db.query(...)`, `db.execute(...)`.
    fn call(&mut self, call: &ast::ExprCall) -> Result<()> {
        if !call.keywords.is_empty() {
            return Err(self.err(call, "keyword arguments are not supported"));
        }
        match &*call.func {
            ast::Expr::Name(n) if n.id.as_str() == "len" => {
                let [arg] = &call.args[..] else {
                    return Err(self.err(call, "len() takes exactly one argument"));
                };
                self.expr(arg)?;
                self.b.emit(Op::Len)?;
                Ok(())
            }
            ast::Expr::Attribute(a) => {
                let ast::Expr::Name(recv) = &*a.value else {
                    return Err(self.err(call, "only db.query(...)/db.execute(...) may be called"));
                };
                if recv.id.as_str() != "db" {
                    return Err(self.err(call, "only db.query(...)/db.execute(...) may be called"));
                }
                let kind = match a.attr.as_str() {
                    "query" => CallKind::Query,
                    "execute" => CallKind::Exec,
                    "rows" => {
                        return Err(self.err(
                            call,
                            "db.rows(...) may only be used as the iterable of a \
                             for loop: `for row in db.rows(\"SQL\", [args...]):`",
                        ))
                    }
                    m => {
                        return Err(self.err(
                            call,
                            format!(
                                "db.{m} does not exist; use db.query, db.execute \
                                 or `for row in db.rows(...)`"
                            ),
                        ))
                    }
                };
                self.db_call(call, kind)
            }
            _ => Err(self.err(
                call,
                "only len(), db.query(...) and db.execute(...) may be called",
            )),
        }
    }

    fn db_call(&mut self, call: &ast::ExprCall, kind: CallKind) -> Result<()> {
        let (sql_expr, list) = match &call.args[..] {
            [s] => (s, None),
            [s, l] => (s, Some(l)),
            _ => {
                return Err(self.err(
                    call,
                    "db calls take (\"SQL\") or (\"SQL\", [args...])",
                ))
            }
        };
        let ast::Expr::Constant(k) = sql_expr else {
            return Err(self.err(
                sql_expr,
                "SQL must be a string literal — it is compiled once at define time, \
                 never at run time",
            ));
        };
        let ast::Constant::Str(sql) = &k.value else {
            return Err(self.err(sql_expr, "SQL must be a string literal"));
        };
        let mut argc = 0usize;
        if let Some(l) = list {
            let ast::Expr::List(items) = l else {
                return Err(self.err(
                    l,
                    "db call arguments must be a list literal: db.query(\"...\", [a, b])",
                ));
            };
            argc = items.elts.len();
            for item in &items.elts {
                self.expr(item)?;
            }
        }
        if argc > u8::MAX as usize {
            return Err(self.err(call, "too many SQL parameters"));
        }
        let (line, col) = line_col(self.src, call.range().start().to_usize());
        let plan_idx = self
            .b
            .add_call(SqlCall {
                sql: sql.clone(),
                kind,
                argc: argc as u8,
                line,
                col,
            })
            .map_err(|e| self.werr(call, e))?;
        self.b.emit(match kind {
            CallKind::Query => Op::DbQuery(plan_idx),
            CallKind::Exec => Op::DbExec(plan_idx),
            CallKind::Rows => Op::CursorOpen(plan_idx),
        })?;
        Ok(())
    }
}

/// Pre-scan for assigned names in first-assignment order (Python function
/// scoping: any name assigned anywhere in the body is a local everywhere).
fn collect_assigned(body: &[ast::Stmt], out: &mut Vec<String>) {
    let push = |id: &str, out: &mut Vec<String>| {
        if !out.iter().any(|n| n == id) {
            out.push(id.to_owned());
        }
    };
    for s in body {
        match s {
            ast::Stmt::Assign(a) => {
                if let [ast::Expr::Name(n)] = &a.targets[..] {
                    push(n.id.as_str(), out);
                }
            }
            ast::Stmt::AugAssign(a) => {
                if let ast::Expr::Name(n) = &*a.target {
                    push(n.id.as_str(), out);
                }
            }
            ast::Stmt::If(i) => {
                collect_assigned(&i.body, out);
                collect_assigned(&i.orelse, out);
            }
            ast::Stmt::While(w) => {
                collect_assigned(&w.body, out);
                collect_assigned(&w.orelse, out);
            }
            // The for target is an assigned name (only db.rows fors compile,
            // but pre-scanning rejected forms is harmless).
            ast::Stmt::For(f) => {
                if let ast::Expr::Name(n) = &*f.target {
                    push(n.id.as_str(), out);
                }
                collect_assigned(&f.body, out);
                collect_assigned(&f.orelse, out);
            }
            _ => {}
        }
    }
}

fn stmt_name(s: &ast::Stmt) -> &'static str {
    match s {
        ast::Stmt::Import(_) | ast::Stmt::ImportFrom(_) => "import",
        ast::Stmt::ClassDef(_) => "class definition",
        ast::Stmt::FunctionDef(_) | ast::Stmt::AsyncFunctionDef(_) => "nested function definition",
        // Plain `for` is handled in stmt() (db.rows only); only async
        // reaches this fallback.
        ast::Stmt::For(_) | ast::Stmt::AsyncFor(_) => "async for loop",
        ast::Stmt::With(_) | ast::Stmt::AsyncWith(_) => "with statement",
        ast::Stmt::Try(_) | ast::Stmt::TryStar(_) => "try/except",
        ast::Stmt::Raise(_) => "raise",
        ast::Stmt::Assert(_) => "assert",
        ast::Stmt::Global(_) | ast::Stmt::Nonlocal(_) => "global/nonlocal",
        ast::Stmt::Delete(_) => "del",
        ast::Stmt::AnnAssign(_) => "annotated assignment (use a plain assignment)",
        ast::Stmt::Match(_) => "match statement",
        _ => "this statement",
    }
}

fn expr_name(e: &ast::Expr) -> &'static str {
    match e {
        ast::Expr::Lambda(_) => "lambda",
        ast::Expr::IfExp(_) => "conditional expression (use an if statement)",
        ast::Expr::Dict(_) => "dict literal",
        ast::Expr::Set(_) => "set literal",
        ast::Expr::ListComp(_) | ast::Expr::SetComp(_) | ast::Expr::DictComp(_) => "comprehension",
        ast::Expr::GeneratorExp(_) => "generator expression",
        ast::Expr::Await(_) | ast::Expr::Yield(_) | ast::Expr::YieldFrom(_) => "await/yield",
        ast::Expr::JoinedStr(_) | ast::Expr::FormattedValue(_) => "f-string",
        ast::Expr::Starred(_) => "starred expression",
        ast::Expr::Tuple(_) => "tuple literal",
        ast::Expr::NamedExpr(_) => "walrus operator",
        ast::Expr::Slice(_) => "slice",
        _ => "this expression",
    }
}
