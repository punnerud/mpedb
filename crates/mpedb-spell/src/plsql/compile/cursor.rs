//! Explicit PL/SQL cursors: `CURSOR c IS SELECT ...; OPEN; FETCH INTO; CLOSE`
//! and `%FOUND`, `%NOTFOUND`, `%ISOPEN`, `%ROWCOUNT`.
//! Cursor parameters are IN only. In the query they are written
//! `cursor.parameter` and become positional binds. A bare parameter is
//! refused: Oracle prefers a same-named column, and this compiler has no
//! schema that could tell the two apart. A `DEFAULT` or `:=` expression is
//! resolved where the cursor is declared and evaluated again at every OPEN,
//! including NULL. An omitted argument is legal only when that parameter and
//! every parameter after it has a default. Packages stay refused.
//! A fetch that finds no row leaves its targets unchanged and leaves the
//! cursor open. A NULL column assigns NULL. CLOSE releases the stream
//! without fetching the remaining rows. `SELECT *` is refused: the select
//! list must name its columns. `count(*)` is one column.
//! `%FOUND` and `%NOTFOUND` are NULL after OPEN and before FETCH. A cursor
//! that is not open makes `%ISOPEN` false and raises INVALID_CURSOR for
//! the other three. `%NOTFOUND` is the boolean not of `%FOUND`.
//! `name cursor%ROWTYPE` is a record. Every field starts NULL, the record
//! is NULL only when every field is NULL, and FETCH INTO it stores each
//! cell, including NULL. A missing row leaves the fields unchanged.
//! `%ROWTYPE` used as a value is a type error. A table `%ROWTYPE` stays
//! refused: this compiler has no row schema for it.
use super::*;
use crate::emit::{line_col, CallKind, SqlCall};

#[derive(Clone)]
pub(super) struct CursorDecl {
    slot: u16,
    call: u16,
    width: u16,
    /// Select-list names in order. `None` when any item has no simple name
    /// or an alias. The cursor still compiles; `%ROWTYPE` is what refuses it.
    columns: Option<Vec<String>>,
    /// Formal types in declaration order. Empty means `OPEN name;`.
    formals: Vec<Type>,
    /// Parallel to `formals`. `None` means OPEN must supply the argument.
    /// `Some` is replayed at each OPEN when that argument is omitted.
    defaults: Vec<Option<Vec<Op>>>,
    /// One entry per placeholder, in occurrence order, indexing `formals`.
    /// `SqlCall.argc` is this length, not the formal count.
    uses: Vec<u16>,
    /// Temps filled once per OPEN, then loaded in `uses` order.
    arg_slots: Vec<u16>,
}

struct Formal {
    name: String,
    ty: Type,
    /// Lifted out of the declaration stream so a later OPEN sees current values.
    default_ops: Option<Vec<Op>>,
    slot: u16,
}

impl CursorDecl {
    /// Column names when every select item has one and the count matches the
    /// width `select_width` already accepted.
    pub(super) fn named_columns(&self) -> Option<&[String]> {
        self.columns
            .as_deref()
            .filter(|names| names.len() == usize::from(self.width))
    }
}

impl Parser<'_> {
    pub(super) fn declare_cursor(&mut self) -> Result<()> {
        self.pending_declarations.remove("CURSOR");
        let name = self.identifier()?;
        if !self.declared_here.insert(name.clone()) {
            return Err(self.error("duplicate parameter or variable name"));
        }
        let formals = self.cursor_formals()?;
        self.want_word("IS")?;
        if !self.is_word("SELECT") {
            return Err(self.error("a cursor declaration requires SELECT"));
        }
        let start = self.at;
        let mut depth = 0i32;
        let mut end = start;
        loop {
            let token = self.tokens.get(end).ok_or_else(|| {
                self.error("unterminated cursor declaration")
            })?;
            match &token.token {
                Token::Symbol(";") if depth == 0 => break,
                Token::Symbol("(") => depth += 1,
                Token::Symbol(")") => {
                    depth -= 1;
                    if depth < 0 {
                        return Err(self.error("unmatched parenthesis in cursor SELECT"));
                    }
                }
                Token::Identifier { name, quoted: false }
                    if name == "INTO" && depth == 0 =>
                {
                    return Err(self.error("a cursor SELECT cannot contain INTO"));
                }
                _ => {}
            }
            end += 1;
        }
        let source_start = self.tokens[start].span.start;
        let width = match select_width(&self.tokens[start..end]) {
            Ok(width) => width,
            Err(WidthError::Star { at }) => {
                let saved = self.at;
                self.at = start + at;
                let err = self.error("cursor SELECT * is not expanded; name the columns");
                self.at = saved;
                return Err(err);
            }
            Err(WidthError::TooWide) => return Err(self.error("cursor SELECT list is too wide")),
            Err(WidthError::MissingFrom) => {
                return Err(self.error("a cursor SELECT requires FROM"))
            }
        };
        // Names are optional. `count(*)` and `1 * 2` stay legal cursors and
        // simply have no `%ROWTYPE` shape.
        let columns = select_field_names(&self.tokens[start..end])
            .filter(|names| names.len() == usize::from(width));
        self.at = end + 1;
        let (rewritten, uses) = self.bind_cursor_query(start, end, &name, &formals)?;
        let (sql, count) = super::sql::normalize_sql(&rewritten, CallKind::Query)
            .map_err(|e| self.error(&e.to_string()))?;
        if count != uses.len() {
            return Err(self.error("cursor parameter bind count mismatch"));
        }
        let argc = u8::try_from(count).map_err(|_| self.error("too many cursor parameter uses"))?;
        let (line, col) = line_col(self.source, source_start);
        let call = self.builder.add_call(SqlCall {
            sql,
            kind: CallKind::Rows,
            argc,
            expected_columns: Some(width),
            line,
            col,
        })?;
        self.constant(Value::Null)?;
        let slot = self.builder.alloc_local()?;
        self.builder.emit(Op::StoreLocal(slot))?;
        self.cursors.insert(
            name,
            CursorDecl {
                slot,
                call,
                width,
                columns,
                formals: formals.iter().map(|formal| formal.ty).collect(),
                defaults: formals.iter().map(|formal| formal.default_ops.clone()).collect(),
                uses,
                arg_slots: formals.iter().map(|formal| formal.slot).collect(),
            },
        );
        Ok(())
    }

    /// `(name [IN] type [{:= | DEFAULT} expr], ...)`. Empty parentheses are
    /// refused. The names stay out of the routine's locals: a cursor parameter
    /// is not a body variable. While the list is parsed, each finished
    /// parameter is visible to a later default, then removed again.
    fn cursor_formals(&mut self) -> Result<Vec<Formal>> {
        if !self.symbol("(") {
            return Ok(Vec::new());
        }
        if matches!(self.peek(), Some(Token::Symbol(")"))) {
            return Err(self.error("omit the parentheses for a cursor with no parameters"));
        }
        let mut formals: Vec<Formal> = Vec::new();
        // (name, locals entry it shadowed, whether a later declaration owns the name).
        let mut hidden: Vec<(String, Option<Local>, bool)> = Vec::new();
        loop {
            let name = self.identifier()?;
            if formals.iter().any(|formal| formal.name == name) {
                return Err(self.error("duplicate cursor parameter name"));
            }
            // IN is optional. OUT and IN OUT are not cursor modes.
            self.word("IN");
            if self.is_word("OUT") {
                return Err(self.error("cursor parameters are IN only"));
            }
            if self.word("NOCOPY") {
                return Err(self.error("NOCOPY is only valid for OUT or IN OUT parameters"));
            }
            let ty = self.data_type(false)?;
            if self.word("NOCOPY") {
                return Err(self.error("NOCOPY is only valid for OUT or IN OUT parameters"));
            }
            // The parameter's own name is not visible in its default. An
            // earlier parameter is. A variable declared later in this section
            // stays a forward reference.
            let default_ops = if self.symbol(":=") || self.word("DEFAULT") {
                Some(self.capture_default(ty)?)
            } else {
                None
            };
            let slot = self.builder.alloc_local()?;
            let previous = self.locals.insert(
                name.clone(),
                Local {
                    slot,
                    ty,
                    writable: false,
                    not_null: false,
                },
            );
            let was_pending = self.pending_declarations.remove(&name);
            hidden.push((name.clone(), previous, was_pending));
            formals.push(Formal {
                name,
                ty,
                default_ops,
                slot,
            });
            if !self.symbol(",") {
                self.want_symbol(")")?;
                break;
            }
        }
        for (name, previous, was_pending) in hidden.into_iter().rev() {
            match previous {
                Some(local) => {
                    self.locals.insert(name.clone(), local);
                }
                None => {
                    self.locals.remove(&name);
                }
            }
            if was_pending {
                self.pending_declarations.insert(name);
            }
        }
        Ok(formals)
    }

    /// Compile one default and lift its instructions out of the declaration.
    /// Jump targets become offsets so each OPEN can replay the expression.
    fn capture_default(&mut self, ty: Type) -> Result<Vec<Op>> {
        let start = self.builder.instrs.len();
        let actual = self.expression()?;
        self.compatible(ty, actual)?;
        self.builder.emit(ty.check())?;
        let mut ops = self.builder.instrs.split_off(start);
        let base = u32::try_from(start).map_err(|_| self.error("cursor parameter default is too large"))?;
        let end = base
            .checked_add(u32::try_from(ops.len()).map_err(|_| self.error("cursor parameter default is too large"))?)
            .ok_or_else(|| self.error("cursor parameter default is too large"))?;
        for op in &mut ops {
            let target = match op {
                Op::Jump(target)
                | Op::JumpIfFalse(target)
                | Op::JumpIfTrue(target)
                | Op::JumpIfSqlFalse(target)
                | Op::JumpIfSqlTrue(target) => target,
                _ => continue,
            };
            if *target < base || *target > end {
                return Err(self.error("cursor parameter default cannot leave its expression"));
            }
            *target -= base;
        }
        Ok(ops)
    }

    fn replay_default(&mut self, ops: &[Op]) -> Result<()> {
        let base = self.builder.here();
        for op in ops {
            let op = match *op {
                Op::Jump(target) => Op::Jump(
                    base.checked_add(target)
                        .ok_or_else(|| self.error("cursor parameter default is too large"))?,
                ),
                Op::JumpIfFalse(target) => Op::JumpIfFalse(
                    base.checked_add(target)
                        .ok_or_else(|| self.error("cursor parameter default is too large"))?,
                ),
                Op::JumpIfTrue(target) => Op::JumpIfTrue(
                    base.checked_add(target)
                        .ok_or_else(|| self.error("cursor parameter default is too large"))?,
                ),
                Op::JumpIfSqlFalse(target) => Op::JumpIfSqlFalse(
                    base.checked_add(target)
                        .ok_or_else(|| self.error("cursor parameter default is too large"))?,
                ),
                Op::JumpIfSqlTrue(target) => Op::JumpIfSqlTrue(
                    base.checked_add(target)
                        .ok_or_else(|| self.error("cursor parameter default is too large"))?,
                ),
                other => other,
            };
            self.builder.emit(op)?;
        }
        Ok(())
    }

    /// Rewrite `cursor.formal` to a bind marker, in occurrence order.
    /// `start..end` are the SELECT tokens in the routine, not a substring.
    fn bind_cursor_query(
        &self,
        start: usize,
        end: usize,
        cursor_name: &str,
        formals: &[Formal],
    ) -> Result<(String, Vec<u16>)> {
        let tokens = &self.tokens[start..end];
        let mut output = String::new();
        let mut uses = Vec::new();
        let mut index = 0;
        let mut copied = tokens
            .first()
            .map(|token| token.span.start)
            .unwrap_or(0);
        while let Some(token) = tokens.get(index) {
            output.push_str(&self.source[copied..token.span.start]);
            if let Token::Identifier { name, quoted: false } = &token.token {
                if name == cursor_name {
                    if !matches!(
                        tokens.get(index + 1).map(|token| &token.token),
                        Some(Token::Symbol("."))
                    ) {
                        return Err(self.located(
                            start + index,
                            "cursor name in the query requires .parameter",
                        ));
                    }
                    let Some(Token::Identifier {
                        name: formal,
                        quoted: false,
                    }) = tokens.get(index + 2).map(|token| &token.token)
                    else {
                        return Err(self.located(
                            start + index + 2,
                            "expected a cursor parameter after the cursor name",
                        ));
                    };
                    if matches!(
                        tokens.get(index + 3).map(|token| &token.token),
                        Some(Token::Symbol("."))
                    ) {
                        return Err(self.located(
                            start + index + 2,
                            "cursor parameter cannot have further qualification",
                        ));
                    }
                    let Some(pos) = formals.iter().position(|candidate| candidate.name == *formal)
                    else {
                        return Err(self.located(
                            start + index + 2,
                            &format!("unknown cursor parameter {formal}"),
                        ));
                    };
                    let pos = u16::try_from(pos).map_err(|_| {
                        self.located(start + index + 2, "too many cursor parameters")
                    })?;
                    uses.push(pos);
                    output.push_str(":mpedb_cursor_bind");
                    copied = tokens[index + 2].span.end;
                    index += 3;
                    continue;
                }
                let qualified = matches!(
                    index
                        .checked_sub(1)
                        .and_then(|i| tokens.get(i))
                        .map(|token| &token.token),
                    Some(Token::Symbol("."))
                ) || matches!(
                    tokens.get(index + 1).map(|token| &token.token),
                    Some(Token::Symbol("."))
                );
                if !qualified && formals.iter().any(|formal| formal.name == *name) {
                    return Err(self.located(
                        start + index,
                        "bare cursor parameter in the query requires qualification with the cursor name",
                    ));
                }
            }
            if matches!(token.token, Token::Bind(_)) {
                return Err(self.located(
                    start + index,
                    "cursor query takes cursor-qualified parameters, not bind placeholders",
                ));
            }
            output.push_str(&self.source[token.span.clone()]);
            copied = token.span.end;
            index += 1;
        }
        let semi = self.tokens.get(end).map_or(self.source.len(), |token| token.span.start);
        output.push_str(&self.source[copied..semi]);
        Ok((output, uses))
    }

    fn located(&self, at: usize, message: &str) -> mpedb_types::Error {
        super::super::lex::error(
            self.source,
            self.tokens.get(at).map_or(self.source.len(), |token| token.span.start),
            message,
        )
    }

    pub(super) fn cursor_statement(&mut self) -> Result<bool> {
        if self.word("OPEN") {
            let cursor = self.cursor_name()?;
            if cursor.formals.is_empty() {
                if self.symbol("(") {
                    return Err(self.error("this cursor has no parameters"));
                }
            } else {
                self.open_arguments(&cursor)?;
            }
            self.want_symbol(";")?;
            self.builder.emit(Op::CursorOpen(cursor.call))?;
            self.builder.emit(Op::StoreLocal(cursor.slot))?;
            return Ok(true);
        }
        if self.word("FETCH") {
            let cursor = self.cursor_name()?;
            self.want_word("INTO")?;
            let mut targets = Vec::new();
            loop {
                let name = self.identifier()?;
                // A scalar local hides a record of the same name. One record
                // target is the whole row; mixing it with other targets is
                // not a scalar list.
                if targets.is_empty() && !self.locals.contains_key(&name) {
                    if let Some(record) = self.records.get(&name).cloned() {
                        if self.symbol(",") {
                            return Err(self.error(
                                "FETCH INTO a %ROWTYPE record takes one target",
                            ));
                        }
                        self.fetch_record(&cursor, &record)?;
                        return Ok(true);
                    }
                }
                let target = *self
                    .locals
                    .get(&name)
                    .ok_or_else(|| self.error("unknown FETCH target"))?;
                if !target.writable {
                    return Err(self.error("FETCH target is constant or an IN parameter"));
                }
                targets.push(target);
                if !self.symbol(",") {
                    break;
                }
            }
            if targets.len() != cursor.width as usize {
                return Err(self.error(
                    "FETCH INTO target count does not match the cursor SELECT list",
                ));
            }
            self.want_symbol(";")?;
            self.builder.emit(Op::LoadLocal(cursor.slot))?;
            self.builder.emit(Op::CursorFetch)?;
            let none = self.builder.emit_jump(Op::JumpIfFalse(u32::MAX))?;
            self.builder.emit(Op::LoadLocal(cursor.slot))?;
            self.builder.emit(Op::CursorRow)?;
            for (i, target) in targets.iter().enumerate() {
                self.builder.emit(Op::Dup)?;
                self.constant(Value::Int(i as i64))?;
                self.builder.emit(Op::Index)?;
                self.emit_local_check(*target)?;
                self.builder.emit(Op::StoreLocal(target.slot))?;
            }
            self.builder.emit(Op::Pop)?;
            let done = self.builder.emit_jump(Op::Jump(u32::MAX))?;
            self.builder.patch_to_here(none);
            self.builder.patch_to_here(done);
            return Ok(true);
        }
        if self.word("CLOSE") {
            let cursor = self.cursor_name()?;
            self.want_symbol(";")?;
            self.builder.emit(Op::LoadLocal(cursor.slot))?;
            self.builder.emit(Op::CursorClose)?;
            self.constant(Value::Null)?;
            self.builder.emit(Op::StoreLocal(cursor.slot))?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Evaluate each supplied argument, then each omitted default, once.
    /// Placeholders are pushed afterwards in query order. NULL stays a bound
    /// NULL. `OPEN name` and `OPEN name()` are the same when every parameter
    /// has a default. A missing required parameter stays an arity error.
    fn open_arguments(&mut self, cursor: &CursorDecl) -> Result<()> {
        let mut got = 0usize;
        if self.symbol("(") {
            if !matches!(self.peek(), Some(Token::Symbol(")"))) {
                loop {
                    if got == cursor.formals.len() {
                        return Err(self.error(
                            "OPEN argument count does not match the cursor parameters",
                        ));
                    }
                    let actual = self.expression()?;
                    let ty = cursor.formals[got];
                    self.compatible(ty, actual)?;
                    self.builder.emit(ty.check())?;
                    self.builder.emit(Op::StoreLocal(cursor.arg_slots[got]))?;
                    got += 1;
                    if !self.symbol(",") {
                        break;
                    }
                }
            }
            self.want_symbol(")")?;
        }
        for index in got..cursor.formals.len() {
            let Some(ops) = cursor.defaults[index].as_deref() else {
                return Err(self.error(
                    "OPEN argument count does not match the cursor parameters",
                ));
            };
            self.replay_default(ops)?;
            self.builder.emit(Op::StoreLocal(cursor.arg_slots[index]))?;
        }
        for &formal in &cursor.uses {
            self.builder.emit(Op::LoadLocal(cursor.arg_slots[formal as usize]))?;
        }
        Ok(())
    }

    /// Store the current row into the record's field slots. No per-field
    /// type check: a NULL cell stays NULL, and text is not coerced to NUMBER.
    /// A missing row jumps over the stores and leaves every field as it was.
    fn fetch_record(&mut self, cursor: &CursorDecl, record: &super::RecordDecl) -> Result<()> {
        let Some(columns) = cursor.columns.as_ref() else {
            return Err(self.error("cursor SELECT item needs an alias"));
        };
        if columns.len() != record.fields.len()
            || columns
                .iter()
                .zip(&record.fields)
                .any(|(column, (field, _))| column != field)
        {
            return Err(self.error(
                "FETCH INTO record fields do not match the cursor SELECT list",
            ));
        }
        self.want_symbol(";")?;
        self.builder.emit(Op::LoadLocal(cursor.slot))?;
        self.builder.emit(Op::CursorFetch)?;
        let none = self.builder.emit_jump(Op::JumpIfFalse(u32::MAX))?;
        self.builder.emit(Op::LoadLocal(cursor.slot))?;
        self.builder.emit(Op::CursorRow)?;
        for (index, (_, slot)) in record.fields.iter().enumerate() {
            self.builder.emit(Op::Dup)?;
            self.constant(Value::Int(index as i64))?;
            self.builder.emit(Op::Index)?;
            self.builder.emit(Op::StoreLocal(*slot))?;
        }
        self.builder.emit(Op::Pop)?;
        let done = self.builder.emit_jump(Op::Jump(u32::MAX))?;
        self.builder.patch_to_here(none);
        self.builder.patch_to_here(done);
        Ok(())
    }

    fn cursor_name(&mut self) -> Result<CursorDecl> {
        let name = self.identifier()?;
        self.cursors
            .get(&name)
            .cloned()
            .ok_or_else(|| self.error("unknown explicit cursor"))
    }

    pub(super) fn cursor_attribute(&mut self, name: &str) -> Result<Option<Type>> {
        let attr = self.identifier()?;
        if attr == "ROWTYPE" {
            return Err(self.error("%ROWTYPE is a type, not a value"));
        }
        let (code, ty) = match attr.as_str() {
            "ISOPEN" => (crate::ir::CURSOR_ATTR_ISOPEN, Type::Boolean),
            "FOUND" => (crate::ir::CURSOR_ATTR_FOUND, Type::Boolean),
            "NOTFOUND" => (crate::ir::CURSOR_ATTR_NOTFOUND, Type::Boolean),
            "ROWCOUNT" => (crate::ir::CURSOR_ATTR_ROWCOUNT, Type::PlsInteger),
            _ => return Err(self.error(&format!("unknown cursor attribute {attr}"))),
        };
        // Oracle reserves SQL for the implicit cursor, even when a cursor
        // declaration uses that name.
        if name == "SQL" {
            self.builder.emit(Op::SqlAttr(code))?;
            return Ok(Some(ty));
        }
        let cursor = self.cursors.get(name).cloned().ok_or_else(|| {
            self.error("cursor attributes require an explicit cursor")
        })?;
        self.builder.emit(Op::LoadLocal(cursor.slot))?;
        self.builder.emit(Op::CursorAttr(code))?;
        Ok(Some(ty))
    }
}

enum WidthError {
    Star { at: usize },
    TooWide,
    MissingFrom,
}

/// Column count of a cursor SELECT list. A `*` at item-start or after `.`
/// is star expansion and is refused. A `*` inside parentheses, including
/// `count(*)`, is one expression. Multiplication (`1 * 2`) is not expansion.
fn select_width(tokens: &[Lexed]) -> std::result::Result<u16, WidthError> {
    let mut depth = 0i32;
    let mut width = 1u16;
    let mut saw_from = false;
    let mut item_boundary = false;
    let mut after_dot = false;
    for (index, token) in tokens.iter().enumerate() {
        match &token.token {
            Token::Symbol("(") => {
                depth += 1;
                after_dot = false;
                item_boundary = false;
            }
            Token::Symbol(")") => {
                depth -= 1;
                after_dot = false;
            }
            Token::Identifier { name, quoted: false } if depth == 0 && name == "FROM" => {
                saw_from = true;
                break;
            }
            Token::Identifier { name, quoted: false }
                if depth == 0 && name == "SELECT" && index == 0 =>
            {
                item_boundary = true;
                after_dot = false;
            }
            Token::Identifier { name, quoted: false }
                if depth == 0
                    && item_boundary
                    && matches!(name.as_str(), "DISTINCT" | "ALL" | "UNIQUE") =>
            {
                after_dot = false;
            }
            Token::Symbol(",") if depth == 0 => {
                width = width.checked_add(1).ok_or(WidthError::TooWide)?;
                item_boundary = true;
                after_dot = false;
            }
            Token::Symbol(".") if depth == 0 => {
                after_dot = true;
                item_boundary = false;
            }
            Token::Symbol("*") if depth == 0 && (item_boundary || after_dot) => {
                return Err(WidthError::Star { at: index });
            }
            _ if depth == 0 => {
                item_boundary = false;
                after_dot = false;
            }
            _ => {}
        }
    }
    if !saw_from {
        return Err(WidthError::MissingFrom);
    }
    Ok(width)
}

/// Select-list names, or `None` when any item has none. This does not decide
/// star expansion: [`select_width`] already refused `*` before this runs.
/// `id`, `t.id`, `id AS part`, `id part`, `a.b c`, and `count(*) AS n` have
/// names. `count(*)` and `1 * 2` do not. A quoted identifier needs an
/// unquoted alias. Fail closed on any shape this walk cannot name.
fn select_field_names(tokens: &[Lexed]) -> Option<Vec<String>> {
    let mut depth = 0i32;
    let mut items: Vec<Vec<Token>> = Vec::new();
    let mut current: Vec<Token> = Vec::new();
    let mut prefix = true;
    let mut saw_from = false;
    for token in tokens {
        match &token.token {
            Token::Symbol("(") => {
                depth += 1;
                prefix = false;
                current.push(token.token.clone());
            }
            Token::Symbol(")") => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
                current.push(token.token.clone());
            }
            Token::Identifier { name, quoted: false } if depth == 0 && name == "FROM" => {
                items.push(std::mem::take(&mut current));
                saw_from = true;
                break;
            }
            Token::Identifier { name, quoted: false }
                if depth == 0
                    && prefix
                    && matches!(name.as_str(), "SELECT" | "DISTINCT" | "ALL" | "UNIQUE") => {}
            Token::Symbol(",") if depth == 0 => {
                items.push(std::mem::take(&mut current));
                prefix = false;
            }
            other => {
                prefix = false;
                current.push(other.clone());
            }
        }
    }
    if !saw_from || items.is_empty() {
        return None;
    }
    items.into_iter().map(|item| column_name(&item)).collect()
}

fn column_name(item: &[Token]) -> Option<String> {
    if item.is_empty() {
        return None;
    }
    if item.len() >= 3 && unquoted(&item[item.len() - 2]) == Some("AS") {
        return unquoted(item.last()?).map(str::to_string);
    }
    if qualified(item) {
        return unquoted(item.last()?).map(str::to_string);
    }
    let alias = unquoted(item.last()?)?;
    let rest = &item[..item.len() - 1];
    if rest.is_empty() || unquoted(&rest[0]) == Some("AS") {
        return None;
    }
    if qualified(rest) || literal_item(rest) || closed_item(rest) {
        Some(alias.to_string())
    } else {
        None
    }
}

fn unquoted(token: &Token) -> Option<&str> {
    match token {
        Token::Identifier { name, quoted: false } => Some(name.as_str()),
        _ => None,
    }
}

/// `ident` or `ident.ident` with every identifier unquoted.
fn qualified(item: &[Token]) -> bool {
    if item.is_empty() {
        return false;
    }
    let mut expect_ident = true;
    for token in item {
        if expect_ident {
            if unquoted(token).is_none() {
                return false;
            }
        } else if !matches!(token, Token::Symbol(".")) {
            return false;
        }
        expect_ident = !expect_ident;
    }
    !expect_ident
}

fn literal_item(item: &[Token]) -> bool {
    matches!(item, [Token::Number(_)] | [Token::String { .. }])
}

fn closed_item(item: &[Token]) -> bool {
    if !matches!(item.last(), Some(Token::Symbol(")"))) {
        return false;
    }
    let mut depth = 0i32;
    for token in item {
        match token {
            Token::Symbol("(") => depth += 1,
            Token::Symbol(")") => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

#[cfg(test)]
mod tests {
    use super::super::compile;
    use crate::emit::CallKind;
    use crate::interp::{self, testutil::MockBridge, Budget, ProcValue};
    use crate::ir::{Op, PlanKind, PlanRef, Proc};
    use mpedb_types::{PlanHash, Value};

    fn run(rows: Vec<Vec<Value>>, body: &str) -> (Value, MockBridge) {
        let (value, bridge, _) = execute("NUMBER", rows, body);
        (value, bridge)
    }

    /// Compile through the shipped compiler, round-trip the bytecode, and
    /// execute it. The returned procedure is the one that ran.
    fn execute(returns: &str, rows: Vec<Vec<Value>>, body: &str) -> (Value, MockBridge, Proc) {
        let source = format!("CREATE FUNCTION f RETURN {returns} IS {body}");
        let skel = compile(&source).unwrap();
        assert!(skel.calls.iter().any(|call| call.kind == CallKind::Rows));
        let plans = skel
            .calls
            .iter()
            .map(|call| PlanRef {
                hash: PlanHash([0; 32]),
                kind: if call.kind == CallKind::Exec {
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
        bridge.rows = rows;
        let value = match interp::run(
            &proc,
            &[],
            &mut bridge,
            Budget {
                instrs: 10_000,
                db_calls: 4,
                rows: 100,
            },
        )
        .unwrap()
        {
            ProcValue::Scalar(value) => value,
            other => panic!("{other:?}"),
        };
        (value, bridge, proc)
    }

    /// Like [`execute`], but the body may be DML or SELECT INTO and need not
    /// open a cursor. `affected` is the canned `DbExec` count. `returning`
    /// is the canned `DbReturning` row list.
    fn scene(
        returns: &str,
        body: &str,
        rows: Vec<Vec<Value>>,
        affected: u64,
        returning: Option<Vec<Vec<Value>>>,
    ) -> (Value, MockBridge, Proc) {
        let source = format!("CREATE FUNCTION f RETURN {returns} IS {body}");
        let skel = compile(&source).unwrap_or_else(|err| panic!("{err}"));
        let plans = skel
            .calls
            .iter()
            .map(|call| PlanRef {
                hash: PlanHash([0; 32]),
                kind: if call.kind == CallKind::Exec {
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
        bridge.rows = rows;
        bridge.affected = affected;
        bridge.returning_rows = returning;
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

    #[test]
    fn explicit_cursor_fetch_assigns_null_and_leaves_a_missing_row_unchanged() {
        let body = "CURSOR c IS SELECT amount FROM dual; v NUMBER := 7; BEGIN OPEN c; FETCH c INTO v; CLOSE c; RETURN v; END;";
        let (value, bridge) = run(vec![vec![Value::Null]], body);
        assert_eq!(value, Value::Null);
        assert_eq!(bridge.cursor_opens, 1);
        assert_eq!(bridge.live_streams(), 0);
        let (value, _) = run(vec![vec![Value::Numeric("4".into())]], body);
        assert_eq!(value, Value::Numeric("4".into()));
        let (value, bridge) = run(vec![], body);
        assert_eq!(value, Value::Numeric("7".into()));
        assert_eq!(bridge.advances, 1);
        assert_eq!(bridge.closes, 1);
        assert_eq!(bridge.live_streams(), 0);
        let (value, bridge) = run(
            vec![
                vec![Value::Numeric("4".into())],
                vec![Value::Numeric("9".into())],
            ],
            body,
        );
        assert_eq!(value, Value::Numeric("4".into()));
        assert_eq!(bridge.advances, 1, "CLOSE must not fetch the remaining row");
        assert_eq!(bridge.closes, 1);
        assert_eq!(bridge.live_streams(), 0);
    }

    #[test]
    fn explicit_cursor_refuses_star_expansion_and_counts_parenthesized_star() {
        for query in ["SELECT * FROM dual", "SELECT t.* FROM dual", "SELECT DISTINCT * FROM dual"] {
            let source = format!(
                "CREATE FUNCTION f RETURN NUMBER IS CURSOR c IS {query}; BEGIN RETURN 1; END;"
            );
            let err = compile(&source).unwrap_err().to_string();
            assert!(err.contains("PL/SQL at "), "{err}");
            assert!(err.contains("SELECT *"), "{err}");
            assert!(!err.starts_with("unsupported: cursor SELECT list"), "{err}");
        }
        let count = "CREATE FUNCTION f RETURN NUMBER IS CURSOR c IS SELECT count(*) FROM dual; n NUMBER; BEGIN OPEN c; FETCH c INTO n; CLOSE c; RETURN n; END;";
        let skel = compile(count).unwrap();
        assert_eq!(skel.calls[0].expected_columns, Some(1));
        let product = "CREATE FUNCTION f RETURN NUMBER IS CURSOR c IS SELECT 1 * 2 FROM dual; n NUMBER; BEGIN OPEN c; FETCH c INTO n; CLOSE c; RETURN n; END;";
        let skel = compile(product).unwrap();
        assert_eq!(skel.calls[0].expected_columns, Some(1));
        let two = "CREATE FUNCTION f RETURN NUMBER IS CURSOR c IS SELECT a, b FROM dual; x NUMBER; y NUMBER; BEGIN OPEN c; FETCH c INTO x, y; CLOSE c; RETURN x; END;";
        let skel = compile(two).unwrap();
        assert_eq!(skel.calls[0].expected_columns, Some(2));
    }

    #[test]
    fn cursor_attributes_follow_oracle_null_logic() {
        let row = vec![vec![Value::Numeric("4".into())]];
        let decl = "CURSOR c IS SELECT amount FROM dual; v NUMBER;";
        let invalid = "ORA-01001: cursor number is invalid or does not exist";

        let (found, bridge, proc) = execute(
            "BOOLEAN",
            row.clone(),
            &format!("{decl} BEGIN OPEN c; RETURN c%Found; END;"),
        );
        assert!(proc.instrs.contains(&Op::CursorAttr(crate::ir::CURSOR_ATTR_FOUND)));
        assert_eq!(found, Value::Null);
        assert_eq!(bridge.cursor_opens, 1);
        assert_eq!(bridge.advances, 0);

        let (notfound, _, proc) = execute(
            "BOOLEAN",
            row.clone(),
            &format!("{decl} BEGIN OPEN c; RETURN c%notfound; END;"),
        );
        assert!(proc.instrs.contains(&Op::CursorAttr(crate::ir::CURSOR_ATTR_NOTFOUND)));
        assert_eq!(notfound, Value::Null);

        let (open, _) = run(
            row.clone(),
            &format!("{decl} BEGIN OPEN c; IF c%ISOPEN THEN RETURN 1; ELSE RETURN 0; END IF; END;"),
        );
        assert_eq!(open, Value::Numeric("1".into()));
        let (count, _, proc) = execute(
            "PLS_INTEGER",
            row.clone(),
            &format!("{decl} BEGIN OPEN c; RETURN c%ROWCOUNT; END;"),
        );
        assert!(proc.instrs.contains(&Op::CursorAttr(crate::ir::CURSOR_ATTR_ROWCOUNT)));
        assert_eq!(count, Value::Int(0));

        let (closed, bridge) = run(
            vec![],
            "CURSOR c IS SELECT amount FROM dual; BEGIN IF c%ISOPEN THEN RETURN 1; ELSE RETURN 0; END IF; END;",
        );
        assert_eq!(closed, Value::Numeric("0".into()));
        assert_eq!(bridge.cursor_opens, 0);
        for (attr, assign) in [
            ("FOUND", "flag := c%FOUND;"),
            ("NOTFOUND", "flag := c%NOTFOUND;"),
            ("ROWCOUNT", "n := c%ROWCOUNT;"),
        ] {
            let (text, bridge, proc) = execute(
                "VARCHAR2",
                vec![],
                &format!(
                    "CURSOR c IS SELECT amount FROM dual; flag BOOLEAN; n PLS_INTEGER; BEGIN {assign} RETURN 'open'; EXCEPTION WHEN INVALID_CURSOR THEN RETURN SQLERRM; END;"
                ),
            );
            assert!(
                proc.instrs.iter().any(|op| matches!(op, Op::CursorAttr(_))),
                "{attr}"
            );
            assert_eq!(text, Value::Text(invalid.into()), "{attr}");
            assert_eq!(bridge.cursor_opens, 0, "{attr}");
        }

        let (found, bridge, _) = execute(
            "BOOLEAN",
            row.clone(),
            &format!("{decl} BEGIN OPEN c; FETCH c INTO v; RETURN c%FOUND; END;"),
        );
        assert_eq!(found, Value::Bool(true));
        assert_eq!(bridge.advances, 1);
        let (notfound, _) = run(
            row.clone(),
            &format!("{decl} BEGIN OPEN c; FETCH c INTO v; IF c%NOTFOUND THEN RETURN 1; ELSE RETURN 0; END IF; END;"),
        );
        assert_eq!(notfound, Value::Numeric("0".into()));
        let (count, _, _) = execute(
            "PLS_INTEGER",
            row.clone(),
            &format!("{decl} BEGIN OPEN c; FETCH c INTO v; RETURN c%ROWCOUNT; END;"),
        );
        assert_eq!(count, Value::Int(1));

        let (found, bridge, _) = execute(
            "BOOLEAN",
            vec![],
            &format!("{decl} BEGIN OPEN c; FETCH c INTO v; RETURN c%FOUND; END;"),
        );
        assert_eq!(found, Value::Bool(false));
        assert_eq!(bridge.advances, 1);
        let (notfound, _, _) = execute(
            "BOOLEAN",
            vec![],
            &format!("{decl} BEGIN OPEN c; FETCH c INTO v; RETURN c%NOTFOUND; END;"),
        );
        assert_eq!(notfound, Value::Bool(true));
        let (count, _, _) = execute(
            "PLS_INTEGER",
            vec![],
            &format!("{decl} BEGIN OPEN c; FETCH c INTO v; RETURN c%ROWCOUNT; END;"),
        );
        assert_eq!(count, Value::Int(0));

        let (count, bridge, _) = execute(
            "PLS_INTEGER",
            row.clone(),
            &format!(
                "{decl} BEGIN OPEN c; FETCH c INTO v; FETCH c INTO v; FETCH c INTO v; RETURN c%ROWCOUNT; END;"
            ),
        );
        assert_eq!(count, Value::Int(1));
        assert_eq!(bridge.advances, 2, "a fetch after exhaustion must not pull");
        let (notfound, _, _) = execute(
            "BOOLEAN",
            row.clone(),
            &format!("{decl} BEGIN OPEN c; FETCH c INTO v; FETCH c INTO v; RETURN c%NOTFOUND; END;"),
        );
        assert_eq!(notfound, Value::Bool(true));
        let (found, _, _) = execute(
            "BOOLEAN",
            row.clone(),
            &format!("{decl} BEGIN OPEN c; FETCH c INTO v; FETCH c INTO v; RETURN c%FOUND; END;"),
        );
        assert_eq!(found, Value::Bool(false));

        let (open, bridge) = run(
            vec![],
            "CURSOR c IS SELECT amount FROM dual; BEGIN OPEN c; CLOSE c; IF c%ISOPEN THEN RETURN 1; ELSE RETURN 0; END IF; END;",
        );
        assert_eq!(open, Value::Numeric("0".into()));
        assert_eq!(bridge.closes, 1);
        assert_eq!(bridge.advances, 0);
        assert_eq!(bridge.live_streams(), 0);
        let (text, bridge, _) = execute(
            "VARCHAR2",
            row.clone(),
            &format!(
                "{decl} n PLS_INTEGER; BEGIN OPEN c; FETCH c INTO v; CLOSE c; n := c%ROWCOUNT; RETURN 'open'; EXCEPTION WHEN INVALID_CURSOR THEN RETURN SQLERRM; END;"
            ),
        );
        assert_eq!(text, Value::Text(invalid.into()));
        assert_eq!(bridge.advances, 1);
        assert_eq!(bridge.closes, 1);
        for (attr, assign) in [("FOUND", "flag := c%FOUND;"), ("NOTFOUND", "flag := c%NOTFOUND;")] {
            let (text, _, _) = execute(
                "VARCHAR2",
                vec![],
                &format!(
                    "CURSOR c IS SELECT amount FROM dual; flag BOOLEAN; BEGIN OPEN c; CLOSE c; {assign} RETURN 'open'; EXCEPTION WHEN INVALID_CURSOR THEN RETURN SQLERRM; END;"
                ),
            );
            assert_eq!(text, Value::Text(invalid.into()), "{attr}");
        }

        let err = compile(
            "CREATE FUNCTION f RETURN BOOLEAN IS CURSOR c IS SELECT amount FROM dual; BEGIN RETURN c%ROWTYPE; END;",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("PL/SQL at "), "{err}");
        assert!(err.contains("%ROWTYPE is a type, not a value"), "{err}");
    }

    #[test]
    fn cursor_query_parameters_bind_null_and_values() {
        let query = "SELECT id FROM t WHERE t.part = c.part AND t.amount = c.amount AND t.again = c.part";
        let body = format!(
            "CURSOR c (part IN VARCHAR2, amount NUMBER, unused BOOLEAN) IS {query}; BEGIN OPEN c(NULL, 4, TRUE); CLOSE c; RETURN 1; END;"
        );
        let skel = compile(&format!("CREATE FUNCTION f RETURN NUMBER IS {body}")).unwrap();
        assert_eq!(
            skel.calls[0].sql,
            "SELECT id FROM t WHERE t.part = $1 AND t.amount = $2 AND t.again = $3"
        );
        assert!(!skel.calls[0].sql.contains("c.part"));
        assert!(!skel.calls[0].sql.contains("IS NULL"));
        assert_eq!(skel.calls[0].argc, 3);
        assert_eq!(skel.calls[0].expected_columns, Some(1));
        let (_value, bridge, proc) = execute("NUMBER", vec![], &body);
        assert_eq!(
            bridge.cursor_params,
            vec![vec![Value::Null, Value::Numeric("4".into()), Value::Null]]
        );
        let open_at = proc
            .instrs
            .iter()
            .position(|op| matches!(op, Op::CursorOpen(_)))
            .unwrap();
        let before = &proc.instrs[..open_at];
        let loads = before.iter().filter(|op| matches!(op, Op::LoadLocal(_))).count();
        let stores = before.iter().filter(|op| matches!(op, Op::StoreLocal(_))).count();
        assert_eq!(loads, 3, "a repeated formal is loaded twice and evaluated once");
        assert_eq!(stores, 4, "the handle plus one store per formal, including the unused one");

        let one = "CURSOR c (part VARCHAR2) IS SELECT id FROM t WHERE t.part = c.part; v NUMBER := 7;";
        let (missing, bridge) = run(
            vec![],
            &format!("{one} BEGIN OPEN c(NULL); FETCH c INTO v; CLOSE c; RETURN v; END;"),
        );
        assert_eq!(missing, Value::Numeric("7".into()));
        assert_eq!(bridge.cursor_params, vec![vec![Value::Null]]);
        assert_eq!(bridge.advances, 1);
        assert_eq!(bridge.closes, 1);
        let (found, bridge) = run(
            vec![vec![Value::Numeric("4".into())]],
            &format!("{one} BEGIN OPEN c(NULL); FETCH c INTO v; CLOSE c; RETURN v; END;"),
        );
        assert_eq!(found, Value::Numeric("4".into()));
        assert_eq!(bridge.cursor_params, vec![vec![Value::Null]]);
        let (kept, bridge, _) = execute(
            "VARCHAR2",
            vec![],
            "CURSOR c (part VARCHAR2) IS SELECT id FROM t WHERE t.part = c.part; part VARCHAR2 := 'keep'; BEGIN OPEN c('wheel'); CLOSE c; RETURN part; END;",
        );
        assert_eq!(kept, Value::Text("keep".into()));
        assert_eq!(bridge.cursor_params, vec![vec![Value::Text("wheel".into())]]);
        let text_sql = compile(
            "CREATE FUNCTION f RETURN VARCHAR2 IS CURSOR c (part VARCHAR2) IS SELECT id FROM t WHERE t.part = c.part; part VARCHAR2 := 'keep'; BEGIN OPEN c('wheel'); CLOSE c; RETURN part; END;",
        )
        .unwrap();
        assert_eq!(text_sql.calls[0].sql, "SELECT id FROM t WHERE t.part = $1");
        assert_eq!(text_sql.calls[0].argc, 1);
        let (unused, bridge, _) = execute(
            "NUMBER",
            vec![],
            "CURSOR c (part VARCHAR2) IS SELECT id FROM t WHERE t.x = 1; BEGIN OPEN c(NULL); CLOSE c; RETURN 1; END;",
        );
        assert_eq!(unused, Value::Numeric("1".into()));
        assert_eq!(bridge.cursor_params, vec![vec![]]);
        assert_eq!(bridge.cursor_opens, 1);
        let unused_sql = compile(
            "CREATE FUNCTION f RETURN NUMBER IS CURSOR c (part VARCHAR2) IS SELECT id FROM t WHERE t.x = 1; BEGIN OPEN c(NULL); CLOSE c; RETURN 1; END;",
        )
        .unwrap();
        assert_eq!(unused_sql.calls[0].sql, "SELECT id FROM t WHERE t.x = 1");
        assert_eq!(unused_sql.calls[0].argc, 0);

        let bare = compile("CREATE FUNCTION f RETURN NUMBER IS CURSOR c (part VARCHAR2) IS SELECT id FROM t WHERE part = c.part; BEGIN RETURN 1; END;")
            .unwrap_err()
            .to_string();
        assert!(bare.contains("PL/SQL at "), "{bare}");
        assert!(bare.contains("bare cursor parameter"), "{bare}");
        for (body, needle) in [
            (
                "CURSOR c (part VARCHAR2) IS SELECT id FROM t WHERE t.part = :part; BEGIN RETURN 1; END;",
                "not bind placeholders",
            ),
            (
                "CURSOR c () IS SELECT id FROM t; BEGIN RETURN 1; END;",
                "omit the parentheses",
            ),
            (
                "CURSOR c IS SELECT id FROM t; BEGIN OPEN c(1); RETURN 1; END;",
                "this cursor has no parameters",
            ),
            (
                "CURSOR c (part VARCHAR2, amount NUMBER) IS SELECT id FROM t WHERE t.part = c.part AND t.amount = c.amount; BEGIN OPEN c(NULL); RETURN 1; END;",
                "OPEN argument count does not match",
            ),
            (
                "CURSOR c (part VARCHAR2) IS SELECT id FROM t WHERE t.part = c.part; BEGIN OPEN c(NULL, 4); RETURN 1; END;",
                "OPEN argument count does not match",
            ),
            (
                "CURSOR c (part VARCHAR2) IS SELECT id FROM t WHERE t.part = c.part; BEGIN OPEN c; RETURN 1; END;",
                "OPEN argument count does not match",
            ),
            (
                "CURSOR c (part OUT VARCHAR2) IS SELECT id FROM t WHERE t.part = c.part; BEGIN RETURN 1; END;",
                "cursor parameters are IN only",
            ),
            (
                "CURSOR c (part IN OUT VARCHAR2) IS SELECT id FROM t WHERE t.part = c.part; BEGIN RETURN 1; END;",
                "cursor parameters are IN only",
            ),
            (
                "CURSOR c (part IN NOCOPY VARCHAR2) IS SELECT id FROM t WHERE t.part = c.part; BEGIN RETURN 1; END;",
                "NOCOPY is only valid for OUT or IN OUT parameters",
            ),
            (
                "CURSOR c (part VARCHAR2, part NUMBER) IS SELECT id FROM t WHERE t.part = c.part; BEGIN RETURN 1; END;",
                "duplicate cursor parameter name",
            ),
            (
                "CURSOR c (part VARCHAR2) IS SELECT id FROM t WHERE t.part = c.missing; BEGIN RETURN 1; END;",
                "unknown cursor parameter",
            ),
            (
                "CURSOR c (part VARCHAR2) IS SELECT id FROM t WHERE t.part = c.part.extra; BEGIN RETURN 1; END;",
                "cannot have further qualification",
            ),
            (
                "CURSOR c (part VARCHAR2) IS SELECT id FROM t WHERE c = 1; BEGIN RETURN 1; END;",
                "cursor name in the query requires .parameter",
            ),
            (
                "CURSOR c (part VARCHAR2) IS SELECT id FROM t WHERE t.part = c.part; BEGIN RETURN part; END;",
                "unknown variable",
            ),
            (
                "part VARCHAR2 := 'keep'; CURSOR c (p VARCHAR2) IS SELECT id FROM t WHERE t.part = c.p; BEGIN OPEN c(part => 'wheel'); RETURN 1; END;",
                "expected )",
            ),
        ] {
            let err = compile(&format!("CREATE FUNCTION f RETURN NUMBER IS {body}"))
                .unwrap_err()
                .to_string();
            assert!(err.contains("PL/SQL at "), "{needle}: {err}");
            assert!(err.contains(needle), "{needle}: {err}");
        }
    }

    #[test]
    fn cursor_parameter_defaults_apply_at_open_including_null() {
        let query = "SELECT id FROM t WHERE t.part = c.part AND t.amount = c.amount AND t.again = c.part";
        let body = format!(
            "CURSOR c (part IN VARCHAR2 DEFAULT NULL, amount NUMBER := 4) IS {query}; BEGIN OPEN c; CLOSE c; OPEN c(); CLOSE c; OPEN c('wheel'); CLOSE c; OPEN c('wheel', 9); CLOSE c; RETURN 1; END;"
        );
        let (_value, bridge, _) = execute("NUMBER", vec![], &body);
        let four = Value::Numeric("4".into());
        let nine = Value::Numeric("9".into());
        let wheel = Value::Text("wheel".into());
        assert_eq!(
            bridge.cursor_params,
            vec![
                vec![Value::Null, four.clone(), Value::Null],
                vec![Value::Null, four.clone(), Value::Null],
                vec![wheel.clone(), four, wheel.clone()],
                vec![wheel.clone(), nine, wheel],
            ]
        );
        assert_eq!(bridge.cursor_opens, 4);
        assert_eq!(bridge.closes, 4);

        let timed = "n VARCHAR2(20) := 'before'; CURSOR c (part VARCHAR2 DEFAULT n) IS SELECT id FROM t WHERE t.part = c.part AND t.again = c.part; BEGIN n := 'after'; OPEN c; CLOSE c; n := 'again'; OPEN c; CLOSE c; RETURN n; END;";
        let (kept, bridge, proc) = execute("VARCHAR2", vec![], timed);
        assert_eq!(kept, Value::Text("again".into()));
        assert_eq!(
            bridge.cursor_params,
            vec![
                vec![Value::Text("after".into()), Value::Text("after".into())],
                vec![Value::Text("again".into()), Value::Text("again".into())],
            ]
        );
        let open_at = proc
            .instrs
            .iter()
            .position(|op| matches!(op, Op::CursorOpen(_)))
            .unwrap();
        let loads = proc.instrs[..open_at]
            .iter()
            .filter(|op| matches!(op, Op::LoadLocal(_)))
            .count();
        assert_eq!(loads, 3, "the default is evaluated once and pushed twice: {proc:?}");

        let skipped = "CURSOR c (part VARCHAR2 DEFAULT 'seed') IS SELECT id FROM t WHERE t.part = c.part; BEGIN OPEN c; OPEN c('wheel'); CLOSE c; RETURN 1; END;";
        let (_value, bridge, proc) = execute("NUMBER", vec![], skipped);
        assert_eq!(
            bridge.cursor_params,
            vec![vec![Value::Text("seed".into())], vec![Value::Text("wheel".into())]]
        );
        let seed = proc
            .consts
            .iter()
            .position(|value| value == &Value::Text("seed".into()))
            .unwrap() as u16;
        let opens: Vec<usize> = proc
            .instrs
            .iter()
            .enumerate()
            .filter(|(_, op)| matches!(op, Op::CursorOpen(_)))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(opens.len(), 2);
        assert!(proc.instrs[..opens[0]].contains(&Op::LoadConst(seed)));
        assert!(!proc.instrs[opens[0] + 1..opens[1]].contains(&Op::LoadConst(seed)));

        let shadowed = "low NUMBER := 1; CURSOR c (low NUMBER := 4, high NUMBER := low) IS SELECT id FROM t WHERE t.amount = c.high; BEGIN low := 6; OPEN c(9); CLOSE c; RETURN low; END;";
        let (kept, bridge, _) = execute("NUMBER", vec![], shadowed);
        assert_eq!(kept, Value::Numeric("6".into()));
        assert_eq!(bridge.cursor_params, vec![vec![Value::Numeric("9".into())]]);

        let logic = "left BOOLEAN := TRUE; right BOOLEAN := FALSE; CURSOR c (flag BOOLEAN DEFAULT left OR right) IS SELECT id FROM t WHERE t.ok = c.flag; BEGIN left := FALSE; right := NULL; OPEN c; CLOSE c; OPEN c(TRUE); CLOSE c; RETURN 1; END;";
        let (_value, bridge, _) = execute("NUMBER", vec![], logic);
        assert_eq!(
            bridge.cursor_params,
            vec![vec![Value::Null], vec![Value::Bool(true)]]
        );

        for (body, needle) in [
            (
                "CURSOR c (part VARCHAR2, amount NUMBER DEFAULT NULL) IS SELECT id FROM t WHERE t.part = c.part; BEGIN OPEN c; RETURN 1; END;",
                "OPEN argument count does not match",
            ),
            (
                "CURSOR c (part VARCHAR2 DEFAULT NULL, amount NUMBER) IS SELECT id FROM t WHERE t.part = c.part AND t.amount = c.amount; BEGIN OPEN c(NULL); RETURN 1; END;",
                "OPEN argument count does not match",
            ),
            (
                "CURSOR c (part VARCHAR2 DEFAULT NULL) IS SELECT id FROM t WHERE t.part = c.part; BEGIN OPEN c(NULL, 4); RETURN 1; END;",
                "OPEN argument count does not match",
            ),
            (
                "CURSOR c (part VARCHAR2 DEFAULT later) IS SELECT id FROM t WHERE t.part = c.part; later VARCHAR2(20) := 'no'; BEGIN OPEN c; RETURN 1; END;",
                "forward reference",
            ),
            (
                "CURSOR c (part VARCHAR2 DEFAULT missing) IS SELECT id FROM t WHERE t.part = c.part; BEGIN RETURN 1; END;",
                "unknown variable",
            ),
            (
                "CURSOR c (part VARCHAR2 DEFAULT 4) IS SELECT id FROM t WHERE t.part = c.part; BEGIN RETURN 1; END;",
                "incompatible expression type",
            ),
            (
                "CURSOR c (part VARCHAR2 DEFAULT NULL) IS SELECT id FROM t WHERE t.part = c.part; BEGIN RETURN part; END;",
                "unknown variable",
            ),
        ] {
            let err = compile(&format!("CREATE FUNCTION f RETURN NUMBER IS {body}"))
                .unwrap_err()
                .to_string();
            assert!(err.contains("PL/SQL at "), "{needle}: {err}");
            assert!(err.contains(needle), "{needle}: {err}");
        }
    }

    #[test]
    fn implicit_sql_attributes_follow_oracle_null_logic() {
        let (found, _, proc) = scene("BOOLEAN", "BEGIN RETURN SQL%FOUND; END;", vec![], 0, None);
        assert!(proc.instrs.contains(&Op::SqlAttr(crate::ir::CURSOR_ATTR_FOUND)));
        assert!(!proc.instrs.iter().any(|op| matches!(op, Op::CursorAttr(_))));
        assert!(!proc.has_exec());
        assert_eq!(found, Value::Null);
        let (notfound, _, _) = scene("BOOLEAN", "BEGIN RETURN SQL%NOTFOUND; END;", vec![], 0, None);
        assert_eq!(notfound, Value::Null);
        let (count, _, _) = scene("PLS_INTEGER", "BEGIN RETURN SQL%ROWCOUNT; END;", vec![], 0, None);
        assert_eq!(count, Value::Null);
        let (open, _, proc) = scene("BOOLEAN", "BEGIN RETURN SQL%ISOPEN; END;", vec![], 0, None);
        assert!(proc.instrs.contains(&Op::SqlAttr(crate::ir::CURSOR_ATTR_ISOPEN)));
        assert_eq!(open, Value::Bool(false));

        let (found, _, _) = scene(
            "BOOLEAN",
            "BEGIN UPDATE t SET amount = 1; RETURN SQL%FOUND; END;",
            vec![],
            4,
            None,
        );
        assert_eq!(found, Value::Bool(true));
        let (notfound, _, _) = scene(
            "BOOLEAN",
            "BEGIN UPDATE t SET amount = 1; RETURN SQL%NOTFOUND; END;",
            vec![],
            4,
            None,
        );
        assert_eq!(notfound, Value::Bool(false));
        let (count, _, _) = scene(
            "PLS_INTEGER",
            "BEGIN UPDATE t SET amount = 1; RETURN SQL%ROWCOUNT; END;",
            vec![],
            4,
            None,
        );
        assert_eq!(count, Value::Int(4));
        let (open, _, _) = scene(
            "BOOLEAN",
            "BEGIN UPDATE t SET amount = 1; RETURN SQL%ISOPEN; END;",
            vec![],
            4,
            None,
        );
        assert_eq!(open, Value::Bool(false));

        let (found, _, _) = scene(
            "BOOLEAN",
            "BEGIN UPDATE t SET amount = 1; RETURN SQL%FOUND; END;",
            vec![],
            0,
            None,
        );
        assert_eq!(found, Value::Bool(false));
        let (notfound, _, _) = scene(
            "BOOLEAN",
            "BEGIN UPDATE t SET amount = 1; RETURN SQL%NOTFOUND; END;",
            vec![],
            0,
            None,
        );
        assert_eq!(notfound, Value::Bool(true));
        let (count, _, _) = scene(
            "PLS_INTEGER",
            "BEGIN UPDATE t SET amount = 1; RETURN SQL%ROWCOUNT; END;",
            vec![],
            0,
            None,
        );
        assert_eq!(count, Value::Int(0));

        let null_row = vec![vec![Value::Null]];
        let (assigned, _, _) = scene(
            "NUMBER",
            "v NUMBER := 7; BEGIN SELECT amount INTO v FROM dual; RETURN v; END;",
            null_row.clone(),
            0,
            None,
        );
        assert_eq!(assigned, Value::Null);
        let (found, _, _) = scene(
            "BOOLEAN",
            "v NUMBER := 7; BEGIN SELECT amount INTO v FROM dual; RETURN SQL%FOUND; END;",
            null_row.clone(),
            0,
            None,
        );
        assert_eq!(found, Value::Bool(true));
        let (notfound, _, _) = scene(
            "BOOLEAN",
            "v NUMBER := 7; BEGIN SELECT amount INTO v FROM dual; RETURN SQL%NOTFOUND; END;",
            null_row.clone(),
            0,
            None,
        );
        assert_eq!(notfound, Value::Bool(false));
        let (count, _, _) = scene(
            "PLS_INTEGER",
            "v NUMBER := 7; BEGIN SELECT amount INTO v FROM dual; RETURN SQL%ROWCOUNT; END;",
            null_row,
            0,
            None,
        );
        assert_eq!(count, Value::Int(1));

        let miss = "v NUMBER := 7; BEGIN BEGIN SELECT amount INTO v FROM dual; RETURN 0; EXCEPTION WHEN NO_DATA_FOUND THEN RETURN v; END; END;";
        let (kept, _, _) = scene("NUMBER", miss, vec![], 0, None);
        assert_eq!(kept, Value::Numeric("7".into()));
        let miss_found = "v NUMBER := 7; BEGIN BEGIN SELECT amount INTO v FROM dual; RETURN TRUE; EXCEPTION WHEN NO_DATA_FOUND THEN RETURN SQL%FOUND; END; END;";
        let (found, _, _) = scene("BOOLEAN", miss_found, vec![], 0, None);
        assert_eq!(found, Value::Bool(false));
        let miss_not = "v NUMBER := 7; BEGIN BEGIN SELECT amount INTO v FROM dual; RETURN FALSE; EXCEPTION WHEN NO_DATA_FOUND THEN RETURN SQL%NOTFOUND; END; END;";
        let (notfound, _, _) = scene("BOOLEAN", miss_not, vec![], 0, None);
        assert_eq!(notfound, Value::Bool(true));
        let miss_count = "v NUMBER := 7; BEGIN BEGIN SELECT amount INTO v FROM dual; RETURN -1; EXCEPTION WHEN NO_DATA_FOUND THEN RETURN SQL%ROWCOUNT; END; END;";
        let (count, _, _) = scene("PLS_INTEGER", miss_count, vec![], 0, None);
        assert_eq!(count, Value::Int(0));

        let many = vec![vec![Value::Numeric("4".into())], vec![Value::Numeric("9".into())]];
        let too_many = "v NUMBER := 7; BEGIN BEGIN SELECT amount INTO v FROM dual; RETURN 0; EXCEPTION WHEN TOO_MANY_ROWS THEN RETURN v; END; END;";
        let (kept, _, _) = scene("NUMBER", too_many, many.clone(), 0, None);
        assert_eq!(kept, Value::Numeric("7".into()));
        let too_many_found = "v NUMBER := 7; BEGIN BEGIN SELECT amount INTO v FROM dual; RETURN FALSE; EXCEPTION WHEN TOO_MANY_ROWS THEN RETURN SQL%FOUND; END; END;";
        let (found, _, _) = scene("BOOLEAN", too_many_found, many.clone(), 0, None);
        assert_eq!(found, Value::Bool(true));
        let too_many_not = "v NUMBER := 7; BEGIN BEGIN SELECT amount INTO v FROM dual; RETURN TRUE; EXCEPTION WHEN TOO_MANY_ROWS THEN RETURN SQL%NOTFOUND; END; END;";
        let (notfound, _, _) = scene("BOOLEAN", too_many_not, many.clone(), 0, None);
        assert_eq!(notfound, Value::Bool(false));
        let too_many_count = "v NUMBER := 7; BEGIN BEGIN SELECT amount INTO v FROM dual; RETURN -1; EXCEPTION WHEN TOO_MANY_ROWS THEN RETURN SQL%ROWCOUNT; END; END;";
        let (count, _, _) = scene("PLS_INTEGER", too_many_count, many, 0, None);
        assert_eq!(count, Value::Int(1));

        let replaced = "v NUMBER := 7; BEGIN SELECT amount INTO v FROM dual; UPDATE t SET amount = 1; RETURN SQL%ROWCOUNT; END;";
        let (count, _, _) = scene(
            "PLS_INTEGER",
            replaced,
            vec![vec![Value::Null]],
            0,
            None,
        );
        assert_eq!(count, Value::Int(0));
        let replaced_not = "v NUMBER := 7; BEGIN SELECT amount INTO v FROM dual; UPDATE t SET amount = 1; RETURN SQL%NOTFOUND; END;";
        let (notfound, _, _) = scene(
            "BOOLEAN",
            replaced_not,
            vec![vec![Value::Null]],
            0,
            None,
        );
        assert_eq!(notfound, Value::Bool(true));

        let row = vec![vec![Value::Numeric("4".into())]];
        let opened = "CURSOR c IS SELECT amount FROM dual; v NUMBER; BEGIN SELECT amount INTO v FROM dual; OPEN c; RETURN SQL%FOUND; END;";
        let (found, bridge, _) = scene("BOOLEAN", opened, row.clone(), 0, None);
        assert_eq!(found, Value::Bool(true));
        assert_eq!(bridge.cursor_opens, 1);
        assert_eq!(bridge.advances, 0);
        let opened_count = "CURSOR c IS SELECT amount FROM dual; v NUMBER; BEGIN SELECT amount INTO v FROM dual; OPEN c; RETURN SQL%ROWCOUNT; END;";
        let (count, _, _) = scene("PLS_INTEGER", opened_count, row.clone(), 0, None);
        assert_eq!(count, Value::Int(1));
        let cursor_count = "CURSOR c IS SELECT amount FROM dual; v NUMBER; BEGIN SELECT amount INTO v FROM dual; OPEN c; RETURN c%ROWCOUNT; END;";
        let (count, _, _) = scene("PLS_INTEGER", cursor_count, row.clone(), 0, None);
        assert_eq!(count, Value::Int(0));
        let implicit_open = "CURSOR c IS SELECT amount FROM dual; v NUMBER; BEGIN SELECT amount INTO v FROM dual; OPEN c; RETURN SQL%ISOPEN; END;";
        let (open, _, _) = scene("BOOLEAN", implicit_open, row.clone(), 0, None);
        assert_eq!(open, Value::Bool(false));
        let explicit_open = "CURSOR c IS SELECT amount FROM dual; v NUMBER; BEGIN SELECT amount INTO v FROM dual; OPEN c; RETURN c%ISOPEN; END;";
        let (open, _, _) = scene("BOOLEAN", explicit_open, row.clone(), 0, None);
        assert_eq!(open, Value::Bool(true));
        let after_fetch = "CURSOR c IS SELECT amount FROM dual; v NUMBER; BEGIN SELECT amount INTO v FROM dual; OPEN c; FETCH c INTO v; FETCH c INTO v; RETURN SQL%FOUND; END;";
        let (found, bridge, _) = scene("BOOLEAN", after_fetch, row.clone(), 0, None);
        assert_eq!(found, Value::Bool(true));
        assert_eq!(bridge.advances, 2);
        let after_count = "CURSOR c IS SELECT amount FROM dual; v NUMBER; BEGIN SELECT amount INTO v FROM dual; OPEN c; FETCH c INTO v; FETCH c INTO v; RETURN SQL%ROWCOUNT; END;";
        let (count, _, _) = scene("PLS_INTEGER", after_count, row.clone(), 0, None);
        assert_eq!(count, Value::Int(1));
        let cursor_found = "CURSOR c IS SELECT amount FROM dual; v NUMBER; BEGIN SELECT amount INTO v FROM dual; OPEN c; FETCH c INTO v; FETCH c INTO v; RETURN c%FOUND; END;";
        let (found, bridge, _) = scene("BOOLEAN", cursor_found, row, 0, None);
        assert_eq!(found, Value::Bool(false));
        assert_eq!(bridge.advances, 2);

        let named = "CURSOR sql IS SELECT amount FROM dual; BEGIN OPEN sql; RETURN SQL%ISOPEN; END;";
        let (open, bridge, proc) = scene("BOOLEAN", named, vec![], 0, None);
        assert_eq!(open, Value::Bool(false));
        assert_eq!(bridge.cursor_opens, 1);
        assert!(proc.instrs.contains(&Op::SqlAttr(crate::ir::CURSOR_ATTR_ISOPEN)));
        assert!(proc.instrs.iter().any(|op| matches!(op, Op::CursorOpen(_))));

        let returning_null = "v NUMBER := 7; incoming VARCHAR2(20); BEGIN EXECUTE IMMEDIATE 'UPDATE t SET note = :1 WHERE id = :2 RETURNING note INTO :3' USING incoming, 1, OUT v; RETURN v; END;";
        let (assigned, bridge, _) = scene(
            "NUMBER",
            returning_null,
            vec![],
            0,
            Some(vec![vec![Value::Null]]),
        );
        assert_eq!(assigned, Value::Null);
        assert_eq!(bridge.execs, 1);
        assert_eq!(bridge.exec_params, vec![vec![Value::Null, Value::Int(1)]]);
        let returning_count = "v NUMBER := 7; incoming VARCHAR2(20); BEGIN EXECUTE IMMEDIATE 'UPDATE t SET note = :1 WHERE id = :2 RETURNING note INTO :3' USING incoming, 1, OUT v; RETURN SQL%ROWCOUNT; END;";
        let (count, _, _) = scene(
            "PLS_INTEGER",
            returning_count,
            vec![],
            0,
            Some(vec![vec![Value::Null]]),
        );
        assert_eq!(count, Value::Int(1));
        let returning_found = "v NUMBER := 7; incoming VARCHAR2(20); BEGIN EXECUTE IMMEDIATE 'UPDATE t SET note = :1 WHERE id = :2 RETURNING note INTO :3' USING incoming, 1, OUT v; RETURN SQL%FOUND; END;";
        let (found, _, _) = scene(
            "BOOLEAN",
            returning_found,
            vec![],
            0,
            Some(vec![vec![Value::Null]]),
        );
        assert_eq!(found, Value::Bool(true));

        let returning_miss = "v NUMBER := 7; BEGIN BEGIN EXECUTE IMMEDIATE 'UPDATE t SET amount = 9 WHERE id = :1 RETURNING amount INTO :2' USING 99, OUT v; RETURN 0; EXCEPTION WHEN NO_DATA_FOUND THEN RETURN v; END; END;";
        let (kept, _, _) = scene("NUMBER", returning_miss, vec![], 0, Some(vec![]));
        assert_eq!(kept, Value::Numeric("7".into()));
        let returning_miss_count = "v NUMBER := 7; BEGIN BEGIN EXECUTE IMMEDIATE 'UPDATE t SET amount = 9 WHERE id = :1 RETURNING amount INTO :2' USING 99, OUT v; RETURN -1; EXCEPTION WHEN NO_DATA_FOUND THEN RETURN SQL%ROWCOUNT; END; END;";
        let (count, _, _) = scene("PLS_INTEGER", returning_miss_count, vec![], 0, Some(vec![]));
        assert_eq!(count, Value::Int(0));
        let returning_miss_not = "v NUMBER := 7; BEGIN BEGIN EXECUTE IMMEDIATE 'UPDATE t SET amount = 9 WHERE id = :1 RETURNING amount INTO :2' USING 99, OUT v; RETURN FALSE; EXCEPTION WHEN NO_DATA_FOUND THEN RETURN SQL%NOTFOUND; END; END;";
        let (notfound, _, _) = scene("BOOLEAN", returning_miss_not, vec![], 0, Some(vec![]));
        assert_eq!(notfound, Value::Bool(true));

        let many_rows = Some(vec![
            vec![Value::Numeric("4".into())],
            vec![Value::Numeric("9".into())],
        ]);
        let returning_many = "v NUMBER := 7; BEGIN BEGIN EXECUTE IMMEDIATE 'UPDATE t SET amount = amount + 1 RETURNING amount INTO :1' USING OUT v; RETURN 0; EXCEPTION WHEN TOO_MANY_ROWS THEN RETURN v; END; END;";
        let (kept, _, _) = scene("NUMBER", returning_many, vec![], 0, many_rows.clone());
        assert_eq!(kept, Value::Numeric("7".into()));
        let returning_many_count = "v NUMBER := 7; BEGIN BEGIN EXECUTE IMMEDIATE 'UPDATE t SET amount = amount + 1 RETURNING amount INTO :1' USING OUT v; RETURN -1; EXCEPTION WHEN TOO_MANY_ROWS THEN RETURN SQL%ROWCOUNT; END; END;";
        let (count, _, _) = scene("PLS_INTEGER", returning_many_count, vec![], 0, many_rows.clone());
        assert_eq!(count, Value::Int(1));
        let returning_many_found = "v NUMBER := 7; BEGIN BEGIN EXECUTE IMMEDIATE 'UPDATE t SET amount = amount + 1 RETURNING amount INTO :1' USING OUT v; RETURN FALSE; EXCEPTION WHEN TOO_MANY_ROWS THEN RETURN SQL%FOUND; END; END;";
        let (found, _, _) = scene("BOOLEAN", returning_many_found, vec![], 0, many_rows);
        assert_eq!(found, Value::Bool(true));

        for (source, needle) in [
            (
                "CREATE FUNCTION f RETURN BOOLEAN IS BEGIN RETURN SQL%ROWTYPE; END;",
                "%ROWTYPE is a type, not a value",
            ),
            (
                "CREATE FUNCTION f RETURN BOOLEAN IS BEGIN RETURN SQL%BULK_ROWCOUNT; END;",
                "unknown cursor attribute",
            ),
            (
                "CREATE FUNCTION f RETURN BOOLEAN IS BEGIN RETURN missing%FOUND; END;",
                "cursor attributes require an explicit cursor",
            ),
        ] {
            let err = compile(source).unwrap_err().to_string();
            assert!(err.contains("PL/SQL at "), "{err}");
            assert!(err.contains(needle), "{needle}: {err}");
        }
    }

    #[test]
    fn cursor_rowtype_assigns_null_fields_and_leaves_a_missing_fetch_unchanged() {
        let row = vec![vec![Value::Numeric("4".into()), Value::Null]];
        let decl = "CURSOR c IS SELECT t.id, note AS label FROM t; v c%ROWTYPE;";
        let (null_record, _, _) = execute(
            "BOOLEAN",
            vec![],
            &format!(
                "{decl} BEGIN IF v IS NULL AND v.id IS NULL AND v.label IS NULL THEN RETURN TRUE; ELSE RETURN FALSE; END IF; END;"
            ),
        );
        assert_eq!(null_record, Value::Bool(true));
        let (inited, _, _) = execute(
            "BOOLEAN",
            vec![],
            "CURSOR c IS SELECT id, note FROM t; v c%ROWTYPE := NULL; BEGIN RETURN v IS NULL; END;",
        );
        assert_eq!(inited, Value::Bool(true));

        let (label, bridge, _) = execute(
            "VARCHAR2",
            row.clone(),
            &format!("{decl} BEGIN OPEN c; FETCH c INTO v; RETURN v.label; END;"),
        );
        assert_eq!(label, Value::Null);
        assert_eq!(bridge.cursor_opens, 1);
        assert_eq!(bridge.advances, 1);
        assert_eq!(bridge.live_streams(), 1);
        let (id, _, _) = execute(
            "NUMBER",
            row.clone(),
            &format!("{decl} BEGIN OPEN c; FETCH c INTO v; RETURN v.id; END;"),
        );
        assert_eq!(id, Value::Numeric("4".into()));
        let (is_null, _, _) = execute(
            "BOOLEAN",
            row.clone(),
            &format!("{decl} BEGIN OPEN c; FETCH c INTO v; RETURN v IS NULL; END;"),
        );
        assert_eq!(is_null, Value::Bool(false));
        let (is_not, _, _) = execute(
            "BOOLEAN",
            row.clone(),
            &format!("{decl} BEGIN OPEN c; FETCH c INTO v; RETURN v IS NOT NULL; END;"),
        );
        assert_eq!(is_not, Value::Bool(true));

        let (kept, bridge, _) = execute(
            "VARCHAR2",
            row.clone(),
            &format!(
                "{decl} BEGIN OPEN c; FETCH c INTO v; v.label := 'keep'; FETCH c INTO v; RETURN v.label; END;"
            ),
        );
        assert_eq!(kept, Value::Text("keep".into()));
        assert_eq!(bridge.advances, 2);
        let (kept_id, _, _) = execute(
            "NUMBER",
            row.clone(),
            &format!(
                "{decl} BEGIN OPEN c; FETCH c INTO v; v.label := 'keep'; FETCH c INTO v; RETURN v.id; END;"
            ),
        );
        assert_eq!(kept_id, Value::Numeric("4".into()));
        let (cleared, _, _) = execute(
            "BOOLEAN",
            row.clone(),
            &format!(
                "{decl} BEGIN OPEN c; FETCH c INTO v; v := NULL; RETURN v IS NULL AND v.id IS NULL AND v.label IS NULL; END;"
            ),
        );
        assert_eq!(cleared, Value::Bool(true));

        let (outer, _, _) = execute(
            "VARCHAR2",
            vec![],
            &format!(
                "{decl} BEGIN v.label := 'outer'; DECLARE v c%ROWTYPE; BEGIN v.label := 'inner'; END; RETURN v.label; END;"
            ),
        );
        assert_eq!(outer, Value::Text("outer".into()));
        let (shadow, _, _) = execute(
            "VARCHAR2",
            vec![],
            &format!(
                "{decl} BEGIN v.label := 'outer'; DECLARE v NUMBER := 4; BEGIN IF v IS NULL THEN RETURN 'shadow'; END IF; END; RETURN v.label; END;"
            ),
        );
        assert_eq!(shadow, Value::Text("outer".into()));

        let named = "CURSOR c IS SELECT t.id, note label, a.b part, count(*) AS n FROM t; v c%ROWTYPE;";
        let wide = vec![vec![
            Value::Numeric("4".into()),
            Value::Null,
            Value::Text("p".into()),
            Value::Numeric("8".into()),
        ]];
        let (part, _, _) = execute(
            "VARCHAR2",
            wide.clone(),
            &format!("{named} BEGIN OPEN c; FETCH c INTO v; RETURN v.part; END;"),
        );
        assert_eq!(part, Value::Text("p".into()));
        let (count, _, _) = execute(
            "NUMBER",
            wide.clone(),
            &format!("{named} BEGIN OPEN c; FETCH c INTO v; RETURN v.n; END;"),
        );
        assert_eq!(count, Value::Numeric("8".into()));
        let (aliased, _, _) = execute(
            "VARCHAR2",
            wide,
            &format!("{named} BEGIN OPEN c; FETCH c INTO v; RETURN v.label; END;"),
        );
        assert_eq!(aliased, Value::Null);
        let (same_shape, _, _) = execute(
            "NUMBER",
            row,
            "CURSOR a IS SELECT id, note FROM t; CURSOR b IS SELECT t.id, note AS note FROM u; v a%ROWTYPE; BEGIN OPEN b; FETCH b INTO v; RETURN v.id; END;",
        );
        assert_eq!(same_shape, Value::Numeric("4".into()));

        compile("CREATE FUNCTION f RETURN NUMBER IS CURSOR c IS SELECT count(*) FROM t; n NUMBER; BEGIN OPEN c; FETCH c INTO n; CLOSE c; RETURN 1; END;").unwrap();
        compile("CREATE FUNCTION f RETURN NUMBER IS CURSOR c IS SELECT 1 * 2 FROM t; BEGIN OPEN c; CLOSE c; RETURN 1; END;").unwrap();

        let refuse = |source: &str, needle: &str| {
            let err = compile(source).unwrap_err().to_string();
            assert!(err.contains("PL/SQL at "), "{err}");
            assert!(err.contains(needle), "{needle}: {err}");
        };
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS CURSOR c IS SELECT count(*) FROM t; v c%ROWTYPE; BEGIN RETURN 1; END;",
            "cursor SELECT item needs an alias",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS CURSOR c IS SELECT 1 * 2 FROM t; v c%ROWTYPE; BEGIN RETURN 1; END;",
            "cursor SELECT item needs an alias",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS CURSOR c IS SELECT \"id\" FROM t; v c%ROWTYPE; BEGIN RETURN 1; END;",
            "cursor SELECT item needs an alias",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS v employees%ROWTYPE; BEGIN RETURN 1; END;",
            "%ROWTYPE requires an explicit cursor in this scope",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS CURSOR c IS SELECT id, t.id FROM t; v c%ROWTYPE; BEGIN RETURN 1; END;",
            "duplicate %ROWTYPE field",
        );
        refuse(
            "CREATE FUNCTION f RETURN BOOLEAN IS CURSOR c IS SELECT id FROM t; BEGIN RETURN c%ROWTYPE; END;",
            "%ROWTYPE is a type, not a value",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS CURSOR c IS SELECT id, note FROM t; v c%ROWTYPE; BEGIN RETURN v; END;",
            "a %ROWTYPE record requires a field",
        );
        refuse(
            "CREATE FUNCTION f RETURN VARCHAR2 IS CURSOR c IS SELECT id, note FROM t; v c%ROWTYPE; BEGIN RETURN v.missing; END;",
            "unknown %ROWTYPE field",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS CURSOR c IS SELECT id FROM t; v CONSTANT c%ROWTYPE; BEGIN RETURN 1; END;",
            "a %ROWTYPE record cannot be CONSTANT",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS CURSOR c IS SELECT id FROM t; v c%ROWTYPE NOT NULL; BEGIN RETURN 1; END;",
            "a %ROWTYPE record cannot be NOT NULL",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS CURSOR c IS SELECT id FROM t; v c%ROWTYPE := 1; BEGIN RETURN 1; END;",
            "a %ROWTYPE record can only be initialized with NULL",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS CURSOR a IS SELECT id, note FROM t; CURSOR b IS SELECT note, id FROM t; v a%ROWTYPE; BEGIN FETCH b INTO v; RETURN 1; END;",
            "FETCH INTO record fields do not match the cursor SELECT list",
        );
        refuse(
            "CREATE FUNCTION f RETURN NUMBER IS CURSOR c IS SELECT id, note FROM t; v c%ROWTYPE; n NUMBER; BEGIN FETCH c INTO v, n; RETURN 1; END;",
            "FETCH INTO a %ROWTYPE record takes one target",
        );
    }
}
