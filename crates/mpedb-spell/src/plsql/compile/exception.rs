//! Native block handlers; declarations execute outside their own protected body.
use super::*;

pub(super) fn code(name: &str) -> Option<u16> {
    Some(match name {
        "DUP_VAL_ON_INDEX" => 1,
        "NO_DATA_FOUND" => 1403,
        "TOO_MANY_ROWS" => 1422,
        "ZERO_DIVIDE" => 1476,
        "VALUE_ERROR" => 6502,
        _ => return None,
    })
}
impl Parser<'_> {
    pub(super) fn raise_statement(&mut self) -> Result<()> {
        if self.symbol(";") {
            if self.in_handler == 0 {
                return Err(self.error("RAISE without a name requires an exception handler"));
            }
            self.builder.emit(Op::OracleReraise)?;
        } else {
            let name = self.identifier()?;
            if self.locals.contains_key(&name) {
                return Err(self.error("RAISE target is a variable, not an exception"));
            }
            let code =
                code(&name).ok_or_else(|| self.error("named exception is not implemented"))?;
            self.want_symbol(";")?;
            self.builder.emit(Op::OracleRaise(code))?;
        }
        Ok(())
    }
    pub(super) fn block_body(&mut self) -> Result<()> {
        if !self.exception_blocks {
            return self.statements();
        }
        let entry = self.builder.emit(Op::OracleTry {
            handler: 0,
            end: 0,
            handler_end: 0,
        })?;
        self.statements()?;
        if !self.word("EXCEPTION") {
            // This block has no handler; nested handlers were compiled normally.
            self.builder.instrs[entry] = Op::Jump(entry as u32 + 1);
            return Ok(());
        }
        let protected_end = self.builder.here();
        let normal = self.builder.emit(Op::Jump(0))?;
        let handler = self.builder.here();
        let mut done = vec![normal];
        let mut seen = HashSet::new();
        let mut others = false;
        if !self.is_word("WHEN") {
            return Err(self.error("EXCEPTION requires at least one WHEN handler"));
        }
        while self.word("WHEN") {
            if others {
                return Err(self.error("WHEN OTHERS must be last"));
            }
            let mut yes = Vec::new();
            others = self.word("OTHERS");
            if others {
                if self.is_word("OR") {
                    return Err(self.error("OTHERS cannot be combined with another exception"));
                }
            } else {
                loop {
                    let name = self.identifier()?;
                    if self.locals.contains_key(&name) {
                        return Err(self.error("handler name is a variable, not an exception"));
                    }
                    let code = code(&name)
                        .ok_or_else(|| self.error("named exception handler is not implemented"))?;
                    if !seen.insert(code) {
                        return Err(
                            self.error("exception appears in more than one handler alternative")
                        );
                    }
                    self.builder.emit(Op::OracleExceptionCode)?;
                    self.constant(Value::Int(code as i64))?;
                    self.builder.emit(Op::Eq)?;
                    yes.push(self.builder.emit(Op::JumpIfTrue(0))?);
                    if !self.word("OR") {
                        break;
                    }
                }
            }
            self.want_word("THEN")?;
            let next = if others {
                None
            } else {
                Some(self.builder.emit(Op::Jump(0))?)
            };
            for jump in yes {
                self.builder.patch_to_here(jump);
            }
            self.in_handler += 1;
            self.statements()?;
            self.in_handler -= 1;
            done.push(self.builder.emit(Op::Jump(0))?);
            if let Some(next) = next {
                self.builder.patch_to_here(next);
            }
        }
        // No matching handler propagates to the enclosing protected block.
        self.builder.emit(Op::OracleReraise)?;
        let handler_end = self.builder.here();
        for jump in done {
            self.builder.patch_to_here(jump);
        }
        self.builder.instrs[entry] = Op::OracleTry {
            handler,
            end: protected_end,
            handler_end,
        };
        Ok(())
    }
}

impl Parser<'_> {
    pub(super) fn raise_application(&mut self) -> Result<()> {
        self.want_symbol("(")?;
        let actual = self.expression()?;
        self.compatible(Type::Number, actual)?;
        self.builder.emit(Op::OraclePlSqlInteger)?;
        self.want_symbol(",")?;
        let actual = self.expression()?;
        self.compatible(Type::Varchar(0), actual)?;
        self.builder.emit(Op::OracleVarchar(0))?;
        if self.symbol(",") && !self.word("FALSE") {
            return Err(self.error("RAISE_APPLICATION_ERROR error-stack preservation is not implemented; only FALSE is supported"));
        }
        self.want_symbol(")")?;
        self.want_symbol(";")?;
        self.builder.emit(Op::OracleRaiseApplication)?;
        Ok(())
    }
}
