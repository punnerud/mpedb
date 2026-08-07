//! The PL/pgSQL body: a single-pass recursive-descent parser that EMITS as it
//! parses, straight into the shared [`FuncBuilder`].
//!
//! Single-pass because there is no reason for an AST here: PL/pgSQL's control
//! flow is structured (no `goto`, every block explicitly terminated), so every
//! jump target is either known when the jump is emitted or patchable at the
//! matching `END`. That is the same shape the Python and Rust frontends have.
//!
//! # What the IR can and cannot say, and where that bites
//!
//! The interpreter uses LANGUAGE semantics, not SQL's three-valued logic — the
//! module docs of `mpedb-proc` state it, and it is the one place a PL/pgSQL
//! body can mean something different here than in PostgreSQL. Two consequences,
//! both handled rather than hoped over:
//!
//! * `IF <null> THEN` does NOT take the branch, because the interpreter's
//!   truthiness maps `Null` to false. That MATCHES PostgreSQL, where a NULL
//!   condition is not true. No action needed, and it is worth saying so
//!   explicitly since it is the case one would expect to be wrong.
//! * `x = NULL` is NULL in PostgreSQL — never true, whatever `x` is — while the
//!   interpreter's `Eq` says `None == None` is true. Rather than emit code that
//!   quietly disagrees, a literal `NULL` on either side of `=`/`<>` is a
//!   COMPILE ERROR pointing at `IS NULL`. In PostgreSQL that comparison is
//!   already a bug (it can never be true), so refusing it costs nothing real.

use crate::emit::FuncBuilder;
use crate::ir::Op;
use mpedb_types::{Result, Value};
use std::collections::HashMap;

use super::lex::{err, Lexed, Tok};

/// Where a name resolves to. Parameters and DECLAREd variables share one flat
/// space of local slots — PL/pgSQL lets a DECLARE shadow a parameter, and the
/// map records the innermost binding, which is what shadowing means.
type Scope = HashMap<String, u16>;

pub struct Body<'a> {
    src: &'a str,
    t: &'a [Lexed],
    i: usize,
    pub b: FuncBuilder,
    scope: Scope,
}

impl<'a> Body<'a> {
    pub fn new(src: &'a str, t: &'a [Lexed], params: &[String]) -> Result<Body<'a>> {
        let mut b = FuncBuilder::new();
        let mut scope = Scope::new();
        for (n, p) in params.iter().enumerate() {
            let slot = b.alloc_local()?;
            debug_assert_eq!(slot as usize, n, "parameters occupy slots 0..argc");
            // Both spellings reach the same slot: PostgreSQL lets a named
            // parameter also be addressed positionally, and a dump's body may
            // use either.
            scope.insert(p.clone(), slot);
            scope.insert(format!("${}", n + 1), slot);
        }
        Ok(Body { src, t, i: 0, b, scope })
    }

    // ----------------------------------------------------------- the cursor

    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i).map(|l| &l.tok)
    }

    fn at(&self) -> usize {
        self.t.get(self.i).map_or(self.src.len(), |l| l.at)
    }

    fn err(&self, msg: impl AsRef<str>) -> mpedb_types::Error {
        err(self.src, self.at(), msg)
    }

    /// Consume the keyword `w` if it is next.
    fn eat_kw(&mut self, w: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Word(x)) if x == w) {
            self.i += 1;
            return true;
        }
        false
    }

    fn is_kw(&self, w: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(x)) if x == w)
    }

    fn expect_kw(&mut self, w: &str) -> Result<()> {
        if self.eat_kw(w) {
            return Ok(());
        }
        Err(self.err(format!("expected `{}`", w.to_uppercase())))
    }

    fn eat_punct(&mut self, p: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Punct(x)) if *x == p) {
            self.i += 1;
            return true;
        }
        false
    }

    fn expect_punct(&mut self, p: &str) -> Result<()> {
        if self.eat_punct(p) {
            return Ok(());
        }
        Err(self.err(format!("expected `{p}`")))
    }

    fn ident(&mut self, what: &str) -> Result<String> {
        match self.peek() {
            Some(Tok::Word(w)) => {
                let w = w.clone();
                self.i += 1;
                Ok(w)
            }
            Some(Tok::Quoted(w)) => {
                let w = w.clone();
                self.i += 1;
                Ok(w)
            }
            _ => Err(self.err(format!("expected {what}"))),
        }
    }

    // --------------------------------------------------------------- blocks

    /// `[ DECLARE … ] BEGIN … END [label] [;]` — the whole function body.
    pub fn compile_block(&mut self) -> Result<()> {
        if self.eat_kw("declare") {
            self.declarations()?;
        }
        self.expect_kw("begin")?;
        self.statements()?;
        self.expect_kw("end")?;
        // An optional trailing label, then an optional `;`.
        if matches!(self.peek(), Some(Tok::Word(_) | Tok::Quoted(_))) {
            self.i += 1;
        }
        let _ = self.eat_punct(";");
        if self.i != self.t.len() {
            return Err(self.err("trailing input after the function body's END"));
        }
        // ALWAYS emitted, even when every path already returned, and that is
        // not belt-and-braces — it is what makes every forward jump legal.
        //
        // `IF c THEN RETURN 1; ELSE RETURN 2; END IF;` ends with the ELSE arm's
        // `Return`, and the jump that skips the ELSE is patched to the current
        // end of the program: instruction `len()`, one past the last. The IR
        // validator rightly calls that `jump target out of range`, so a body
        // whose arms all returned compiled and then failed to load. Two
        // unreachable instructions cost nothing and remove the whole class.
        //
        // The VALUE is also right for the reachable case. PostgreSQL raises
        // `control reached end of function without RETURN` at run time, and
        // only on the path that skipped the RETURN; this returns NULL there
        // instead. That difference is recorded rather than papered over — it is
        // a missing RAISE, which the frontend refuses everywhere else too.
        let c = self.b.const_idx(Value::Null)?;
        self.b.emit(Op::LoadConst(c))?;
        self.b.emit(Op::Return)?;
        Ok(())
    }

    /// `name type [ := expr ];` repeated until BEGIN.
    fn declarations(&mut self) -> Result<()> {
        while !self.is_kw("begin") {
            if self.peek().is_none() {
                return Err(self.err("DECLARE section without a BEGIN"));
            }
            let name = self.ident("a variable name in the DECLARE section")?;
            if self.eat_kw("constant") {
                // A CONSTANT is a variable this frontend never lets you assign
                // to; enforcing it needs a per-name flag, and without one the
                // honest move is to refuse rather than silently allow writes.
                return Err(self.err(
                    "CONSTANT is not enforced by this frontend, and a constant that can be \
                     assigned to is worse than one that is refused",
                ));
            }
            if self.eat_kw("alias") {
                return Err(self.err(
                    "ALIAS FOR renames a parameter; write the parameter's own name (or `$n`) \
                     instead — both resolve here",
                ));
            }
            self.skip_type()?;
            let slot = self.b.alloc_local()?;
            // NOT NULL / DEFAULT / := — a declared initialiser.
            if self.eat_kw("not") {
                self.expect_kw("null")?;
                return Err(self.err(
                    "a NOT NULL declaration raises at run time on a null assignment; this \
                     frontend does not track it, and an unenforced NOT NULL is a false claim",
                ));
            }
            if self.eat_punct(":=") || self.eat_kw("default") || self.eat_punct("=") {
                self.expr()?;
            } else {
                let c = self.b.const_idx(Value::Null)?;
                self.b.emit(Op::LoadConst(c))?;
            }
            self.b.emit(Op::StoreLocal(slot))?;
            // Inserted AFTER the initialiser is compiled, so `DECLARE x int :=
            // x` reads the OUTER x — PostgreSQL's rule, and the reverse would
            // read an uninitialised slot.
            self.scope.insert(name, slot);
            self.expect_punct(";")?;
        }
        Ok(())
    }

    /// Statements until a block terminator (`END`, `ELSE`, `ELSIF`, `EXCEPTION`).
    fn statements(&mut self) -> Result<()> {
        loop {
            match self.peek() {
                None => return Ok(()),
                Some(Tok::Word(w))
                    if matches!(w.as_str(), "end" | "else" | "elsif" | "elseif" | "exception") =>
                {
                    if w == "exception" {
                        return Err(self.err(
                            "an EXCEPTION block needs a SUBTRANSACTION — PostgreSQL wraps the \
                             block so a caught error can roll back only what the block did. \
                             mpedb has savepoints, but a stored function runs inside the \
                             caller's statement, where a rollback would unmake work the \
                             caller did not ask to lose",
                        ));
                    }
                    return Ok(());
                }
                _ => {}
            }
            self.statement()?;
        }
    }

    fn statement(&mut self) -> Result<()> {
        // A bare `;` is an empty statement.
        if self.eat_punct(";") {
            return Ok(());
        }
        // A block label `<<name>>` — accepted and dropped, since EXIT-by-label
        // is refused below and nothing else reads it.
        if self.eat_punct("<") {
            return Err(self.err(
                "a block LABEL is only useful for `EXIT <label>`, which this frontend refuses \
                 (it can only leave the innermost loop)",
            ));
        }
        let Some(Tok::Word(w)) = self.peek() else {
            return Err(self.err("expected a statement"));
        };
        match w.as_str() {
            "if" => self.if_stmt(),
            "while" => self.while_stmt(),
            "loop" => self.loop_stmt(),
            "for" => self.for_stmt(),
            "return" => self.return_stmt(),
            "exit" => self.exit_stmt(true),
            "continue" => self.exit_stmt(false),
            "null" => {
                self.i += 1;
                self.expect_punct(";")
            }
            "raise" => Err(self.err(
                "RAISE reports an error to the CALLER, and the IR has no opcode that raises \
                 — a stored function can only return a value. Adding one is a real feature \
                 (the error has to cross the interpreter, the SQL expression evaluator and \
                 the statement), not a frontend detail",
            )),
            "execute" => Err(self.err(
                "dynamic EXECUTE builds SQL at RUN time. Every other SQL string in a proc is \
                 compiled at DEFINE time and reaches the runtime as a plan hash — that is \
                 the PySpell security boundary, and dynamic EXECUTE is exactly the hole it \
                 exists to close",
            )),
            "perform" => self.perform_stmt(),
            "select" => self.select_into_stmt(),
            "insert" | "update" | "delete" => self.dml_stmt(),
            "declare" | "begin" => Err(self.err(
                "a nested BEGIN block introduces its own scope and its own EXCEPTION handler; \
                 this frontend compiles one block per function",
            )),
            "get" => Err(self.err(
                "GET DIAGNOSTICS reads statement metadata (ROW_COUNT, and after an EXCEPTION \
                 the error fields) — the row count is available as the value of an INSERT/\
                 UPDATE/DELETE statement here",
            )),
            "case" => Err(self.err(
                "a CASE statement is IF/ELSIF written differently; write the IF form",
            )),
            _ => self.assign_stmt(),
        }
    }

    /// `IF c THEN … [ELSIF c THEN …] [ELSE …] END IF;`
    fn if_stmt(&mut self) -> Result<()> {
        self.expect_kw("if")?;
        let mut ends: Vec<usize> = Vec::new();
        loop {
            self.expr()?;
            self.expect_kw("then")?;
            let jf = self.b.emit_jump(Op::JumpIfFalse(u32::MAX))?;
            self.statements()?;
            // `ELSIF`/`ELSE` need a jump PAST the rest of the chain.
            let has_more = self.is_kw("elsif") || self.is_kw("elseif") || self.is_kw("else");
            if has_more {
                ends.push(self.b.emit_jump(Op::Jump(u32::MAX))?);
            }
            self.b.patch_to_here(jf);
            if self.eat_kw("elsif") || self.eat_kw("elseif") {
                continue;
            }
            if self.eat_kw("else") {
                self.statements()?;
            }
            break;
        }
        for e in ends {
            self.b.patch_to_here(e);
        }
        self.expect_kw("end")?;
        self.expect_kw("if")?;
        self.expect_punct(";")
    }

    /// `WHILE c LOOP … END LOOP;`
    fn while_stmt(&mut self) -> Result<()> {
        self.expect_kw("while")?;
        let top = self.b.here();
        self.expr()?;
        let out = self.b.emit_jump(Op::JumpIfFalse(u32::MAX))?;
        self.expect_kw("loop")?;
        self.b.push_loop(top);
        self.statements()?;
        self.b.emit(Op::Jump(top))?;
        self.b.patch_to_here(out);
        self.b.pop_loop();
        self.expect_kw("end")?;
        self.expect_kw("loop")?;
        self.expect_punct(";")
    }

    /// `LOOP … END LOOP;` — endless; only EXIT leaves it.
    fn loop_stmt(&mut self) -> Result<()> {
        self.expect_kw("loop")?;
        let top = self.b.here();
        self.b.push_loop(top);
        self.statements()?;
        self.b.emit(Op::Jump(top))?;
        self.b.pop_loop();
        self.expect_kw("end")?;
        self.expect_kw("loop")?;
        self.expect_punct(";")
    }

    /// `FOR v IN [REVERSE] lo..hi [BY step] LOOP … END LOOP;`
    ///
    /// The query form (`FOR r IN SELECT …`) is a cursor and is refused by name:
    /// the IR has the opcodes, but a stored FUNCTION may not touch the database
    /// at all, so wiring it here would produce something only the procedure
    /// path could run — a difference better stated than discovered.
    fn for_stmt(&mut self) -> Result<()> {
        self.expect_kw("for")?;
        let var = self.ident("the loop variable")?;
        self.expect_kw("in")?;
        if self.is_kw("select") || self.is_kw("execute") || self.eat_punct("(") {
            return Err(self.err(
                "`FOR … IN SELECT` iterates a QUERY. mpedb has the streaming cursor it needs \
                 (`db.rows` in the Python/Rust frontends), but a stored SQL FUNCTION may not \
                 touch the database — it is evaluated per row inside a statement that is \
                 already scanning. Bodies that query belong in a stored PROCEDURE",
            ));
        }
        let reverse = self.eat_kw("reverse");
        // The loop variable is its own slot and shadows an outer name for the
        // duration; PostgreSQL scopes it to the loop, and restoring the old
        // binding after `END LOOP` is what makes that true here.
        let slot = self.b.alloc_local()?;
        let shadowed = self.scope.insert(var.clone(), slot);
        self.expr()?; // lo
        self.b.emit(Op::StoreLocal(slot))?;
        self.expect_punct("..")?;
        let hi = self.b.alloc_local()?;
        self.expr()?; // hi
        self.b.emit(Op::StoreLocal(hi))?;
        let step = self.b.alloc_local()?;
        if self.eat_kw("by") {
            self.expr()?;
        } else {
            let c = self.b.const_idx(Value::Int(1))?;
            self.b.emit(Op::LoadConst(c))?;
        }
        self.b.emit(Op::StoreLocal(step))?;
        self.expect_kw("loop")?;

        // INCREMENT BEFORE TEST, with an entry jump over the first increment:
        //
        //     jump ---------------> TEST
        //   INCR: v := v +/- step
        //   TEST: v <=/>= hi ? ---> OUT (if false)
        //         body
        //         jump ----------> INCR
        //   OUT:
        //
        // Not the obvious test-first shape, and the reason is `CONTINUE`. The
        // shared loop context has ONE continue target, baked into each
        // `Op::Jump` as it is emitted — so it must be an address that already
        // exists when the body is compiled. Test-first would need the target to
        // be the increment, which sits AFTER the body; a continue would have to
        // jump back to the test instead, the variable would never advance, and
        // `CONTINUE` inside a FOR would hang. This layout makes the increment
        // the earlier address, so the one target the context can hold is the
        // right one.
        let entry = self.b.emit_jump(Op::Jump(u32::MAX))?;
        let incr = self.b.here();
        self.b.emit(Op::LoadLocal(slot))?;
        self.b.emit(Op::LoadLocal(step))?;
        self.b.emit(if reverse { Op::Sub } else { Op::Add })?;
        self.b.emit(Op::StoreLocal(slot))?;
        // `here()` is now the test, which is exactly what the entry jump wants.
        self.b.patch_to_here(entry);
        self.b.emit(Op::LoadLocal(slot))?;
        self.b.emit(Op::LoadLocal(hi))?;
        self.b.emit(if reverse { Op::Ge } else { Op::Le })?;
        let out = self.b.emit_jump(Op::JumpIfFalse(u32::MAX))?;
        self.b.push_loop(incr);
        self.statements()?;
        self.b.emit(Op::Jump(incr))?;
        self.b.patch_to_here(out);
        self.b.pop_loop();

        match shadowed {
            Some(old) => {
                self.scope.insert(var, old);
            }
            None => {
                self.scope.remove(&var);
            }
        }
        self.expect_kw("end")?;
        self.expect_kw("loop")?;
        self.expect_punct(";")
    }

    /// `RETURN [expr];` — a bare RETURN returns NULL, as PostgreSQL does for a
    /// non-SETOF function.
    fn return_stmt(&mut self) -> Result<()> {
        self.expect_kw("return")?;
        if self.is_kw("query") || self.is_kw("next") {
            return Err(self.err(
                "RETURN NEXT / RETURN QUERY belong to a SETOF function, which produces ROWS \
                 rather than one value",
            ));
        }
        if self.eat_punct(";") {
            let c = self.b.const_idx(Value::Null)?;
            self.b.emit(Op::LoadConst(c))?;
        } else {
            self.expr()?;
            self.expect_punct(";")?;
        }
        self.b.emit(Op::Return)?;
        Ok(())
    }

    /// `EXIT [WHEN c];` / `CONTINUE [WHEN c];`
    fn exit_stmt(&mut self, is_exit: bool) -> Result<()> {
        self.i += 1;
        if matches!(self.peek(), Some(Tok::Word(w)) if w != "when") {
            return Err(self.err(
                "EXIT/CONTINUE with a LABEL leaves an OUTER loop; this frontend can only \
                 leave the innermost one",
            ));
        }
        // `EXIT WHEN c` is `IF c THEN EXIT; END IF` — compiled as exactly that
        // rather than as a conditional jump, so one path emits the loop
        // bookkeeping and the two forms cannot drift.
        let guard = if self.eat_kw("when") {
            self.expr()?;
            Some(self.b.emit_jump(Op::JumpIfFalse(u32::MAX))?)
        } else {
            None
        };
        let ok = if is_exit { self.b.emit_break()? } else { self.b.emit_continue()? };
        if !ok {
            return Err(self.err(if is_exit {
                "EXIT outside a loop"
            } else {
                "CONTINUE outside a loop"
            }));
        }
        if let Some(g) = guard {
            self.b.patch_to_here(g);
        }
        self.expect_punct(";")
    }

    // ------------------------------------------------- the SQL-bearing forms

    fn perform_stmt(&mut self) -> Result<()> {
        Err(self.err(
            "PERFORM runs a query for its SIDE EFFECTS. A stored SQL function is evaluated \
             per row inside a statement that is already running, and may not touch the \
             database — bodies that do belong in a stored PROCEDURE (`mpedb proc`)",
        ))
    }

    fn select_into_stmt(&mut self) -> Result<()> {
        Err(self.err(
            "`SELECT … INTO` reads the database, and a stored SQL function may not — it is \
             evaluated per row inside a statement that is already scanning. Bodies that \
             query belong in a stored PROCEDURE (`mpedb proc`)",
        ))
    }

    fn dml_stmt(&mut self) -> Result<()> {
        Err(self.err(
            "an INSERT/UPDATE/DELETE inside a function would WRITE while the statement that \
             called it is reading. mpedb's stored SQL functions are pure by construction; \
             a body that writes belongs in a stored PROCEDURE (`mpedb proc`)",
        ))
    }

    /// `name := expr;` — the fallthrough. PostgreSQL also accepts `name = expr`
    /// in a body, and dumps contain both.
    fn assign_stmt(&mut self) -> Result<()> {
        let at = self.at();
        let name = self.ident("a statement or a variable to assign to")?;
        if !(self.eat_punct(":=") || self.eat_punct("=")) {
            return Err(err(
                self.src,
                at,
                format!("`{name}` starts no statement this frontend knows"),
            ));
        }
        let Some(&slot) = self.scope.get(&name) else {
            return Err(err(self.src, at, format!("`{name}` is not declared")));
        };
        self.expr()?;
        self.b.emit(Op::StoreLocal(slot))?;
        self.expect_punct(";")
    }

    /// Consume a declared type, discarding it — see `head::type_name` for why.
    fn skip_type(&mut self) -> Result<()> {
        // `v tbl%ROWTYPE` / `v tbl.col%TYPE` bind a SHAPE from the schema, and
        // this frontend has no schema.
        let start = self.i;
        while let Some(tok) = self.peek() {
            match tok {
                Tok::Punct(";") | Tok::Punct(":=") => break,
                Tok::Word(w) if w == "default" || w == "not" => break,
                Tok::Punct("%") => {
                    return Err(self.err(
                        "`%TYPE` / `%ROWTYPE` copies a type from the schema, and this \
                         frontend compiles without one — write the type",
                    ))
                }
                _ => self.i += 1,
            }
        }
        if self.i == start {
            return Err(self.err("a DECLARE entry needs a type"));
        }
        Ok(())
    }

    // ---------------------------------------------------------- expressions

    fn expr(&mut self) -> Result<()> {
        self.or_expr()
    }

    fn or_expr(&mut self) -> Result<()> {
        self.and_expr()?;
        while self.eat_kw("or") {
            // Value-preserving short circuit, the same shape the Python
            // frontend uses for `or`: keep the left value if it is truthy.
            self.b.emit(Op::Dup)?;
            let skip = self.b.emit_jump(Op::JumpIfTrue(u32::MAX))?;
            self.b.emit(Op::Pop)?;
            self.and_expr()?;
            self.b.patch_to_here(skip);
        }
        Ok(())
    }

    fn and_expr(&mut self) -> Result<()> {
        self.not_expr()?;
        while self.eat_kw("and") {
            self.b.emit(Op::Dup)?;
            let skip = self.b.emit_jump(Op::JumpIfFalse(u32::MAX))?;
            self.b.emit(Op::Pop)?;
            self.not_expr()?;
            self.b.patch_to_here(skip);
        }
        Ok(())
    }

    fn not_expr(&mut self) -> Result<()> {
        if self.eat_kw("not") {
            self.not_expr()?;
            self.b.emit(Op::Not)?;
            return Ok(());
        }
        self.cmp_expr()
    }

    fn cmp_expr(&mut self) -> Result<()> {
        self.add_expr()?;
        loop {
            // `IS [NOT] NULL` — the only NULL test that means what it says.
            if self.is_kw("is") {
                self.i += 1;
                let negate = self.eat_kw("not");
                self.expect_kw("null")?;
                let c = self.b.const_idx(Value::Null)?;
                self.b.emit(Op::LoadConst(c))?;
                self.b.emit(if negate { Op::Ne } else { Op::Eq })?;
                continue;
            }
            let op = match self.peek() {
                Some(Tok::Punct("=")) => Op::Eq,
                Some(Tok::Punct("<>")) | Some(Tok::Punct("!=")) => Op::Ne,
                Some(Tok::Punct("<")) => Op::Lt,
                Some(Tok::Punct("<=")) => Op::Le,
                Some(Tok::Punct(">")) => Op::Gt,
                Some(Tok::Punct(">=")) => Op::Ge,
                _ => return Ok(()),
            };
            let at = self.at();
            self.i += 1;
            // See the module docs: `x = NULL` is NULL in PostgreSQL and true in
            // this interpreter when x is also null. Refused rather than
            // silently disagreed with — and in PostgreSQL the comparison is
            // already a bug, since it can never be true.
            if self.is_kw("null") && matches!(op, Op::Eq | Op::Ne) {
                return Err(err(
                    self.src,
                    at,
                    "comparing to NULL with `=`/`<>` is NULL in PostgreSQL — never true, \
                     whatever the other side is. Write `IS NULL` / `IS NOT NULL`",
                ));
            }
            self.add_expr()?;
            self.b.emit(op)?;
        }
    }

    fn add_expr(&mut self) -> Result<()> {
        self.mul_expr()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Punct("+")) => Op::Add,
                Some(Tok::Punct("-")) => Op::Sub,
                Some(Tok::Punct("||")) => {
                    return Err(self.err(
                        "`||` concatenates, and the IR has no concatenation opcode — every \
                         binary operator it has is arithmetic or a comparison",
                    ))
                }
                _ => return Ok(()),
            };
            self.i += 1;
            self.mul_expr()?;
            self.b.emit(op)?;
        }
    }

    fn mul_expr(&mut self) -> Result<()> {
        self.unary()?;
        loop {
            // PostgreSQL's integer `/` truncates toward zero and `%` takes the
            // DIVIDEND's sign — which is the Rust rule, not the Python one.
            // Emitting `TrueDiv`/`PyMod` here would be a silently different
            // answer for negative operands.
            let op = match self.peek() {
                Some(Tok::Punct("*")) => Op::Mul,
                Some(Tok::Punct("/")) => Op::IntDiv,
                Some(Tok::Punct("%")) => Op::IntRem,
                _ => return Ok(()),
            };
            self.i += 1;
            self.unary()?;
            self.b.emit(op)?;
        }
    }

    fn unary(&mut self) -> Result<()> {
        if self.eat_punct("-") {
            self.unary()?;
            self.b.emit(Op::Neg)?;
            return Ok(());
        }
        if self.eat_punct("+") {
            return self.unary();
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<()> {
        self.atom()?;
        // `expr::type` — a cast. Accepted and DROPPED: the IR's values carry
        // their own type and the interpreter has no cast opcode, so the cast
        // could only be a claim. Dropping it is right for the overwhelmingly
        // common `$1::int` (already an int) and wrong for a converting cast —
        // which is why `::text` and friends are refused by name.
        while self.eat_punct("::") {
            let at = self.at();
            let ty = self.ident("a type name after `::`")?;
            let converts = !matches!(
                ty.as_str(),
                "int" | "int2" | "int4" | "int8" | "integer" | "smallint" | "bigint"
            );
            if converts {
                return Err(err(
                    self.src,
                    at,
                    format!(
                        "a cast to `{ty}` CONVERTS, and the IR has no cast opcode — the value \
                         would keep whatever type it already had, which is a wrong answer \
                         rather than a missing one. Only integer casts (which this frontend's \
                         arithmetic already produces) are dropped as identities"
                    ),
                ));
            }
        }
        Ok(())
    }

    fn atom(&mut self) -> Result<()> {
        match self.peek().cloned() {
            Some(Tok::Int(n)) => {
                self.i += 1;
                let c = self.b.const_idx(Value::Int(n))?;
                self.b.emit(Op::LoadConst(c))?;
                Ok(())
            }
            Some(Tok::Float(f)) => {
                self.i += 1;
                let c = self.b.const_idx(Value::Float(f))?;
                self.b.emit(Op::LoadConst(c))?;
                Ok(())
            }
            Some(Tok::Str(s)) => {
                self.i += 1;
                let c = self.b.const_idx(Value::Text(s))?;
                self.b.emit(Op::LoadConst(c))?;
                Ok(())
            }
            Some(Tok::Param(n)) => {
                self.i += 1;
                let key = format!("${n}");
                let Some(&slot) = self.scope.get(&key) else {
                    return Err(self.err(format!("`${n}` — the function has fewer parameters")));
                };
                self.b.emit(Op::LoadLocal(slot))?;
                Ok(())
            }
            Some(Tok::Punct("(")) => {
                self.i += 1;
                self.expr()?;
                self.expect_punct(")")
            }
            Some(Tok::Word(w)) => {
                let at = self.at();
                match w.as_str() {
                    "true" | "false" => {
                        self.i += 1;
                        let c = self.b.const_idx(Value::Bool(w == "true"))?;
                        self.b.emit(Op::LoadConst(c))?;
                        return Ok(());
                    }
                    "null" => {
                        self.i += 1;
                        let c = self.b.const_idx(Value::Null)?;
                        self.b.emit(Op::LoadConst(c))?;
                        return Ok(());
                    }
                    _ => {}
                }
                self.i += 1;
                // A call — including a call to ANOTHER stored function. The IR
                // has no call opcode; a proc is one flat program, and there is
                // no frame to push.
                if matches!(self.peek(), Some(Tok::Punct("("))) {
                    return Err(err(
                        self.src,
                        at,
                        format!(
                            "calling `{w}(…)` from a function body needs a CALL opcode — a \
                             compiled proc is one flat program with no frame to push, so a \
                             call is a real IR feature rather than a frontend detail"
                        ),
                    ));
                }
                let Some(&slot) = self.scope.get(&w) else {
                    return Err(err(self.src, at, format!("`{w}` is not declared")));
                };
                self.b.emit(Op::LoadLocal(slot))?;
                Ok(())
            }
            Some(Tok::Quoted(w)) => {
                let at = self.at();
                self.i += 1;
                let Some(&slot) = self.scope.get(&w) else {
                    return Err(err(self.src, at, format!("`{w}` is not declared")));
                };
                self.b.emit(Op::LoadLocal(slot))?;
                Ok(())
            }
            _ => Err(self.err("expected an expression")),
        }
    }
}
