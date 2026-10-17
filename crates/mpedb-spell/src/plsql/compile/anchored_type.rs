//! Scalar `anchor%TYPE`. The new variable inherits the anchor's datatype,
//! size, and `NOT NULL` constraint, and does not inherit its value. An
//! omitted initializer is therefore NULL whenever the anchor allows NULL.
//! A column or record field (`table.col%TYPE`) stays a named refusal: this
//! compiler has no schema, and a column anchor would not inherit `NOT NULL`.

use super::Parser;
use crate::ir::Op;
use crate::plsql::lex::Token;
use mpedb_types::{Result, Value};

enum AnchoredForm {
    No,
    Scalar,
    Dotted,
}

impl Parser<'_> {
    pub(super) fn declare_anchored_type(&mut self, name: &str, constant: bool) -> Result<bool> {
        match self.anchored_form() {
            AnchoredForm::No => return Ok(false),
            AnchoredForm::Dotted => {
                return Err(self.error(
                    "table, view, and record-field %TYPE anchors are not implemented",
                ));
            }
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
        let inherited = anchor.not_null;
        let mut not_null = inherited;
        let wrote_not_null = self.word("NOT");
        if wrote_not_null {
            self.want_word("NULL")?;
            not_null = true;
        }
        if self.symbol(":=") || self.word("DEFAULT") {
            let actual = self.expression()?;
            self.compatible(anchor.ty, actual)?;
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
        self.builder.emit(anchor.ty.check())?;
        if not_null {
            self.builder.emit(Op::OracleNotNull)?;
        }
        self.pending_declarations.remove(name);
        let slot = self.add_local(name.to_string(), anchor.ty, !constant)?;
        self.locals.get_mut(name).unwrap().not_null = not_null;
        self.builder.emit(Op::StoreLocal(slot))?;
        self.want_symbol(";")?;
        Ok(true)
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

    fn type_word_at(&self, index: usize) -> bool {
        matches!(
            self.tokens.get(index).map(|token| &token.token),
            Some(Token::Identifier { name, quoted: false }) if name == "TYPE"
        )
    }

    /// `table.column%TYPE` and `record.field%TYPE`, including a schema prefix.
    fn dotted_percent_type(&self) -> bool {
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
    use crate::plsql::compile;
    use mpedb_types::Value;

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
                "record-field %TYPE anchors are not implemented",
            ),
            (
                "n schema.employees.last_name%TYPE; BEGIN RETURN n; END;",
                "record-field %TYPE anchors are not implemented",
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
}
