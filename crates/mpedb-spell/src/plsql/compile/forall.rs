//! Dense `FORALL index IN low..high` over one INSERT, UPDATE, DELETE, or
//! `EXECUTE IMMEDIATE`. `SQL%BULK_ROWCOUNT(index)` reads that statement.
//! A subscript that did not run is NULL. An iteration that changes zero
//! rows stores 0. `SQL%ROWCOUNT` becomes the sum, and `SQL%FOUND` follows
//! only the last iteration.
//!
//! The index is not a value. Collections, `INDICES OF`, `VALUES OF`,
//! `REVERSE`, and `SAVE EXCEPTIONS` stay refused. An exception does not
//! roll back earlier iterations of the same `FORALL`; Oracle would.

use super::*;

impl Parser<'_> {
    pub(super) fn forall_statement(&mut self) -> Result<()> {
        let name = self.identifier()?;
        self.want_word("IN")?;
        if self.word("REVERSE") {
            return Err(self.error("FORALL does not support REVERSE"));
        }
        if self.word("INDICES") {
            return Err(self.error("FORALL does not support INDICES OF"));
        }
        if self.word("VALUES") {
            return Err(self.error("FORALL does not support VALUES OF"));
        }
        let outer_locals = self.locals.clone();
        let outer_records = self.records.clone();
        let outer_nested_types = self.nested_types.clone();
        let outer_nested_vars = self.nested_vars.clone();
        let outer_declared = std::mem::take(&mut self.declared_here);
        let outer_pending = std::mem::take(&mut self.pending_declarations);
        // The index hides an outer name while the bounds are evaluated.
        // A bound that reads it sees the initial NULL and fails there,
        // before BEGIN replaces the previous bulk map.
        let index = self.add_local(name.clone(), Type::PlsInteger, false)?;
        self.constant(Value::Null)?;
        self.builder.emit(Op::StoreLocal(index))?;
        let lower = self.builder.alloc_local()?;
        let upper = self.builder.alloc_local()?;
        let ty = self.expression()?;
        self.compatible(Type::Number, ty)?;
        self.builder.emit(Op::OraclePlSqlLoopBound)?;
        self.builder.emit(Op::StoreLocal(lower))?;
        self.want_symbol("..")?;
        let ty = self.expression()?;
        self.compatible(Type::Number, ty)?;
        self.builder.emit(Op::OraclePlSqlLoopBound)?;
        self.builder.emit(Op::StoreLocal(upper))?;
        if self.word("SAVE") {
            return Err(self.error("FORALL does not support SAVE EXCEPTIONS"));
        }
        self.builder.emit(Op::SqlBulk(crate::ir::SQL_BULK_BEGIN))?;
        self.builder.emit(Op::LoadLocal(lower))?;
        self.builder.emit(Op::StoreLocal(index))?;
        let first = self.builder.emit_jump(Op::Jump(u32::MAX))?;
        let advance = self.builder.here();
        self.builder.emit(Op::LoadLocal(index))?;
        self.builder.emit(Op::LoadLocal(upper))?;
        self.builder.emit(Op::SqlEq)?;
        let finished = self.builder.emit_jump(Op::JumpIfTrue(u32::MAX))?;
        self.builder.emit(Op::LoadLocal(index))?;
        self.constant(Value::Int(1))?;
        self.builder.emit(Op::Add)?;
        self.builder.emit(Op::StoreLocal(index))?;
        self.builder.patch_to_here(first);
        self.builder.emit(Op::LoadLocal(index))?;
        self.builder.emit(Op::LoadLocal(upper))?;
        self.builder.emit(Op::SqlLe)?;
        let out = self.builder.emit_jump(Op::JumpIfFalse(u32::MAX))?;
        self.forall_index = Some(name);
        let body = self.forall_dml();
        self.forall_index = None;
        body?;
        self.builder.emit(Op::LoadLocal(index))?;
        self.builder.emit(Op::SqlBulk(crate::ir::SQL_BULK_RECORD))?;
        self.builder.emit(Op::Jump(advance))?;
        self.builder.patch_to_here(out);
        self.builder.patch_to_here(finished);
        self.builder.emit(Op::SqlBulk(crate::ir::SQL_BULK_FINISH))?;
        self.locals = outer_locals;
        self.records = outer_records;
        self.nested_types = outer_nested_types;
        self.nested_vars = outer_nested_vars;
        self.declared_here = outer_declared;
        self.pending_declarations = outer_pending;
        Ok(())
    }

    fn forall_dml(&mut self) -> Result<()> {
        if self.word("EXECUTE") {
            return self.execute_immediate();
        }
        if self.is_word("INSERT") || self.is_word("UPDATE") || self.is_word("DELETE") {
            return self.static_sql();
        }
        Err(self.error(
            "FORALL requires one INSERT, UPDATE, DELETE, or EXECUTE IMMEDIATE",
        ))
    }

    pub(super) fn bulk_rowcount(&mut self, name: &str) -> Result<Option<Type>> {
        if name != "SQL" {
            return Err(self.error(
                "SQL%BULK_ROWCOUNT is an implicit cursor attribute",
            ));
        }
        if self.symbol(".") {
            return Err(self.error(
                "SQL%BULK_ROWCOUNT does not support collection methods",
            ));
        }
        if !self.symbol("(") {
            return Err(self.error("SQL%BULK_ROWCOUNT requires an index"));
        }
        let actual = self.expression()?;
        self.compatible(Type::PlsInteger, actual)?;
        // NULL stays NULL here. LOAD then raises 6502 for that subscript.
        self.builder.emit(Op::OraclePlSqlInteger)?;
        self.want_symbol(")")?;
        if self.symbol(".") {
            return Err(self.error(
                "SQL%BULK_ROWCOUNT does not support collection methods",
            ));
        }
        self.builder.emit(Op::SqlBulk(crate::ir::SQL_BULK_LOAD))?;
        Ok(Some(Type::PlsInteger))
    }
}

#[cfg(test)]
mod tests {
    use super::super::compile;
    use crate::interp::{self, testutil::MockBridge, Budget, ProcValue};
    use crate::ir::{Op, PlanKind, PlanRef, Proc, SQL_BULK_BEGIN, SQL_BULK_FINISH, SQL_BULK_LOAD, SQL_BULK_RECORD};
    use mpedb_types::{PlanHash, Value};

    fn scene(returns: &str, body: &str, affected: u64) -> (Value, MockBridge, Proc) {
        let source = format!("CREATE FUNCTION f RETURN {returns} IS {body}");
        let skel = compile(&source).unwrap_or_else(|err| panic!("{err}"));
        let plans = skel
            .calls
            .iter()
            .map(|call| PlanRef {
                hash: PlanHash([0; 32]),
                kind: if call.kind == crate::emit::CallKind::Exec {
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
        bridge.affected = affected;
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

    fn modes(proc: &Proc, mode: u8) -> usize {
        proc.instrs
            .iter()
            .filter(|op| matches!(op, Op::SqlBulk(found) if *found == mode))
            .count()
    }

    #[test]
    fn forall_bulk_rowcount_returns_zero_and_a_missing_index_is_null() {
        let dml = "EXECUTE IMMEDIATE 'UPDATE t SET note = :1 WHERE id = 1' USING n";
        let forall = format!("n VARCHAR2(20); BEGIN FORALL i IN 1..2 {dml};");
        let (before, bridge, _) = scene(
            "PLS_INTEGER",
            "BEGIN RETURN SQL%BULK_ROWCOUNT(1); END;",
            4,
        );
        assert_eq!(before, Value::Null);
        assert_eq!(bridge.execs, 0);
        assert!(bridge.dml_params.is_empty());

        let (one, bridge, proc) = scene(
            "PLS_INTEGER",
            &format!("{forall} RETURN SQL%BULK_ROWCOUNT(1); END;"),
            4,
        );
        assert_eq!(one, Value::Int(4));
        assert_eq!(bridge.execs, 2);
        assert_eq!(bridge.dml_params, vec![vec![Value::Null], vec![Value::Null]]);
        assert_eq!(proc.plans.len(), 1);
        assert_eq!(proc.plans[0].kind, PlanKind::Exec);
        assert_eq!(proc.plans[0].argc, 1);
        assert_eq!(modes(&proc, SQL_BULK_BEGIN), 1);
        assert_eq!(modes(&proc, SQL_BULK_RECORD), 1);
        assert_eq!(modes(&proc, SQL_BULK_FINISH), 1);
        assert_eq!(modes(&proc, SQL_BULK_LOAD), 1);
        let (two, _, _) = scene(
            "PLS_INTEGER",
            &format!("{forall} RETURN SQL%BULK_ROWCOUNT(2); END;"),
            4,
        );
        assert_eq!(two, Value::Int(4));
        let (missing, _, _) = scene(
            "PLS_INTEGER",
            &format!("{forall} RETURN SQL%BULK_ROWCOUNT(3); END;"),
            4,
        );
        assert_eq!(missing, Value::Null);
        let (missing_is_null, _, _) = scene(
            "BOOLEAN",
            &format!("{forall} RETURN SQL%BULK_ROWCOUNT(3) IS NULL; END;"),
            4,
        );
        assert_eq!(missing_is_null, Value::Bool(true));
        let (sum, _, _) = scene(
            "PLS_INTEGER",
            &format!("{forall} RETURN SQL%ROWCOUNT; END;"),
            4,
        );
        assert_eq!(sum, Value::Int(8));
        let (found, _, _) = scene(
            "BOOLEAN",
            &format!("{forall} RETURN SQL%FOUND; END;"),
            4,
        );
        assert_eq!(found, Value::Bool(true));

        let after = format!("{forall} {dml};");
        let (rowcount, bridge, _) = scene(
            "PLS_INTEGER",
            &format!("{after} RETURN SQL%ROWCOUNT; END;"),
            4,
        );
        assert_eq!(rowcount, Value::Int(4));
        assert_eq!(bridge.execs, 3);
        assert_eq!(
            bridge.dml_params,
            vec![vec![Value::Null], vec![Value::Null], vec![Value::Null]]
        );
        let (kept, _, _) = scene(
            "PLS_INTEGER",
            &format!("{after} RETURN SQL%BULK_ROWCOUNT(1); END;"),
            4,
        );
        assert_eq!(kept, Value::Int(4));

        let empty = format!("{after} FORALL j IN 2..1 {dml};");
        let (cleared, bridge, _) = scene(
            "BOOLEAN",
            &format!("{empty} RETURN SQL%BULK_ROWCOUNT(1) IS NULL; END;"),
            4,
        );
        assert_eq!(cleared, Value::Bool(true));
        assert_eq!(bridge.execs, 3, "an empty range executes no statement");
        let (kept_count, _, _) = scene(
            "PLS_INTEGER",
            &format!("{empty} RETURN SQL%ROWCOUNT; END;"),
            4,
        );
        assert_eq!(kept_count, Value::Int(4));

        let (zero, bridge, _) = scene(
            "PLS_INTEGER",
            &format!("{forall} RETURN SQL%BULK_ROWCOUNT(1); END;"),
            0,
        );
        assert_eq!(zero, Value::Int(0));
        assert_eq!(bridge.execs, 2);
        assert_eq!(bridge.dml_params, vec![vec![Value::Null], vec![Value::Null]]);
        let (zero_too, _, _) = scene(
            "PLS_INTEGER",
            &format!("{forall} RETURN SQL%BULK_ROWCOUNT(2); END;"),
            0,
        );
        assert_eq!(zero_too, Value::Int(0));
        let (zero_null, _, _) = scene(
            "BOOLEAN",
            &format!("{forall} RETURN SQL%BULK_ROWCOUNT(1) IS NULL; END;"),
            0,
        );
        assert_eq!(zero_null, Value::Bool(false));
        let (zero_count, _, _) = scene(
            "PLS_INTEGER",
            &format!("{forall} RETURN SQL%ROWCOUNT; END;"),
            0,
        );
        assert_eq!(zero_count, Value::Int(0));
        let (notfound, _, _) = scene(
            "BOOLEAN",
            &format!("{forall} RETURN SQL%NOTFOUND; END;"),
            0,
        );
        assert_eq!(notfound, Value::Bool(true));

        let (static_count, bridge, _) = scene(
            "PLS_INTEGER",
            "BEGIN FORALL i IN 1..1 DELETE FROM t; RETURN SQL%BULK_ROWCOUNT(1); END;",
            4,
        );
        assert_eq!(static_count, Value::Int(4));
        assert_eq!(bridge.execs, 1);
        assert_eq!(bridge.dml_params, vec![vec![]]);
        let (outer, _, _) = scene(
            "PLS_INTEGER",
            "i PLS_INTEGER := 7; BEGIN FORALL i IN 1..1 DELETE FROM t; RETURN i; END;",
            4,
        );
        assert_eq!(outer, Value::Int(7));

        let refuse = |source: &str, needle: &str| {
            let err = compile(source).unwrap_err().to_string();
            assert!(err.contains("PL/SQL at "), "{err}");
            assert!(err.contains(needle), "{needle}: {err}");
        };
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS BEGIN RETURN SQL%BULK_ROWCOUNT; END;",
            "SQL%BULK_ROWCOUNT requires an index",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS BEGIN RETURN SQL%BULK_ROWCOUNT.COUNT; END;",
            "SQL%BULK_ROWCOUNT does not support collection methods",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS CURSOR c IS SELECT id FROM t; BEGIN RETURN c%BULK_ROWCOUNT(1); END;",
            "SQL%BULK_ROWCOUNT is an implicit cursor attribute",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS n VARCHAR2(20); BEGIN FORALL i IN 1..1 EXECUTE IMMEDIATE 'UPDATE t SET note = :1' USING i; RETURN 1; END;",
            "a FORALL index can only subscript a collection",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS BEGIN FORALL i IN REVERSE 1..2 DELETE FROM t; RETURN 1; END;",
            "FORALL does not support REVERSE",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS BEGIN FORALL i IN INDICES OF t DELETE FROM t; RETURN 1; END;",
            "FORALL does not support INDICES OF",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS BEGIN FORALL i IN VALUES OF t DELETE FROM t; RETURN 1; END;",
            "FORALL does not support VALUES OF",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS BEGIN FORALL i IN 1..1 SAVE EXCEPTIONS DELETE FROM t; RETURN 1; END;",
            "FORALL does not support SAVE EXCEPTIONS",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS n NUMBER; BEGIN FORALL i IN 1..1 SELECT id INTO n FROM t; RETURN n; END;",
            "FORALL requires one INSERT, UPDATE, DELETE, or EXECUTE IMMEDIATE",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS BEGIN FORALL i IN 1..1 NULL; RETURN 1; END;",
            "FORALL requires one INSERT, UPDATE, DELETE, or EXECUTE IMMEDIATE",
        );
    }
}
