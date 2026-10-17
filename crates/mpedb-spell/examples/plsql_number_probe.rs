//! Host-side arithmetic probe for differential tests against real Oracle.
use mpedb_spell::interp::{self, Budget, DbBridge, ProcValue};
use mpedb_spell::ir::{PlanRef, Proc};
use mpedb_types::{Error, Result, Value};
use std::io::{self, BufRead, Write};

struct NoDb;
impl DbBridge for NoDb {
    fn query(&mut self, _: &PlanRef, _: &[Value]) -> Result<Vec<Vec<Value>>> {
        Err(Error::Unsupported("arithmetic probe cannot query".into()))
    }
    fn exec(&mut self, _: &PlanRef, _: &[Value]) -> Result<u64> {
        Err(Error::Unsupported("arithmetic probe cannot write".into()))
    }
    fn cursor_open(&mut self, _: &PlanRef, _: &[Value]) -> Result<u32> {
        Err(Error::Unsupported(
            "arithmetic probe cannot open cursors".into(),
        ))
    }
    fn cursor_advance(&mut self, _: u32) -> Result<Option<Vec<Value>>> {
        Err(Error::Unsupported(
            "arithmetic probe cannot advance cursors".into(),
        ))
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut output = io::stdout().lock();
    for expression in io::stdin().lock().lines() {
        let expression = expression?;
        let result = (|| -> Result<ProcValue> {
            let source =
                format!("CREATE FUNCTION f RETURN NUMBER IS BEGIN RETURN {expression}; END;");
            let s = mpedb_spell::plsql::compile_function(&source)?;
            let p = Proc::new(s.name, s.argc, s.nlocals, vec![], s.consts, s.instrs)?;
            let p = Proc::decode(&p.encode())?;
            interp::run(&p, &[], &mut NoDb, Budget::default())
        })();
        match result {
            Ok(ProcValue::Scalar(Value::Numeric(value))) => writeln!(output, "OK\t{value}")?,
            Ok(ProcValue::Scalar(Value::Null)) => writeln!(output, "NULL")?,
            other => writeln!(output, "ERR\t{}", format!("{other:?}").replace('\n', " "))?,
        }
    }
    Ok(())
}
