//! `SELECT … BULK COLLECT INTO` a local `TABLE OF` scalar.
//! An uninitialized nested table is NULL. Zero fetched rows leave an empty
//! table, so `IS NULL` is false and `SQL%ROWCOUNT` is 0. A NULL column stays
//! NULL. `COUNT` of an uninitialized table raises `COLLECTION_IS_NULL`.
//! An empty table's `COUNT` is 0, and a NULL element still counts.
//! `INDEX BY` and `VARRAY` stay refused.

use super::*;
use crate::emit::{CallKind, SqlCall};

impl Parser<'_> {
    pub(super) fn looks_like_nested_type(&self) -> bool {
        let word = |offset: usize, name: &str| {
            matches!(
                self.tokens.get(self.at + offset).map(|token| &token.token),
                Some(Token::Identifier { name: found, quoted: false }) if found == name
            )
        };
        word(0, "TYPE")
            && matches!(
                self.tokens.get(self.at + 1).map(|token| &token.token),
                Some(Token::Identifier { .. })
            )
            && word(2, "IS")
    }

    pub(super) fn declare_nested_type(&mut self) -> Result<()> {
        self.want_word("TYPE")?;
        let name = self.identifier()?;
        self.want_word("IS")?;
        if self.word("RECORD") {
            return self.declare_record_body(name);
        }
        if self.word("VARRAY") {
            return Err(self.error("VARRAY collections are not implemented"));
        }
        self.want_word("TABLE")?;
        self.want_word("OF")?;
        let elem = self.data_type(true)?;
        if self.word("INDEX") {
            return Err(self.error("INDEX BY collections are not implemented"));
        }
        self.want_symbol(";")?;
        if !self.declared_here.insert(name.clone()) {
            return Err(self.error("duplicate parameter or variable name"));
        }
        self.pending_declarations.remove("TYPE");
        self.pending_declarations.remove(&name);
        self.nested_types.insert(name, elem);
        Ok(())
    }

    pub(super) fn declare_nested_var(&mut self, name: &str, constant: bool) -> Result<bool> {
        let Some(Token::Identifier {
            name: type_name,
            quoted: false,
        }) = self.peek()
        else {
            return Ok(false);
        };
        let Some(elem) = self.nested_types.get(type_name).copied() else {
            return Ok(false);
        };
        self.at += 1;
        if constant {
            return Err(self.error("a nested table cannot be CONSTANT"));
        }
        if self.word("NOT") {
            return Err(self.error("a nested table cannot be NOT NULL"));
        }
        if (self.symbol(":=") || self.word("DEFAULT")) && !self.word("NULL") {
            return Err(self.error("a nested table can only be initialized with NULL"));
        }
        self.want_symbol(";")?;
        if !self.declared_here.insert(name.to_string()) {
            return Err(self.error("duplicate parameter or variable name"));
        }
        self.pending_declarations.remove(name);
        self.locals.remove(name);
        self.records.remove(name);
        self.constant(Value::Null)?;
        let slot = self.builder.alloc_local()?;
        self.builder.emit(Op::StoreLocal(slot))?;
        self.nested_vars.insert(name.to_string(), NestedVar { slot, elem });
        Ok(true)
    }

    pub(super) fn assign_nested(&mut self, nested: NestedVar) -> Result<()> {
        self.want_symbol(":=")?;
        if !self.word("NULL") {
            return Err(self.error("a nested table can only be assigned NULL"));
        }
        self.constant(Value::Null)?;
        self.builder.emit(Op::StoreLocal(nested.slot))?;
        self.want_symbol(";")
    }

    pub(super) fn nested_expr(&mut self, nested: NestedVar) -> Result<Option<Type>> {
        if self.symbol(".") {
            if self.word("COUNT") {
                if self.symbol("(") {
                    self.want_symbol(")")?;
                }
                self.builder.emit(Op::LoadLocal(nested.slot))?;
                self.builder.emit(Op::NestedCount)?;
                return Ok(Some(Type::PlsInteger));
            }
            return Err(self.error("nested table methods are not implemented"));
        }
        if self.symbol("(") {
            self.forall_subscripts = self.forall_subscripts.saturating_add(1);
            let actual = self.expression();
            self.forall_subscripts = self.forall_subscripts.saturating_sub(1);
            let actual = actual?;
            self.compatible(Type::PlsInteger, actual)?;
            self.builder.emit(Op::OraclePlSqlInteger)?;
            self.want_symbol(")")?;
            self.builder.emit(Op::LoadLocal(nested.slot))?;
            self.builder.emit(Op::NestedIndex)?;
            return Ok(Some(nested.elem));
        }
        if self.is_word("IS") {
            self.builder.emit(Op::LoadLocal(nested.slot))?;
            return Ok(None);
        }
        Err(self.error("a nested table requires a subscript"))
    }

    pub(super) fn looks_like_bulk_collect(&self) -> bool {
        let mut depth = 0i32;
        let mut index = self.at;
        while let Some(token) = self.tokens.get(index) {
            match &token.token {
                Token::Symbol("(") => depth += 1,
                Token::Symbol(")") => depth -= 1,
                Token::Symbol(";") if depth == 0 => return false,
                Token::Identifier { name, quoted: false }
                    if depth == 0 && name == "BULK" && self.bulk_collect_follows(index) =>
                {
                    return true;
                }
                _ => {}
            }
            index += 1;
        }
        false
    }

    fn bulk_collect_follows(&self, bulk: usize) -> bool {
        let word = |offset: usize, expected: &str| {
            matches!(
                self.tokens.get(bulk + offset).map(|token| &token.token),
                Some(Token::Identifier { name, quoted: false }) if name == expected
            )
        };
        word(1, "COLLECT") && word(2, "INTO")
    }

    pub(super) fn bulk_collect(&mut self) -> Result<()> {
        let start = self.at;
        self.want_word("SELECT")?;
        let mut depth = 0i32;
        let mut bulk_at = None;
        let mut end = start;
        loop {
            let token = self
                .tokens
                .get(end)
                .ok_or_else(|| self.error("unterminated BULK COLLECT"))?;
            match &token.token {
                Token::Symbol(";") if depth == 0 => break,
                Token::Symbol("(") => depth += 1,
                Token::Symbol(")") => {
                    depth -= 1;
                    if depth < 0 {
                        return Err(self.error("unmatched parenthesis in BULK COLLECT"));
                    }
                }
                // A failed guard still stores the first clause. The arm runs
                // only when a second one replaces it.
                Token::Identifier { name, quoted: false }
                    if depth == 0
                        && name == "BULK"
                        && self.bulk_collect_follows(end)
                        && bulk_at.replace(end).is_some() =>
                {
                    return Err(self.error("multiple BULK COLLECT clauses"));
                }
                _ => {}
            }
            end += 1;
        }
        let bulk_at = bulk_at.ok_or_else(|| self.error("BULK COLLECT requires INTO"))?;
        if select_items(&self.tokens[start + 1..bulk_at]) != 1 {
            return Err(self.error("BULK COLLECT requires one select item"));
        }
        let target_at = bulk_at + 3;
        let target_name = match self.tokens.get(target_at).map(|token| &token.token) {
            Some(Token::Identifier { name, .. }) => name.clone(),
            _ => return Err(self.error("BULK COLLECT INTO requires a nested table")),
        };
        let nested = self
            .nested_vars
            .get(&target_name)
            .copied()
            .ok_or_else(|| self.error("BULK COLLECT INTO requires a nested table"))?;
        if matches!(
            self.tokens.get(target_at + 1).map(|token| &token.token),
            Some(Token::Symbol(","))
        ) {
            return Err(self.error("BULK COLLECT INTO takes one nested table"));
        }
        let sql_start = self.tokens[start].span.start;
        let sql_end = self.tokens[end].span.start;
        let cut_start = self.tokens[bulk_at].span.start;
        let cut_end = self.tokens[target_at].span.end;
        let mut sql_source = String::new();
        sql_source.push_str(&self.source[sql_start..cut_start]);
        sql_source.push(' ');
        sql_source.push_str(&self.source[cut_end..sql_end]);
        let (sql, bindings) = self.bind_static_inputs(&sql_source)?;
        let (sql, count) = super::sql::normalize_sql(&sql, CallKind::Query)
            .map_err(|err| self.error(&err.to_string()))?;
        if count != bindings.len() {
            return Err(self.error("static SQL bind count mismatch"));
        }
        for binding in &bindings {
            match binding {
                super::static_sql::StaticInput::Local(local) => {
                    self.builder.emit(Op::LoadLocal(local.slot))?;
                }
                super::static_sql::StaticInput::Number(text) => {
                    self.constant(Value::Numeric(text.clone()))?;
                    self.builder.emit(Op::OracleNumber)?;
                }
            }
        }
        let (line, col) = crate::emit::line_col(self.source, sql_start);
        let index = self.builder.add_call(SqlCall {
            sql,
            kind: CallKind::Query,
            argc: u8::try_from(count).map_err(|_| self.error("too many BULK COLLECT inputs"))?,
            expected_columns: Some(1),
            line,
            col,
        })?;
        self.builder.emit(Op::DbQuery(index))?;
        self.builder.emit(Op::BulkCollect)?;
        self.builder.emit(Op::StoreLocal(nested.slot))?;
        self.at = end + 1;
        Ok(())
    }
}

/// Select items before `BULK`. `DISTINCT` does not count. A comma at depth 0
/// separates items.
fn select_items(tokens: &[lex::Lexed]) -> usize {
    let mut depth = 0i32;
    let mut items = 0usize;
    let mut saw = false;
    let mut prefix = true;
    for token in tokens {
        match &token.token {
            Token::Symbol("(") => {
                depth += 1;
                prefix = false;
                saw = true;
            }
            Token::Symbol(")") => {
                depth -= 1;
                prefix = false;
            }
            Token::Symbol(",") if depth == 0 => {
                items += 1;
                saw = false;
                prefix = false;
            }
            Token::Identifier { name, quoted: false }
                if depth == 0
                    && prefix
                    && matches!(name.as_str(), "DISTINCT" | "ALL" | "UNIQUE") => {}
            _ if depth == 0 => {
                prefix = false;
                saw = true;
            }
            _ => {}
        }
    }
    if saw {
        items += 1;
    }
    items
}

#[cfg(test)]
mod tests {
    use super::super::compile;
    use crate::emit::CallKind;
    use crate::interp::{self, testutil::MockBridge, Budget, ProcValue};
    use crate::ir::{PlanKind, PlanRef, Proc};
    use mpedb_types::{PlanHash, Value};

    fn scene(returns: &str, body: &str, rows: Vec<Vec<Value>>) -> (Value, MockBridge, Proc) {
        let source = format!("CREATE FUNCTION f RETURN {returns} IS {body}");
        let skel = compile(&source).unwrap_or_else(|err| panic!("{err}"));
        assert!(skel.calls.iter().all(|call| call.kind == CallKind::Query || call.kind == CallKind::Exec));
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
        let proc = Proc::new(skel.name, skel.argc, skel.nlocals, plans, skel.consts, skel.instrs)
            .unwrap();
        let proc = Proc::decode(&proc.encode()).unwrap();
        let mut bridge = MockBridge::new();
        bridge.rows = rows;
        bridge.affected = 4;
        let value = match interp::run(
            &proc,
            &[],
            &mut bridge,
            Budget {
                instrs: 10_000,
                db_calls: 8,
                rows: 100,
            },
        )
        .unwrap_or_else(|err| panic!("{err}"))
        {
            ProcValue::Scalar(value) => value,
            other => panic!("{other:?}"),
        };
        (value, bridge, proc)
    }

    fn fail(returns: &str, body: &str, rows: Vec<Vec<Value>>) -> String {
        let source = format!("CREATE FUNCTION f RETURN {returns} IS {body}");
        let skel = compile(&source).unwrap_or_else(|err| panic!("{err}"));
        let plans = skel
            .calls
            .iter()
            .map(|call| PlanRef {
                hash: PlanHash([0; 32]),
                kind: PlanKind::Query,
                argc: call.argc,
            })
            .collect();
        let proc = Proc::new(skel.name, skel.argc, skel.nlocals, plans, skel.consts, skel.instrs)
            .unwrap();
        let proc = Proc::decode(&proc.encode()).unwrap();
        let mut bridge = MockBridge::new();
        bridge.rows = rows;
        interp::run(
            &proc,
            &[],
            &mut bridge,
            Budget {
                instrs: 10_000,
                db_calls: 8,
                rows: 100,
            },
        )
        .unwrap_err()
        .to_string()
    }

    #[test]
    fn bulk_collect_keeps_null_elements_and_an_uninitialized_table_is_null() {
        let decl = "TYPE nums IS TABLE OF NUMBER; n nums;";
        let (uninitialized, bridge, _) = scene(
            "BOOLEAN",
            &format!("{decl} BEGIN RETURN n IS NULL; END;"),
            vec![],
        );
        assert_eq!(uninitialized, Value::Bool(true));
        assert_eq!(bridge.queries, 0);
        let (assigned, _, _) = scene(
            "BOOLEAN",
            &format!("{decl} BEGIN n := NULL; RETURN n IS NULL; END;"),
            vec![],
        );
        assert_eq!(assigned, Value::Bool(true));

        let collect = format!("{decl} BEGIN SELECT amount BULK COLLECT INTO n FROM t;");
        let (element, bridge, proc) = scene(
            "NUMBER",
            &format!("{collect} RETURN n(1); END;"),
            vec![vec![Value::Null]],
        );
        assert_eq!(element, Value::Null);
        assert_eq!(bridge.queries, 1);
        assert_eq!(proc.plans.len(), 1);
        assert_eq!(proc.plans[0].kind, PlanKind::Query);
        assert_eq!(proc.plans[0].argc, 0);
        assert!(proc.instrs.iter().any(|op| matches!(op, crate::ir::Op::BulkCollect)));
        assert!(!proc.instrs.iter().any(|op| matches!(op, crate::ir::Op::OracleSelectOne)));
        let (present, _, _) = scene(
            "BOOLEAN",
            &format!("{collect} RETURN n IS NULL; END;"),
            vec![vec![Value::Null]],
        );
        assert_eq!(present, Value::Bool(false));
        let (count, _, _) = scene(
            "PLS_INTEGER",
            &format!("{collect} RETURN SQL%ROWCOUNT; END;"),
            vec![vec![Value::Null]],
        );
        assert_eq!(count, Value::Int(1));
        let (found, _, _) = scene(
            "BOOLEAN",
            &format!("{collect} RETURN SQL%FOUND; END;"),
            vec![vec![Value::Null]],
        );
        assert_eq!(found, Value::Bool(true));

        let rows = vec![vec![Value::Numeric("4".into())], vec![Value::Null]];
        let (first, _, _) = scene(
            "NUMBER",
            &format!("{collect} RETURN n(1); END;"),
            rows.clone(),
        );
        assert_eq!(first, Value::Numeric("4".into()));
        let (second, _, _) = scene(
            "NUMBER",
            &format!("{collect} RETURN n(2); END;"),
            rows.clone(),
        );
        assert_eq!(second, Value::Null);
        let (many, _, _) = scene(
            "PLS_INTEGER",
            &format!("{collect} RETURN SQL%ROWCOUNT; END;"),
            rows,
        );
        assert_eq!(many, Value::Int(2));

        let (empty_null, bridge, _) = scene(
            "BOOLEAN",
            &format!("{collect} RETURN n IS NULL; END;"),
            vec![],
        );
        assert_eq!(empty_null, Value::Bool(false));
        assert_eq!(bridge.queries, 1);
        let (empty_count, _, _) = scene(
            "PLS_INTEGER",
            &format!("{collect} RETURN SQL%ROWCOUNT; END;"),
            vec![],
        );
        assert_eq!(empty_count, Value::Int(0));
        let (empty_not, _, _) = scene(
            "BOOLEAN",
            &format!("{collect} RETURN SQL%NOTFOUND; END;"),
            vec![],
        );
        assert_eq!(empty_not, Value::Bool(true));
        let beyond = fail(
            "NUMBER",
            &format!("{collect} RETURN n(1); END;"),
            vec![],
        );
        assert!(beyond.contains("ORA-06533"), "{beyond}");
        assert!(beyond.contains("Subscript beyond count"), "{beyond}");
        let uninit = fail("NUMBER", &format!("{decl} BEGIN RETURN n(1); END;"), vec![]);
        assert!(uninit.contains("ORA-06531"), "{uninit}");
        assert!(uninit.contains("Reference to uninitialized collection"), "{uninit}");
        let null_index = fail(
            "NUMBER",
            &format!("{decl} v NUMBER; BEGIN SELECT amount BULK COLLECT INTO n FROM t; RETURN n(v); END;"),
            vec![vec![Value::Numeric("4".into())]],
        );
        assert!(null_index.contains("ORA-06502"), "{null_index}");
        assert!(null_index.contains("PL/SQL: numeric or value error"), "{null_index}");

        let (bound, bridge, _) = scene(
            "PLS_INTEGER",
            &format!(
                "{decl} BEGIN SELECT amount BULK COLLECT INTO n FROM t; FORALL i IN 1..1 EXECUTE IMMEDIATE 'UPDATE t SET note = :1 WHERE id = 1' USING n(i); RETURN SQL%BULK_ROWCOUNT(1); END;"
            ),
            vec![vec![Value::Numeric("4".into())]],
        );
        assert_eq!(bound, Value::Int(4));
        assert_eq!(bridge.queries, 1);
        assert_eq!(bridge.execs, 1);
        assert_eq!(bridge.dml_params, vec![vec![Value::Numeric("4".into())]]);

        let refuse = |source: &str, needle: &str| {
            let err = compile(source).unwrap_err().to_string();
            assert!(err.contains("PL/SQL at "), "{err}");
            assert!(err.contains(needle), "{needle}: {err}");
        };
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS TYPE nums IS VARRAY(2) OF NUMBER; BEGIN RETURN 1; END;",
            "VARRAY collections are not implemented",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS TYPE nums IS TABLE OF NUMBER INDEX BY PLS_INTEGER; BEGIN RETURN 1; END;",
            "INDEX BY collections are not implemented",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS TYPE nums IS TABLE OF NUMBER; n nums; BEGIN SELECT a, b BULK COLLECT INTO n FROM t; RETURN 1; END;",
            "BULK COLLECT requires one select item",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS TYPE nums IS TABLE OF NUMBER; n nums; m nums; BEGIN SELECT amount BULK COLLECT INTO n, m FROM t; RETURN 1; END;",
            "BULK COLLECT INTO takes one nested table",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS n NUMBER; BEGIN SELECT amount BULK COLLECT INTO n FROM t; RETURN n; END;",
            "BULK COLLECT INTO requires a nested table",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS TYPE nums IS TABLE OF NUMBER; n nums; BEGIN RETURN n; END;",
            "a nested table requires a subscript",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS TYPE nums IS TABLE OF NUMBER; n CONSTANT nums; BEGIN RETURN 1; END;",
            "a nested table cannot be CONSTANT",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS TYPE nums IS TABLE OF NUMBER; n nums := 1; BEGIN RETURN 1; END;",
            "a nested table can only be initialized with NULL",
        );
    }

    #[test]
    fn nested_table_count_of_null_raises_and_a_null_element_still_counts() {
        let decl = "TYPE nums IS TABLE OF NUMBER; n nums;";
        let (caught, _, _) = scene(
            "PLS_INTEGER",
            &format!(
                "{decl} BEGIN RETURN n.COUNT; \
                 EXCEPTION WHEN COLLECTION_IS_NULL THEN RETURN NULL; END;"
            ),
            vec![],
        );
        assert_eq!(caught, Value::Null);
        let (assigned, _, _) = scene(
            "PLS_INTEGER",
            &format!(
                "{decl} BEGIN n := NULL; RETURN n.COUNT(); \
                 EXCEPTION WHEN COLLECTION_IS_NULL THEN RETURN NULL; END;"
            ),
            vec![],
        );
        assert_eq!(assigned, Value::Null);

        let collect = format!("{decl} BEGIN SELECT amount BULK COLLECT INTO n FROM t;");
        let rows = vec![vec![Value::Numeric("4".into())], vec![Value::Null]];
        let (count, _, _) = scene(
            "PLS_INTEGER",
            &format!("{collect} RETURN n.COUNT; END;"),
            rows,
        );
        assert_eq!(count, Value::Int(2));
        let (empty, _, _) = scene(
            "PLS_INTEGER",
            &format!("{collect} RETURN n.COUNT; END;"),
            vec![],
        );
        assert_eq!(empty, Value::Int(0));

        let uninit = fail(
            "PLS_INTEGER",
            &format!("{decl} BEGIN RETURN n.COUNT; END;"),
            vec![],
        );
        assert!(uninit.contains("ORA-06531"), "{uninit}");
        let err = compile(
            "CREATE FUNCTION f RETURN NUMBER IS TYPE nums IS TABLE OF NUMBER; n nums; BEGIN RETURN n.DELETE; END;",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("nested table methods are not implemented"), "{err}");
    }
}
