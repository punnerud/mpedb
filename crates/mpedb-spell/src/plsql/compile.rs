//! Native Oracle routine compilation; no translation into Python or PL/pgSQL.

mod sql;
mod exception;
mod number;
mod static_sql;

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
}

pub fn compile(source: &str) -> Result<Skeleton> {
    compile_on_stack(source, false)
}

/// Compile only a scalar SQL function; procedures and OUT signatures belong
/// to the procedure calling convention and must never enter a scalar catalog.
pub fn compile_function(source: &str) -> Result<Skeleton> {
    compile_on_stack(source, true)
}

fn compile_on_stack(source: &str, scalar_only: bool) -> Result<Skeleton> {
    // Host-side parsing must not depend on the embedding application's worker
    // stack size. Keep the documented nesting bound while giving the recursive
    // parser a known stack; execution still uses the bounded, validated IR.
    std::thread::scope(|scope| {
        let worker = std::thread::Builder::new()
            .name("mpedb-plsql-compile".into())
            .stack_size(8 * 1024 * 1024)
            .spawn_scoped(scope, || compile_inner(source, scalar_only))
            .map_err(|_| mpedb_types::Error::OutOfMemory {
                what: "PL/SQL compiler worker",
            })?;
        worker.join().map_err(|_| {
            mpedb_types::Error::Unsupported("PL/SQL compiler worker panicked".into())
        })?
    })
}

fn compile_inner(source: &str, scalar_only: bool) -> Result<Skeleton> {
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
    };
    p.want_word("CREATE")?;
    if p.word("OR") {
        p.want_word("REPLACE")?;
    }
    let function = if p.word("FUNCTION") {
        true
    } else {
        p.want_word("PROCEDURE")?;
        false
    };
    if scalar_only && !function {
        return Err(p.error("a stored SQL function requires CREATE FUNCTION, not CREATE PROCEDURE"));
    }
    let name = p.identifier()?;
    p.routine_name = name.clone();
    if p.symbol(".") {
        return Err(p.error("schema-qualified routine registration is not implemented"));
    }
    let mut argc = 0;
    if p.symbol("(") {
        if matches!(p.peek(), Some(Token::Symbol(")"))) {
            return Err(p.error("Oracle parameter lists cannot be empty; omit parentheses for a no-argument routine"));
        }
        loop {
            let name = p.identifier()?;
            let input = p.word("IN");
            let output = p.word("OUT");
            let out_only = output && !input;
            if p.word("NOCOPY") && !output {
                return Err(p.error("NOCOPY is only valid for OUT or IN OUT parameters"));
            }
            // NOCOPY is an Oracle hint, not a guarantee. This runtime always
            // uses value/copy-out semantics, which Oracle explicitly permits.
            let ty = p.data_type(false)?;
            let slot = p.add_local(name, ty, output)?;
            if out_only {
                p.constant(Value::Null)?;
            } else {
                p.builder.emit(Op::LoadLocal(slot))?;
                p.builder.emit(ty.check())?;
            }
            p.builder.emit(Op::StoreLocal(slot))?;
            p.parameters.push(slot);
            p.has_out |= output;
            argc += 1;
            if !p.symbol(",") {
                p.want_symbol(")")?;
                break;
            }
        }
    }
    if scalar_only && p.has_out {
        return Err(
            p.error("a scalar SQL function cannot have OUT or IN OUT parameters; use ProcEngine")
        );
    }
    if function {
        p.want_word("RETURN")?;
        p.returns = Some(p.data_type(false)?);
    }
    if !(p.word("IS") || p.word("AS")) {
        return Err(p.error("expected IS or AS before routine body"));
    }
    p.declarations()?;
    p.routine_locals = p.locals.clone();
    p.want_word("BEGIN")?;
    p.block_body()?;
    p.want_word("END")?;
    if matches!(p.peek(), Some(Token::Identifier { .. })) {
        let end_name = p.identifier()?;
        if end_name != name {
            return Err(p.error("END name does not match routine name"));
        }
    }
    p.want_symbol(";")?;
    if p.symbol("/") {
        let span = &p.tokens[p.at - 1].span;
        let before = source[..span.start].rsplit('\n').next().unwrap_or("");
        let after = source[span.end..].split('\n').next().unwrap_or("");
        if !before.trim().is_empty() || !after.trim().is_empty() {
            return Err(p.error("SQL*Plus slash terminator must occupy its own line"));
        }
    }
    if p.at != p.tokens.len() {
        return Err(p.error("trailing input after routine"));
    }
    if function {
        p.builder.emit(Op::MissingReturn)?;
    } else {
        p.emit_procedure_result()?;
        p.builder.emit(Op::Return)?;
    }
    Ok(Skeleton {
        name,
        argc,
        nlocals: p.builder.nlocals(),
        consts: p.builder.consts,
        instrs: p.builder.instrs,
        calls: p.builder.calls,
    })
}

impl Parser<'_> {
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
            let position = self.tokens.get(self.at).map_or(self.source.len(), |t| t.span.start);
            let name = self.identifier()?;
            if self.word("EXCEPTION") {
                self.declare_exception(name, position)?;
                self.want_symbol(";")?;
                continue;
            }
            let constant = self.word("CONSTANT");
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
        let slot = self.builder.alloc_local()?;
        self.locals.insert(name, Local { slot, ty, writable, not_null: false });
        Ok(slot)
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
        let name = self.identifier()?;
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
                    return Err(self.error("explicit SQLERRM error-code arguments are not implemented"));
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
        let name = self.identifier()?;
        if self.pending_declarations.contains(&name) {
            return Err(self.error("self or forward reference in a declaration initializer"));
        }
        let local = *self.locals.get(&name).ok_or_else(|| {
            self.error(&format!(
                "unknown variable or unsupported expression {name}"
            ))
        })?;
        self.builder.emit(Op::LoadLocal(local.slot))?;
        Ok(Some(local.ty))
    }
}
