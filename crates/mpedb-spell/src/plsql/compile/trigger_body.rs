//! Oracle row-trigger bodies. `:NEW.col` and `:OLD.col` read the image
//! arguments. The caller passes NULL for a side the event does not have and
//! for a NULL column. Assignment is refused, so the row buffer stays as the
//! statement built it.

use super::{FuncBuilder, Local, Parser, Type};
use crate::emit::Skeleton;
use crate::plsql::lex::{self, Token};
use mpedb_types::{Error, Result};
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TriggerImageType {
    Text,
    Integer,
}

#[derive(Clone, Debug)]
pub struct TriggerImage {
    pub name: String,
    pub ty: TriggerImageType,
}

pub(super) struct TriggerImages {
    new_cols: HashMap<String, Local>,
    old_cols: HashMap<String, Local>,
}

pub(super) fn compile_trigger_inner(source: &str, columns: &[TriggerImage]) -> Result<Skeleton> {
    if columns.len() > (u16::MAX as usize) / 2 {
        return Err(Error::Unsupported(
            "a PL/SQL trigger has too many columns".into(),
        ));
    }
    let tokens = lex::lex(source)?;
    let exception_blocks = tokens.iter().any(|token| {
        matches!(&token.token, Token::Identifier { name, quoted: false } if name == "EXCEPTION")
    });
    let mut builder = FuncBuilder::new();
    let mut new_cols = HashMap::with_capacity(columns.len());
    let mut old_cols = HashMap::with_capacity(columns.len());
    // `:NEW` occupies locals[0..width] and `:OLD` locals[width..2*width],
    // both in table order. The caller passes the same layout.
    for column in columns {
        let key = column.name.to_uppercase();
        if key.is_empty() || new_cols.contains_key(&key) {
            return Err(Error::Unsupported(
                "a PL/SQL trigger column name is empty or duplicated".into(),
            ));
        }
        let slot = builder.alloc_local()?;
        new_cols.insert(key, image_local(slot, image_type(column.ty)));
    }
    for column in columns {
        let key = column.name.to_uppercase();
        let slot = builder.alloc_local()?;
        old_cols.insert(key, image_local(slot, image_type(column.ty)));
    }
    let argc = (columns.len() * 2) as u16;
    let mut parser = Parser {
        source,
        routine_name: "TRG".into(),
        routine_locals: HashMap::new(),
        tokens,
        at: 0,
        builder,
        locals: HashMap::new(),
        declared_here: Default::default(),
        pending_declarations: Default::default(),
        returns: None,
        depth: 0,
        parameters: Vec::new(),
        has_out: false,
        exception_blocks,
        in_handler: 0,
        exceptions: HashMap::new(),
        cursors: HashMap::new(),
        records: HashMap::new(),
        nested_types: HashMap::new(),
        nested_vars: HashMap::new(),
        record_types: HashMap::new(),
        record_field_info: HashMap::new(),
        forall_index: None,
        forall_subscripts: 0,
        package_name: None,
        package_var: None,
        trigger_images: Some(TriggerImages { new_cols, old_cols }),
    };
    if parser.word("DECLARE") {
        parser.declarations()?;
    }
    parser.routine_locals = parser.locals.clone();
    parser.want_word("BEGIN")?;
    parser.block_body()?;
    parser.want_word("END")?;
    if matches!(parser.peek(), Some(Token::Identifier { .. })) {
        return Err(parser.error("an anonymous block cannot have an END name"));
    }
    parser.want_symbol(";")?;
    parser.finish_unit(source)?;
    parser.emit_procedure_result()?;
    parser.builder.emit(crate::ir::Op::Return)?;
    Ok(Skeleton {
        name: "TRG".into(),
        argc,
        nlocals: parser.builder.nlocals(),
        consts: std::mem::take(&mut parser.builder.consts),
        instrs: std::mem::take(&mut parser.builder.instrs),
        calls: std::mem::take(&mut parser.builder.calls),
    })
}

fn image_type(ty: TriggerImageType) -> Type {
    match ty {
        TriggerImageType::Text => Type::Varchar(0),
        TriggerImageType::Integer => Type::PlsInteger,
    }
}

fn image_local(slot: u16, ty: Type) -> Local {
    Local {
        slot,
        ty,
        writable: false,
        not_null: false,
    }
}

impl Parser<'_> {
    pub(super) fn refuse_correlation_statement(&mut self) -> Result<()> {
        if self.trigger_images.is_some() && matches!(self.peek(), Some(Token::Bind(_))) {
            return Err(self.error("a trigger cannot assign :NEW or :OLD"));
        }
        Ok(())
    }

    pub(super) fn correlation_expr(&mut self) -> Result<Option<Type>> {
        let Some(Token::Bind(side)) = self.peek().cloned() else {
            return Err(self.error("a trigger correlation name must be :NEW or :OLD"));
        };
        self.at += 1;
        if self.trigger_images.is_none() {
            return Err(self.error(
                "trigger correlation names are only valid in a trigger body",
            ));
        }
        if side != "NEW" && side != "OLD" {
            return Err(self.error("a trigger correlation name must be :NEW or :OLD"));
        }
        if !self.symbol(".") {
            return Err(self.error("a trigger correlation name needs a column"));
        }
        if matches!(self.peek(), Some(Token::Identifier { quoted: true, .. })) {
            return Err(self.error("quoted trigger correlation names are not implemented"));
        }
        let column = self.identifier()?;
        if self.symbol(".") {
            return Err(self.error("a trigger correlation name has one column"));
        }
        let local = self
            .trigger_images
            .as_ref()
            .and_then(|images| {
                let map = if side == "NEW" {
                    &images.new_cols
                } else {
                    &images.old_cols
                };
                map.get(&column).copied()
            })
            .ok_or_else(|| self.error(&format!("no such column {column} in :{side}")))?;
        self.builder.emit(crate::ir::Op::LoadLocal(local.slot))?;
        Ok(Some(local.ty))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emit::CallKind;
    use crate::interp::testutil::MockBridge;
    use crate::interp::{self, Budget, ProcValue};
    use crate::ir::{PlanKind, PlanRef, Proc};
    use crate::plsql::compile_trigger;
    use mpedb_types::{PlanHash, Value};

    const BODY: &str = r#"
DECLARE
  n VARCHAR2(40);
  o VARCHAR2(40);
  i PLS_INTEGER;
BEGIN
  n := :NEW.note;
  o := :OLD.note;
  i := :NEW.id;
  IF n IS NULL THEN
    NULL;
  END IF;
  INSERT INTO seen (id, new_note, old_note) VALUES (TRG.i, TRG.n, TRG.o);
END;
"#;

    fn columns() -> Vec<TriggerImage> {
        vec![
            TriggerImage {
                name: "id".into(),
                ty: TriggerImageType::Integer,
            },
            TriggerImage {
                name: "note".into(),
                ty: TriggerImageType::Text,
            },
        ]
    }

    fn proc(source: &str) -> Proc {
        let skel = compile_trigger(source, &columns()).unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(skel.name, "TRG");
        assert_eq!(skel.argc, 4);
        let plans = skel
            .calls
            .iter()
            .map(|call| PlanRef {
                hash: PlanHash([0; 32]),
                kind: if call.kind == CallKind::Query {
                    PlanKind::Query
                } else {
                    PlanKind::Exec
                },
                argc: call.argc,
            })
            .collect();
        Proc::new(skel.name, skel.argc, skel.nlocals, plans, skel.consts, skel.instrs).unwrap()
    }

    fn fire(args: &[Value]) -> Vec<Value> {
        let proc = proc(BODY);
        let mut bridge = MockBridge::new();
        let value = interp::run(
            &proc,
            args,
            &mut bridge,
            Budget {
                instrs: 10_000,
                db_calls: 8,
                rows: 100,
            },
        )
        .unwrap_or_else(|err| panic!("{err}"));
        assert!(matches!(value, ProcValue::Scalar(Value::Null)));
        assert_eq!(bridge.execs, 1);
        bridge.dml_params.pop().unwrap()
    }

    #[test]
    fn trigger_correlation_reads_null_images() {
        let null_insert = fire(&[Value::Int(1), Value::Null, Value::Null, Value::Null]);
        assert_eq!(
            null_insert,
            vec![Value::Int(1), Value::Null, Value::Null]
        );
        let text_insert = fire(&[
            Value::Int(2),
            Value::Text("kept".into()),
            Value::Null,
            Value::Null,
        ]);
        assert_eq!(
            text_insert,
            vec![Value::Int(2), Value::Text("kept".into()), Value::Null]
        );
        // DELETE: no NEW image. :NEW.id and :NEW.note are NULL; :OLD.note is the text.
        let deleted = fire(&[
            Value::Null,
            Value::Null,
            Value::Int(2),
            Value::Text("kept".into()),
        ]);
        assert_eq!(
            deleted,
            vec![Value::Null, Value::Null, Value::Text("kept".into())]
        );
    }

    #[test]
    fn trigger_correlation_refuses_assignment_and_unknown_columns() {
        let refused = [
            (
                "BEGIN :NEW.note := 'x'; END;",
                "a trigger cannot assign :NEW or :OLD",
            ),
            (
                "DECLARE n VARCHAR2(4); BEGIN n := :NEW.missing; END;",
                "no such column MISSING",
            ),
            ("DECLARE n VARCHAR2(4); BEGIN n := :NEW; END;", "needs a column"),
            (
                "DECLARE n VARCHAR2(4); BEGIN n := :NEW.note.extra; END;",
                "one column",
            ),
            (
                "DECLARE n VARCHAR2(4); BEGIN n := :NEW.\"note\"; END;",
                "quoted trigger correlation names are not implemented",
            ),
        ];
        for (source, expected) in refused {
            let err = compile_trigger(source, &columns()).unwrap_err().to_string();
            assert!(err.contains(expected), "{source}\n{err}");
        }
    }
}
