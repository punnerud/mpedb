//! Literal native dynamic SQL lowered to MPEdb's existing shared SQL plans.
//! Runtime-generated SQL text, OUT binds, DDL and anonymous blocks
//! remain explicit gaps; no Python or PostgreSQL procedure translation occurs.
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
        let query = matches!(lex::lex(&source)?.first().map(|t| &t.token), Some(Token::Identifier { name, quoted: false }) if name == "SELECT");
        let kind = if query {
            CallKind::Query
        } else {
            CallKind::Exec
        };
        let (sql, required) =
            normalize_sql(&source, kind).map_err(|e| self.error(&e.to_string()))?;
        let mut into = Vec::new();
        if query {
            self.want_word("INTO")?;
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
        }
        let mut argc = 0usize;
        if self.word("USING") {
            loop {
                self.word("IN");
                if self.is_word("OUT") {
                    return Err(
                        self.error("EXECUTE IMMEDIATE OUT and IN OUT binds are not implemented")
                    );
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
        self.emit_sql_call(sql, kind, argc, &into, span)
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
                self.builder.emit(target.ty.check())?;
                self.builder.emit(Op::StoreLocal(target.slot))?;
            }
        } else {
            self.builder.emit(Op::DbExec(index))?;
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
            } if name == "RETURNING" => {
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
