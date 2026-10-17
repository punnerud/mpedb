//! Literal native dynamic SQL lowered to MPEdb's existing shared SQL plans.
//! DML `RETURNING INTO` binds are OUT. `IN OUT` evaluates the variable, then
//! stores the returned value, including NULL. No row raises `NO_DATA_FOUND`
//! and leaves the variable unchanged. Runtime-generated SQL text, DDL and
//! anonymous blocks remain explicit gaps; no Python or PostgreSQL procedure
//! translation occurs.
use super::*;
use crate::emit::{CallKind, SqlCall};

impl Parser<'_> {
    pub(super) fn execute_immediate(&mut self) -> Result<()> {
        self.want_word("IMMEDIATE")?;
        let Some(Token::String {
            value,
            national: false,
        }) = self.peek()
        else {
            return Err(self.error(
                "EXECUTE IMMEDIATE currently requires literal SQL compiled at define time",
            ));
        };
        let source = value.clone();
        let span = self.tokens[self.at].span.start;
        self.at += 1;
        let query = matches!(
            lex::lex(&source)?.first().map(|t| &t.token),
            Some(Token::Identifier { name, quoted: false }) if name == "SELECT"
        );
        if query {
            let (sql, required) =
                normalize_sql(&source, CallKind::Query).map_err(|e| self.error(&e.to_string()))?;
            self.want_word("INTO")?;
            let mut into = Vec::new();
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
            let mut argc = 0usize;
            if self.word("USING") {
                loop {
                    self.word("IN");
                    if self.is_word("OUT") {
                        return Err(self.error(
                            "OUT and IN OUT binds require a RETURNING INTO placeholder",
                        ));
                    }
                    if self.is_word("NULL") {
                        return Err(self.error(
                            "USING cannot bind the reserved word NULL; use an uninitialized variable",
                        ));
                    }
                    self.expression()?;
                    argc += 1;
                    if !self.symbol(",") {
                        break;
                    }
                }
            }
            if argc != required {
                return Err(self.error(
                    "EXECUTE IMMEDIATE requires one USING argument per placeholder occurrence",
                ));
            }
            self.want_symbol(";")?;
            return self.emit_sql_call(sql, CallKind::Query, argc, &into, span);
        }
        let DynamicDml {
            sql,
            inputs,
            returning,
            into_binds,
        } = prepare_dynamic_dml(&source).map_err(|e| self.error(&e.to_string()))?;
        let mut into = Vec::new();
        let total = inputs + into_binds;
        let mut index = 0usize;
        if self.word("USING") {
            loop {
                if index >= total {
                    return Err(self.error(
                        "EXECUTE IMMEDIATE requires one USING argument per placeholder occurrence",
                    ));
                }
                let output = into_binds > 0 && index >= inputs;
                let saw_in = self.word("IN");
                let saw_out = self.is_word("OUT");
                if output {
                    if saw_in && !saw_out {
                        return Err(self.error("a RETURNING INTO bind is OUT"));
                    }
                    if saw_out {
                        self.at += 1;
                    }
                    into.push(self.out_target(saw_in && saw_out)?);
                } else if saw_out {
                    return Err(self.error(
                        "OUT and IN OUT binds require a RETURNING INTO placeholder",
                    ));
                } else if self.is_word("NULL") {
                    return Err(self.error(
                        "USING cannot bind the reserved word NULL; use an uninitialized variable",
                    ));
                } else {
                    self.expression()?;
                }
                index += 1;
                if !self.symbol(",") {
                    break;
                }
            }
        }
        if index != total {
            return Err(self.error(
                "EXECUTE IMMEDIATE requires one USING argument per placeholder occurrence",
            ));
        }
        if self.word("RETURNING") || self.word("RETURN") {
            if into_binds > 0 {
                return Err(self.error(
                    "RETURNING INTO binds are already in the dynamic statement",
                ));
            }
            if returning == 0 {
                return Err(self.error(
                    "EXECUTE IMMEDIATE RETURNING INTO requires a RETURNING clause in the statement",
                ));
            }
            self.want_word("INTO")?;
            loop {
                into.push(self.out_target(false)?);
                if !self.symbol(",") {
                    break;
                }
            }
            if into.len() != returning {
                return Err(self.error(
                    "RETURNING INTO target count does not match the RETURNING list",
                ));
            }
        } else if returning > 0 && into_binds == 0 {
            return Err(self.error("EXECUTE IMMEDIATE RETURNING requires INTO targets"));
        }
        self.want_symbol(";")?;
        self.emit_sql_call(sql, CallKind::Exec, inputs, &into, span)
    }

    /// `evaluate` is the `IN OUT` pre-call: type-check the current value and
    /// drop it. The placeholder is not a SQL parameter.
    fn out_target(&mut self, evaluate: bool) -> Result<Local> {
        if self.is_word("NULL") {
            return Err(self.error(
                "USING cannot bind the reserved word NULL; use an uninitialized variable",
            ));
        }
        let name = self.identifier()?;
        let target = *self
            .locals
            .get(&name)
            .ok_or_else(|| self.error("unknown OUT target"))?;
        if !target.writable {
            return Err(self.error("OUT target is constant or an IN parameter"));
        }
        if evaluate {
            self.builder.emit(Op::LoadLocal(target.slot))?;
            self.emit_local_check(target)?;
            self.builder.emit(Op::Pop)?;
        }
        Ok(target)
    }

    pub(super) fn emit_sql_call(
        &mut self,
        sql: String,
        kind: CallKind,
        argc: usize,
        into: &[Local],
        span: usize,
    ) -> Result<()> {
        let query = kind == CallKind::Query;
        let (line, col) = crate::emit::line_col(self.source, span);
        let index = self.builder.add_call(SqlCall {
            sql,
            kind,
            argc: u8::try_from(argc).map_err(|_| self.error("too many USING arguments"))?,
            expected_columns: if query {
                Some(u16::try_from(into.len()).map_err(|_| self.error("too many INTO targets"))?)
            } else {
                None
            },
            line,
            col,
        })?;
        if query {
            self.builder.emit(Op::DbQuery(index))?;
            self.builder.emit(Op::OracleSelectOne)?;
            for (i, target) in into.iter().enumerate() {
                self.builder.emit(Op::Dup)?;
                self.constant(Value::Int(i as i64))?;
                self.builder.emit(Op::Index)?;
                self.emit_local_check(*target)?;
                self.builder.emit(Op::StoreLocal(target.slot))?;
            }
        } else if into.is_empty() {
            self.builder.emit(Op::DbExec(index))?;
        } else {
            self.builder.emit(Op::DbReturning(index))?;
            self.builder.emit(Op::OracleSelectOne)?;
            for (i, target) in into.iter().enumerate() {
                self.builder.emit(Op::Dup)?;
                self.constant(Value::Int(i as i64))?;
                self.builder.emit(Op::Index)?;
                self.emit_local_check(*target)?;
                self.builder.emit(Op::StoreLocal(target.slot))?;
            }
        }
        self.builder.emit(Op::Pop)?;
        Ok(())
    }
}

#[cfg(test)]
fn normalize_dml(source: &str) -> Result<(String, usize)> {
    normalize_sql(source, CallKind::Exec)
}

pub(super) fn normalize_sql(source: &str, kind: CallKind) -> Result<(String, usize)> {
    normalize_sql_allow(source, kind, false)
}

/// `allow_returning` copies a top-level `RETURNING` list through. The default
/// path still refuses it, so cursor text cannot grow a clause by accident.
pub(super) fn normalize_sql_allow(
    source: &str,
    kind: CallKind,
    allow_returning: bool,
) -> Result<(String, usize)> {
    let tokens = lex::lex(source)?;
    if !matches!(tokens.first().map(|t|&t.token),Some(Token::Identifier{name,quoted:false}) if match kind { CallKind::Query=>name=="SELECT", CallKind::Exec=>matches!(name.as_str(),"INSERT"|"UPDATE"|"DELETE"), CallKind::Rows=>false })
    {
        return Err(mpedb_types::Error::Unsupported(
            "EXECUTE IMMEDIATE accepts literal INSERT/UPDATE/DELETE or SELECT INTO only".into(),
        ));
    }
    let mut output = String::new();
    let mut end = 0;
    let mut count = 0;
    for token in tokens {
        output.push_str(&source[end..token.span.start]);
        match token.token {
            Token::Bind(_) => {
                count += 1;
                output.push_str(&format!("${count}"));
            }
            Token::String {
                value,
                national: false,
            } => {
                // Oracle empty SQL strings are NULL, including q-quoted ones.
                if value.is_empty() {
                    output.push_str("NULL");
                } else {
                    output.push('\'');
                    output.push_str(&value.replace('\'', "''"));
                    output.push('\'');
                }
            }
            Token::String { national: true, .. } => {
                return Err(mpedb_types::Error::Unsupported(
                    "national SQL strings require NVARCHAR2 support".into(),
                ))
            }
            Token::Symbol(";") => {
                return Err(mpedb_types::Error::Unsupported(
                    "dynamic SQL must contain one SQL statement without a semicolon".into(),
                ))
            }
            Token::Identifier {
                ref name,
                quoted: false,
            } if name == "RETURNING" && !allow_returning => {
                return Err(mpedb_types::Error::Unsupported(
                    "dynamic DML RETURNING is not implemented".into(),
                ))
            }
            Token::Symbol("~=" | "^=") => output.push_str("<>"),
            _ => output.push_str(&source[token.span.clone()]),
        }
        end = token.span.end;
    }
    output.push_str(&source[end..]);
    Ok((output, count))
}

struct DynamicDml {
    sql: String,
    inputs: usize,
    returning: usize,
    into_binds: usize,
}

fn unsupported(message: &str) -> mpedb_types::Error {
    mpedb_types::Error::Unsupported(message.into())
}

fn note_returning_expr(phase: u8, open: &mut bool, returning: &mut usize) {
    if phase == 1 && *open {
        if *returning == 0 {
            *returning = 1;
        }
        *open = false;
    }
}

/// DML preparer for `EXECUTE IMMEDIATE` only. [`normalize_sql`] keeps refusing
/// `RETURNING`. Static `RETURNING INTO` uses [`normalize_sql_allow`]; cursor
/// text stays on the refusing path. A `RETURNING` inside parentheses is part
/// of the statement, not the clause.
fn prepare_dynamic_dml(source: &str) -> Result<DynamicDml> {
    let tokens = lex::lex(source)?;
    if !matches!(
        tokens.first().map(|t| &t.token),
        Some(Token::Identifier { name, quoted: false })
            if matches!(name.as_str(), "INSERT" | "UPDATE" | "DELETE")
    ) {
        return Err(unsupported(
            "EXECUTE IMMEDIATE accepts literal INSERT/UPDATE/DELETE or SELECT INTO only",
        ));
    }
    let mut output = String::new();
    let mut end = 0usize;
    let mut inputs = 0usize;
    let mut returning = 0usize;
    let mut into_binds = 0usize;
    let mut phase = 0u8;
    let mut depth = 0i32;
    let mut expr_open = false;
    for token in &tokens {
        let gap = &source[end..token.span.start];
        let text = &source[token.span.clone()];
        match &token.token {
            Token::String { national: true, .. } => {
                return Err(unsupported(
                    "national SQL strings require NVARCHAR2 support",
                ));
            }
            Token::Symbol(";") => {
                return Err(unsupported(
                    "dynamic SQL must contain one SQL statement without a semicolon",
                ));
            }
            Token::Symbol(")") if depth == 0 => {
                return Err(unsupported("unmatched )"));
            }
            Token::Identifier { name, quoted: false } if name == "RETURNING" && depth == 0 => {
                if phase != 0 {
                    return Err(unsupported("multiple RETURNING clauses"));
                }
                output.push_str(gap);
                output.push_str(text);
                phase = 1;
                expr_open = true;
                returning = 0;
            }
            Token::Identifier { name, quoted: false }
                if name == "INTO" && depth == 0 && phase == 1 =>
            {
                if expr_open {
                    return Err(unsupported("RETURNING requires an expression"));
                }
                phase = 2;
            }
            Token::Bind(_) if phase == 1 => {
                return Err(unsupported(
                    "a RETURNING expression cannot be a bind placeholder",
                ));
            }
            Token::Bind(_) if phase == 2 => into_binds += 1,
            Token::Symbol(",") if phase == 2 => {}
            _ if phase == 2 => {
                return Err(unsupported("RETURNING INTO placeholders must be binds"));
            }
            Token::Symbol(",") if phase == 1 && depth == 0 => {
                if expr_open {
                    return Err(unsupported("RETURNING requires an expression"));
                }
                output.push_str(gap);
                output.push_str(&source[token.span.clone()]);
                returning += 1;
                expr_open = true;
            }
            Token::Bind(_) => {
                output.push_str(gap);
                inputs += 1;
                output.push_str(&format!("${inputs}"));
                note_returning_expr(phase, &mut expr_open, &mut returning);
            }
            Token::String {
                value,
                national: false,
            } => {
                output.push_str(gap);
                if value.is_empty() {
                    output.push_str("NULL");
                } else {
                    output.push('\'');
                    output.push_str(&value.replace('\'', "''"));
                    output.push('\'');
                }
                note_returning_expr(phase, &mut expr_open, &mut returning);
            }
            Token::Symbol("~=" | "^=") => {
                output.push_str(gap);
                output.push_str("<>");
                note_returning_expr(phase, &mut expr_open, &mut returning);
            }
            Token::Symbol("(") => {
                output.push_str(gap);
                output.push_str(text);
                depth += 1;
                note_returning_expr(phase, &mut expr_open, &mut returning);
            }
            Token::Symbol(")") => {
                output.push_str(gap);
                output.push_str(text);
                depth -= 1;
                note_returning_expr(phase, &mut expr_open, &mut returning);
            }
            _ => {
                output.push_str(gap);
                output.push_str(text);
                note_returning_expr(phase, &mut expr_open, &mut returning);
            }
        }
        end = token.span.end;
    }
    if depth != 0 {
        return Err(unsupported("unmatched ("));
    }
    if phase == 1 && expr_open {
        return Err(unsupported("RETURNING requires an expression"));
    }
    if phase != 2 {
        output.push_str(&source[end..]);
    }
    if phase == 2 && into_binds != returning {
        return Err(unsupported(
            "RETURNING INTO bind count does not match the RETURNING list",
        ));
    }
    Ok(DynamicDml {
        sql: output,
        inputs,
        returning,
        into_binds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interp::{self, testutil::MockBridge, Budget, ProcValue};
    use crate::ir::{Op, PlanKind, PlanRef, Proc};
    use mpedb_types::{PlanHash, Value};

    #[test]
    fn repeated_names_are_positional_and_quoted_colons_are_not_binds() {
        let (sql, n) =
            normalize_dml("INSERT INTO t VALUES(:x,:x,q'[a':z]', '') -- :ignored\n").unwrap();
        assert_eq!(n, 2);
        assert_eq!(
            sql,
            "INSERT INTO t VALUES($1,$2,'a'':z', NULL) -- :ignored\n"
        );
    }
    #[test]
    fn statements_outside_the_supported_transport_contract_refuse() {
        for sql in [
            "SELECT 1 FROM dual",
            "CREATE TABLE t(id NUMBER)",
            "BEGIN NULL; END;",
            "INSERT INTO t VALUES(1);",
            "UPDATE t SET x=:1 RETURNING x INTO :2",
        ] {
            assert!(normalize_dml(sql).is_err(), "{sql}");
        }
    }

    const UPDATE_IN: &str =
        "UPDATE t SET payload = :1 WHERE id = :2 RETURNING payload INTO :3";
    const UPDATE_OUT: &str = "UPDATE t SET payload = $1 WHERE id = $2 RETURNING payload";

    fn run(
        rows: Option<Vec<Vec<Value>>>,
        returns: &str,
        body: &str,
    ) -> (Value, MockBridge, Proc, String, u8) {
        let source = format!("CREATE FUNCTION f RETURN {returns} IS {body}");
        let skel = super::super::compile(&source).unwrap();
        let sql = skel.calls[0].sql.clone();
        let argc = skel.calls[0].argc;
        assert_eq!(skel.calls[0].kind, CallKind::Exec);
        assert_eq!(skel.calls[0].expected_columns, None);
        let plans = skel
            .calls
            .iter()
            .map(|call| PlanRef {
                hash: PlanHash([0; 32]),
                kind: PlanKind::Exec,
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
        assert!(proc.has_exec());
        let mut bridge = MockBridge::new();
        bridge.returning_rows = rows;
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
        (value, bridge, proc, sql, argc)
    }

    fn loads_before_returning(proc: &Proc) -> usize {
        let at = proc
            .instrs
            .iter()
            .position(|op| matches!(op, Op::DbReturning(_)))
            .unwrap();
        proc.instrs[..at]
            .iter()
            .filter(|op| matches!(op, Op::LoadLocal(_)))
            .count()
    }

    fn refuse(source: &str, needle: &str) {
        let err = super::super::compile(source).unwrap_err().to_string();
        assert!(err.contains(needle), "{err}");
    }

    #[test]
    fn dynamic_out_binds_assign_null_and_keep_a_missing_row() {
        let null_body = format!(
            "v NUMBER := 7; BEGIN EXECUTE IMMEDIATE '{UPDATE_IN}' USING 4, 1, OUT v; RETURN v; END;"
        );
        let (value, bridge, proc, sql, argc) =
            run(Some(vec![vec![Value::Null]]), "NUMBER", &null_body);
        assert_eq!(value, Value::Null);
        assert_eq!(sql, UPDATE_OUT);
        assert!(sql.contains("RETURNING"));
        assert!(!sql.contains("INTO"));
        assert!(!sql.contains(':'));
        assert_eq!(argc, 2);
        assert_eq!(
            bridge.exec_params,
            vec![vec![Value::Int(4), Value::Int(1)]]
        );
        assert_eq!(loads_before_returning(&proc), 0);

        let (value, bridge, _, sql, argc) = run(
            Some(vec![vec![Value::Numeric("4".into())]]),
            "NUMBER",
            &null_body,
        );
        assert_eq!(value, Value::Numeric("4".into()));
        assert_eq!(sql, UPDATE_OUT);
        assert_eq!(argc, 2);
        assert_eq!(
            bridge.exec_params,
            vec![vec![Value::Int(4), Value::Int(1)]]
        );

        let bare = format!(
            "v NUMBER := 7; BEGIN EXECUTE IMMEDIATE '{UPDATE_IN}' USING 4, 1, v; RETURN v; END;"
        );
        let (value, _, proc, _, _) = run(
            Some(vec![vec![Value::Numeric("4".into())]]),
            "NUMBER",
            &bare,
        );
        assert_eq!(value, Value::Numeric("4".into()));
        assert_eq!(loads_before_returning(&proc), 0);

        let inout = format!(
            "v NUMBER := 7; BEGIN EXECUTE IMMEDIATE '{UPDATE_IN}' USING 4, 1, IN OUT v; RETURN v; END;"
        );
        let (value, bridge, proc, sql, argc) =
            run(Some(vec![vec![Value::Null]]), "NUMBER", &inout);
        assert_eq!(value, Value::Null);
        assert_eq!(sql, UPDATE_OUT);
        assert_eq!(argc, 2);
        assert_eq!(loads_before_returning(&proc), 1);
        assert_eq!(
            bridge.exec_params,
            vec![vec![Value::Int(4), Value::Int(1)]]
        );

        let missing = format!(
            "v NUMBER := 7; BEGIN EXECUTE IMMEDIATE '{UPDATE_IN}' USING 4, 1, OUT v; RETURN v; EXCEPTION WHEN NO_DATA_FOUND THEN RETURN v; END;"
        );
        let (value, _, _, _, _) = run(Some(vec![]), "NUMBER", &missing);
        assert_eq!(value, Value::Numeric("7".into()));
        let errmsg = format!(
            "v NUMBER := 7; BEGIN EXECUTE IMMEDIATE '{UPDATE_IN}' USING 4, 1, OUT v; RETURN 'ok'; EXCEPTION WHEN NO_DATA_FOUND THEN RETURN SQLERRM; END;"
        );
        let (text, _, _, _, _) = run(Some(vec![]), "VARCHAR2", &errmsg);
        assert_eq!(text, Value::Text("ORA-01403: no data found".into()));

        let two = "a NUMBER; b NUMBER; BEGIN EXECUTE IMMEDIATE 'UPDATE t SET payload = :1 WHERE id = :2 RETURNING x, y INTO :3, :4' USING 4, 1, OUT a, OUT b; RETURN a; END;";
        let (value, _, _, sql, argc) = run(
            Some(vec![vec![Value::Null, Value::Numeric("9".into())]]),
            "NUMBER",
            two,
        );
        assert_eq!(value, Value::Null);
        assert_eq!(
            sql,
            "UPDATE t SET payload = $1 WHERE id = $2 RETURNING x, y"
        );
        assert!(sql.contains("RETURNING"));
        assert!(!sql.contains("INTO"));
        assert_eq!(argc, 2);
        let two_b = "a NUMBER; b NUMBER; BEGIN EXECUTE IMMEDIATE 'UPDATE t SET payload = :1 WHERE id = :2 RETURNING x, y INTO :3, :4' USING 4, 1, OUT a, OUT b; RETURN b; END;";
        let (value, _, _, _, _) = run(
            Some(vec![vec![Value::Null, Value::Numeric("9".into())]]),
            "NUMBER",
            two_b,
        );
        assert_eq!(value, Value::Numeric("9".into()));

        let external = "v NUMBER := 7; BEGIN EXECUTE IMMEDIATE 'UPDATE t SET payload = :1 WHERE id = :2 RETURNING payload' USING 4, 1 RETURNING INTO v; RETURN v; END;";
        let (value, bridge, _, sql, argc) = run(
            Some(vec![vec![Value::Numeric("4".into())]]),
            "NUMBER",
            external,
        );
        assert_eq!(value, Value::Numeric("4".into()));
        assert_eq!(sql, UPDATE_OUT);
        assert!(!sql.contains("INTO"));
        assert_eq!(argc, 2);
        assert_eq!(
            bridge.exec_params,
            vec![vec![Value::Int(4), Value::Int(1)]]
        );

        let insert = "v NUMBER; BEGIN EXECUTE IMMEDIATE 'INSERT INTO t(id) VALUES (:1) RETURNING id INTO :2' USING 4, OUT v; RETURN v; END;";
        let (value, _, _, sql, argc) =
            run(Some(vec![vec![Value::Numeric("4".into())]]), "NUMBER", insert);
        assert_eq!(value, Value::Numeric("4".into()));
        assert_eq!(sql, "INSERT INTO t(id) VALUES ($1) RETURNING id");
        assert!(sql.contains("INSERT INTO"));
        assert_eq!(argc, 1);

        let many = format!(
            "v NUMBER := 7; BEGIN EXECUTE IMMEDIATE '{UPDATE_IN}' USING 4, 1, OUT v; RETURN 'assigned'; EXCEPTION WHEN TOO_MANY_ROWS THEN RETURN SQLERRM; END;"
        );
        let (text, _, _, _, _) = run(
            Some(vec![
                vec![Value::Numeric("4".into())],
                vec![Value::Numeric("9".into())],
            ]),
            "VARCHAR2",
            &many,
        );
        assert_eq!(
            text,
            Value::Text(
                "ORA-01422: exact fetch returns more than requested number of rows".into()
            )
        );
        let many_kept = format!(
            "v NUMBER := 7; BEGIN EXECUTE IMMEDIATE '{UPDATE_IN}' USING 4, 1, OUT v; RETURN v; EXCEPTION WHEN TOO_MANY_ROWS THEN RETURN v; END;"
        );
        let (value, _, _, _, _) = run(
            Some(vec![
                vec![Value::Numeric("4".into())],
                vec![Value::Numeric("9".into())],
            ]),
            "NUMBER",
            &many_kept,
        );
        assert_eq!(value, Value::Numeric("7".into()));

        let plain = "BEGIN EXECUTE IMMEDIATE 'UPDATE t SET x = :1' USING 1; RETURN 1; END;";
        let (value, bridge, proc, sql, argc) = run(None, "NUMBER", plain);
        assert_eq!(value, Value::Numeric("1".into()));
        assert_eq!(sql, "UPDATE t SET x = $1");
        assert_eq!(argc, 1);
        assert!(bridge.exec_params.is_empty());
        assert!(proc.instrs.iter().any(|op| matches!(op, Op::DbExec(_))));
        assert!(!proc
            .instrs
            .iter()
            .any(|op| matches!(op, Op::DbReturning(_))));

        let update = format!("v NUMBER; BEGIN EXECUTE IMMEDIATE '{UPDATE_IN}'");
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS v NUMBER; BEGIN EXECUTE IMMEDIATE 'UPDATE t SET x = :1' USING OUT v; RETURN v; END;",
            "OUT and IN OUT binds require a RETURNING INTO placeholder",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS n NUMBER; BEGIN EXECUTE IMMEDIATE 'SELECT payload FROM t WHERE id = :1' INTO n USING OUT n; RETURN n; END;",
            "OUT and IN OUT binds require a RETURNING INTO placeholder",
        );
        refuse(
            &format!(
                "CREATE FUNCTION f RETURN NUMBER IS {update} USING 4, 1, IN v; RETURN v; END;"
            ),
            "a RETURNING INTO bind is OUT",
        );
        refuse(
            &format!(
                "CREATE FUNCTION f RETURN NUMBER IS {update} USING OUT v, 1, OUT v; RETURN v; END;"
            ),
            "OUT and IN OUT binds require a RETURNING INTO placeholder",
        );
        refuse(
            &format!(
                "CREATE FUNCTION f RETURN NUMBER IS {update} USING NULL, 1, OUT v; RETURN v; END;"
            ),
            "USING cannot bind the reserved word NULL; use an uninitialized variable",
        );
        refuse(
            &format!(
                "CREATE FUNCTION f RETURN NUMBER IS {update} USING 4, 1, OUT 1; RETURN v; END;"
            ),
            "expected identifier",
        );
        refuse(
            "CREATE FUNCTION f(p IN NUMBER) RETURN NUMBER IS BEGIN EXECUTE IMMEDIATE 'UPDATE t SET payload = :1 WHERE id = :2 RETURNING payload INTO :3' USING 1, 1, OUT p; RETURN p; END;",
            "OUT target is constant or an IN parameter",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS v NUMBER; BEGIN EXECUTE IMMEDIATE 'UPDATE t SET payload = :1 WHERE id = :2 RETURNING payload' USING 4, 1; RETURN v; END;",
            "EXECUTE IMMEDIATE RETURNING requires INTO targets",
        );
        refuse(
            &format!(
                "CREATE FUNCTION f RETURN NUMBER IS v NUMBER; w NUMBER; BEGIN EXECUTE IMMEDIATE '{UPDATE_IN}' USING 4, 1, OUT v RETURNING INTO w; RETURN v; END;"
            ),
            "RETURNING INTO binds are already in the dynamic statement",
        );
    }
}
