//! Explicit PL/SQL cursors: `CURSOR c IS SELECT ...; OPEN; FETCH INTO; CLOSE`
//! and `%FOUND`, `%NOTFOUND`, `%ISOPEN`, `%ROWCOUNT`.
//! Packages and parameters in the cursor query stay refused.
//! A fetch that finds no row leaves its targets unchanged and leaves the
//! cursor open. A NULL column assigns NULL. CLOSE releases the stream
//! without fetching the remaining rows. `SELECT *` is refused: the select
//! list must name its columns. `count(*)` is one column.
//! `%FOUND` and `%NOTFOUND` are NULL after OPEN and before FETCH. A cursor
//! that is not open makes `%ISOPEN` false and raises INVALID_CURSOR for
//! the other three. `%NOTFOUND` is the boolean not of `%FOUND`.
use super::*;
use crate::emit::{line_col, CallKind, SqlCall};

#[derive(Clone, Copy)]
pub(super) struct CursorDecl {
    slot: u16,
    call: u16,
    width: u16,
}

impl Parser<'_> {
    pub(super) fn declare_cursor(&mut self) -> Result<()> {
        self.pending_declarations.remove("CURSOR");
        let name = self.identifier()?;
        if !self.declared_here.insert(name.clone()) {
            return Err(self.error("duplicate parameter or variable name"));
        }
        self.want_word("IS")?;
        if !self.is_word("SELECT") {
            return Err(self.error("a cursor declaration requires SELECT"));
        }
        let start = self.at;
        let mut depth = 0i32;
        let mut end = start;
        loop {
            let token = self.tokens.get(end).ok_or_else(|| {
                self.error("unterminated cursor declaration")
            })?;
            match &token.token {
                Token::Symbol(";") if depth == 0 => break,
                Token::Symbol("(") => depth += 1,
                Token::Symbol(")") => {
                    depth -= 1;
                    if depth < 0 {
                        return Err(self.error("unmatched parenthesis in cursor SELECT"));
                    }
                }
                Token::Identifier { name, quoted: false }
                    if name == "INTO" && depth == 0 =>
                {
                    return Err(self.error("a cursor SELECT cannot contain INTO"));
                }
                _ => {}
            }
            end += 1;
        }
        let source_start = self.tokens[start].span.start;
        let source_end = self.tokens[end].span.start;
        let source = self.source[source_start..source_end].to_string();
        let width = match select_width(&self.tokens[start..end]) {
            Ok(width) => width,
            Err(WidthError::Star { at }) => {
                let saved = self.at;
                self.at = start + at;
                let err = self.error("cursor SELECT * is not expanded; name the columns");
                self.at = saved;
                return Err(err);
            }
            Err(WidthError::TooWide) => return Err(self.error("cursor SELECT list is too wide")),
            Err(WidthError::MissingFrom) => {
                return Err(self.error("a cursor SELECT requires FROM"))
            }
        };
        self.at = end + 1;
        let (sql, count) = super::sql::normalize_sql(&source, CallKind::Query)
            .map_err(|e| self.error(&e.to_string()))?;
        if count != 0 {
            return Err(self.error("cursor SELECT parameters are not implemented"));
        }
        let (line, col) = line_col(self.source, source_start);
        let call = self.builder.add_call(SqlCall {
            sql,
            kind: CallKind::Rows,
            argc: 0,
            expected_columns: Some(width),
            line,
            col,
        })?;
        self.constant(Value::Null)?;
        let slot = self.builder.alloc_local()?;
        self.builder.emit(Op::StoreLocal(slot))?;
        self.cursors.insert(name, CursorDecl { slot, call, width });
        Ok(())
    }

    pub(super) fn cursor_statement(&mut self) -> Result<bool> {
        if self.word("OPEN") {
            let cursor = self.cursor_name()?;
            self.want_symbol(";")?;
            self.builder.emit(Op::CursorOpen(cursor.call))?;
            self.builder.emit(Op::StoreLocal(cursor.slot))?;
            return Ok(true);
        }
        if self.word("FETCH") {
            let cursor = self.cursor_name()?;
            self.want_word("INTO")?;
            let mut targets = Vec::new();
            loop {
                let name = self.identifier()?;
                let target = *self
                    .locals
                    .get(&name)
                    .ok_or_else(|| self.error("unknown FETCH target"))?;
                if !target.writable {
                    return Err(self.error("FETCH target is constant or an IN parameter"));
                }
                targets.push(target);
                if !self.symbol(",") {
                    break;
                }
            }
            if targets.len() != cursor.width as usize {
                return Err(self.error(
                    "FETCH INTO target count does not match the cursor SELECT list",
                ));
            }
            self.want_symbol(";")?;
            self.builder.emit(Op::LoadLocal(cursor.slot))?;
            self.builder.emit(Op::CursorFetch)?;
            let none = self.builder.emit_jump(Op::JumpIfFalse(u32::MAX))?;
            self.builder.emit(Op::LoadLocal(cursor.slot))?;
            self.builder.emit(Op::CursorRow)?;
            for (i, target) in targets.iter().enumerate() {
                self.builder.emit(Op::Dup)?;
                self.constant(Value::Int(i as i64))?;
                self.builder.emit(Op::Index)?;
                self.emit_local_check(*target)?;
                self.builder.emit(Op::StoreLocal(target.slot))?;
            }
            self.builder.emit(Op::Pop)?;
            let done = self.builder.emit_jump(Op::Jump(u32::MAX))?;
            self.builder.patch_to_here(none);
            self.builder.patch_to_here(done);
            return Ok(true);
        }
        if self.word("CLOSE") {
            let cursor = self.cursor_name()?;
            self.want_symbol(";")?;
            self.builder.emit(Op::LoadLocal(cursor.slot))?;
            self.builder.emit(Op::CursorClose)?;
            self.constant(Value::Null)?;
            self.builder.emit(Op::StoreLocal(cursor.slot))?;
            return Ok(true);
        }
        Ok(false)
    }

    fn cursor_name(&mut self) -> Result<CursorDecl> {
        let name = self.identifier()?;
        self.cursors
            .get(&name)
            .copied()
            .ok_or_else(|| self.error("unknown explicit cursor"))
    }

    pub(super) fn cursor_attribute(&mut self, name: &str) -> Result<Option<Type>> {
        let cursor = self.cursors.get(name).copied().ok_or_else(|| {
            self.error("cursor attributes require an explicit cursor")
        })?;
        let attr = self.identifier()?;
        let (code, ty) = match attr.as_str() {
            "ISOPEN" => (crate::ir::CURSOR_ATTR_ISOPEN, Type::Boolean),
            "FOUND" => (crate::ir::CURSOR_ATTR_FOUND, Type::Boolean),
            "NOTFOUND" => (crate::ir::CURSOR_ATTR_NOTFOUND, Type::Boolean),
            "ROWCOUNT" => (crate::ir::CURSOR_ATTR_ROWCOUNT, Type::PlsInteger),
            _ => return Err(self.error(&format!("unknown cursor attribute {attr}"))),
        };
        self.builder.emit(Op::LoadLocal(cursor.slot))?;
        self.builder.emit(Op::CursorAttr(code))?;
        Ok(Some(ty))
    }
}

enum WidthError {
    Star { at: usize },
    TooWide,
    MissingFrom,
}

/// Column count of a cursor SELECT list. A `*` at item-start or after `.`
/// is star expansion and is refused. A `*` inside parentheses, including
/// `count(*)`, is one expression. Multiplication (`1 * 2`) is not expansion.
fn select_width(tokens: &[Lexed]) -> std::result::Result<u16, WidthError> {
    let mut depth = 0i32;
    let mut width = 1u16;
    let mut saw_from = false;
    let mut item_boundary = false;
    let mut after_dot = false;
    for (index, token) in tokens.iter().enumerate() {
        match &token.token {
            Token::Symbol("(") => {
                depth += 1;
                after_dot = false;
                item_boundary = false;
            }
            Token::Symbol(")") => {
                depth -= 1;
                after_dot = false;
            }
            Token::Identifier { name, quoted: false } if depth == 0 && name == "FROM" => {
                saw_from = true;
                break;
            }
            Token::Identifier { name, quoted: false }
                if depth == 0 && name == "SELECT" && index == 0 =>
            {
                item_boundary = true;
                after_dot = false;
            }
            Token::Identifier { name, quoted: false }
                if depth == 0
                    && item_boundary
                    && matches!(name.as_str(), "DISTINCT" | "ALL" | "UNIQUE") =>
            {
                after_dot = false;
            }
            Token::Symbol(",") if depth == 0 => {
                width = width.checked_add(1).ok_or(WidthError::TooWide)?;
                item_boundary = true;
                after_dot = false;
            }
            Token::Symbol(".") if depth == 0 => {
                after_dot = true;
                item_boundary = false;
            }
            Token::Symbol("*") if depth == 0 && (item_boundary || after_dot) => {
                return Err(WidthError::Star { at: index });
            }
            _ if depth == 0 => {
                item_boundary = false;
                after_dot = false;
            }
            _ => {}
        }
    }
    if !saw_from {
        return Err(WidthError::MissingFrom);
    }
    Ok(width)
}

#[cfg(test)]
mod tests {
    use super::super::compile;
    use crate::emit::CallKind;
    use crate::interp::{self, testutil::MockBridge, Budget, ProcValue};
    use crate::ir::{Op, PlanKind, PlanRef, Proc};
    use mpedb_types::{PlanHash, Value};

    fn run(rows: Vec<Vec<Value>>, body: &str) -> (Value, MockBridge) {
        let (value, bridge, _) = execute("NUMBER", rows, body);
        (value, bridge)
    }

    /// Compile through the shipped compiler, round-trip the bytecode, and
    /// execute it. The returned procedure is the one that ran.
    fn execute(returns: &str, rows: Vec<Vec<Value>>, body: &str) -> (Value, MockBridge, Proc) {
        let source = format!("CREATE FUNCTION f RETURN {returns} IS {body}");
        let skel = compile(&source).unwrap();
        assert!(skel.calls.iter().any(|call| call.kind == CallKind::Rows));
        let plans = skel
            .calls
            .iter()
            .map(|call| PlanRef {
                hash: PlanHash([0; 32]),
                kind: if call.kind == CallKind::Exec {
                    PlanKind::Exec
                } else {
                    PlanKind::Query
                },
                argc: call.argc,
            })
            .collect();
        let proc = Proc::new(
            skel.name,
            skel.argc,
            skel.nlocals,
            plans,
            skel.consts,
            skel.instrs,
        )
        .unwrap();
        let proc = Proc::decode(&proc.encode()).unwrap();
        let mut bridge = MockBridge::new();
        bridge.rows = rows;
        let value = match interp::run(
            &proc,
            &[],
            &mut bridge,
            Budget {
                instrs: 10_000,
                db_calls: 4,
                rows: 100,
            },
        )
        .unwrap()
        {
            ProcValue::Scalar(value) => value,
            other => panic!("{other:?}"),
        };
        (value, bridge, proc)
    }

    #[test]
    fn explicit_cursor_fetch_assigns_null_and_leaves_a_missing_row_unchanged() {
        let body = "CURSOR c IS SELECT amount FROM dual; v NUMBER := 7; BEGIN OPEN c; FETCH c INTO v; CLOSE c; RETURN v; END;";
        let (value, bridge) = run(vec![vec![Value::Null]], body);
        assert_eq!(value, Value::Null);
        assert_eq!(bridge.cursor_opens, 1);
        assert_eq!(bridge.live_streams(), 0);
        let (value, _) = run(vec![vec![Value::Numeric("4".into())]], body);
        assert_eq!(value, Value::Numeric("4".into()));
        let (value, bridge) = run(vec![], body);
        assert_eq!(value, Value::Numeric("7".into()));
        assert_eq!(bridge.advances, 1);
        assert_eq!(bridge.closes, 1);
        assert_eq!(bridge.live_streams(), 0);
        let (value, bridge) = run(
            vec![
                vec![Value::Numeric("4".into())],
                vec![Value::Numeric("9".into())],
            ],
            body,
        );
        assert_eq!(value, Value::Numeric("4".into()));
        assert_eq!(bridge.advances, 1, "CLOSE must not fetch the remaining row");
        assert_eq!(bridge.closes, 1);
        assert_eq!(bridge.live_streams(), 0);
    }

    #[test]
    fn explicit_cursor_refuses_star_expansion_and_counts_parenthesized_star() {
        for query in ["SELECT * FROM dual", "SELECT t.* FROM dual", "SELECT DISTINCT * FROM dual"] {
            let source = format!(
                "CREATE FUNCTION f RETURN NUMBER IS CURSOR c IS {query}; BEGIN RETURN 1; END;"
            );
            let err = compile(&source).unwrap_err().to_string();
            assert!(err.contains("PL/SQL at "), "{err}");
            assert!(err.contains("SELECT *"), "{err}");
            assert!(!err.starts_with("unsupported: cursor SELECT list"), "{err}");
        }
        let count = "CREATE FUNCTION f RETURN NUMBER IS CURSOR c IS SELECT count(*) FROM dual; n NUMBER; BEGIN OPEN c; FETCH c INTO n; CLOSE c; RETURN n; END;";
        let skel = compile(count).unwrap();
        assert_eq!(skel.calls[0].expected_columns, Some(1));
        let product = "CREATE FUNCTION f RETURN NUMBER IS CURSOR c IS SELECT 1 * 2 FROM dual; n NUMBER; BEGIN OPEN c; FETCH c INTO n; CLOSE c; RETURN n; END;";
        let skel = compile(product).unwrap();
        assert_eq!(skel.calls[0].expected_columns, Some(1));
        let two = "CREATE FUNCTION f RETURN NUMBER IS CURSOR c IS SELECT a, b FROM dual; x NUMBER; y NUMBER; BEGIN OPEN c; FETCH c INTO x, y; CLOSE c; RETURN x; END;";
        let skel = compile(two).unwrap();
        assert_eq!(skel.calls[0].expected_columns, Some(2));
    }

    #[test]
    fn cursor_attributes_follow_oracle_null_logic() {
        let row = vec![vec![Value::Numeric("4".into())]];
        let decl = "CURSOR c IS SELECT amount FROM dual; v NUMBER;";
        let invalid = "ORA-01001: cursor number is invalid or does not exist";

        let (found, bridge, proc) = execute(
            "BOOLEAN",
            row.clone(),
            &format!("{decl} BEGIN OPEN c; RETURN c%Found; END;"),
        );
        assert!(proc.instrs.contains(&Op::CursorAttr(crate::ir::CURSOR_ATTR_FOUND)));
        assert_eq!(found, Value::Null);
        assert_eq!(bridge.cursor_opens, 1);
        assert_eq!(bridge.advances, 0);

        let (notfound, _, proc) = execute(
            "BOOLEAN",
            row.clone(),
            &format!("{decl} BEGIN OPEN c; RETURN c%notfound; END;"),
        );
        assert!(proc.instrs.contains(&Op::CursorAttr(crate::ir::CURSOR_ATTR_NOTFOUND)));
        assert_eq!(notfound, Value::Null);

        let (open, _) = run(
            row.clone(),
            &format!("{decl} BEGIN OPEN c; IF c%ISOPEN THEN RETURN 1; ELSE RETURN 0; END IF; END;"),
        );
        assert_eq!(open, Value::Numeric("1".into()));
        let (count, _, proc) = execute(
            "PLS_INTEGER",
            row.clone(),
            &format!("{decl} BEGIN OPEN c; RETURN c%ROWCOUNT; END;"),
        );
        assert!(proc.instrs.contains(&Op::CursorAttr(crate::ir::CURSOR_ATTR_ROWCOUNT)));
        assert_eq!(count, Value::Int(0));

        let (closed, bridge) = run(
            vec![],
            "CURSOR c IS SELECT amount FROM dual; BEGIN IF c%ISOPEN THEN RETURN 1; ELSE RETURN 0; END IF; END;",
        );
        assert_eq!(closed, Value::Numeric("0".into()));
        assert_eq!(bridge.cursor_opens, 0);
        for (attr, assign) in [
            ("FOUND", "flag := c%FOUND;"),
            ("NOTFOUND", "flag := c%NOTFOUND;"),
            ("ROWCOUNT", "n := c%ROWCOUNT;"),
        ] {
            let (text, bridge, proc) = execute(
                "VARCHAR2",
                vec![],
                &format!(
                    "CURSOR c IS SELECT amount FROM dual; flag BOOLEAN; n PLS_INTEGER; BEGIN {assign} RETURN 'open'; EXCEPTION WHEN INVALID_CURSOR THEN RETURN SQLERRM; END;"
                ),
            );
            assert!(
                proc.instrs.iter().any(|op| matches!(op, Op::CursorAttr(_))),
                "{attr}"
            );
            assert_eq!(text, Value::Text(invalid.into()), "{attr}");
            assert_eq!(bridge.cursor_opens, 0, "{attr}");
        }

        let (found, bridge, _) = execute(
            "BOOLEAN",
            row.clone(),
            &format!("{decl} BEGIN OPEN c; FETCH c INTO v; RETURN c%FOUND; END;"),
        );
        assert_eq!(found, Value::Bool(true));
        assert_eq!(bridge.advances, 1);
        let (notfound, _) = run(
            row.clone(),
            &format!("{decl} BEGIN OPEN c; FETCH c INTO v; IF c%NOTFOUND THEN RETURN 1; ELSE RETURN 0; END IF; END;"),
        );
        assert_eq!(notfound, Value::Numeric("0".into()));
        let (count, _, _) = execute(
            "PLS_INTEGER",
            row.clone(),
            &format!("{decl} BEGIN OPEN c; FETCH c INTO v; RETURN c%ROWCOUNT; END;"),
        );
        assert_eq!(count, Value::Int(1));

        let (found, bridge, _) = execute(
            "BOOLEAN",
            vec![],
            &format!("{decl} BEGIN OPEN c; FETCH c INTO v; RETURN c%FOUND; END;"),
        );
        assert_eq!(found, Value::Bool(false));
        assert_eq!(bridge.advances, 1);
        let (notfound, _, _) = execute(
            "BOOLEAN",
            vec![],
            &format!("{decl} BEGIN OPEN c; FETCH c INTO v; RETURN c%NOTFOUND; END;"),
        );
        assert_eq!(notfound, Value::Bool(true));
        let (count, _, _) = execute(
            "PLS_INTEGER",
            vec![],
            &format!("{decl} BEGIN OPEN c; FETCH c INTO v; RETURN c%ROWCOUNT; END;"),
        );
        assert_eq!(count, Value::Int(0));

        let (count, bridge, _) = execute(
            "PLS_INTEGER",
            row.clone(),
            &format!(
                "{decl} BEGIN OPEN c; FETCH c INTO v; FETCH c INTO v; FETCH c INTO v; RETURN c%ROWCOUNT; END;"
            ),
        );
        assert_eq!(count, Value::Int(1));
        assert_eq!(bridge.advances, 2, "a fetch after exhaustion must not pull");
        let (notfound, _, _) = execute(
            "BOOLEAN",
            row.clone(),
            &format!("{decl} BEGIN OPEN c; FETCH c INTO v; FETCH c INTO v; RETURN c%NOTFOUND; END;"),
        );
        assert_eq!(notfound, Value::Bool(true));
        let (found, _, _) = execute(
            "BOOLEAN",
            row.clone(),
            &format!("{decl} BEGIN OPEN c; FETCH c INTO v; FETCH c INTO v; RETURN c%FOUND; END;"),
        );
        assert_eq!(found, Value::Bool(false));

        let (open, bridge) = run(
            vec![],
            "CURSOR c IS SELECT amount FROM dual; BEGIN OPEN c; CLOSE c; IF c%ISOPEN THEN RETURN 1; ELSE RETURN 0; END IF; END;",
        );
        assert_eq!(open, Value::Numeric("0".into()));
        assert_eq!(bridge.closes, 1);
        assert_eq!(bridge.advances, 0);
        assert_eq!(bridge.live_streams(), 0);
        let (text, bridge, _) = execute(
            "VARCHAR2",
            row.clone(),
            &format!(
                "{decl} n PLS_INTEGER; BEGIN OPEN c; FETCH c INTO v; CLOSE c; n := c%ROWCOUNT; RETURN 'open'; EXCEPTION WHEN INVALID_CURSOR THEN RETURN SQLERRM; END;"
            ),
        );
        assert_eq!(text, Value::Text(invalid.into()));
        assert_eq!(bridge.advances, 1);
        assert_eq!(bridge.closes, 1);
        for (attr, assign) in [("FOUND", "flag := c%FOUND;"), ("NOTFOUND", "flag := c%NOTFOUND;")] {
            let (text, _, _) = execute(
                "VARCHAR2",
                vec![],
                &format!(
                    "CURSOR c IS SELECT amount FROM dual; flag BOOLEAN; BEGIN OPEN c; CLOSE c; {assign} RETURN 'open'; EXCEPTION WHEN INVALID_CURSOR THEN RETURN SQLERRM; END;"
                ),
            );
            assert_eq!(text, Value::Text(invalid.into()), "{attr}");
        }

        for source in [
            "CREATE FUNCTION f RETURN BOOLEAN IS CURSOR c IS SELECT amount FROM dual; BEGIN RETURN c%ROWTYPE; END;",
            "CREATE FUNCTION f RETURN BOOLEAN IS CURSOR c IS SELECT amount FROM dual; BEGIN RETURN SQL%FOUND; END;",
        ] {
            let err = compile(source).unwrap_err().to_string();
            assert!(err.contains("PL/SQL at "), "{err}");
            assert!(
                err.contains("unknown cursor attribute")
                    || err.contains("cursor attributes require an explicit cursor"),
                "{err}"
            );
        }
    }
}
