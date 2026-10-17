//! Native Oracle routine compilation. A top-level `DECLARE`/`BEGIN` block is
//! an anonymous block: it is not stored under a caller-chosen name, its
//! uninitialized variables are NULL, and an unhandled exception leaves the
//! block. No translation into Python or PL/pgSQL.

mod sql;
mod exception;
mod number;
mod static_sql;
mod cursor;
mod forall;
mod bulk_collect;
mod package;
mod trigger_body;
mod anchored_type;
mod record_type;
pub use anchored_type::ColumnAnchor;
pub use trigger_body::{TriggerImage, TriggerImageType};

use super::lex::{self, Lexed, Token};
use crate::emit::{FuncBuilder, Skeleton};
use crate::ir::Op;
use mpedb_types::{Result, Value};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Type {
    Boolean,
    Number,
    ConstrainedNumber { precision: u8, scale: i16 },
    PlsInteger,
    Varchar(u16),
    CharLiteral,
}

impl Type {
    fn check(self) -> Op {
        match self {
            Self::Boolean => Op::SqlBool,
            Self::Number => Op::OraclePlSqlNumber,
            Self::ConstrainedNumber { precision, scale } => Op::OracleNumberConstraint { precision, scale },
            Self::PlsInteger => Op::OraclePlSqlInteger,
            Self::Varchar(limit) => Op::OracleVarchar(limit),
            Self::CharLiteral => Op::OracleVarchar(0),
        }
    }
}

#[derive(Clone, Copy)]
struct Local {
    slot: u16,
    ty: Type,
    writable: bool,
    not_null: bool,
}

/// One local slot per scalar field. Nested record fields live in `nested`,
/// each with its own slots. The record name is not itself a local: a scalar
/// declared later in an inner block can hide it.
#[derive(Clone)]
struct RecordDecl {
    fields: Vec<(String, u16)>,
    /// `Some` for a `TYPE name IS RECORD` variable. `None` for `%ROWTYPE`.
    type_name: Option<String>,
    /// Nested record fields, in declaration order. A `%ROWTYPE` record has none.
    nested: Vec<(String, RecordDecl)>,
}

/// A `TYPE name IS TABLE OF` element type, stored as one local. NULL means
/// the table is uninitialized. An empty list is a completed `BULK COLLECT`
/// that returned no rows.
#[derive(Clone, Copy)]
struct NestedVar {
    slot: u16,
    elem: Type,
}

/// One package-level scalar. NULL means it has not been stored in this
/// session, and an explicit NULL store stays NULL. The key const is
/// `PACKAGE.NAME`.
#[derive(Clone)]
struct PackageVar {
    key_const: u16,
    ty: Type,
}

struct Parser<'a> {
    source: &'a str,
    routine_name: String,
    routine_locals: HashMap<String, Local>,
    tokens: Vec<Lexed>,
    at: usize,
    builder: FuncBuilder,
    locals: HashMap<String, Local>,
    declared_here: HashSet<String>,
    pending_declarations: HashSet<String>,
    returns: Option<Type>,
    depth: usize,
    parameters: Vec<u16>,
    has_out: bool,
    exception_blocks: bool,
    in_handler: usize,
    exceptions: HashMap<String, exception::ExceptionRef>,
    cursors: HashMap<String, cursor::CursorDecl>,
    records: HashMap<String, RecordDecl>,
    nested_types: HashMap<String, Type>,
    nested_vars: HashMap<String, NestedVar>,
    /// `TYPE name IS RECORD`. Each variable of the type gets its own slots.
    record_types: HashMap<String, record_type::RecordType>,
    /// Slots of user-record fields. `%ROWTYPE` slots stay absent, so those
    /// assignments remain untyped.
    record_field_info: HashMap<u16, record_type::FieldInfo>,
    /// Set only while the single `FORALL` statement is compiled. The index
    /// is not an expression; every use is a named refusal.
    forall_index: Option<String>,
    /// How many nested-table subscripts are being compiled. A `FORALL`
    /// index is legal only there.
    forall_subscripts: u16,
    /// Set while a package body is compiled. The stored routine name is
    /// `PACKAGE.ROUTINE`; `routine_name` stays the unqualified name.
    package_name: Option<String>,
    package_vars: HashMap<String, PackageVar>,
    /// Set only while an Oracle row trigger is compiled. Slots are the
    /// call arguments (`:NEW` columns, then `:OLD` columns), not locals.
    trigger_images: Option<trigger_body::TriggerImages>,
    /// Catalog columns for `table.column%TYPE`. Empty when the caller has
    /// no schema. Not saved per block: the catalog is database-global.
    column_anchors: &'a [anchored_type::ColumnAnchor],
}

pub fn compile(source: &str) -> Result<Skeleton> {
    compile_with_anchors(source, &[])
}

/// Compile PL/SQL with a snapshot of catalog columns. `table.column%TYPE`
/// resolves against `anchors` and does not inherit a column `NOT NULL`.
pub fn compile_with_anchors(source: &str, anchors: &[ColumnAnchor]) -> Result<Skeleton> {
    compile_on_stack(source, false, anchors)
}

/// Compile only a scalar SQL function; procedures and OUT signatures belong
/// to the procedure calling convention and must never enter a scalar catalog.
pub fn compile_function(source: &str) -> Result<Skeleton> {
    compile_on_stack(source, true, &[])
}

/// Compile one Oracle row-trigger body. `columns` is the target table in
/// order. Arguments are `:NEW` then `:OLD`; a column the event does not have
/// is passed as NULL by the caller. `:NEW.col :=` is refused.
pub fn compile_trigger(source: &str, columns: &[trigger_body::TriggerImage]) -> Result<Skeleton> {
    let owned = columns.to_vec();
    std::thread::scope(|scope| {
        let worker = std::thread::Builder::new()
            .name("mpedb-plsql-compile".into())
            .stack_size(8 * 1024 * 1024)
            .spawn_scoped(scope, || trigger_body::compile_trigger_inner(source, &owned))
            .map_err(|_| mpedb_types::Error::OutOfMemory {
                what: "PL/SQL compiler worker",
            })?;
        worker.join().map_err(|_| {
            mpedb_types::Error::Unsupported("PL/SQL compiler worker panicked".into())
        })?
    })
}

fn compile_on_stack(source: &str, scalar_only: bool, anchors: &[ColumnAnchor]) -> Result<Skeleton> {
    // Host-side parsing must not depend on the embedding application's worker
    // stack size. Keep the documented nesting bound while giving the recursive
    // parser a known stack; execution still uses the bounded, validated IR.
    std::thread::scope(|scope| {
        let worker = std::thread::Builder::new()
            .name("mpedb-plsql-compile".into())
            .stack_size(8 * 1024 * 1024)
            .spawn_scoped(scope, || compile_inner(source, scalar_only, anchors))
            .map_err(|_| mpedb_types::Error::OutOfMemory {
                what: "PL/SQL compiler worker",
            })?;
        worker.join().map_err(|_| {
            mpedb_types::Error::Unsupported("PL/SQL compiler worker panicked".into())
        })?
    })
}

fn compile_inner(source: &str, scalar_only: bool, anchors: &[ColumnAnchor]) -> Result<Skeleton> {
    let tokens = lex::lex(source)?;
    let exception_blocks = tokens.iter().any(|t| matches!(&t.token, Token::Identifier { name, quoted: false } if name == "EXCEPTION"));
    let mut p = Parser {
        source,
        routine_name: String::new(),
        routine_locals: HashMap::new(),
        tokens,
        at: 0,
        builder: FuncBuilder::new(),
        locals: HashMap::new(),
        declared_here: HashSet::new(),
        pending_declarations: HashSet::new(),
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
        package_vars: HashMap::new(),
        trigger_images: None,
        column_anchors: anchors,
    };
    if p.is_word("DECLARE") || p.is_word("BEGIN") {
        if scalar_only {
            return Err(p.error(
                "a stored SQL function requires CREATE FUNCTION, not an anonymous block",
            ));
        }
        return p.compile_anonymous(source);
    }
    p.want_word("CREATE")?;
    if p.word("OR") {
        p.want_word("REPLACE")?;
    }
    if p.word("PACKAGE") {
        if scalar_only {
            return Err(p.error("a stored SQL function requires CREATE FUNCTION, not a package"));
        }
        return p.compile_package(source);
    }
    p.compile_routine(source, scalar_only, false)
}

impl Parser<'_> {
    fn compile_routine(
        &mut self,
        source: &str,
        scalar_only: bool,
        embedded: bool,
    ) -> Result<Skeleton> {
        let function = if self.word("FUNCTION") {
            true
        } else {
            self.want_word("PROCEDURE")?;
            false
        };
        if scalar_only && !function {
            return Err(self.error("a stored SQL function requires CREATE FUNCTION, not CREATE PROCEDURE"));
        }
        if self.package_name.is_some()
            && matches!(self.peek(), Some(Token::Identifier { quoted: true, .. }))
        {
            return Err(self.error("quoted package names are not implemented"));
        }
        let name = self.identifier()?;
        if self.package_vars.contains_key(&name) {
            return Err(self.error("duplicate package variable and subprogram name"));
        }
        self.routine_name = name.clone();
        if self.symbol(".") {
            return Err(self.error("schema-qualified routine registration is not implemented"));
        }
        let mut argc = 0;
        if self.symbol("(") {
            if matches!(self.peek(), Some(Token::Symbol(")"))) {
                return Err(self.error("Oracle parameter lists cannot be empty; omit parentheses for a no-argument routine"));
            }
            loop {
                let name = self.identifier()?;
                let input = self.word("IN");
                let output = self.word("OUT");
                let out_only = output && !input;
                if self.word("NOCOPY") && !output {
                    return Err(self.error("NOCOPY is only valid for OUT or IN OUT parameters"));
                }
                // NOCOPY is an Oracle hint, not a guarantee. This runtime always
                // uses value/copy-out semantics, which Oracle explicitly permits.
                let ty = self.data_type(false)?;
                let slot = self.add_local(name, ty, output)?;
                if out_only {
                    self.constant(Value::Null)?;
                } else {
                    self.builder.emit(Op::LoadLocal(slot))?;
                    self.builder.emit(ty.check())?;
                }
                self.builder.emit(Op::StoreLocal(slot))?;
                self.parameters.push(slot);
                self.has_out |= output;
                argc += 1;
                if !self.symbol(",") {
                    self.want_symbol(")")?;
                    break;
                }
            }
        }
        if scalar_only && self.has_out {
            return Err(
                self.error("a scalar SQL function cannot have OUT or IN OUT parameters; use ProcEngine")
            );
        }
        if function {
            self.want_word("RETURN")?;
            self.returns = Some(self.data_type(false)?);
        }
        if !(self.word("IS") || self.word("AS")) {
            return Err(self.error("expected IS or AS before routine body"));
        }
        self.declarations()?;
        self.routine_locals = self.locals.clone();
        self.want_word("BEGIN")?;
        self.block_body()?;
        self.want_word("END")?;
        if matches!(self.peek(), Some(Token::Identifier { .. })) {
            let end_name = self.identifier()?;
            if end_name != name {
                return Err(self.error("END name does not match routine name"));
            }
        }
        self.want_symbol(";")?;
        if !embedded {
            self.finish_unit(source)?;
        }
        if function {
            self.builder.emit(Op::MissingReturn)?;
        } else {
            self.emit_procedure_result()?;
            self.builder.emit(Op::Return)?;
        }
        let stored = match &self.package_name {
            Some(package) => format!("{package}.{name}"),
            None => name,
        };
        Ok(Skeleton {
            name: stored,
            argc,
            nlocals: self.builder.nlocals(),
            consts: std::mem::take(&mut self.builder.consts),
            instrs: std::mem::take(&mut self.builder.instrs),
            calls: std::mem::take(&mut self.builder.calls),
        })
    }

    fn finish_unit(&mut self, source: &str) -> Result<()> {
        if self.symbol("/") {
            let span = &self.tokens[self.at - 1].span;
            let before = source[..span.start].rsplit('\n').next().unwrap_or("");
            let after = source[span.end..].split('\n').next().unwrap_or("");
            if !before.trim().is_empty() || !after.trim().is_empty() {
                return Err(self.error("SQL*Plus slash terminator must occupy its own line"));
            }
        }
        if self.at != self.tokens.len() {
            return Err(self.error("trailing input after routine"));
        }
        Ok(())
    }

    /// `[DECLARE …] BEGIN … [EXCEPTION …] END;` with no routine name.
    /// `RETURN;` leaves the block. `RETURN <expr>` stays a statement error
    /// because an anonymous block has no return type.
    fn compile_anonymous(&mut self, source: &str) -> Result<Skeleton> {
        if self.word("DECLARE") {
            self.declarations()?;
        }
        self.routine_locals = self.locals.clone();
        self.want_word("BEGIN")?;
        self.block_body()?;
        self.want_word("END")?;
        if matches!(self.peek(), Some(Token::Identifier { .. })) {
            return Err(self.error("an anonymous block cannot have an END name"));
        }
        self.want_symbol(";")?;
        if self.symbol("/") {
            let span = &self.tokens[self.at - 1].span;
            let before = source[..span.start].rsplit('\n').next().unwrap_or("");
            let after = source[span.end..].split('\n').next().unwrap_or("");
            if !before.trim().is_empty() || !after.trim().is_empty() {
                return Err(self.error("SQL*Plus slash terminator must occupy its own line"));
            }
        }
        if self.at != self.tokens.len() {
            return Err(self.error("trailing input after anonymous block"));
        }
        self.emit_procedure_result()?;
        self.builder.emit(Op::Return)?;
        Ok(Skeleton {
            name: "ANON".into(),
            argc: 0,
            nlocals: self.builder.nlocals(),
            consts: std::mem::take(&mut self.builder.consts),
            instrs: std::mem::take(&mut self.builder.instrs),
            calls: std::mem::take(&mut self.builder.calls),
        })
    }

    fn declarations(&mut self) -> Result<()> {
        // All names in a declaration section hide outer names, including
        // declarations not initialized yet. A self/forward reference must
        // therefore fail instead of accidentally binding an outer variable.
        let mut start = true;
        for token in &self.tokens[self.at..] {
            if matches!(&token.token, Token::Identifier { name, quoted: false } if name == "BEGIN")
            {
                break;
            }
            if start {
                if let Token::Identifier { name, .. } = &token.token {
                    self.pending_declarations.insert(name.clone());
                }
            }
            start = matches!(token.token, Token::Symbol(";"));
        }
        while !self.is_word("BEGIN") {
            if self.word("PRAGMA") {
                self.exception_init()?;
                continue;
            }
            if self.word("CURSOR") {
                self.declare_cursor()?;
                continue;
            }
            if self.looks_like_nested_type() {
                self.declare_nested_type()?;
                continue;
            }
            let position = self.tokens.get(self.at).map_or(self.source.len(), |t| t.span.start);
            let name = self.identifier()?;
            if self.word("EXCEPTION") {
                self.declare_exception(name, position)?;
                self.want_symbol(";")?;
                continue;
            }
            let constant = self.word("CONSTANT");
            if self.declare_rowtype(&name, constant)? {
                continue;
            }
            if self.declare_anchored_type(&name, constant)? {
                continue;
            }
            if self.declare_record_var(&name, constant)? {
                continue;
            }
            if self.declare_nested_var(&name, constant)? {
                continue;
            }
            let ty = self.data_type(true)?;
            let not_null = self.word("NOT");
            if not_null {
                self.want_word("NULL")?;
            }
            if self.symbol(":=") || self.word("DEFAULT") {
                let actual = self.expression()?;
                self.compatible(ty, actual)?;
                if not_null && actual.is_none() {
                    return Err(self.error("a NOT NULL variable cannot be initialized with literal NULL"));
                }
            } else {
                if not_null {
                    return Err(self.error("a NOT NULL declaration requires an initializer"));
                }
                if constant {
                    return Err(self.error("a CONSTANT declaration requires an initializer"));
                }
                self.constant(Value::Null)?;
            }
            self.builder.emit(ty.check())?;
            if not_null {
                self.builder.emit(Op::OracleNotNull)?;
            }
            self.pending_declarations.remove(&name);
            let slot = self.add_local(name.clone(), ty, !constant)?;
            self.locals.get_mut(&name).unwrap().not_null = not_null;
            self.builder.emit(Op::StoreLocal(slot))?;
            self.want_symbol(";")?;
        }
        Ok(())
    }

    fn emit_parameters(&mut self, with_return: bool) -> Result<()> {
        let count = self.parameters.len() + usize::from(with_return);
        if count > crate::ir::MAX_STACK {
            return Err(self.error("OUT result exceeds the procedure stack bound"));
        }
        for &slot in &self.parameters {
            self.builder.emit(Op::LoadLocal(slot))?;
        }
        self.builder.emit(Op::Tuple(count as u16))?;
        Ok(())
    }
    fn emit_procedure_result(&mut self) -> Result<()> {
        if self.has_out {
            self.emit_parameters(false)
        } else {
            self.constant(Value::Null)
        }
    }
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at).map(|t| &t.token)
    }
    fn error(&self, message: &str) -> mpedb_types::Error {
        lex::error(
            self.source,
            self.tokens
                .get(self.at)
                .map_or(self.source.len(), |t| t.span.start),
            message,
        )
    }
    fn is_word(&self, name: &str) -> bool {
        matches!(self.peek(), Some(Token::Identifier { name: value, quoted: false }) if value == name)
    }
    fn word(&mut self, name: &str) -> bool {
        if self.is_word(name) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn want_word(&mut self, name: &str) -> Result<()> {
        if self.word(name) {
            Ok(())
        } else {
            Err(self.error(&format!("expected {name}")))
        }
    }
    fn symbol(&mut self, value: &str) -> bool {
        if matches!(self.peek(), Some(Token::Symbol(s)) if *s == value) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn want_symbol(&mut self, value: &str) -> Result<()> {
        if self.symbol(value) {
            Ok(())
        } else {
            Err(self.error(&format!("expected {value}")))
        }
    }
    fn identifier(&mut self) -> Result<String> {
        let Some(Token::Identifier { name, quoted }) = self.peek() else {
            return Err(self.error("expected identifier"));
        };
        if name.len() > 128 {
            return Err(self.error("identifier exceeds 128 bytes"));
        }
        if !quoted
            && matches!(
                name.as_str(),
                "CREATE"
                    | "FUNCTION"
                    | "PROCEDURE"
                    | "BEGIN"
                    | "END"
                    | "DECLARE"
                    | "IF"
                    | "THEN"
                    | "ELSE"
                    | "ELSIF"
                    | "LOOP"
                    | "WHILE"
                    | "FOR"
                    | "FORALL"
                    | "IN"
                    | "REVERSE"
                    | "RETURN"
                    | "NULL"
                    | "TRUE"
                    | "FALSE"
                    | "AND"
                    | "OR"
                    | "NOT"
                    | "IS"
                    | "AS"
                    | "EXCEPTION"
                    | "SELECT"
                    | "INSERT"
                    | "UPDATE"
                    | "DELETE"
            )
        {
            return Err(self.error(&format!("reserved word or unimplemented construct {name}")));
        }
        let name = name.clone();
        self.at += 1;
        Ok(name)
    }
    fn data_type(&mut self, local: bool) -> Result<Type> {
        if self.word("BOOLEAN") {
            Ok(Type::Boolean)
        } else if self.word("NUMBER") {
            self.number_type(local)
        } else if self.word("INTEGER") || self.word("INT") || self.word("SMALLINT") {
            if matches!(self.peek(), Some(Token::Symbol("("))) {
                return Err(self.error("INTEGER/INT/SMALLINT do not take explicit precision or scale"));
            }
            Ok(if local { Type::ConstrainedNumber { precision: 38, scale: 0 } } else { Type::Number })
        } else if self.word("DEC") || self.word("DECIMAL") || self.word("NUMERIC") {
            let explicit = matches!(self.peek(), Some(Token::Symbol("(")));
            let ty = self.number_type(local)?;
            Ok(if local && !explicit { Type::ConstrainedNumber { precision: 38, scale: 0 } } else { ty })
        } else if self.word("PLS_INTEGER") || self.word("BINARY_INTEGER") {
            Ok(Type::PlsInteger)
        } else if self.word("VARCHAR2") {
            let mut constraint = 0;
            if self.symbol("(") {
                if !local {
                    return Err(self.error(
                        "formal parameters and return types cannot specify a VARCHAR2 length",
                    ));
                }
                let Some(Token::Number(text)) = self.peek() else {
                    return Err(self.error("VARCHAR2 length must be an integer from 1 to 32767"));
                };
                constraint = text
                    .parse::<u16>()
                    .ok()
                    .filter(|n| (1..=32767).contains(n))
                    .ok_or_else(|| {
                        self.error("VARCHAR2 length must be an integer from 1 to 32767")
                    })?;
                self.at += 1;
                if self.word("CHAR") {
                    constraint |= 0x8000;
                } else {
                    self.word("BYTE");
                }
                self.want_symbol(")")?;
            }
            Ok(Type::Varchar(constraint))
        } else {
            Err(self.error("this PL/SQL compiler currently supports BOOLEAN, NUMBER and its integer/decimal aliases, PLS_INTEGER/BINARY_INTEGER and VARCHAR2; other types require typed runtime support"))
        }
    }
    fn add_local(&mut self, name: String, ty: Type, writable: bool) -> Result<u16> {
        if !self.declared_here.insert(name.clone()) {
            return Err(self.error("duplicate parameter or variable name"));
        }
        // A scalar hides an outer record of the same name for this scope.
        // The caller's saved map puts the record back when the scope ends.
        self.records.remove(&name);
        self.nested_vars.remove(&name);
        let slot = self.builder.alloc_local()?;
        self.locals.insert(name, Local { slot, ty, writable, not_null: false });
        Ok(slot)
    }
    /// `name cursor%ROWTYPE`. Every field is stored as NULL. `:= NULL` and
    /// `DEFAULT NULL` are the same state. CONSTANT, NOT NULL, and any other
    /// initializer stay refused.
    fn declare_rowtype(&mut self, name: &str, constant: bool) -> Result<bool> {
        if !self.looks_like_rowtype() {
            return Ok(false);
        }
        let cursor_name = self.identifier()?;
        self.want_symbol("%")?;
        self.want_word("ROWTYPE")?;
        if constant {
            return Err(self.error("a %ROWTYPE record cannot be CONSTANT"));
        }
        if self.word("NOT") {
            return Err(self.error("a %ROWTYPE record cannot be NOT NULL"));
        }
        if (self.symbol(":=") || self.word("DEFAULT")) && !self.word("NULL") {
            return Err(self.error(
                "a %ROWTYPE record can only be initialized with NULL",
            ));
        }
        self.want_symbol(";")?;
        let cursor = self
            .cursors
            .get(&cursor_name)
            .cloned()
            .ok_or_else(|| self.error("%ROWTYPE requires an explicit cursor in this scope"))?;
        let columns = cursor.named_columns().map(|names| names.to_vec()).ok_or_else(|| {
            self.error("cursor SELECT item needs an alias")
        })?;
        let mut seen = HashSet::new();
        for column in &columns {
            if !seen.insert(column.clone()) {
                return Err(self.error("duplicate %ROWTYPE field"));
            }
        }
        if !self.declared_here.insert(name.to_string()) {
            return Err(self.error("duplicate parameter or variable name"));
        }
        self.pending_declarations.remove(name);
        self.locals.remove(name);
        let mut fields = Vec::with_capacity(columns.len());
        for column in columns {
            self.constant(Value::Null)?;
            let slot = self.builder.alloc_local()?;
            self.builder.emit(Op::StoreLocal(slot))?;
            fields.push((column, slot));
        }
        self.records.insert(
            name.to_string(),
            RecordDecl {
                fields,
                type_name: None,
                nested: Vec::new(),
            },
        );
        Ok(true)
    }
    fn looks_like_rowtype(&self) -> bool {
        let Some(Token::Identifier { .. }) = self.peek() else {
            return false;
        };
        let percent = matches!(
            self.tokens.get(self.at + 1).map(|token| &token.token),
            Some(Token::Symbol("%"))
        );
        let rowtype = matches!(
            self.tokens.get(self.at + 2).map(|token| &token.token),
            Some(Token::Identifier { name, quoted: false }) if name == "ROWTYPE"
        );
        percent && rowtype
    }
    fn assign_record(&mut self, record: RecordDecl) -> Result<()> {
        if self.symbol(".") {
            let field = self.identifier()?;
            if let Some((_, child)) = record.nested.iter().find(|(name, _)| name == &field) {
                return self.assign_record(child.clone());
            }
            let slot = self.record_field(&record, &field)?;
            self.want_symbol(":=")?;
            let actual = self.expression()?;
            self.store_record_field(slot, actual)?;
            return self.want_symbol(";");
        }
        self.want_symbol(":=")?;
        if self.word("NULL") {
            self.refuse_null_record(&record)?;
            self.store_null_leaves(&record)?;
            return self.want_symbol(";");
        }
        if let Some(Token::Identifier { name, .. }) = self.peek() {
            let source_name = name.clone();
            let whole = matches!(
                self.tokens.get(self.at + 1).map(|token| &token.token),
                Some(Token::Symbol(";"))
            );
            if whole {
                if let Some(source) = self.records.get(&source_name).cloned() {
                    self.at += 1;
                    self.assign_whole_record(&record, &source)?;
                    return self.want_symbol(";");
                }
            }
        }
        Err(self.error(if self.user_record(&record) {
            "a record can only be assigned NULL or one field"
        } else {
            "a %ROWTYPE record requires a field"
        }))
    }
    fn record_expr(&mut self, record: RecordDecl) -> Result<Option<Type>> {
        if self.symbol(".") {
            let field = self.identifier()?;
            if let Some((_, child)) = record.nested.iter().find(|(name, _)| name == &field) {
                return self.record_expr(child.clone());
            }
            let slot = self.record_field(&record, &field)?;
            self.builder.emit(Op::LoadLocal(slot))?;
            if let Some(info) = self.record_field_info.get(&slot).copied() {
                return Ok(Some(info.ty));
            }
            // `%ROWTYPE` fields stay untyped. NULL stays NULL through the
            // caller's check.
            return Ok(None);
        }
        if self.word("IS") {
            let negated = self.word("NOT");
            self.want_word("NULL")?;
            // Oracle: the record is NULL exactly when every leaf is NULL.
            // A nested record is NULL when every field inside it is NULL.
            // `Op::Eq` treats NULL as equal to NULL, so this is not SQL's
            // three-valued comparison.
            self.emit_record_null_test(&record)?;
            if negated {
                self.builder.emit(Op::SqlNot)?;
            }
            return Ok(Some(Type::Boolean));
        }
        Err(self.error("a %ROWTYPE record requires a field"))
    }
    fn user_record(&self, record: &RecordDecl) -> bool {
        record.type_name.is_some()
            || record
                .fields
                .iter()
                .any(|(_, slot)| self.record_field_info.contains_key(slot))
    }
    fn record_field(&self, record: &RecordDecl, field: &str) -> Result<u16> {
        record
            .fields
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, slot)| *slot)
            .ok_or_else(|| {
                self.error(&format!(
                    "unknown {} field {field}",
                    if self.user_record(record) {
                        "record"
                    } else {
                        "%ROWTYPE"
                    }
                ))
            })
    }
    fn emit_local_check(&mut self, local: Local) -> Result<()> {
        self.builder.emit(local.ty.check())?;
        if local.not_null {
            self.builder.emit(Op::OracleNotNull)?;
        }
        Ok(())
    }
    fn constant(&mut self, value: Value) -> Result<()> {
        let slot = self.builder.const_idx(value)?;
        self.builder.emit(Op::LoadConst(slot))?;
        Ok(())
    }
    fn compatible(&self, expected: Type, actual: Option<Type>) -> Result<()> {
        if actual.is_some_and(|actual| {
            actual != expected
                && !matches!(
                    (expected, actual),
                    (
                        Type::Varchar(_) | Type::CharLiteral,
                        Type::Varchar(_) | Type::CharLiteral
                    ) | (
                        Type::Number | Type::ConstrainedNumber { .. } | Type::PlsInteger,
                        Type::Number | Type::ConstrainedNumber { .. } | Type::PlsInteger
                    )
                )
        }) {
            Err(self.error("incompatible expression type"))
        } else {
            Ok(())
        }
    }
    fn condition(&mut self) -> Result<()> {
        let actual = self.expression()?;
        self.compatible(Type::Boolean, actual)?;
        self.builder.emit(Op::SqlBool)?;
        Ok(())
    }
    fn statements(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > 128 {
            return Err(self.error("control-flow nesting exceeds 128 levels"));
        }
        while !self.is_word("END") && !self.is_word("ELSE") && !self.is_word("ELSIF") && !self.is_word("EXCEPTION") && !self.is_word("WHEN") {
            if self.peek().is_none() {
                return Err(self.error("unterminated routine or control-flow block"));
            }
            self.statement()?;
        }
        self.depth -= 1;
        Ok(())
    }
    fn statement(&mut self) -> Result<()> {
        if self.is_word("SELECT") && self.looks_like_bulk_collect() {
            return self.bulk_collect();
        }
        if self.is_word("SELECT") || self.is_word("INSERT") || self.is_word("UPDATE") || self.is_word("DELETE") {
            return self.static_sql();
        }
        if self.is_word("RAISE_APPLICATION_ERROR") && !self.locals.contains_key("RAISE_APPLICATION_ERROR") { self.at += 1; return self.raise_application(); }
        if self.word("RAISE") { return self.raise_statement(); }
        if self.word("EXECUTE") { return self.execute_immediate(); }
        let declarations = self.word("DECLARE");
        if declarations || self.is_word("BEGIN") {
            let outer_locals = self.locals.clone();
            let outer_exceptions = self.exceptions.clone();
            let outer_cursors = self.cursors.clone();
            let outer_records = self.records.clone();
            let outer_nested_types = self.nested_types.clone();
            let outer_nested_vars = self.nested_vars.clone();
            let outer_record_types = self.record_types.clone();
            let outer_record_fields = self.record_field_info.clone();
            let outer_declared = std::mem::take(&mut self.declared_here);
            let outer_pending = std::mem::take(&mut self.pending_declarations);
            if declarations {
                self.declarations()?;
            }
            self.want_word("BEGIN")?;
            self.block_body()?;
            self.want_word("END")?;
            self.want_symbol(";")?;
            self.locals = outer_locals;
            self.exceptions = outer_exceptions;
            self.cursors = outer_cursors;
            self.records = outer_records;
            self.nested_types = outer_nested_types;
            self.nested_vars = outer_nested_vars;
            self.record_types = outer_record_types;
            self.record_field_info = outer_record_fields;
            self.declared_here = outer_declared;
            self.pending_declarations = outer_pending;
            return Ok(());
        }
        if self.word("NULL") {
            return self.want_symbol(";");
        }
        if self.word("RETURN") {
            if let Some(ty) = self.returns {
                let actual = self.expression()?;
                self.compatible(ty, actual)?;
                self.builder.emit(ty.check())?;
                if self.has_out {
                    self.emit_parameters(true)?;
                }
            } else {
                self.emit_procedure_result()?;
            }
            self.want_symbol(";")?;
            self.builder.emit(Op::Return)?;
            return Ok(());
        }
        if self.word("IF") {
            return self.if_statement();
        }
        if self.word("FORALL") {
            return self.forall_statement();
        }
        if self.word("FOR") {
            return self.for_statement();
        }
        if self.word("WHILE") {
            let top = self.builder.here();
            self.condition()?;
            let out = self.builder.emit_jump(Op::JumpIfFalse(u32::MAX))?;
            self.want_word("LOOP")?;
            self.builder.push_loop(top);
            self.statements()?;
            self.want_word("END")?;
            self.want_word("LOOP")?;
            self.want_symbol(";")?;
            self.builder.emit(Op::Jump(top))?;
            self.builder.patch_to_here(out);
            self.builder.pop_loop();
            return Ok(());
        }
        if self.word("LOOP") {
            let top = self.builder.here();
            self.builder.push_loop(top);
            self.statements()?;
            self.want_word("END")?;
            self.want_word("LOOP")?;
            self.want_symbol(";")?;
            self.builder.emit(Op::Jump(top))?;
            self.builder.pop_loop();
            return Ok(());
        }
        let exit = self.word("EXIT");
        if exit || self.word("CONTINUE") {
            let skip = if self.word("WHEN") {
                self.condition()?;
                Some(self.builder.emit_jump(Op::JumpIfFalse(u32::MAX))?)
            } else {
                None
            };
            let valid = if exit {
                self.builder.emit_break()?
            } else {
                self.builder.emit_continue()?
            };
            if !valid {
                return Err(self.error("EXIT/CONTINUE outside a loop"));
            }
            if let Some(skip) = skip {
                self.builder.patch_to_here(skip);
            }
            return self.want_symbol(";");
        }
        if self.cursor_statement()? {
            return Ok(());
        }
        self.refuse_correlation_statement()?;
        let name = self.identifier()?;
        if self.cursors.contains_key(&name) {
            return Err(self.error("cannot assign to a cursor"));
        }
        if !self.locals.contains_key(&name) {
            if let Some(record) = self.records.get(&name).cloned() {
                return self.assign_record(record);
            }
            if let Some(nested) = self.nested_vars.get(&name).copied() {
                return self.assign_nested(nested);
            }
            if let Some(package) = self.package_vars.get(&name).cloned() {
                return self.assign_package(package);
            }
        }
        let local = *self.locals.get(&name).ok_or_else(|| {
            self.error(&format!(
                "unknown variable or unimplemented statement {name}"
            ))
        })?;
        if !local.writable {
            return Err(self.error("cannot assign to an IN parameter, CONSTANT or FOR index"));
        }
        self.want_symbol(":=")?;
        let actual = self.expression()?;
        self.compatible(local.ty, actual)?;
        if local.not_null && actual.is_none() {
            return Err(self.error("a NOT NULL variable cannot be assigned literal NULL"));
        }
        self.emit_local_check(local)?;
        self.builder.emit(Op::StoreLocal(local.slot))?;
        self.want_symbol(";")
    }
    fn for_statement(&mut self) -> Result<()> {
        let name = self.identifier()?;
        self.want_word("IN")?;
        let reverse = self.word("REVERSE");
        let outer_locals = self.locals.clone();
        let outer_records = self.records.clone();
        let outer_nested_types = self.nested_types.clone();
        let outer_nested_vars = self.nested_vars.clone();
        let outer_record_types = self.record_types.clone();
        let outer_record_fields = self.record_field_info.clone();
        let outer_declared = std::mem::take(&mut self.declared_here);
        let outer_pending = std::mem::take(&mut self.pending_declarations);
        // Oracle's index hides outer names even while the bounds are being
        // evaluated; referring to the index in a bound sees its initial NULL.
        let index = self.add_local(name, Type::PlsInteger, false)?;
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
        self.want_word("LOOP")?;
        let limit = if reverse { lower } else { upper };
        self.builder
            .emit(Op::LoadLocal(if reverse { upper } else { lower }))?;
        self.builder.emit(Op::StoreLocal(index))?;
        let first = self.builder.emit_jump(Op::Jump(u32::MAX))?;
        let advance = self.builder.here();
        self.builder.emit(Op::LoadLocal(index))?;
        self.builder.emit(Op::LoadLocal(limit))?;
        self.builder.emit(Op::SqlEq)?;
        let finished = self.builder.emit_jump(Op::JumpIfTrue(u32::MAX))?;
        self.builder.emit(Op::LoadLocal(index))?;
        self.constant(Value::Int(1))?;
        self.builder.emit(if reverse { Op::Sub } else { Op::Add })?;
        self.builder.emit(Op::StoreLocal(index))?;
        self.builder.patch_to_here(first);
        self.builder.emit(Op::LoadLocal(index))?;
        self.builder.emit(Op::LoadLocal(limit))?;
        self.builder
            .emit(if reverse { Op::SqlGe } else { Op::SqlLe })?;
        let out = self.builder.emit_jump(Op::JumpIfFalse(u32::MAX))?;
        self.builder.push_loop(advance);
        self.statements()?;
        self.want_word("END")?;
        self.want_word("LOOP")?;
        self.want_symbol(";")?;
        self.builder.emit(Op::Jump(advance))?;
        self.builder.patch_to_here(out);
        self.builder.patch_to_here(finished);
        self.builder.pop_loop();
        self.locals = outer_locals;
        self.records = outer_records;
        self.nested_types = outer_nested_types;
        self.nested_vars = outer_nested_vars;
        self.record_types = outer_record_types;
        self.record_field_info = outer_record_fields;
        self.declared_here = outer_declared;
        self.pending_declarations = outer_pending;
        Ok(())
    }

    fn if_statement(&mut self) -> Result<()> {
        let mut ends = Vec::new();
        loop {
            self.condition()?;
            self.want_word("THEN")?;
            let next = self.builder.emit_jump(Op::JumpIfFalse(u32::MAX))?;
            self.statements()?;
            ends.push(self.builder.emit_jump(Op::Jump(u32::MAX))?);
            self.builder.patch_to_here(next);
            if !self.word("ELSIF") {
                break;
            }
        }
        if self.word("ELSE") {
            self.statements()?;
        }
        self.want_word("END")?;
        self.want_word("IF")?;
        self.want_symbol(";")?;
        for end in ends {
            self.builder.patch_to_here(end);
        }
        Ok(())
    }
    fn expression(&mut self) -> Result<Option<Type>> {
        self.depth += 1;
        if self.depth > 128 {
            return Err(self.error("expression nesting exceeds 128 levels"));
        }
        let mut ty = self.and_expression()?;
        while self.word("OR") {
            self.compatible(Type::Boolean, ty)?;
            self.builder.emit(Op::Dup)?;
            let skip = self.builder.emit_jump(Op::JumpIfSqlTrue(u32::MAX))?;
            let right = self.and_expression()?;
            self.compatible(Type::Boolean, right)?;
            self.builder.emit(Op::SqlOr)?;
            self.builder.patch_to_here(skip);
            ty = Some(Type::Boolean);
        }
        self.depth -= 1;
        Ok(ty)
    }
    fn and_expression(&mut self) -> Result<Option<Type>> {
        let mut ty = self.not_expression()?;
        while self.word("AND") {
            self.compatible(Type::Boolean, ty)?;
            self.builder.emit(Op::Dup)?;
            let skip = self.builder.emit_jump(Op::JumpIfSqlFalse(u32::MAX))?;
            let right = self.not_expression()?;
            self.compatible(Type::Boolean, right)?;
            self.builder.emit(Op::SqlAnd)?;
            self.builder.patch_to_here(skip);
            ty = Some(Type::Boolean);
        }
        Ok(ty)
    }
    fn not_expression(&mut self) -> Result<Option<Type>> {
        let mut count = 0;
        while self.word("NOT") {
            count += 1;
        }
        let ty = self.comparison()?;
        if count > 0 {
            self.compatible(Type::Boolean, ty)?;
        }
        for _ in 0..count {
            self.builder.emit(Op::SqlNot)?;
        }
        Ok(if count > 0 { Some(Type::Boolean) } else { ty })
    }
    fn comparison(&mut self) -> Result<Option<Type>> {
        let ty = self.additive()?;
        if self.word("IS") {
            let not = self.word("NOT");
            self.want_word("NULL")?;
            self.constant(Value::Null)?;
            self.builder.emit(if not { Op::Ne } else { Op::Eq })?;
            Ok(Some(Type::Boolean))
        } else {
            let op = match self.peek() {
                Some(Token::Symbol("=")) => Some(Op::SqlEq),
                Some(Token::Symbol("<>" | "!=" | "~=" | "^=")) => Some(Op::SqlNe),
                Some(Token::Symbol("<")) => Some(Op::SqlLt),
                Some(Token::Symbol("<=")) => Some(Op::SqlLe),
                Some(Token::Symbol(">")) => Some(Op::SqlGt),
                Some(Token::Symbol(">=")) => Some(Op::SqlGe),
                _ => None,
            };
            if let Some(op) = op {
                self.at += 1;
                if ty == Some(Type::PlsInteger) {
                    self.builder.emit(Op::OracleNumber)?;
                }
                let right = self.additive()?;
                if let Some(ty) = ty {
                    self.compatible(ty, right)?;
                }
                if right == Some(Type::PlsInteger) {
                    self.builder.emit(Op::OracleNumber)?;
                }
                let op = if ty == Some(Type::CharLiteral) && right == Some(Type::CharLiteral) {
                    match op {
                        Op::SqlEq => Op::OracleCharEq,
                        Op::SqlNe => Op::OracleCharNe,
                        Op::SqlLt => Op::OracleCharLt,
                        Op::SqlLe => Op::OracleCharLe,
                        Op::SqlGt => Op::OracleCharGt,
                        Op::SqlGe => Op::OracleCharGe,
                        _ => unreachable!("comparison operator"),
                    }
                } else {
                    op
                };
                self.builder.emit(op)?;
                Ok(Some(Type::Boolean))
            } else {
                Ok(ty)
            }
        }
    }
    fn additive(&mut self) -> Result<Option<Type>> {
        let mut ty = self.multiplicative()?;
        loop {
            let op = if self.symbol("+") {
                Op::OraclePlSqlAdd
            } else if self.symbol("-") {
                Op::OraclePlSqlSub
            } else if self.symbol("||") {
                Op::OracleConcat
            } else {
                break;
            };
            let expected = if op == Op::OracleConcat {
                Type::Varchar(0)
            } else {
                Type::Number
            };
            self.compatible(expected, ty)?;
            let right = self.multiplicative()?;
            self.compatible(expected, right)?;
            let integer = op != Op::OracleConcat
                && matches!(ty, None | Some(Type::PlsInteger))
                && matches!(right, None | Some(Type::PlsInteger));
            let op = if integer {
                if op == Op::OraclePlSqlAdd {
                    Op::OracleIntAdd
                } else {
                    Op::OracleIntSub
                }
            } else {
                op
            };
            self.builder.emit(op)?;
            ty = Some(if integer { Type::PlsInteger } else { expected });
        }
        Ok(ty)
    }
    fn multiplicative(&mut self) -> Result<Option<Type>> {
        let mut ty = self.unary()?;
        loop {
            let op = if self.symbol("*") {
                Op::OraclePlSqlMul
            } else if self.symbol("/") {
                Op::OraclePlSqlDivide
            } else {
                break;
            };
            self.compatible(Type::Number, ty)?;
            let right = self.unary()?;
            self.compatible(Type::Number, right)?;
            let integer = op == Op::OraclePlSqlMul
                && matches!(ty, None | Some(Type::PlsInteger))
                && matches!(right, None | Some(Type::PlsInteger));
            self.builder
                .emit(if integer { Op::OracleIntMul } else { op })?;
            ty = Some(if integer {
                Type::PlsInteger
            } else {
                Type::Number
            });
        }
        Ok(ty)
    }
    fn unary(&mut self) -> Result<Option<Type>> {
        let mut negations = 0;
        let mut signed = false;
        loop {
            if self.symbol("-") {
                negations += 1;
                signed = true;
            } else if self.symbol("+") {
                signed = true;
            } else {
                break;
            }
        }
        let ty = self.atom()?;
        if signed {
            self.compatible(Type::Number, ty)?;
        }
        for _ in 0..negations {
            self.builder.emit(if ty == Some(Type::PlsInteger) {
                Op::OracleIntNeg
            } else {
                Op::OraclePlSqlNeg
            })?;
        }
        Ok(if signed && ty.is_none() {
            Some(Type::Number)
        } else {
            ty
        })
    }
    fn atom(&mut self) -> Result<Option<Type>> {
        for (name, op, ty) in [("SQLCODE", Op::OracleSqlCode, Type::Number), ("SQLERRM", Op::OracleSqlErrm, Type::Varchar(0))] {
            if self.is_word(name) && !self.locals.contains_key(name) {
                self.at += 1;
                if self.symbol("(") && !self.symbol(")") {
                    if name == "SQLCODE" { return Err(self.error("SQLCODE does not take arguments")); }
                    let actual = self.expression()?;
                    self.compatible(Type::Number, actual)?;
                    self.want_symbol(")")?;
                    self.builder.emit(Op::OracleSqlErrmCode)?;
                    return Ok(Some(ty));
                }
                self.builder.emit(op)?;
                return Ok(Some(ty));
            }
        }
        if self.word("NULL") {
            self.constant(Value::Null)?;
            return Ok(None);
        }
        if self.word("TRUE") {
            self.constant(Value::Bool(true))?;
            return Ok(Some(Type::Boolean));
        }
        if self.word("FALSE") {
            self.constant(Value::Bool(false))?;
            return Ok(Some(Type::Boolean));
        }
        if self.symbol("(") {
            let ty = self.expression()?;
            self.want_symbol(")")?;
            return Ok(ty);
        }
        if let Some(Token::String { value, national }) = self.peek() {
            if *national {
                return Err(
                    self.error("national character strings require NVARCHAR2 runtime support")
                );
            }
            let value = value.clone();
            self.at += 1;
            self.constant(Value::Text(value))?;
            self.builder.emit(Op::OracleVarchar(0))?;
            return Ok(Some(Type::CharLiteral));
        }
        if let Some(Token::Number(text)) = self.peek() {
            let value = mpedb_types::value::parse_numeric(text)
                .ok_or_else(|| self.error("unsupported NUMBER literal"))?;
            let integer = text
                .parse::<i32>()
                .ok()
                .filter(|_| text.bytes().all(|c| c.is_ascii_digit()));
            crate::interp::validate_oracle_number_literal(&value)
                .map_err(|_| self.error("NUMBER literal is outside Oracle's finite range"))?;
            self.at += 1;
            if let Some(integer) = integer {
                self.constant(Value::Int(integer as i64))?;
                return Ok(Some(Type::PlsInteger));
            }
            self.constant(Value::Numeric(value))?;
            self.builder.emit(Op::OracleNumber)?;
            return Ok(Some(Type::Number));
        }
        if self.trigger_images.is_some() && matches!(self.peek(), Some(Token::Bind(_))) {
            return self.correlation_expr();
        }
        let name = self.identifier()?;
        if self.symbol("%") {
            if self.is_word("TYPE") {
                return Err(self.error("%TYPE is a type, not a value"));
            }
            return self.cursor_attribute(&name);
        }
        if self.forall_subscripts == 0 && self.forall_index.as_deref() == Some(name.as_str()) {
            return Err(self.error("a FORALL index can only subscript a collection"));
        }
        if self.pending_declarations.contains(&name) {
            return Err(self.error("self or forward reference in a declaration initializer"));
        }
        if let Some(local) = self.locals.get(&name).copied() {
            self.builder.emit(Op::LoadLocal(local.slot))?;
            return Ok(Some(local.ty));
        }
        if let Some(record) = self.records.get(&name).cloned() {
            return self.record_expr(record);
        }
        if let Some(nested) = self.nested_vars.get(&name).copied() {
            return self.nested_expr(nested);
        }
        if let Some(package) = self.package_vars.get(&name).cloned() {
            return self.package_expr(package);
        }
        Err(self.error(&format!(
            "unknown variable or unsupported expression {name}"
        )))
    }
}
