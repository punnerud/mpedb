//! Native Oracle routine compilation; no translation into Python or PL/pgSQL.

use super::lex::{self, Lexed, Token};
use crate::emit::{FuncBuilder, Skeleton};
use crate::ir::Op;
use mpedb_types::{Result, Value};
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Type {
    Boolean,
    Number,
}

impl Type {
    fn check(self) -> Op {
        match self {
            Self::Boolean => Op::SqlBool,
            Self::Number => Op::OracleNumber,
        }
    }
}

#[derive(Clone, Copy)]
struct Local {
    slot: u16,
    ty: Type,
    writable: bool,
}

struct Parser<'a> {
    source: &'a str,
    tokens: Vec<Lexed>,
    at: usize,
    builder: FuncBuilder,
    locals: HashMap<String, Local>,
    returns: Option<Type>,
    depth: usize,
    parameters: Vec<u16>,
    has_out: bool,
}

pub fn compile(source: &str) -> Result<Skeleton> {
    compile_inner(source, false)
}

/// Compile only a scalar SQL function; procedures and OUT signatures belong
/// to the procedure calling convention and must never enter a scalar catalog.
pub fn compile_function(source: &str) -> Result<Skeleton> {
    compile_inner(source, true)
}

fn compile_inner(source: &str, scalar_only: bool) -> Result<Skeleton> {
    let mut p = Parser {
        source,
        tokens: lex::lex(source)?,
        at: 0,
        builder: FuncBuilder::new(),
        locals: HashMap::new(),
        returns: None,
        depth: 0,
        parameters: Vec::new(),
        has_out: false,
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
            let ty = p.data_type()?;
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
        p.returns = Some(p.data_type()?);
    }
    if !(p.word("IS") || p.word("AS")) {
        return Err(p.error("expected IS or AS before routine body"));
    }
    while !p.is_word("BEGIN") {
        let name = p.identifier()?;
        let ty = p.data_type()?;
        if p.word("CONSTANT") || p.word("NOT") {
            return Err(p.error("CONSTANT/NOT NULL variable constraints are not implemented"));
        }
        if p.symbol(":=") || p.word("DEFAULT") {
            let actual = p.expression()?;
            p.compatible(ty, actual)?;
        } else {
            p.constant(Value::Null)?;
        }
        p.builder.emit(ty.check())?;
        let slot = p.add_local(name, ty, true)?;
        p.builder.emit(Op::StoreLocal(slot))?;
        p.want_symbol(";")?;
    }
    p.want_word("BEGIN")?;
    p.statements()?;
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
    fn data_type(&mut self) -> Result<Type> {
        if self.word("BOOLEAN") {
            Ok(Type::Boolean)
        } else if self.word("NUMBER") {
            Ok(Type::Number)
        } else {
            Err(self.error("this PL/SQL compiler currently supports BOOLEAN and unconstrained NUMBER; other types require typed runtime support"))
        }
    }
    fn add_local(&mut self, name: String, ty: Type, writable: bool) -> Result<u16> {
        if self.locals.contains_key(&name) {
            return Err(self.error("duplicate parameter or variable name"));
        }
        let slot = self.builder.alloc_local()?;
        self.locals.insert(name, Local { slot, ty, writable });
        Ok(slot)
    }
    fn constant(&mut self, value: Value) -> Result<()> {
        let slot = self.builder.const_idx(value)?;
        self.builder.emit(Op::LoadConst(slot))?;
        Ok(())
    }
    fn compatible(&self, expected: Type, actual: Option<Type>) -> Result<()> {
        if actual.is_some_and(|actual| actual != expected) {
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
        while !self.is_word("END") && !self.is_word("ELSE") && !self.is_word("ELSIF") {
            if self.peek().is_none() {
                return Err(self.error("unterminated routine or control-flow block"));
            }
            self.statement()?;
        }
        self.depth -= 1;
        Ok(())
    }
    fn statement(&mut self) -> Result<()> {
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
            return Err(self.error("cannot assign to an IN parameter"));
        }
        self.want_symbol(":=")?;
        let actual = self.expression()?;
        self.compatible(local.ty, actual)?;
        self.builder.emit(local.ty.check())?;
        self.builder.emit(Op::StoreLocal(local.slot))?;
        self.want_symbol(";")
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
                let right = self.additive()?;
                if let Some(ty) = ty {
                    self.compatible(ty, right)?;
                }
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
                Op::OracleAdd
            } else if self.symbol("-") {
                Op::OracleSub
            } else {
                break;
            };
            self.compatible(Type::Number, ty)?;
            let right = self.multiplicative()?;
            self.compatible(Type::Number, right)?;
            self.builder.emit(op)?;
            ty = Some(Type::Number);
        }
        Ok(ty)
    }
    fn multiplicative(&mut self) -> Result<Option<Type>> {
        let mut ty = self.unary()?;
        loop {
            let op = if self.symbol("*") {
                Op::OracleMul
            } else if self.symbol("/") {
                Op::OracleDiv
            } else {
                break;
            };
            self.compatible(Type::Number, ty)?;
            let right = self.unary()?;
            self.compatible(Type::Number, right)?;
            self.builder.emit(op)?;
            ty = Some(Type::Number);
        }
        Ok(ty)
    }
    fn unary(&mut self) -> Result<Option<Type>> {
        let mut negate = false;
        let mut signed = false;
        loop {
            if self.symbol("-") {
                negate = !negate;
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
        if negate {
            self.builder.emit(Op::OracleNeg)?;
        }
        Ok(if signed { Some(Type::Number) } else { ty })
    }
    fn atom(&mut self) -> Result<Option<Type>> {
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
        if let Some(Token::Number(text)) = self.peek() {
            let value = mpedb_types::value::parse_numeric(text)
                .ok_or_else(|| self.error("unsupported NUMBER literal"))?;
            self.at += 1;
            self.constant(Value::Numeric(value))?;
            self.builder.emit(Op::OracleNumber)?;
            return Ok(Some(Type::Number));
        }
        let name = self.identifier()?;
        let local = *self.locals.get(&name).ok_or_else(|| {
            self.error(&format!(
                "unknown variable or unsupported expression {name}"
            ))
        })?;
        self.builder.emit(Op::LoadLocal(local.slot))?;
        Ok(Some(local.ty))
    }
}
