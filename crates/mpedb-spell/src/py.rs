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
//! - `for row in db.rows("SQL", [args...]):` — a streaming cursor pulling
//!   one row per iteration (O(1) memory in the result size), each row bound
//!   as a tuple. Compiles to `CursorOpen`/`CursorAdvance`/`CursorRow`;
//!   allowed only in read-only procedures (the IR's v1 cursor rule).
//!   `break`/`continue` work; a `break` leaves the cursor open until the
//!   call ends (at most `interp::MAX_CURSORS` cursors may be open at once).
//! - `for x in <expr>:` over a materialized list or tuple (a `db.query`
//!   result, a row), and `for i in range(stop)` / `range(start, stop)` /
//!   `range(start, stop, step)` where `step` is a non-zero int literal.
//!   Both are **frontend sugar**: they compile to the same hidden-index
//!   `while` a user would write by hand (`Len`/`Index`/`Lt`/`Jump`), so the
//!   IR, its validator and stored blobs are unchanged. The iterable is
//!   evaluated once. `range` arguments must be ints (not checked).
//! - chained comparisons (`a < b <= c`, the middle evaluated once),
//!   conditional expressions (`x if c else y`), and `x in [a, b]` /
//!   `x not in (a, b)` against a **list or tuple literal** (an `==` chain);
//! - `sql.NAME(args...)` — one scalar SQL function call, e.g.
//!   `sql.upper(s)`, `sql.round(x, 2)`, `sql.strftime("%Y-%m-%d", "now")`.
//!   Sugar for `db.query("SELECT NAME($1, ...)", [args...])[0][0]`: compiled
//!   at define time like every embedded statement (an unknown function is a
//!   define error), costs one db call, and has that SQL function's
//!   semantics — which is why it does not borrow Python's method names.
//!   A plain string literal argument is written into the statement as a
//!   SQL literal (so `sql.date("now")` binds like `date('now')`); every
//!   other argument is a `$n` parameter.
//!
//! - list displays `[a, b, *more]` and dict displays `{"k": v, **more}`
//!   (text keys), `d["k"]`, `len(d)`, and `x in c` for a list, a row, a
//!   dict (its keys) or text (a substring). Lists and dicts are **values**,
//!   as tuples are in Python: there is no in-place change (`xs.append(v)`,
//!   `d["k"] = v` and `xs[i] = v` are compile errors), so a container can
//!   never change behind another variable. Grow one with a new display:
//!   `xs = [*xs, v]`, `d = {**d, "k": v}`. The one place this differs from
//!   CPython is `xs += [v]` after `ys = xs`: here `ys` keeps its value,
//!   which is why `+` does not join lists at all. Compiles to the IR's
//!   built-container ops; depth, size and work are bounded there.
//!
//! - `other(a, b)` — a call to another stored procedure, by name, with
//!   positional arguments (lists and dicts included). The name is resolved
//!   when THIS procedure is defined and fixed to that definition's content
//!   hash: the callee must already exist, a procedure cannot call itself,
//!   and redefining the callee changes nothing here until this procedure is
//!   defined again. The callee runs inside the caller's transaction and
//!   budgets. Not available in stored functions and lenses (which may not
//!   touch the database), nor in a trigger body.
//!
//! The `for` target is one plain name (no tuple unpacking), and there is no
//! `for`/`else`. A `for` over a dict is a run-time error (no key order to
//! borrow from an index).
//!
//! Python semantics notes: `/` on two ints yields a float, `//` floors,
//! `%` takes the divisor's sign, int overflow is an error (no bigints).
//! Everything else — imports, attribute access (other than `db.*` /
//! `sql.*` call heads), comprehensions, f-strings, classes, lambdas, ...
//! — is a compile error with source location.

use crate::emit::{cerr, line_col, CallKind, FuncBuilder, ProcCall, Skeleton, SqlCall};
use mpedb_types::{Error, Result, Value};
use rustpython_parser::ast::{self, Ranged};
use rustpython_parser::{parse, Mode};
use std::collections::HashMap;

const LANG: &str = "py";

/// Compile a function, lens, operator or trigger body: everything in the
/// subset except calls to other stored procedures.
pub fn compile(src: &str) -> Result<Skeleton> {
    compile_with(src, false)
}

/// Compile a stored procedure, which may also call other stored procedures
/// by name. Only the procedure engine can link those calls, so only it uses
/// this entry point.
pub fn compile_procedure(src: &str) -> Result<Skeleton> {
    compile_with(src, true)
}

fn compile_with(src: &str, allow_calls: bool) -> Result<Skeleton> {
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
        name: f.name.to_string(),
        allow_calls,
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
        proc_calls: c.b.proc_calls,
    })
}

struct Compiler<'a> {
    src: &'a str,
    b: FuncBuilder,
    locals: HashMap<String, u16>,
    /// The procedure being compiled (a call to it is refused).
    name: String,
    /// Whether calls to other stored procedures may be compiled.
    allow_calls: bool,
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
                if matches!(target, ast::Expr::Subscript(_)) {
                    return Err(self.err(
                        a,
                        "lists and dicts are values and cannot be changed in place; build a \
                         new one: `d = {**d, \"k\": v}` or `xs = [*xs, v]`",
                    ));
                }
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
            ast::Stmt::For(f) => self.for_stmt(f),
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

    /// `for <name> in <iterable>:` — three shapes, chosen by the iterable:
    /// `db.rows(...)` (a streaming cursor), `range(...)` (a counter), and
    /// anything else (a materialized list/tuple walked by a hidden index).
    fn for_stmt(&mut self, f: &ast::StmtFor) -> Result<()> {
        if !f.orelse.is_empty() {
            return Err(self.err(f, "for/else is not supported"));
        }
        let ast::Expr::Name(target) = &*f.target else {
            return Err(self.err(
                f,
                "the for target must be a plain variable (no tuple unpacking)",
            ));
        };
        let target_slot = self.locals[target.id.as_str()]; // prescan guarantees presence
        if let ast::Expr::Call(call) = &*f.iter {
            match &*call.func {
                ast::Expr::Attribute(a)
                    if a.attr.as_str() == "rows"
                        && matches!(&*a.value, ast::Expr::Name(n) if n.id.as_str() == "db") =>
                {
                    return self.for_rows(f, call, target_slot);
                }
                ast::Expr::Name(n)
                    if n.id.as_str() == "range" && !self.locals.contains_key("range") =>
                {
                    return self.for_range(f, call, target_slot);
                }
                _ => {}
            }
        }
        self.for_seq(f, target_slot)
    }

    /// `for <name> in db.rows("SQL", [args...]):` — a streaming cursor.
    /// Compiled shape:
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
    fn for_rows(&mut self, f: &ast::StmtFor, call: &ast::ExprCall, target_slot: u16) -> Result<()> {
        // <args...>; CursorOpen — via the shared db-call path.
        self.db_call(call, CallKind::Rows)?;
        // Hidden temp holding the cursor handle; anonymous, so it can never
        // collide with a user variable.
        let cur = self.b.alloc_local().map_err(|e| self.werr(f, e))?;
        self.b.emit(Op::StoreLocal(cur))?;
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

    /// `for <name> in <expr>:` over a materialized list or tuple. Pure
    /// sugar for the index loop the subset already had — no new IR:
    ///
    /// ```text
    ///   <expr>; StoreLocal(seq); LoadConst(0); StoreLocal(i)   ; hidden temps
    ///   Jump(check)
    /// start:
    ///   LoadLocal(i); LoadConst(1); Add; StoreLocal(i)
    /// check:
    ///   LoadLocal(i); LoadLocal(seq); Len; Lt; JumpIfFalse(end)
    ///   LoadLocal(seq); LoadLocal(i); Index; StoreLocal(target)
    ///   <body>
    ///   Jump(start)
    /// end:
    /// ```
    ///
    /// The iterable is evaluated once, so rebinding its variable in the body
    /// does not change the walk (as in Python). A value that is not a list
    /// or tuple fails at run time in `Len`/`Index`.
    fn for_seq(&mut self, f: &ast::StmtFor, target_slot: u16) -> Result<()> {
        self.expr(&f.iter)?;
        let seq = self.b.alloc_local().map_err(|e| self.werr(f, e))?;
        self.b.emit(Op::StoreLocal(seq))?;
        let i = self.b.alloc_local().map_err(|e| self.werr(f, e))?;
        let zero = self.b.const_idx(Value::Int(0))?;
        self.b.emit(Op::LoadConst(zero))?;
        self.b.emit(Op::StoreLocal(i))?;
        let to_check = self.b.emit_jump(Op::Jump(u32::MAX))?;
        let start = self.b.here();
        let one = self.b.const_idx(Value::Int(1))?;
        self.b.emit(Op::LoadLocal(i))?;
        self.b.emit(Op::LoadConst(one))?;
        self.b.emit(Op::Add)?;
        self.b.emit(Op::StoreLocal(i))?;
        self.b.patch_to_here(to_check);
        self.b.emit(Op::LoadLocal(i))?;
        self.b.emit(Op::LoadLocal(seq))?;
        self.b.emit(Op::Len)?;
        self.b.emit(Op::Lt)?;
        let jf = self.b.emit_jump(Op::JumpIfFalse(u32::MAX))?;
        self.b.emit(Op::LoadLocal(seq))?;
        self.b.emit(Op::LoadLocal(i))?;
        self.b.emit(Op::Index)?;
        self.b.emit(Op::StoreLocal(target_slot))?;
        self.b.push_loop(start);
        self.stmts(&f.body)?;
        self.b.emit(Op::Jump(start))?; // the backward jump
        self.b.patch_to_here(jf);
        self.b.pop_loop();
        Ok(())
    }

    /// `for <name> in range(stop | start, stop | start, stop, step):` — a
    /// hidden counter; the same shape as [`Self::for_seq`] with the bound
    /// test `i < stop` (`i > stop` for a negative step). `step` must be a
    /// non-zero int literal so the direction is known at compile time.
    /// `start` and `stop` are evaluated once, in that order. The target is a
    /// copy of the counter, so assigning to it does not change the walk.
    fn for_range(&mut self, f: &ast::StmtFor, call: &ast::ExprCall, target_slot: u16) -> Result<()> {
        if !call.keywords.is_empty() {
            return Err(self.err(call, "keyword arguments are not supported"));
        }
        let (start_expr, stop_expr, step) = match &call.args[..] {
            [stop] => (None, stop, 1i64),
            [start, stop] => (Some(start), stop, 1i64),
            [start, stop, step] => {
                let literal = match step {
                    ast::Expr::Constant(c) => match &c.value {
                        ast::Constant::Int(big) => i64::try_from(big).ok(),
                        _ => None,
                    },
                    ast::Expr::UnaryOp(u) if matches!(u.op, ast::UnaryOp::USub) => {
                        match &*u.operand {
                            ast::Expr::Constant(c) => match &c.value {
                                ast::Constant::Int(big) => i64::try_from(&-big.clone()).ok(),
                                _ => None,
                            },
                            _ => None,
                        }
                    }
                    _ => None,
                };
                match literal {
                    Some(v) if v != 0 => (Some(start), stop, v),
                    _ => {
                        return Err(self.err(
                            step,
                            "the range step must be a non-zero int literal",
                        ))
                    }
                }
            }
            _ => return Err(self.err(call, "range() takes 1 to 3 arguments")),
        };
        let i = self.b.alloc_local().map_err(|e| self.werr(f, e))?;
        match start_expr {
            Some(e) => self.expr(e)?,
            None => {
                let zero = self.b.const_idx(Value::Int(0))?;
                self.b.emit(Op::LoadConst(zero))?;
            }
        }
        self.b.emit(Op::StoreLocal(i))?;
        self.expr(stop_expr)?;
        let stop = self.b.alloc_local().map_err(|e| self.werr(f, e))?;
        self.b.emit(Op::StoreLocal(stop))?;
        let to_check = self.b.emit_jump(Op::Jump(u32::MAX))?;
        let start = self.b.here();
        let step_const = self.b.const_idx(Value::Int(step))?;
        self.b.emit(Op::LoadLocal(i))?;
        self.b.emit(Op::LoadConst(step_const))?;
        self.b.emit(Op::Add)?;
        self.b.emit(Op::StoreLocal(i))?;
        self.b.patch_to_here(to_check);
        self.b.emit(Op::LoadLocal(i))?;
        self.b.emit(Op::LoadLocal(stop))?;
        self.b.emit(if step > 0 { Op::Lt } else { Op::Gt })?;
        let jf = self.b.emit_jump(Op::JumpIfFalse(u32::MAX))?;
        self.b.emit(Op::LoadLocal(i))?;
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
            ast::Expr::Compare(c) => self.compare(c),
            ast::Expr::IfExp(i) => {
                // body if test else orelse — both arms leave one value.
                self.expr(&i.test)?;
                let jf = self.b.emit_jump(Op::JumpIfFalse(u32::MAX))?;
                self.expr(&i.body)?;
                let jend = self.b.emit_jump(Op::Jump(u32::MAX))?;
                self.b.patch_to_here(jf);
                self.expr(&i.orelse)?;
                self.b.patch_to_here(jend);
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
            ast::Expr::List(l) => {
                // [a, *b, c]: start empty, then add one element or one
                // whole list at a time (operand stack depth stays at 2).
                self.b.emit(Op::NewList)?;
                for item in &l.elts {
                    if let ast::Expr::Starred(star) = item {
                        self.expr(&star.value)?;
                        self.b.emit(Op::ListExtend)?;
                    } else {
                        self.expr(item)?;
                        self.b.emit(Op::ListAppend)?;
                    }
                }
                Ok(())
            }
            ast::Expr::Dict(d) => {
                // {"k": v, **more}: later entries win, as in Python.
                self.b.emit(Op::NewDict)?;
                for (key, value) in d.keys.iter().zip(&d.values) {
                    match key {
                        Some(key) => {
                            self.expr(key)?;
                            self.expr(value)?;
                            self.b.emit(Op::DictSet)?;
                        }
                        None => {
                            self.expr(value)?;
                            self.b.emit(Op::DictMerge)?;
                        }
                    }
                }
                Ok(())
            }
            other => Err(self.err(other, format!("{} is not supported", expr_name(other)))),
        }
    }

    /// One comparison, a chain (`a < b <= c`: each middle operand is
    /// evaluated once, into a hidden temp, and the chain short-circuits on
    /// the first false link), or membership in a list/tuple literal.
    fn compare(&mut self, c: &ast::ExprCompare) -> Result<()> {
        if let ([op @ (ast::CmpOp::In | ast::CmpOp::NotIn)], [right]) =
            (&c.ops[..], &c.comparators[..])
        {
            return self.member(c, matches!(op, ast::CmpOp::NotIn), right);
        }
        let is_none = |e: &ast::Expr| matches!(e, ast::Expr::Constant(k) if k.value.is_none());
        let mut ops = Vec::with_capacity(c.ops.len());
        let mut left = &*c.left;
        for (op, right) in c.ops.iter().zip(&c.comparators) {
            ops.push(match op {
                ast::CmpOp::Eq => Op::Eq,
                ast::CmpOp::NotEq => Op::Ne,
                ast::CmpOp::Lt => Op::Lt,
                ast::CmpOp::LtE => Op::Le,
                ast::CmpOp::Gt => Op::Gt,
                ast::CmpOp::GtE => Op::Ge,
                ast::CmpOp::Is | ast::CmpOp::IsNot if is_none(left) || is_none(right) => {
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
                    return Err(self.err(c, "`in` cannot be part of a comparison chain"))
                }
            });
            left = right;
        }
        self.expr(&c.left)?;
        let last = ops.len() - 1;
        let tmp = if last > 0 {
            Some(self.b.alloc_local().map_err(|e| self.werr(c, e))?)
        } else {
            None
        };
        let mut ends = Vec::new();
        for (k, (ir, right)) in ops.into_iter().zip(&c.comparators).enumerate() {
            self.expr(right)?;
            match tmp {
                Some(t) if k < last => {
                    self.b.emit(Op::Dup)?;
                    self.b.emit(Op::StoreLocal(t))?;
                    self.b.emit(ir)?;
                    self.b.emit(Op::Dup)?;
                    ends.push(self.b.emit_jump(Op::JumpIfFalse(u32::MAX))?);
                    self.b.emit(Op::Pop)?;
                    self.b.emit(Op::LoadLocal(t))?;
                }
                _ => {
                    self.b.emit(ir)?;
                }
            }
        }
        for at in ends {
            self.b.patch_to_here(at);
        }
        Ok(())
    }

    /// `x in [a, b, ...]` / `x not in (a, b, ...)` against a list or tuple
    /// **literal** is an `==` chain, short-circuiting on the first match
    /// (a starred element makes it a real list). Against anything else it
    /// is the `Contains` op, decided by the container at run time.
    fn member(&mut self, c: &ast::ExprCompare, negate: bool, right: &ast::Expr) -> Result<()> {
        let plain = |elts: &[ast::Expr]| !elts.iter().any(|e| matches!(e, ast::Expr::Starred(_)));
        let items = match right {
            ast::Expr::List(l) if plain(&l.elts) => &l.elts,
            ast::Expr::Tuple(t) if plain(&t.elts) => &t.elts,
            _ => {
                self.expr(&c.left)?;
                self.expr(right)?;
                self.b.emit(Op::Contains)?;
                if negate {
                    self.b.emit(Op::Not)?;
                }
                return Ok(());
            }
        };
        self.expr(&c.left)?;
        let tmp = self.b.alloc_local().map_err(|e| self.werr(c, e))?;
        self.b.emit(Op::StoreLocal(tmp))?;
        let mut ends = Vec::new();
        if items.is_empty() {
            let f = self.b.const_idx(Value::Bool(false))?;
            self.b.emit(Op::LoadConst(f))?;
        }
        for (k, item) in items.iter().enumerate() {
            self.b.emit(Op::LoadLocal(tmp))?;
            self.expr(item)?;
            self.b.emit(Op::Eq)?;
            if k + 1 < items.len() {
                self.b.emit(Op::Dup)?;
                ends.push(self.b.emit_jump(Op::JumpIfTrue(u32::MAX))?);
                self.b.emit(Op::Pop)?;
            }
        }
        for at in ends {
            self.b.patch_to_here(at);
        }
        if negate {
            self.b.emit(Op::Not)?;
        }
        Ok(())
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

    /// The callables: `len(x)`, `db.query(...)`, `db.execute(...)`,
    /// `sql.NAME(...)` and other stored procedures by name.
    fn call(&mut self, call: &ast::ExprCall) -> Result<()> {
        if !call.keywords.is_empty() {
            return Err(self.err(call, "keyword arguments are not supported"));
        }
        match &*call.func {
            ast::Expr::Name(n) if n.id.as_str() == "range" => Err(self.err(
                call,
                "range(...) may only be used as the iterable of a for loop",
            )),
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
                    return Err(self.err(
                        call,
                        "methods do not exist here (only db.query(...)/db.execute(...) and \
                         sql.NAME(...) may be called); for text use a SQL function, \
                         e.g. sql.upper(s)",
                    ));
                };
                if matches!(a.attr.as_str(), "append" | "extend" | "update" | "pop" | "insert")
                    && !matches!(recv.id.as_str(), "db" | "sql")
                {
                    return Err(self.err(
                        call,
                        "lists and dicts are values and cannot be changed in place; build a \
                         new one: `xs = [*xs, v]` or `d = {**d, \"k\": v}`",
                    ));
                }
                if recv.id.as_str() == "sql" && !self.locals.contains_key("sql") {
                    return self.sql_function(call, a.attr.as_str());
                }
                if recv.id.as_str() != "db" {
                    return Err(self.err(
                        call,
                        "methods do not exist here (only db.query(...)/db.execute(...) and \
                         sql.NAME(...) may be called); for text use a SQL function, \
                         e.g. sql.upper(s)",
                    ));
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
            // Any other plain name is another stored procedure.
            ast::Expr::Name(n)
                if self.allow_calls && !self.locals.contains_key(n.id.as_str()) =>
            {
                if n.id.as_str() == self.name {
                    return Err(self.err(
                        call,
                        "a procedure cannot call itself (recursion does not exist here; \
                         use a loop)",
                    ));
                }
                let argc = call.args.len();
                if argc > crate::ir::MAX_CALL_ARGS {
                    return Err(self.err(call, "too many arguments"));
                }
                for arg in &call.args {
                    if matches!(arg, ast::Expr::Starred(_)) {
                        return Err(self.err(arg, "starred arguments are not supported"));
                    }
                    self.expr(arg)?;
                }
                let (line, col) = line_col(self.src, call.range().start().to_usize());
                let idx = self
                    .b
                    .add_proc_call(ProcCall {
                        name: n.id.to_string(),
                        argc: argc as u16,
                        line,
                        col,
                    })
                    .map_err(|e| self.werr(call, e))?;
                self.b.emit(Op::CallProc(idx))?;
                Ok(())
            }
            _ => Err(self.err(
                call,
                "only len(), db.query(...), db.execute(...), sql.NAME(...) and other \
                 stored procedures may be called",
            )),
        }
    }

    /// `sql.NAME(args...)`: one scalar SQL function call, compiled as the
    /// embedded statement `SELECT NAME($1, ..., $n)` and unwrapped with
    /// `[0][0]`. No new IR: it is a `DbQuery` like any other, linked at
    /// define time (so an unknown function fails there), and it costs one
    /// db call. String literal arguments become SQL literals. `NAME` is a plain ASCII identifier — nothing else can reach
    /// the SQL text.
    fn sql_function(&mut self, call: &ast::ExprCall, name: &str) -> Result<()> {
        let plain = !name.is_empty()
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && !name.as_bytes()[0].is_ascii_digit();
        if !plain {
            return Err(self.err(call, "sql.NAME(...): NAME must be a plain ASCII identifier"));
        }
        // A plain string literal is written into the statement as a SQL
        // literal, every other argument is a `$n` parameter. Some SQL
        // functions read a literal at bind time (`date('now')`), and a
        // parameter cannot stand in for one. Only plain text is inlined
        // (no backslash, no control character), with
        // `'` doubled; anything else stays a parameter.
        let mut rendered = Vec::with_capacity(call.args.len());
        let mut argc = 0usize;
        for arg in &call.args {
            if let ast::Expr::Constant(k) = arg {
                if let ast::Constant::Str(text) = &k.value {
                    if !text.chars().any(|ch| ch == '\\' || ch.is_control()) {
                        rendered.push(format!("'{}'", text.replace('\'', "''")));
                        continue;
                    }
                }
            }
            self.expr(arg)?;
            argc += 1;
            rendered.push(format!("${argc}"));
        }
        if argc > crate::ir::MAX_DB_ARGS {
            return Err(self.err(call, "too many SQL parameters"));
        }
        let (line, col) = line_col(self.src, call.range().start().to_usize());
        let plan_idx = self
            .b
            .add_call(SqlCall {
                sql: format!("SELECT {name}({})", rendered.join(", ")),
                kind: CallKind::Query,
                argc: argc as u8,
                expected_columns: None,
                line,
                col,
            })
            .map_err(|e| self.werr(call, e))?;
        self.b.emit(Op::DbQuery(plan_idx))?;
        let zero = self.b.const_idx(Value::Int(0))?;
        for _ in 0..2 {
            self.b.emit(Op::LoadConst(zero))?;
            self.b.emit(Op::Index)?;
        }
        Ok(())
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
                expected_columns: None,
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
            // The for target is an assigned name.
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
        // Plain `for` is handled in stmt(); only async reaches this fallback.
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
