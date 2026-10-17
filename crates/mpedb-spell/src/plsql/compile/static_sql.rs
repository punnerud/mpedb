//! Static SELECT INTO and DML use the same shared plans as dynamic SQL.
//! Until schema-aware escaped-identifier resolution exists, PL/SQL inputs must
//! use routine.variable qualification. Ambiguous bare locals are refused.
use super::*;
use crate::emit::CallKind;
enum StaticInput {
    Local(Local),
    Number(String),
}
impl Parser<'_> {
    pub(super) fn static_sql(&mut self) -> Result<()> {
        let start = self.at;
        let query = self.is_word("SELECT");
        let mut depth = 0i32;
        let mut into_at = None;
        let mut end = start;
        loop {
            let token = self
                .tokens
                .get(end)
                .ok_or_else(|| self.error("unterminated static SQL"))?;
            match &token.token {
                Token::Symbol(";") if depth == 0 => break,
                Token::Symbol("(") => depth += 1,
                Token::Symbol(")") => {
                    depth -= 1;
                    if depth < 0 {
                        return Err(self.error("unmatched parenthesis in static SQL"));
                    }
                }
                Token::Identifier {
                    name,
                    quoted: false,
                } if name == "INTO" && depth == 0 && query && into_at.replace(end).is_some() => {
                    return Err(self.error("multiple static SELECT INTO clauses"));
                }
                _ => {}
            }
            end += 1;
        }
        let source_start = self.tokens[start].span.start;
        let source_end = self.tokens[end].span.start;
        let mut into = Vec::new();
        let source = if query {
            let index = into_at.ok_or_else(|| self.error("static SELECT requires INTO targets"))?;
            self.at = index + 1;
            loop {
                let name = self.identifier()?;
                let target = *self
                    .locals
                    .get(&name)
                    .ok_or_else(|| self.error("unknown INTO target"))?;
                if !target.writable {
                    return Err(self.error("INTO target is constant or an IN parameter"));
                }
                into.push(target);
                if !self.symbol(",") {
                    break;
                }
            }
            if self.at != end && !self.is_word("FROM") {
                return Err(self.error("static SELECT INTO requires simple local targets followed by FROM or the statement end"));
            }
            format!(
                "{} {}",
                &self.source[source_start..self.tokens[index].span.start],
                &self.source[self.tokens[self.at].span.start..source_end]
            )
        } else {
            self.source[source_start..source_end].to_string()
        };
        self.at = end + 1;
        let (source, bindings) = self.bind_static_inputs(&source)?;
        let kind = if query {
            CallKind::Query
        } else {
            CallKind::Exec
        };
        let (sql, count) = super::sql::normalize_sql(&source, kind)?;
        if count != bindings.len() {
            return Err(self.error("static SQL bind count mismatch"));
        }
        for binding in &bindings {
            match binding {
                StaticInput::Local(local) => {
                    self.builder.emit(Op::LoadLocal(local.slot))?;
                }
                StaticInput::Number(text) => {
                    self.constant(Value::Numeric(text.clone()))?;
                    self.builder.emit(Op::OracleNumber)?;
                }
            }
        }
        self.emit_sql_call(sql, kind, count, &into, source_start)
    }
    fn bind_static_inputs(&self, source: &str) -> Result<(String, Vec<StaticInput>)> {
        let tokens = lex::lex(source)?;
        // Bare integer ordinals have clause-specific SQL semantics. Replacing
        // them with decimal expressions would silently change ORDER/GROUP BY.
        if tokens.iter().any(|t| matches!(t.token, Token::Number(_)))
            && tokens.windows(2).any(|pair| matches!((&pair[0].token, &pair[1].token),
                (Token::Identifier { name, quoted: false }, Token::Identifier { name: by, quoted: false })
                if matches!(name.as_str(), "ORDER" | "GROUP") && by == "BY")) {
            return Err(self.error("numeric literals with static SQL ORDER/GROUP BY require ordinal-aware resolution"));
        }
        let mut output = String::new();
        let mut bindings = Vec::new();
        let mut index = 0;
        let mut end = 0;
        while let Some(token) = tokens.get(index) {
            output.push_str(&source[end..token.span.start]);
            if let Token::Identifier { name, .. } = &token.token {
                if name == &self.routine_name {
                    if !matches!(
                        tokens.get(index + 1).map(|t| &t.token),
                        Some(Token::Symbol("."))
                    ) {
                        return Err(self
                            .error("static SQL cannot use the routine name as a table or alias"));
                    }
                    let Some(Token::Identifier {
                        name: local_name, ..
                    }) = tokens.get(index + 2).map(|t| &t.token)
                    else {
                        return Err(self.error("expected routine-qualified PL/SQL variable"));
                    };
                    if matches!(
                        tokens.get(index + 3).map(|t| &t.token),
                        Some(Token::Symbol("."))
                    ) {
                        return Err(
                            self.error("static SQL variable cannot have further qualification")
                        );
                    }
                    let local = *self
                        .routine_locals
                        .get(local_name)
                        .ok_or_else(|| self.error("unknown routine-qualified PL/SQL variable"))?;
                    bindings.push(StaticInput::Local(local));
                    output.push_str(":mpedb_static_bind");
                    end = tokens[index + 2].span.end;
                    index += 3;
                    continue;
                }
                let qualified = matches!(
                    index
                        .checked_sub(1)
                        .and_then(|i| tokens.get(i))
                        .map(|t| &t.token),
                    Some(Token::Symbol("."))
                ) || matches!(
                    tokens.get(index + 1).map(|t| &t.token),
                    Some(Token::Symbol("."))
                );
                if !qualified
                    && (self.locals.contains_key(name) || self.routine_locals.contains_key(name))
                {
                    return Err(self.error("bare PL/SQL names in static SQL require schema-aware resolution; qualify variables with the routine name and columns with a table alias"));
                }
            }
            if matches!(token.token, Token::Bind(_)) {
                return Err(self.error(
                    "static SQL takes routine-qualified variables, not dynamic bind placeholders",
                ));
            }
            if let Token::Number(text) = &token.token {
                // SQL's integer-token inference must not route an Oracle
                // NUMBER literal through int64 or binary floating point.
                let text = mpedb_types::value::parse_numeric(text)
                    .ok_or_else(|| self.error("invalid static SQL NUMBER literal"))?;
                bindings.push(StaticInput::Number(text));
                output.push_str(":mpedb_static_number");
            } else {
                output.push_str(&source[token.span.clone()]);
            }
            end = token.span.end;
            index += 1;
        }
        output.push_str(&source[end..]);
        Ok((output, bindings))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn qualified_inputs_bind_in_occurrence_order_and_into_is_removed() {
        let s = compile("CREATE FUNCTION f(p NUMBER) RETURN NUMBER IS n NUMBER(4,2); BEGIN SELECT t.amount+f.p INTO n FROM ledger t WHERE t.id=f.p; RETURN n; END;").unwrap();
        assert_eq!(s.calls.len(), 1);
        assert_eq!(s.calls[0].argc, 2);
        assert_eq!(s.calls[0].expected_columns, Some(1));
        assert!(s.calls[0].sql.contains("t.amount+$1"));
        assert!(s.calls[0].sql.contains("t.id=$2"));
        assert!(!s.calls[0].sql.contains("INTO"));
        assert!(s.instrs.contains(&Op::OracleNumberConstraint {
            precision: 4,
            scale: 2
        }));
    }
    #[test]
    fn root_qualification_survives_nested_local_shadowing() {
        let s = compile("CREATE PROCEDURE f(p NUMBER) IS BEGIN DECLARE p NUMBER:=9; BEGIN INSERT INTO ledger VALUES(f.p); END; END;").unwrap();
        let call = s.instrs.iter().position(|op| *op == Op::DbExec(0)).unwrap();
        assert_eq!(s.instrs[call - 1], Op::LoadLocal(0));
    }
    #[test]
    fn unsafe_or_unsupported_name_resolution_and_fetch_forms_are_refused() {
        for statement in [
            "DELETE FROM ledger WHERE id=p;",
            "SELECT amount FROM ledger;",
            "SELECT amount INTO p FROM ledger;",
            "SELECT amount INTO n.member FROM ledger;",
            "INSERT INTO ledger VALUES(:p);",
            "INSERT INTO ledger VALUES(f.missing);",
            "SELECT f.p INTO n FROM ledger f;",
            "UPDATE ledger SET amount=f.p RETURNING amount INTO n;",
            "SELECT t.amount INTO n FROM ledger t ORDER BY 1;",
        ] {
            assert!(compile(&format!("CREATE FUNCTION f(p NUMBER) RETURN NUMBER IS n NUMBER; BEGIN {statement} RETURN n; END;")).is_err(), "{statement}");
        }
    }
}
