//! `json(X)` — validate `X` as JSON text and return it MINIFIED, matching
//! sqlite's `jsonFunc`.
//!
//! Minifying, not re-rendering: every token keeps the spelling it had in the
//! input (`1e3` stays `1e3`, `1.50` stays `1.50`, `"xå"` keeps its
//! escape) and only the whitespace BETWEEN tokens is dropped. That is exactly
//! what sqlite does, and it is the only definition under which mpedb can be
//! byte-identical to sqlite without also owning sqlite's number formatting.
//!
//! # Refused (a clean error, never a guessed value)
//!
//! * A non-TEXT argument. sqlite renders `json(5)` as `5` and reads a BLOB as
//!   JSONB; neither is reproduced here.
//! * sqlite 3.45's **JSON5** extensions — unquoted object keys (`{a:1}`),
//!   single-quoted strings, `+`/hex/`Infinity`/`NaN` numbers, trailing commas
//!   and comments. sqlite ACCEPTS those and rewrites them into canonical JSON;
//!   mpedb accepts strict RFC 8259 only and says so by name.
//!
//! The parser is iterative (an explicit container stack), so a pathologically
//! nested document is a bounded error rather than a blown stack.

use crate::error::{Error, Result};
use crate::value::Value;

/// Matching sqlite's `JSON_MAX_DEPTH`. Deeper input is an error, not a crash.
const MAX_DEPTH: usize = 1000;

fn malformed(at: usize) -> Error {
    Error::TypeMismatch(format!(
        "json(): malformed JSON at byte {at}; mpedb accepts strict RFC 8259 JSON only — \
         sqlite 3.45's JSON5 extensions (unquoted keys, single-quoted strings, hex/`+`/\
         Infinity/NaN numbers, trailing commas, comments) are refused rather than rewritten"
    ))
}

fn is_ws(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
    out: String,
}

impl P<'_> {
    fn skip_ws(&mut self) {
        while self.i < self.b.len() && is_ws(self.b[self.i]) {
            self.i += 1;
        }
    }

    fn peek(&self) -> Result<u8> {
        self.b.get(self.i).copied().ok_or_else(|| malformed(self.i))
    }

    /// Copy a `"…"` string token verbatim, validating its escapes.
    fn string(&mut self) -> Result<()> {
        let start = self.i;
        if self.peek()? != b'"' {
            return Err(malformed(self.i));
        }
        self.i += 1;
        loop {
            let c = self.peek()?;
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = self.peek()?;
                    self.i += 1;
                    match e {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {}
                        b'u' => {
                            for _ in 0..4 {
                                let h = self.peek()?;
                                if !h.is_ascii_hexdigit() {
                                    return Err(malformed(self.i));
                                }
                                self.i += 1;
                            }
                        }
                        _ => return Err(malformed(self.i - 1)),
                    }
                }
                // Raw control characters are not legal inside a JSON string.
                0x00..=0x1f => return Err(malformed(self.i - 1)),
                _ => {}
            }
        }
        // The slice is bounded by two ASCII quotes, so it is a char boundary.
        self.out.push_str(std::str::from_utf8(&self.b[start..self.i]).map_err(|_| {
            Error::TypeMismatch("json(): argument is not valid UTF-8".into())
        })?);
        Ok(())
    }

    /// Copy a number token verbatim: `-? (0 | [1-9][0-9]*) (. [0-9]+)? ([eE] [+-]? [0-9]+)?`.
    fn number(&mut self) -> Result<()> {
        let start = self.i;
        if self.peek()? == b'-' {
            self.i += 1;
        }
        match self.peek()? {
            b'0' => self.i += 1,
            b'1'..=b'9' => {
                while self.b.get(self.i).is_some_and(|c| c.is_ascii_digit()) {
                    self.i += 1;
                }
            }
            _ => return Err(malformed(self.i)),
        }
        if self.b.get(self.i).copied() == Some(b'.') {
            self.i += 1;
            if !self.b.get(self.i).is_some_and(|c| c.is_ascii_digit()) {
                return Err(malformed(self.i));
            }
            while self.b.get(self.i).is_some_and(|c| c.is_ascii_digit()) {
                self.i += 1;
            }
        }
        if matches!(self.b.get(self.i).copied(), Some(b'e') | Some(b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i).copied(), Some(b'+') | Some(b'-')) {
                self.i += 1;
            }
            if !self.b.get(self.i).is_some_and(|c| c.is_ascii_digit()) {
                return Err(malformed(self.i));
            }
            while self.b.get(self.i).is_some_and(|c| c.is_ascii_digit()) {
                self.i += 1;
            }
        }
        // ASCII-only token.
        self.out.push_str(std::str::from_utf8(&self.b[start..self.i]).unwrap_or(""));
        Ok(())
    }

    fn keyword(&mut self, word: &str) -> Result<()> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            self.out.push_str(word);
            Ok(())
        } else {
            Err(malformed(self.i))
        }
    }

    /// A `"key":` pair opener.
    fn member_key(&mut self) -> Result<()> {
        self.skip_ws();
        self.string()?;
        self.skip_ws();
        if self.peek()? != b':' {
            return Err(malformed(self.i));
        }
        self.i += 1;
        self.out.push(':');
        Ok(())
    }
}

pub(super) fn sqlite_json(v: &Value) -> Result<Value> {
    let s = match v {
        Value::Text(s) => s.as_str(),
        other => {
            return Err(Error::TypeMismatch(format!(
                "json() expects text, got {} — sqlite renders a number as its JSON literal \
                 and reads a blob as JSONB; mpedb supports neither",
                other.type_name()
            )))
        }
    };
    let mut p = P {
        b: s.as_bytes(),
        i: 0,
        out: String::with_capacity(s.len()),
    };
    // An explicit container stack: b'[' = inside an array, b'{' = inside an
    // object. `want_value` drives the outer loop.
    let mut stack: Vec<u8> = Vec::new();
    'value: loop {
        p.skip_ws();
        match p.peek()? {
            b'{' => {
                if stack.len() >= MAX_DEPTH {
                    return Err(malformed(p.i));
                }
                p.i += 1;
                p.out.push('{');
                stack.push(b'{');
                p.skip_ws();
                if p.peek()? == b'}' {
                    p.i += 1;
                    p.out.push('}');
                    stack.pop();
                } else {
                    p.member_key()?;
                    continue 'value;
                }
            }
            b'[' => {
                if stack.len() >= MAX_DEPTH {
                    return Err(malformed(p.i));
                }
                p.i += 1;
                p.out.push('[');
                stack.push(b'[');
                p.skip_ws();
                if p.peek()? == b']' {
                    p.i += 1;
                    p.out.push(']');
                    stack.pop();
                } else {
                    continue 'value;
                }
            }
            b'"' => p.string()?,
            b't' => p.keyword("true")?,
            b'f' => p.keyword("false")?,
            b'n' => p.keyword("null")?,
            b'-' | b'0'..=b'9' => p.number()?,
            _ => return Err(malformed(p.i)),
        }
        // A value has just been emitted: close out every container it finishes.
        loop {
            let Some(&top) = stack.last() else {
                break 'value;
            };
            p.skip_ws();
            let c = p.peek()?;
            if c == b',' {
                p.i += 1;
                p.out.push(',');
                if top == b'{' {
                    p.member_key()?;
                }
                continue 'value;
            }
            let close = if top == b'{' { b'}' } else { b']' };
            if c != close {
                return Err(malformed(p.i));
            }
            p.i += 1;
            p.out.push(close as char);
            stack.pop();
        }
    }
    p.skip_ws();
    if p.i != p.b.len() {
        return Err(malformed(p.i));
    }
    Ok(Value::Text(p.out))
}
