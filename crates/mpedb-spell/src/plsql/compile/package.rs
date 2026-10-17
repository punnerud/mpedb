//! A package-body variable is session state. It starts NULL. Assigning NULL
//! stores NULL. The value survives the next call on the same package state,
//! including when that call raises. A new state starts NULL again.
//! The optional initialization section runs once per session, before the
//! subprogram, and can store NULL. A package specification is refused.

use super::*;

impl Parser<'_> {
    pub(super) fn compile_package(&mut self, source: &str) -> Result<Skeleton> {
        if !self.word("BODY") {
            return Err(self.error("a package specification is not implemented"));
        }
        let package = self.plain_name()?;
        if !(self.word("IS") || self.word("AS")) {
            return Err(self.error("expected IS or AS before the package body"));
        }
        if self.is_word("FUNCTION") || self.is_word("PROCEDURE") || self.is_word("BEGIN") {
            return Err(self.error(
                "a package body requires one variable before its subprogram",
            ));
        }
        let var_name = self.plain_name()?;
        if self.word("CONSTANT") {
            return Err(self.error("package constants are not implemented"));
        }
        let ty = self.data_type(true)?;
        if self.word("NOT") {
            return Err(self.error("a package variable cannot be NOT NULL"));
        }
        // `:=` / `DEFAULT` is consumed even when the initializer is NULL.
        if (self.symbol(":=") || self.word("DEFAULT")) && !self.word("NULL") {
            return Err(self.error("a package variable can only be initialized with NULL"));
        }
        self.want_symbol(";")?;
        if !self.is_word("FUNCTION") && !self.is_word("PROCEDURE") {
            return Err(self.error(
                "a package body supports one variable and one subprogram",
            ));
        }
        let key_const = self
            .builder
            .const_idx(Value::Text(format!("{package}.{var_name}")))?;
        self.package_var = Some(PackageVar {
            name: var_name,
            key_const,
            ty,
        });
        self.package_name = Some(package.clone());
        let mut skel = self.compile_routine(source, false, true)?;
        if self.word("BEGIN") {
            self.builder.consts = std::mem::take(&mut skel.consts);
            self.builder.instrs = std::mem::take(&mut skel.instrs);
            self.compile_package_init(&package)?;
            skel.consts = std::mem::take(&mut self.builder.consts);
            skel.instrs = std::mem::take(&mut self.builder.instrs);
        }
        if self.is_word("FUNCTION") || self.is_word("PROCEDURE") {
            return Err(self.error("a package body supports one subprogram"));
        }
        self.want_word("END")?;
        if matches!(self.peek(), Some(Token::Identifier { .. })) {
            let end_name = self.identifier()?;
            if end_name != package {
                return Err(self.error("END name does not match package name"));
            }
        }
        self.want_symbol(";")?;
        self.finish_unit(source)?;
        Ok(skel)
    }

    /// Session init. A missing flag means this session has not finished init.
    /// The flag is stored only after the section completes, so a raise retries
    /// on the next call. Assignments made before the raise stay stored.
    fn compile_package_init(&mut self, package: &str) -> Result<()> {
        self.builder.shift_jump_targets(1);
        self.builder.instrs.insert(0, Op::Jump(0));
        let function_at = 1u32;
        let init_at = self.builder.here();
        self.builder.instrs[0] = Op::Jump(init_at);
        let flag = self.builder.const_idx(Value::Text(format!("{package}.\u{1}INIT")))?;
        self.builder.emit(Op::PkgGet(flag))?;
        self.constant(Value::Null)?;
        self.builder.emit(Op::Eq)?;
        self.builder.emit(Op::JumpIfFalse(function_at))?;
        self.statements()?;
        if self.is_word("EXCEPTION") {
            return Err(self.error(
                "package initialization exception handlers are not implemented",
            ));
        }
        self.constant(Value::Int(1))?;
        self.builder.emit(Op::PkgSet(flag))?;
        self.builder.emit(Op::Jump(function_at))?;
        self.want_word("END")?;
        self.want_symbol(";")
    }

    fn plain_name(&mut self) -> Result<String> {
        if matches!(self.peek(), Some(Token::Identifier { quoted: true, .. })) {
            return Err(self.error("quoted package names are not implemented"));
        }
        self.identifier()
    }

    pub(super) fn assign_package(&mut self, package: PackageVar) -> Result<()> {
        self.want_symbol(":=")?;
        let actual = self.expression()?;
        self.compatible(package.ty, actual)?;
        self.builder.emit(package.ty.check())?;
        self.builder.emit(Op::PkgSet(package.key_const))?;
        self.want_symbol(";")
    }

    pub(super) fn package_expr(&mut self, package: PackageVar) -> Result<Option<Type>> {
        if self.symbol(".") || self.symbol("(") {
            return Err(self.error("a package variable is a scalar"));
        }
        self.builder.emit(Op::PkgGet(package.key_const))?;
        Ok(Some(package.ty))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{compile, compile_function};
    use crate::interp::{self, testutil::MockBridge, Budget, ProcValue};
    use crate::ir::{PlanKind, PlanRef, Proc};
    use mpedb_types::{PlanHash, Value};
    use std::collections::HashMap;

    const TOUCH: &str = r#"
CREATE OR REPLACE PACKAGE BODY ledger AS
  n PLS_INTEGER;
  FUNCTION touch(v PLS_INTEGER) RETURN PLS_INTEGER IS
    previous PLS_INTEGER;
  BEGIN
    previous := n;
    n := v;
    IF v IS NULL THEN
      RAISE NO_DATA_FOUND;
    END IF;
    RETURN previous;
  END touch;
END ledger;
"#;

    fn compiled(source: &str) -> Proc {
        let skel = compile(source).unwrap_or_else(|err| panic!("{err}"));
        assert!(skel.calls.is_empty(), "{:?}", skel.calls);
        let proc = Proc::new(
            skel.name,
            skel.argc,
            skel.nlocals,
            skel.calls
                .iter()
                .map(|call| PlanRef {
                    hash: PlanHash([0; 32]),
                    kind: PlanKind::Query,
                    argc: call.argc,
                })
                .collect(),
            skel.consts,
            skel.instrs,
        )
        .unwrap();
        Proc::decode(&proc.encode()).unwrap()
    }

    fn call(proc: &Proc, package: &mut HashMap<String, Value>, arg: Value) -> Result<Value, String> {
        let mut bridge = MockBridge::new();
        match interp::run_with_package(
            proc,
            &[arg],
            &mut bridge,
            Budget {
                instrs: 10_000,
                db_calls: 8,
                rows: 100,
            },
            package,
        ) {
            Ok(ProcValue::Scalar(value)) => Ok(value),
            Ok(other) => panic!("{other:?}"),
            Err(err) => Err(err.to_string()),
        }
    }

    #[test]
    fn package_variable_starts_null_and_survives_an_exception() {
        let proc = compiled(TOUCH);
        assert_eq!(proc.name, "LEDGER.TOUCH");
        assert!(proc.instrs.iter().any(|op| matches!(op, crate::ir::Op::PkgGet(_))));
        assert!(proc.instrs.iter().any(|op| matches!(op, crate::ir::Op::PkgSet(_))));
        let mut session = HashMap::new();
        assert_eq!(call(&proc, &mut session, Value::Int(4)).unwrap(), Value::Null);
        assert_eq!(session.get("LEDGER.N"), Some(&Value::Int(4)));
        assert_eq!(call(&proc, &mut session, Value::Int(9)).unwrap(), Value::Int(4));
        let err = call(&proc, &mut session, Value::Null).unwrap_err();
        assert!(err.contains("ORA-01403"), "{err}");
        assert_eq!(session.get("LEDGER.N"), Some(&Value::Null));
        assert_eq!(call(&proc, &mut session, Value::Int(1)).unwrap(), Value::Null);
        assert_eq!(session.get("LEDGER.N"), Some(&Value::Int(1)));
        let mut fresh = HashMap::new();
        assert_eq!(call(&proc, &mut fresh, Value::Int(2)).unwrap(), Value::Null);
        assert_eq!(session.get("LEDGER.N"), Some(&Value::Int(1)));

        let hidden = compiled(
            "CREATE PACKAGE BODY ledger AS n PLS_INTEGER; FUNCTION touch(v PLS_INTEGER) RETURN PLS_INTEGER IS n PLS_INTEGER; BEGIN n := v; RETURN n; END; END;",
        );
        let mut hidden_state = HashMap::new();
        assert_eq!(
            call(&hidden, &mut hidden_state, Value::Int(4)).unwrap(),
            Value::Int(4)
        );
        assert!(hidden_state.is_empty(), "{hidden_state:?}");

        let refused = [
            (
                "CREATE PACKAGE ledger AS n PLS_INTEGER; END;",
                "a package specification is not implemented",
            ),
            (
                "CREATE PACKAGE BODY ledger AS FUNCTION touch RETURN PLS_INTEGER IS BEGIN RETURN 1; END; END;",
                "a package body requires one variable before its subprogram",
            ),
            (
                "CREATE PACKAGE BODY ledger AS n PLS_INTEGER; m PLS_INTEGER; FUNCTION touch RETURN PLS_INTEGER IS BEGIN RETURN 1; END; END;",
                "a package body supports one variable and one subprogram",
            ),
            (
                "CREATE PACKAGE BODY ledger AS n CONSTANT PLS_INTEGER := NULL; FUNCTION touch RETURN PLS_INTEGER IS BEGIN RETURN 1; END; END;",
                "package constants are not implemented",
            ),
            (
                "CREATE PACKAGE BODY ledger AS n PLS_INTEGER NOT NULL := 1; FUNCTION touch RETURN PLS_INTEGER IS BEGIN RETURN 1; END; END;",
                "a package variable cannot be NOT NULL",
            ),
            (
                "CREATE PACKAGE BODY ledger AS n PLS_INTEGER := 1; FUNCTION touch RETURN PLS_INTEGER IS BEGIN RETURN 1; END; END;",
                "a package variable can only be initialized with NULL",
            ),
            (
                "CREATE PACKAGE BODY ledger AS n PLS_INTEGER; FUNCTION touch RETURN PLS_INTEGER IS BEGIN RETURN 1; END; FUNCTION other RETURN PLS_INTEGER IS BEGIN RETURN 1; END; END;",
                "a package body supports one subprogram",
            ),
            (
                "CREATE PACKAGE BODY ledger AS n PLS_INTEGER; FUNCTION touch RETURN PLS_INTEGER IS BEGIN RETURN n; END; BEGIN n := 1; EXCEPTION WHEN OTHERS THEN NULL; END;",
                "package initialization exception handlers are not implemented",
            ),
            (
                "CREATE PACKAGE BODY ledger AS n PLS_INTEGER; FUNCTION n RETURN PLS_INTEGER IS BEGIN RETURN 1; END; END;",
                "duplicate package variable and subprogram name",
            ),
        ];
        for (source, expected) in refused {
            let err = compile(source).unwrap_err().to_string();
            assert!(err.contains(expected), "{source}\n{err}");
        }
        let err = compile_function(TOUCH).unwrap_err().to_string();
        assert!(
            err.contains("a stored SQL function requires CREATE FUNCTION, not a package"),
            "{err}"
        );
    }

    #[test]
    fn package_init_runs_once_and_a_failed_init_keeps_null() {
        let once = r#"
CREATE PACKAGE BODY ledger AS
  n PLS_INTEGER;
  FUNCTION touch(v PLS_INTEGER) RETURN PLS_INTEGER IS
    previous PLS_INTEGER;
  BEGIN
    previous := n;
    n := v;
    RETURN previous;
  END;
BEGIN
  n := 4;
  n := NULL;
END;
END;
"#;
        let proc = compiled(once);
        let mut session = HashMap::new();
        assert_eq!(call(&proc, &mut session, Value::Int(9)).unwrap(), Value::Null);
        assert_eq!(call(&proc, &mut session, Value::Int(1)).unwrap(), Value::Int(9));

        let failed = r#"
CREATE PACKAGE BODY ledger AS
  n PLS_INTEGER;
  FUNCTION touch(v PLS_INTEGER) RETURN PLS_INTEGER IS
  BEGIN
    RETURN n;
  END;
BEGIN
  n := 4;
  n := NULL;
  RAISE NO_DATA_FOUND;
END;
END;
"#;
        let proc = compiled(failed);
        let mut session = HashMap::new();
        let err = call(&proc, &mut session, Value::Null).unwrap_err();
        assert!(err.contains("ORA-01403"), "{err}");
        assert_eq!(session.get("LEDGER.N"), Some(&Value::Null));
        let again = call(&proc, &mut session, Value::Null).unwrap_err();
        assert!(again.contains("ORA-01403"), "{again}");
    }
}
