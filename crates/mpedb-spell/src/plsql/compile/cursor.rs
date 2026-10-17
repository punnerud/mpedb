//! Explicit PL/SQL cursors: `CURSOR c IS SELECT ...; OPEN; FETCH INTO; CLOSE`.
//! Packages, cursor attributes, and parameters in the cursor query stay refused.
//! A fetch that finds no row leaves its targets unchanged. A NULL column assigns
//! NULL. CLOSE walks the existing cursor until it is exhausted, which is what
//! releases the stream.
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
        let width = select_width(&self.tokens[start..end])?;
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
            self.builder.emit(Op::CursorAdvance)?;
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
            let top = self.builder.here();
            self.builder.emit(Op::LoadLocal(cursor.slot))?;
            self.builder.emit(Op::CursorAdvance)?;
            let done = self.builder.emit_jump(Op::JumpIfFalse(u32::MAX))?;
            self.builder.emit(Op::Jump(top))?;
            self.builder.patch_to_here(done);
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
}

fn select_width(tokens: &[Lexed]) -> Result<u16> {
    let mut depth = 0i32;
    let mut width = 1u16;
    let mut saw_from = false;
    for token in tokens {
        match &token.token {
            Token::Symbol("(") => depth += 1,
            Token::Symbol(")") => depth -= 1,
            Token::Identifier { name, quoted: false } if depth == 0 && name == "FROM" => {
                saw_from = true;
                break;
            }
            Token::Symbol(",") if depth == 0 => {
                width = width
                    .checked_add(1)
                    .ok_or_else(|| mpedb_types::Error::Unsupported("cursor SELECT list is too wide".into()))?;
            }
            _ => {}
        }
    }
    if !saw_from {
        return Err(mpedb_types::Error::Unsupported(
            "a cursor SELECT requires FROM".into(),
        ));
    }
    Ok(width)
}

#[cfg(test)]
mod tests {
    use super::super::compile;
    use crate::emit::CallKind;
    use crate::interp::{self, testutil::MockBridge, Budget, ProcValue};
    use crate::ir::{PlanKind, PlanRef, Proc};
    use mpedb_types::{PlanHash, Value};

    fn run(rows: Vec<Vec<Value>>, body: &str) -> (Value, MockBridge) {
        let source = format!("CREATE FUNCTION f RETURN NUMBER IS {body}");
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
        (value, bridge)
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
        let missing = "CURSOR c IS SELECT amount FROM dual; v NUMBER := 7; BEGIN OPEN c; FETCH c INTO v; RETURN v; END;";
        let (value, bridge) = run(vec![], missing);
        assert_eq!(value, Value::Numeric("7".into()));
        assert_eq!(bridge.live_streams(), 0);
    }
}
