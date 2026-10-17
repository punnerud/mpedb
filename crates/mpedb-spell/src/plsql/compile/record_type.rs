//! `TYPE name IS RECORD (field datatype [NOT NULL] [:= literal], ...)`.
//! A field may name another record type. Each variable gets its own slots,
//! including every nested leaf. A field with no default starts NULL, and a
//! default on one field does not fill the others. `rec IS NULL` is true only
//! when every leaf is NULL. Assigning NULL to the record stores NULL in
//! every leaf, and is refused when any leaf is `NOT NULL`. A nested field
//! cannot itself be `NOT NULL` or carry an initializer: the inner type does.

use super::{Parser, RecordDecl, Type};
use crate::ir::Op;
use crate::plsql::lex::Token;
use mpedb_types::{Result, Value};

#[derive(Clone)]
pub(super) struct RecordType {
    fields: Vec<RecordField>,
}

#[derive(Clone)]
struct RecordField {
    name: String,
    kind: FieldKind,
}

#[derive(Clone)]
enum FieldKind {
    Scalar(ScalarField),
    Nested { type_name: String, shape: RecordType },
}

#[derive(Clone)]
struct ScalarField {
    ty: Type,
    not_null: bool,
    /// `None` means the field starts NULL. A stored NULL is an explicit
    /// `:= NULL`, which is the same initial state.
    default: Option<Value>,
}

#[derive(Clone, Copy)]
pub(super) struct FieldInfo {
    pub ty: Type,
    pub not_null: bool,
}

fn shape_has_not_null(shape: &RecordType) -> bool {
    shape.fields.iter().any(|field| match &field.kind {
        FieldKind::Scalar(scalar) => scalar.not_null,
        FieldKind::Nested { shape, .. } => shape_has_not_null(shape),
    })
}

impl Parser<'_> {
    pub(super) fn declare_record_body(&mut self, name: String) -> Result<()> {
        self.want_symbol("(")?;
        if self.symbol(")") {
            return Err(self.error("a record type needs a field"));
        }
        let mut fields: Vec<RecordField> = Vec::new();
        loop {
            let field_name = self.identifier()?;
            if fields.iter().any(|field| field.name == field_name) {
                return Err(self.error("duplicate record field"));
            }
            let kind = match self.record_field_kind()? {
                FieldKind::Nested { type_name, shape } => {
                    if self.word("NOT") {
                        return Err(self.error("a nested record field cannot be NOT NULL"));
                    }
                    if self.symbol(":=") || self.word("DEFAULT") {
                        return Err(self.error(
                            "a nested record field cannot have an initializer",
                        ));
                    }
                    FieldKind::Nested { type_name, shape }
                }
                FieldKind::Scalar(mut scalar) => {
                    if self.word("NOT") {
                        self.want_word("NULL")?;
                        scalar.not_null = true;
                    }
                    if self.symbol(":=") || self.word("DEFAULT") {
                        scalar.default = Some(self.record_literal()?);
                    }
                    if let Some(value) = &scalar.default {
                        self.literal_fits(scalar.ty, value)?;
                    }
                    if scalar.not_null && matches!(scalar.default, None | Some(Value::Null)) {
                        return Err(self.error(
                            "a NOT NULL record field requires an initializer",
                        ));
                    }
                    FieldKind::Scalar(scalar)
                }
            };
            fields.push(RecordField {
                name: field_name,
                kind,
            });
            if self.symbol(")") {
                break;
            }
            self.want_symbol(",")?;
        }
        self.want_symbol(";")?;
        if !self.declared_here.insert(name.clone()) {
            return Err(self.error("duplicate parameter or variable name"));
        }
        self.pending_declarations.remove("TYPE");
        self.pending_declarations.remove(&name);
        self.record_types.insert(name, RecordType { fields });
        Ok(())
    }

    pub(super) fn declare_record_var(&mut self, name: &str, constant: bool) -> Result<bool> {
        let type_name = match self.peek() {
            Some(Token::Identifier {
                name: type_name,
                quoted: false,
            }) => type_name.clone(),
            _ => return Ok(false),
        };
        let Some(shape) = self.record_types.get(&type_name).cloned() else {
            return Ok(false);
        };
        self.at += 1;
        if constant {
            return Err(self.error("a record variable cannot be CONSTANT"));
        }
        if self.word("NOT") {
            return Err(self.error("a record variable cannot be NOT NULL"));
        }
        let explicit_null = if self.symbol(":=") || self.word("DEFAULT") {
            if !self.word("NULL") {
                return Err(self.error(
                    "a record variable can only be initialized with NULL",
                ));
            }
            true
        } else {
            false
        };
        if explicit_null {
            self.refuse_null_fields(&shape)?;
        }
        self.want_symbol(";")?;
        if !self.declared_here.insert(name.to_string()) {
            return Err(self.error("duplicate parameter or variable name"));
        }
        self.pending_declarations.remove(name);
        self.locals.remove(name);
        let decl = self.lay_out(Some(type_name), &shape, explicit_null)?;
        self.records.insert(name.to_string(), decl);
        Ok(true)
    }

    /// One slot per scalar leaf. A nested field gets the inner type's slots,
    /// with that type's defaults. `explicit_null` stores NULL in every leaf.
    fn lay_out(
        &mut self,
        type_name: Option<String>,
        shape: &RecordType,
        explicit_null: bool,
    ) -> Result<RecordDecl> {
        let mut fields = Vec::new();
        let mut nested = Vec::new();
        for field in &shape.fields {
            match &field.kind {
                FieldKind::Scalar(scalar) => {
                    if explicit_null {
                        self.constant(Value::Null)?;
                        self.builder.emit(scalar.ty.check())?;
                    } else {
                        self.emit_scalar_default(scalar)?;
                    }
                    let slot = self.builder.alloc_local()?;
                    self.builder.emit(Op::StoreLocal(slot))?;
                    self.record_field_info.insert(
                        slot,
                        FieldInfo {
                            ty: scalar.ty,
                            not_null: scalar.not_null,
                        },
                    );
                    fields.push((field.name.clone(), slot));
                }
                FieldKind::Nested {
                    type_name: inner_name,
                    shape: inner,
                } => {
                    let child = self.lay_out(Some(inner_name.clone()), inner, explicit_null)?;
                    nested.push((field.name.clone(), child));
                }
            }
        }
        Ok(RecordDecl {
            fields,
            type_name,
            nested,
        })
    }

    pub(super) fn store_record_field(&mut self, slot: u16, actual: Option<Type>) -> Result<()> {
        if let Some(info) = self.record_field_info.get(&slot).copied() {
            self.compatible(info.ty, actual)?;
            if info.not_null && actual.is_none() {
                return Err(self.error(
                    "a NOT NULL record field cannot be assigned literal NULL",
                ));
            }
            self.builder.emit(info.ty.check())?;
            if info.not_null {
                self.builder.emit(Op::OracleNotNull)?;
            }
        }
        self.builder.emit(Op::StoreLocal(slot))?;
        Ok(())
    }

    pub(super) fn refuse_null_record(&self, record: &RecordDecl) -> Result<()> {
        if self.record_has_not_null(record) {
            return Err(self.error("a NOT NULL record field cannot be assigned NULL"));
        }
        Ok(())
    }

    fn record_has_not_null(&self, record: &RecordDecl) -> bool {
        record.fields.iter().any(|(_, slot)| {
            self.record_field_info
                .get(slot)
                .is_some_and(|info| info.not_null)
        }) || record
            .nested
            .iter()
            .any(|(_, child)| self.record_has_not_null(child))
    }

    /// `target := NULL` stores NULL in every scalar leaf, including leaves
    /// inside a nested record.
    pub(super) fn store_null_leaves(&mut self, record: &RecordDecl) -> Result<()> {
        for (_, slot) in &record.fields {
            self.constant(Value::Null)?;
            self.builder.emit(Op::StoreLocal(*slot))?;
        }
        for (_, child) in &record.nested {
            self.store_null_leaves(child)?;
        }
        Ok(())
    }

    /// The record is NULL exactly when every scalar leaf is NULL.
    pub(super) fn emit_record_null_test(&mut self, record: &RecordDecl) -> Result<()> {
        self.constant(Value::Bool(true))?;
        self.and_null_leaves(record)
    }

    fn and_null_leaves(&mut self, record: &RecordDecl) -> Result<()> {
        for (_, slot) in &record.fields {
            self.builder.emit(Op::LoadLocal(*slot))?;
            self.constant(Value::Null)?;
            self.builder.emit(Op::Eq)?;
            self.builder.emit(Op::SqlAnd)?;
        }
        for (_, child) in &record.nested {
            self.and_null_leaves(child)?;
        }
        Ok(())
    }

    /// `target := source` for two variables of the same record type.
    /// Each field is copied, including NULL. The variables do not alias.
    pub(super) fn assign_whole_record(
        &mut self,
        target: &super::RecordDecl,
        source: &super::RecordDecl,
    ) -> Result<()> {
        match (&target.type_name, &source.type_name) {
            (Some(left), Some(right)) if left == right => {}
            (None, _) | (_, None) => {
                return Err(self.error("a %ROWTYPE record requires a field"));
            }
            _ => {
                return Err(self.error(
                    "a record can only be assigned a record of the same type",
                ));
            }
        }
        if target.fields.len() != source.fields.len() {
            return Err(self.error("a record can only be assigned a record of the same type"));
        }
        for ((target_name, target_slot), (source_name, source_slot)) in
            target.fields.iter().zip(source.fields.iter())
        {
            if target_name != source_name {
                return Err(self.error(
                    "a record can only be assigned a record of the same type",
                ));
            }
            self.builder.emit(Op::LoadLocal(*source_slot))?;
            let actual = self.record_field_info.get(source_slot).map(|info| info.ty);
            self.store_record_field(*target_slot, actual)?;
        }
        if target.nested.len() != source.nested.len() {
            return Err(self.error("a record can only be assigned a record of the same type"));
        }
        for ((target_name, target_child), (source_name, source_child)) in
            target.nested.iter().zip(source.nested.iter())
        {
            if target_name != source_name {
                return Err(self.error(
                    "a record can only be assigned a record of the same type",
                ));
            }
            self.assign_whole_record(target_child, source_child)?;
        }
        Ok(())
    }

    fn refuse_null_fields(&self, shape: &RecordType) -> Result<()> {
        if shape_has_not_null(shape) {
            return Err(self.error("a NOT NULL record field cannot be assigned NULL"));
        }
        Ok(())
    }

    fn emit_scalar_default(&mut self, field: &ScalarField) -> Result<()> {
        let value = match &field.default {
            None => Value::Null,
            Some(Value::Int(n))
                if matches!(field.ty, Type::Number | Type::ConstrainedNumber { .. }) =>
            {
                Value::Numeric(n.to_string())
            }
            Some(value) => value.clone(),
        };
        self.constant(value)?;
        self.builder.emit(field.ty.check())?;
        if field.not_null {
            self.builder.emit(Op::OracleNotNull)?;
        }
        Ok(())
    }

    fn record_field_kind(&mut self) -> Result<FieldKind> {
        if self.dotted_percent_type()
            && matches!(self.peek(), Some(Token::Identifier { .. }))
            && matches!(
                self.tokens.get(self.at + 1).map(|token| &token.token),
                Some(Token::Symbol("."))
            )
        {
            let (ty, inherited) = self.resolve_dotted_type()?;
            return Ok(FieldKind::Scalar(ScalarField {
                ty,
                not_null: inherited,
                default: None,
            }));
        }
        if self.type_word_at(self.at + 2)
            && matches!(
                self.tokens.get(self.at + 1).map(|token| &token.token),
                Some(Token::Symbol("%"))
            )
        {
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
            return Ok(FieldKind::Scalar(ScalarField {
                ty: anchor.ty,
                not_null: anchor.not_null,
                default: None,
            }));
        }
        if let Some(Token::Identifier {
            name,
            quoted: false,
        }) = self.peek()
        {
            if self.record_types.contains_key(name) {
                let type_name = name.clone();
                self.at += 1;
                let shape = self
                    .record_types
                    .get(&type_name)
                    .cloned()
                    .ok_or_else(|| self.error("a nested record field is not declared"))?;
                return Ok(FieldKind::Nested { type_name, shape });
            }
        }
        Ok(FieldKind::Scalar(ScalarField {
            ty: self.data_type(true)?,
            not_null: false,
            default: None,
        }))
    }

    fn record_literal(&mut self) -> Result<Value> {
        if self.word("NULL") {
            return Ok(Value::Null);
        }
        if self.word("TRUE") {
            return Ok(Value::Bool(true));
        }
        if self.word("FALSE") {
            return Ok(Value::Bool(false));
        }
        if let Some(Token::String { value, national }) = self.peek().cloned() {
            if national {
                return Err(self.error(
                    "national character strings require NVARCHAR2 runtime support",
                ));
            }
            self.at += 1;
            return Ok(Value::Text(value));
        }
        if let Some(Token::Number(text)) = self.peek().cloned() {
            self.at += 1;
            if text.bytes().all(|byte| byte.is_ascii_digit()) {
                let number = text.parse::<i64>().map_err(|_| {
                    self.error("a record field default is outside the supported integer range")
                })?;
                return Ok(Value::Int(number));
            }
            let numeric = mpedb_types::value::parse_numeric(&text)
                .ok_or_else(|| self.error("unsupported NUMBER literal"))?;
            return Ok(Value::Numeric(numeric));
        }
        Err(self.error("a record field default must be a literal"))
    }

    fn literal_fits(&self, ty: Type, value: &Value) -> Result<()> {
        if matches!(value, Value::Null) {
            return Ok(());
        }
        let ok = match (ty, value) {
            (Type::Boolean, Value::Bool(_)) => true,
            (Type::Varchar(_), Value::Text(_)) => true,
            (Type::PlsInteger, Value::Int(number)) => i32::try_from(*number).is_ok(),
            (
                Type::Number | Type::ConstrainedNumber { .. },
                Value::Int(_) | Value::Numeric(_),
            ) => true,
            _ => false,
        };
        if ok {
            Ok(())
        } else {
            Err(self.error("incompatible record field default"))
        }
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

    #[test]
    fn record_field_starts_null_and_a_default_does_not_fill_the_other_field() {
        let note = function(
            "VARCHAR2",
            "TYPE pair IS RECORD (id PLS_INTEGER := 7, note VARCHAR2(4)); \
             r pair; \
             BEGIN \
               IF r.note IS NULL THEN r.note := NULL; END IF; \
               RETURN r.note; \
             END;",
        );
        assert_eq!(call(&note).unwrap(), Value::Null);
        let id = function(
            "PLS_INTEGER",
            "TYPE pair IS RECORD (id PLS_INTEGER := 7, note VARCHAR2(4)); \
             r pair; BEGIN RETURN r.id; END;",
        );
        assert_eq!(call(&id).unwrap(), Value::Int(7));
        let whole = function(
            "BOOLEAN",
            "TYPE pair IS RECORD (id PLS_INTEGER := 7, note VARCHAR2(4)); \
             r pair; BEGIN RETURN r IS NULL; END;",
        );
        assert_eq!(call(&whole).unwrap(), Value::Bool(false));
        let empty = function(
            "BOOLEAN",
            "TYPE pair IS RECORD (id PLS_INTEGER, note VARCHAR2(4)); \
             r pair; BEGIN RETURN r IS NULL; END;",
        );
        assert_eq!(call(&empty).unwrap(), Value::Bool(true));
        let cleared = function(
            "VARCHAR2",
            "TYPE pair IS RECORD (note VARCHAR2(4)); \
             r pair; BEGIN r.note := 'ok'; r := NULL; RETURN r.note; END;",
        );
        assert_eq!(call(&cleared).unwrap(), Value::Null);
        let other = function(
            "VARCHAR2",
            "TYPE pair IS RECORD (note VARCHAR2(4)); \
             r pair; s pair; BEGIN r.note := 'ok'; RETURN s.note; END;",
        );
        assert_eq!(call(&other).unwrap(), Value::Null);
        let overflow = function(
            "VARCHAR2",
            "TYPE pair IS RECORD (note VARCHAR2(4)); r pair; \
             BEGIN r.note := 'toolong'; RETURN 'x'; \
             EXCEPTION WHEN VALUE_ERROR THEN RETURN r.note; END;",
        );
        assert_eq!(call(&overflow).unwrap(), Value::Null);
    }

    #[test]
    fn record_type_refuses_not_null_without_a_value_and_nested_anchors() {
        let refused = [
            (
                "TYPE pair IS RECORD (id PLS_INTEGER NOT NULL); r pair; BEGIN RETURN 1; END;",
                "NOT NULL record field requires an initializer",
            ),
            (
                "TYPE pair IS RECORD (id PLS_INTEGER NOT NULL := NULL); r pair; BEGIN RETURN 1; END;",
                "NOT NULL record field requires an initializer",
            ),
            (
                "TYPE pair IS RECORD (id PLS_INTEGER NOT NULL := 1, note VARCHAR2(4)); \
                 r pair; BEGIN r := NULL; RETURN r.note; END;",
                "cannot be assigned NULL",
            ),
            (
                "TYPE pair IS RECORD (note VARCHAR2(4)); \
                 r pair; BEGIN r.note := NULL; RETURN r.note; END;",
                "",
            ),
            (
                "a VARCHAR2(4) := 'kept'; \
                 TYPE pair IS RECORD (note a%TYPE); r pair; BEGIN RETURN r.note; END;",
                "",
            ),
            (
                "TYPE pair IS RECORD (note employees.last_name%TYPE); r pair; BEGIN RETURN 'x'; END;",
                "no such table EMPLOYEES",
            ),
            (
                "TYPE pair IS RECORD (id PLS_INTEGER); TYPE box IS RECORD (inner pair NOT NULL); BEGIN RETURN 1; END;",
                "a nested record field cannot be NOT NULL",
            ),
            (
                "TYPE pair IS RECORD (id PLS_INTEGER); TYPE box IS RECORD (inner pair := 1); BEGIN RETURN 1; END;",
                "a nested record field cannot have an initializer",
            ),
            (
                "TYPE box IS RECORD (inner box); BEGIN RETURN 1; END;",
                "other types require typed runtime support",
            ),
            (
                "TYPE pair IS RECORD (id PLS_INTEGER NOT NULL := 1); \
                 TYPE box IS RECORD (inner pair); \
                 b box; BEGIN b.inner := NULL; RETURN 1; END;",
                "cannot be assigned NULL",
            ),
            (
                "TYPE pair IS RECORD (id PLS_INTEGER, id VARCHAR2(4)); BEGIN RETURN 1; END;",
                "duplicate record field",
            ),
        ];
        for (body, expected) in refused {
            let source = function(if body.contains("RETURN r.note") && body.contains("a%TYPE") {
                "VARCHAR2"
            } else if body.contains("RETURN 1") || body.contains("RETURN 'x'") {
                "NUMBER"
            } else {
                "VARCHAR2"
            }, body);
            // The two successful bodies return NULL. The others fail to compile.
            if expected.is_empty() {
                assert_eq!(call(&source).unwrap(), Value::Null, "{body}");
            } else {
                let err = compile(&source).unwrap_err().to_string();
                assert!(err.contains(expected), "{body}\n{err}");
            }
        }
    }

    #[test]
    fn record_assignment_copies_null_and_does_not_alias() {
        let copied = function(
            "VARCHAR2",
            "TYPE pair IS RECORD (id PLS_INTEGER := 7, note VARCHAR2(4)); \
             s pair; r pair; \
             BEGIN \
               r.note := 'kept'; \
               r := s; \
               s.note := 'kept'; \
               IF r.note IS NULL THEN r.note := NULL; END IF; \
               RETURN r.note; \
             END;",
        );
        assert_eq!(call(&copied).unwrap(), Value::Null);

        let id = function(
            "PLS_INTEGER",
            "TYPE pair IS RECORD (id PLS_INTEGER := 7, note VARCHAR2(4)); \
             s pair; r pair; \
             BEGIN s.id := 3; r := s; RETURN r.id; END;",
        );
        assert_eq!(call(&id).unwrap(), Value::Int(3));

        let refused = [
            (
                "TYPE a IS RECORD (note VARCHAR2(4)); \
                 TYPE b IS RECORD (note VARCHAR2(4)); \
                 ra a; rb b; BEGIN ra := rb; RETURN ra.note; END;",
                "same type",
            ),
            (
                "TYPE pair IS RECORD (note VARCHAR2(4)); r pair; BEGIN r := 1; RETURN r.note; END;",
                "NULL or one field",
            ),
            (
                "CURSOR c IS SELECT id FROM t; v c%ROWTYPE; w c%ROWTYPE; \
                 BEGIN w := v; RETURN 'x'; END;",
                "a %ROWTYPE record requires a field",
            ),
        ];
        for (body, expected) in refused {
            let err = compile(&function("VARCHAR2", body)).unwrap_err().to_string();
            assert!(err.contains(expected), "{body}\n{err}");
        }
    }

    #[test]
    fn record_field_anchored_to_a_field_starts_null() {
        let source = function(
            "VARCHAR2",
            "TYPE pair IS RECORD (note VARCHAR2(4) := 'kept'); \
             r pair; \
             TYPE box IS RECORD (note r.note%TYPE); \
             b box; \
             BEGIN \
               IF b.note IS NULL THEN b.note := NULL; END IF; \
               RETURN b.note; \
             END;",
        );
        assert_eq!(call(&source).unwrap(), Value::Null);

        let err = compile(&function(
            "NUMBER",
            "TYPE pair IS RECORD (id PLS_INTEGER NOT NULL := 7); \
             r pair; \
             TYPE box IS RECORD (id r.id%TYPE); \
             BEGIN RETURN 1; END;",
        ))
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("NOT NULL record field requires an initializer"),
            "{err}"
        );
    }

    #[test]
    fn nested_record_field_starts_null_and_a_default_does_not_fill_it() {
        let note = function(
            "VARCHAR2",
            "TYPE pair IS RECORD (id PLS_INTEGER := 7, note VARCHAR2(4)); \
             TYPE box IS RECORD (inner pair); \
             b box; \
             BEGIN \
               IF b.inner.note IS NOT NULL THEN RETURN 'filled'; END IF; \
               IF b.inner IS NULL THEN RETURN 'whole'; END IF; \
               b.inner.note := NULL; \
               RETURN b.inner.note; \
             END;",
        );
        assert_eq!(call(&note).unwrap(), Value::Null);

        let empty = function(
            "BOOLEAN",
            "TYPE pair IS RECORD (id PLS_INTEGER, note VARCHAR2(4)); \
             TYPE box IS RECORD (inner pair); \
             b box; BEGIN RETURN b.inner IS NULL; END;",
        );
        assert_eq!(call(&empty).unwrap(), Value::Bool(true));

        let cleared = function(
            "VARCHAR2",
            "TYPE pair IS RECORD (note VARCHAR2(4)); \
             TYPE box IS RECORD (inner pair); \
             b box; \
             BEGIN b.inner.note := 'kept'; b := NULL; RETURN b.inner.note; END;",
        );
        assert_eq!(call(&cleared).unwrap(), Value::Null);

        let cleared_inner = function(
            "VARCHAR2",
            "TYPE pair IS RECORD (note VARCHAR2(4)); \
             TYPE box IS RECORD (inner pair); \
             b box; \
             BEGIN b.inner.note := 'kept'; b.inner := NULL; RETURN b.inner.note; END;",
        );
        assert_eq!(call(&cleared_inner).unwrap(), Value::Null);

        let other = function(
            "VARCHAR2",
            "TYPE pair IS RECORD (note VARCHAR2(4)); \
             TYPE box IS RECORD (inner pair); \
             b box; c box; \
             BEGIN b.inner.note := 'ok'; RETURN c.inner.note; END;",
        );
        assert_eq!(call(&other).unwrap(), Value::Null);

        let copied = function(
            "PLS_INTEGER",
            "TYPE pair IS RECORD (id PLS_INTEGER := 7, note VARCHAR2(4)); \
             TYPE box IS RECORD (inner pair); \
             c box; b box; \
             BEGIN \
               c.inner.note := NULL; \
               c.inner.id := 3; \
               b.inner.note := 'kept'; \
               b := c; \
               c.inner.id := 9; \
               RETURN b.inner.id; \
             END;",
        );
        assert_eq!(call(&copied).unwrap(), Value::Int(3));

        let copied_note = function(
            "VARCHAR2",
            "TYPE pair IS RECORD (id PLS_INTEGER := 7, note VARCHAR2(4)); \
             TYPE box IS RECORD (inner pair); \
             c box; b box; \
             BEGIN \
               b.inner.note := 'kept'; \
               b := c; \
               c.inner.note := 'kept'; \
               IF b.inner.note IS NULL THEN b.inner.note := NULL; END IF; \
               RETURN b.inner.note; \
             END;",
        );
        assert_eq!(call(&copied_note).unwrap(), Value::Null);

        let inner_assign = function(
            "VARCHAR2",
            "TYPE pair IS RECORD (id PLS_INTEGER := 7, note VARCHAR2(4)); \
             TYPE box IS RECORD (inner pair); \
             r pair; b box; \
             BEGIN \
               r.note := NULL; \
               b.inner.note := 'kept'; \
               b.inner := r; \
               r.note := 'kept'; \
               IF b.inner.note IS NULL THEN RETURN b.inner.note; END IF; \
               RETURN 'filled'; \
             END;",
        );
        assert_eq!(call(&inner_assign).unwrap(), Value::Null);

        let deep = function(
            "VARCHAR2",
            "TYPE pair IS RECORD (note VARCHAR2(4)); \
             TYPE box IS RECORD (inner pair); \
             TYPE wrap IS RECORD (held box); \
             w wrap; \
             BEGIN \
               IF w.held.inner.note IS NULL THEN w.held.inner.note := NULL; END IF; \
               RETURN w.held.inner.note; \
             END;",
        );
        assert_eq!(call(&deep).unwrap(), Value::Null);

        let overflow = function(
            "VARCHAR2",
            "TYPE pair IS RECORD (note VARCHAR2(4)); \
             TYPE box IS RECORD (inner pair); \
             b box; \
             BEGIN b.inner.note := 'toolong'; RETURN 'x'; \
             EXCEPTION WHEN VALUE_ERROR THEN RETURN b.inner.note; END;",
        );
        assert_eq!(call(&overflow).unwrap(), Value::Null);

        let refused = [
            (
                "TYPE pair IS RECORD (note VARCHAR2(4)); \
                 TYPE box IS RECORD (inner pair); \
                 TYPE case IS RECORD (inner pair); \
                 b box; c case; BEGIN b := c; RETURN b.inner.note; END;",
                "same type",
            ),
            (
                "TYPE pair IS RECORD (note VARCHAR2(4)); \
                 TYPE other IS RECORD (note VARCHAR2(4)); \
                 TYPE box IS RECORD (inner pair); \
                 r other; b box; BEGIN b.inner := r; RETURN b.inner.note; END;",
                "same type",
            ),
        ];
        for (body, expected) in refused {
            let err = compile(&function("VARCHAR2", body)).unwrap_err().to_string();
            assert!(err.contains(expected), "{body}\n{err}");
        }
    }
}
