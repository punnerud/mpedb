//! `anchor%TYPE`. A scalar anchor and a record-field anchor inherit
//! datatype, size, and `NOT NULL`, and do not inherit the value. A
//! `table.column%TYPE` anchor inherits datatype and size only: Oracle does
//! not copy a column's `NOT NULL`, so the variable needs no initializer and
//! starts NULL. A schema-qualified name and a `%ROWTYPE` field stay refused.

use super::{Parser, Type};
use crate::ir::Op;
use crate::plsql::lex::Token;
use mpedb_types::{ColumnType, Result, Value};

/// One catalog column a PL/SQL `%TYPE` declaration may anchor to.
/// `nullable` is the column's flag. A column anchor never copies it.
#[derive(Clone, Debug)]
pub struct ColumnAnchor {
    pub table: String,
    pub column: String,
    pub ty: ColumnType,
    pub nullable: bool,
}

enum AnchoredForm {
    No,
    Scalar,
    Dotted,
}

fn same_ident(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

impl Parser<'_> {
    pub(super) fn declare_anchored_type(&mut self, name: &str, constant: bool) -> Result<bool> {
        match self.anchored_form() {
            AnchoredForm::No => return Ok(false),
            AnchoredForm::Dotted => return self.declare_column_anchor(name, constant),
            AnchoredForm::Scalar => {}
        }
        let anchor_name = self.identifier()?;
        self.want_symbol("%")?;
        self.want_word("TYPE")?;
        if self.pending_declarations.contains(&anchor_name) {
            return Err(self.error("a %TYPE anchor is not declared yet"));
        }
        let Some(anchor) = self.locals.get(&anchor_name).copied() else {
            return Err(self.error(
                "a %TYPE anchor must be a scalar variable in this scope",
            ));
        };
        self.bind_anchored(name, constant, anchor.ty, anchor.not_null)?;
        Ok(true)
    }

    /// `table.column%TYPE` or `record.field%TYPE` as a variable.
    fn declare_column_anchor(&mut self, name: &str, constant: bool) -> Result<bool> {
        let (ty, inherited) = self.resolve_dotted_type()?;
        self.bind_anchored(name, constant, ty, inherited)?;
        Ok(true)
    }

    /// Dotted `%TYPE`. A column does not pass on `NOT NULL`. A record field
    /// does, because it is a PL/SQL item. Neither passes on its value.
    pub(super) fn resolve_dotted_type(&mut self) -> Result<(Type, bool)> {
        let table = self.identifier()?;
        self.want_symbol(".")?;
        let column = self.identifier()?;
        if self.symbol(".") {
            loop {
                self.identifier()?;
                if !self.symbol(".") {
                    break;
                }
            }
            self.want_symbol("%")?;
            self.want_word("TYPE")?;
            return Err(self.error("schema-qualified %TYPE anchors are not implemented"));
        }
        self.want_symbol("%")?;
        self.want_word("TYPE")?;
        if self.pending_declarations.contains(&table) {
            return Err(self.error("a %TYPE anchor is not declared yet"));
        }
        if self.records.contains_key(&table) {
            return self.record_field_anchor_type(&table, &column);
        }
        if self.record_types.contains_key(&table) {
            return Err(self.error(
                "a record type name is not a %TYPE anchor; use a record variable",
            ));
        }
        let Some(anchor) = self.column_anchors.iter().find(|anchor| {
            same_ident(&anchor.table, &table) && same_ident(&anchor.column, &column)
        }) else {
            let known_table = self
                .column_anchors
                .iter()
                .any(|anchor| same_ident(&anchor.table, &table));
            let message = if known_table {
                format!("no such column {column} in {table}")
            } else {
                format!("no such table {table}")
            };
            return Err(self.error(&message));
        };
        // A column NOT NULL is recorded on the anchor and deliberately
        // not copied. Reading it keeps that flag from looking unused.
        let _column_nullable = anchor.nullable;
        let ty = match anchor.ty {
            ColumnType::Text => Type::Varchar(0),
            ColumnType::Int64 => Type::Number,
            _ => {
                return Err(self.error("a %TYPE column supports TEXT and INTEGER only"));
            }
        };
        Ok((ty, false))
    }

    /// `record.field%TYPE`. The field's `NOT NULL` is inherited. Its value
    /// and default are not.
    fn record_field_anchor_type(&mut self, record: &str, field: &str) -> Result<(Type, bool)> {
        let slot = {
            let decl = self
                .records
                .get(record)
                .cloned()
                .ok_or_else(|| self.error("a %TYPE anchor must be a record variable"))?;
            self.record_field(&decl, field)?
        };
        let Some(info) = self.record_field_info.get(&slot).copied() else {
            return Err(self.error("a %ROWTYPE field %TYPE anchor is not implemented"));
        };
        Ok((info.ty, info.not_null))
    }

    fn bind_anchored(
        &mut self,
        name: &str,
        constant: bool,
        ty: Type,
        inherited: bool,
    ) -> Result<()> {
        let mut not_null = inherited;
        let wrote_not_null = self.word("NOT");
        if wrote_not_null {
            self.want_word("NULL")?;
            not_null = true;
        }
        if self.symbol(":=") || self.word("DEFAULT") {
            let actual = self.expression()?;
            self.compatible(ty, actual)?;
            if not_null && actual.is_none() {
                return Err(self.error(
                    "a NOT NULL variable cannot be initialized with literal NULL",
                ));
            }
        } else if not_null {
            let message = if inherited && !wrote_not_null {
                "a %TYPE variable inherits NOT NULL and requires an initializer"
            } else {
                "a NOT NULL declaration requires an initializer"
            };
            return Err(self.error(message));
        } else if constant {
            return Err(self.error("a CONSTANT declaration requires an initializer"));
        } else {
            self.constant(Value::Null)?;
        }
        self.builder.emit(ty.check())?;
        if not_null {
            self.builder.emit(Op::OracleNotNull)?;
        }
        self.pending_declarations.remove(name);
        let slot = self.add_local(name.to_string(), ty, !constant)?;
        self.locals.get_mut(name).unwrap().not_null = not_null;
        self.builder.emit(Op::StoreLocal(slot))?;
        self.want_symbol(";")?;
        Ok(())
    }

    fn anchored_form(&self) -> AnchoredForm {
        let Some(Token::Identifier { .. }) = self.peek() else {
            return AnchoredForm::No;
        };
        match self.tokens.get(self.at + 1).map(|token| &token.token) {
            Some(Token::Symbol("%")) if self.type_word_at(self.at + 2) => AnchoredForm::Scalar,
            Some(Token::Symbol(".")) if self.dotted_percent_type() => AnchoredForm::Dotted,
            _ => AnchoredForm::No,
        }
    }

    pub(super) fn type_word_at(&self, index: usize) -> bool {
        matches!(
            self.tokens.get(index).map(|token| &token.token),
            Some(Token::Identifier { name, quoted: false }) if name == "TYPE"
        )
    }

    /// `table.column%TYPE` and `record.field%TYPE`, including a schema prefix.
    pub(super) fn dotted_percent_type(&self) -> bool {
        let mut depth = 0i32;
        let mut index = self.at;
        while let Some(token) = self.tokens.get(index) {
            match &token.token {
                Token::Symbol("(") => depth += 1,
                Token::Symbol(")") => {
                    if depth == 0 {
                        return false;
                    }
                    depth -= 1;
                }
                Token::Symbol(";") if depth == 0 => return false,
                Token::Symbol("%") if depth == 0 => return self.type_word_at(index + 1),
                _ => {}
            }
            index += 1;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use crate::interp::{self, testutil::MockBridge, Budget, ProcValue};
    use crate::ir::Proc;
    use crate::plsql::{compile, compile_with_anchors};
    use mpedb_types::{ColumnType, Value};

    use super::ColumnAnchor;

    fn function(result: &str, body: &str) -> String {
        format!("CREATE OR REPLACE FUNCTION f RETURN {result} IS {body}")
    }

    fn call(source: &str) -> Result<Value, String> {
        let skel = compile(source).map_err(|err| err.to_string())?;
        let proc = Proc::new(
            skel.name,
            skel.argc,
            skel.nlocals,
            Vec::new(),
            skel.consts,
            skel.instrs,
        )
        .map_err(|err| err.to_string())?;
        let mut bridge = MockBridge::new();
        match interp::run(
            &proc,
            &[],
            &mut bridge,
            Budget {
                instrs: 10_000,
                db_calls: 8,
                rows: 100,
            },
        )
        .map_err(|err| err.to_string())?
        {
            ProcValue::Scalar(value) => Ok(value),
            other => panic!("unexpected {other:?}"),
        }
    }

    fn compile_err(body: &str) -> String {
        compile(&function("VARCHAR2", body))
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn anchored_type_does_not_inherit_the_value_and_null_stays_null() {
        let source = function(
            "VARCHAR2",
            "a VARCHAR2(4) := 'kept'; \
             n a%TYPE; \
             BEGIN \
               IF n IS NULL THEN n := NULL; END IF; \
               RETURN n; \
             END;",
        );
        assert_eq!(call(&source).unwrap(), Value::Null);

        let chained = function(
            "PLS_INTEGER",
            "a PLS_INTEGER := 7; b a%TYPE; n b%TYPE; BEGIN RETURN n; END;",
        );
        assert_eq!(call(&chained).unwrap(), Value::Null);

        let assigned = function(
            "VARCHAR2",
            "a VARCHAR2(4) := 'kept'; n a%TYPE := 'ok'; BEGIN n := NULL; RETURN n; END;",
        );
        assert_eq!(call(&assigned).unwrap(), Value::Null);

        let kept = function(
            "VARCHAR2",
            "a VARCHAR2(4) := 'kept'; n a%TYPE; BEGIN n := 'ok'; RETURN n; END;",
        );
        assert_eq!(call(&kept).unwrap(), Value::Text("ok".into()));

        let overflow = function(
            "VARCHAR2",
            "a VARCHAR2(4); n a%TYPE; \
             BEGIN n := 'toolong'; RETURN 'x'; \
             EXCEPTION WHEN VALUE_ERROR THEN RETURN n; END;",
        );
        assert_eq!(call(&overflow).unwrap(), Value::Null);
    }

    #[test]
    fn anchored_type_inherits_not_null_and_refuses_other_anchors() {
        let refused = [
            (
                "a VARCHAR2(4) NOT NULL := 'kept'; n a%TYPE; BEGIN RETURN n; END;",
                "inherits NOT NULL",
            ),
            (
                "a VARCHAR2(4) NOT NULL := 'kept'; n a%TYPE := NULL; BEGIN RETURN n; END;",
                "cannot be initialized with literal NULL",
            ),
            (
                "a VARCHAR2(4) NOT NULL := 'kept'; n a%TYPE := 'ok'; BEGIN n := NULL; RETURN n; END;",
                "cannot be assigned literal NULL",
            ),
            (
                "a VARCHAR2(4); n a%TYPE NOT NULL; BEGIN RETURN n; END;",
                "a NOT NULL declaration requires an initializer",
            ),
            (
                "n a%TYPE; a VARCHAR2(4); BEGIN RETURN n; END;",
                "not declared yet",
            ),
            (
                "n missing%TYPE; BEGIN RETURN n; END;",
                "must be a scalar variable",
            ),
            (
                "n CONSTANT a%TYPE; a VARCHAR2(4); BEGIN RETURN 'x'; END;",
                "not declared yet",
            ),
            (
                "a VARCHAR2(4); n CONSTANT a%TYPE; BEGIN RETURN n; END;",
                "CONSTANT declaration requires an initializer",
            ),
            (
                "n employees.last_name%TYPE; BEGIN RETURN n; END;",
                "no such table EMPLOYEES",
            ),
            (
                "n schema.employees.last_name%TYPE; BEGIN RETURN n; END;",
                "schema-qualified %TYPE anchors are not implemented",
            ),
            (
                "TYPE nums IS TABLE OF PLS_INTEGER; n nums; x n%TYPE; BEGIN RETURN x; END;",
                "must be a scalar variable",
            ),
            (
                "CURSOR c IS SELECT id FROM t; v c%ROWTYPE; x v%TYPE; BEGIN RETURN 'x'; END;",
                "must be a scalar variable",
            ),
            (
                "a VARCHAR2(4); BEGIN RETURN a%TYPE; END;",
                "%TYPE is a type, not a value",
            ),
        ];
        for (body, expected) in refused {
            let err = compile_err(body);
            assert!(err.contains(expected), "{body}\n{err}");
        }
    }

    fn not_null_columns() -> Vec<ColumnAnchor> {
        vec![
            ColumnAnchor {
                table: "people".into(),
                column: "name".into(),
                ty: ColumnType::Text,
                nullable: false,
            },
            ColumnAnchor {
                table: "people".into(),
                column: "id".into(),
                ty: ColumnType::Int64,
                nullable: false,
            },
            ColumnAnchor {
                table: "people".into(),
                column: "payload".into(),
                ty: ColumnType::Blob,
                nullable: true,
            },
        ]
    }

    fn call_anchored(source: &str) -> Result<Value, String> {
        let skel = compile_with_anchors(source, &not_null_columns()).map_err(|err| err.to_string())?;
        let proc = Proc::new(
            skel.name,
            skel.argc,
            skel.nlocals,
            Vec::new(),
            skel.consts,
            skel.instrs,
        )
        .map_err(|err| err.to_string())?;
        let mut bridge = MockBridge::new();
        match interp::run(
            &proc,
            &[],
            &mut bridge,
            Budget {
                instrs: 10_000,
                db_calls: 8,
                rows: 100,
            },
        )
        .map_err(|err| err.to_string())?
        {
            ProcValue::Scalar(value) => Ok(value),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn column_type_does_not_inherit_not_null_and_null_stays_null() {
        let text = function(
            "VARCHAR2",
            "n people.name%TYPE; \
             BEGIN \
               IF n IS NULL THEN n := NULL; END IF; \
               RETURN n; \
             END;",
        );
        assert_eq!(call_anchored(&text).unwrap(), Value::Null);

        let cleared = function(
            "VARCHAR2",
            "n people.name%TYPE := 'kept'; BEGIN n := NULL; RETURN n; END;",
        );
        assert_eq!(call_anchored(&cleared).unwrap(), Value::Null);

        let number = function(
            "NUMBER",
            "n people.id%TYPE; BEGIN RETURN n; END;",
        );
        assert_eq!(call_anchored(&number).unwrap(), Value::Null);

        let wide = function(
            "NUMBER",
            "n people.id%TYPE; BEGIN n := 3000000000; RETURN n; END;",
        );
        assert_eq!(
            call_anchored(&wide).unwrap(),
            Value::Numeric("3000000000".into())
        );

        let refused = [
            (
                "n people.name%TYPE NOT NULL; BEGIN RETURN n; END;",
                "a NOT NULL declaration requires an initializer",
            ),
            (
                "n people.missing%TYPE; BEGIN RETURN n; END;",
                "no such column MISSING in PEOPLE",
            ),
            (
                "n other.name%TYPE; BEGIN RETURN n; END;",
                "no such table OTHER",
            ),
            (
                "n people.payload%TYPE; BEGIN RETURN n; END;",
                "a %TYPE column supports TEXT and INTEGER only",
            ),
            (
                "n hr.people.name%TYPE; BEGIN RETURN n; END;",
                "schema-qualified %TYPE anchors are not implemented",
            ),
        ];
        for (body, expected) in refused {
            let err = compile_with_anchors(&function("VARCHAR2", body), &not_null_columns())
                .unwrap_err()
                .to_string();
            assert!(err.contains(expected), "{body}\n{err}");
        }
    }

    #[test]
    fn record_field_type_inherits_not_null_not_the_value() {
        let source = function(
            "VARCHAR2",
            "TYPE pair IS RECORD (id PLS_INTEGER NOT NULL := 7, note VARCHAR2(4) := 'kept'); \
             r pair; \
             n r.note%TYPE; \
             BEGIN \
               IF n IS NULL THEN n := NULL; END IF; \
               RETURN n; \
             END;",
        );
        assert_eq!(call(&source).unwrap(), Value::Null);

        let overflow = function(
            "VARCHAR2",
            "TYPE pair IS RECORD (note VARCHAR2(4)); r pair; n r.note%TYPE; \
             BEGIN n := 'toolong'; RETURN 'x'; \
             EXCEPTION WHEN VALUE_ERROR THEN RETURN n; END;",
        );
        assert_eq!(call(&overflow).unwrap(), Value::Null);

        let refused = [
            (
                "TYPE pair IS RECORD (id PLS_INTEGER NOT NULL := 7, note VARCHAR2(4)); \
                 r pair; n r.id%TYPE; BEGIN RETURN n; END;",
                "inherits NOT NULL",
            ),
            (
                "TYPE pair IS RECORD (id PLS_INTEGER NOT NULL := 7, note VARCHAR2(4)); \
                 r pair; n r.id%TYPE := NULL; BEGIN RETURN n; END;",
                "cannot be initialized with literal NULL",
            ),
            (
                "TYPE pair IS RECORD (note VARCHAR2(4)); r pair; n r.missing%TYPE; BEGIN RETURN n; END;",
                "unknown record field",
            ),
            (
                "TYPE pair IS RECORD (note VARCHAR2(4)); n pair.note%TYPE; BEGIN RETURN n; END;",
                "a record type name is not a %TYPE anchor",
            ),
            (
                "n r.note%TYPE; TYPE pair IS RECORD (note VARCHAR2(4)); r pair; BEGIN RETURN n; END;",
                "not declared yet",
            ),
            (
                "CURSOR c IS SELECT id FROM t; v c%ROWTYPE; n v.id%TYPE; BEGIN RETURN n; END;",
                "a %ROWTYPE field %TYPE anchor is not implemented",
            ),
        ];
        for (body, expected) in refused {
            let err = compile_err(body);
            assert!(err.contains(expected), "{body}\n{err}");
        }
    }
}
