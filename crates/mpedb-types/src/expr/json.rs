//! sqlite's JSON function set, over TEXT.
//!
//! sqlite has no JSON *type*: a JSON document is ordinary TEXT and a family of
//! scalar functions reads and rewrites it. mpedb follows that model exactly —
//! no new `ColumnType`, no schema-format change, no operators over a typed
//! `jsonb`. (PostgreSQL's typed `json`/`jsonb` with GIN indexes is a separate,
//! later decision; nothing here constrains it.)
//!
//! # The two rules that make this byte-exact
//!
//! **1. Minifying, not re-rendering.** Every token of a parsed document keeps
//! the spelling it had in the input — `1e3` stays `1e3`, `1.50` stays `1.50`,
//! `"xå"` keeps its escapes — and only the whitespace *between* tokens is
//! dropped. That is what sqlite does, and it is the only definition under
//! which `json()`, `json_extract()` and `json_set()` can be byte-identical to
//! sqlite without also owning sqlite's number formatting. It is why [`Node`]
//! stores raw source slices rather than decoded values.
//!
//! **2. Values that ENTER a document are rendered like sqlite renders them.**
//! `json_array(1e3)` is `[1000.0]`: a REAL becomes exactly the text
//! `CAST(x AS TEXT)` produces, which mpedb already reproduces bit-for-bit in
//! [`float_to_text`](super::printf::float_to_text).
//!
//! # Refused, by name, rather than guessed
//!
//! * **JSON5.** sqlite 3.45's `json()` accepts unquoted object keys (`{a:1}`),
//!   single-quoted strings, `+`/hex/`Infinity`/`NaN` numbers, trailing commas
//!   and comments, and REWRITES them (`json('0x10')` is `16`,
//!   `json('Infinity')` is `9e999`, `json('NaN')` is `null`). Reproducing that
//!   rewrite exactly is a large surface with no user in sight, so mpedb accepts
//!   strict RFC 8259 only and says so. A consequence worth stating plainly:
//!   sqlite's `json()` and `json_valid()` deliberately DISAGREE on JSON5
//!   (`json('{a:1}')` is `{"a":1}` but `json_valid('{a:1}')` is `0`); in mpedb
//!   they agree, because the JSON5 input errors in `json()` instead.
//! * **JSONB** (sqlite 3.45's binary encoding): `jsonb()`, `jsonb_*`, the
//!   `json_valid()` flag bits 4 and 8, and a BLOB argument read as JSONB.
//! * **A path key containing a backslash.** sqlite 3.45 compares a path label
//!   against a DECODED document label but takes the path's own text verbatim,
//!   so `$."a\"b"` matches nothing while `$.a"b` matches `{"a\"b":1}`. Rather
//!   than reproduce that asymmetry, a backslash in a path key is an error.
//! * **A lone surrogate** in an extracted string. sqlite emits the unpaired
//!   code point as three raw bytes (`ED A0 80`), which is not UTF-8 and cannot
//!   live in mpedb's `Value::Text`.
//! * `json_each`/`json_tree` (table-valued) and `json_group_array`/
//!   `json_group_object` (aggregates) are not here at all — different
//!   machinery, refused at bind time with a message naming them.

use super::printf::{atof, float_to_text};
use crate::error::{Error, Result};
use crate::value::Value;
use std::borrow::Cow;

/// How deep a document mpedb will parse.
///
/// **This is not sqlite's bound.** sqlite's `JSON_MAX_DEPTH` is 1000 (verified:
/// 1000 nested arrays are valid, 1001 are not), but sqlite's parser is a hand
/// -written state machine over a flat blob while this one is a recursive
/// descent over a tree, and 1000 frames of it overflow a default 2 MiB thread
/// stack in a debug build. Rather than crash — or silently answer 0 where
/// sqlite answers 1 — a document deeper than this is a clean ERROR that names
/// the limit, in `json_valid()` too. 128 levels is ~16 KiB of stack per level
/// of headroom, and deeper documents do not occur in the ORM traffic this
/// function set exists for.
const MAX_DEPTH: usize = 128;

/// Why a parse failed. The distinction is load-bearing: `json_valid()` answers
/// 0 for a MALFORMED document (matching sqlite) but must RAISE for one that is
/// merely deeper than mpedb parses, because sqlite would have answered 1 and a
/// 0 there would be a wrong answer rather than a refusal.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ParseFail {
    Malformed,
    TooDeep,
}

impl From<ParseFail> for Error {
    fn from(f: ParseFail) -> Error {
        match f {
            ParseFail::Malformed => malformed(),
            ParseFail::TooDeep => Error::TypeMismatch(format!(
                "json: the document nests deeper than {MAX_DEPTH} levels, which is mpedb's \
                 parse bound (sqlite's is 1000). The document is refused rather than reported \
                 invalid, because sqlite would accept it"
            )),
        }
    }
}

/// sqlite's message for every parse failure, verbatim. It carries no offset,
/// so neither does this one.
fn malformed() -> Error {
    Error::TypeMismatch(
        "malformed JSON — mpedb accepts strict RFC 8259 only; sqlite 3.45's JSON5 extensions \
         (unquoted keys, single-quoted strings, hex/`+`/`Infinity`/`NaN` numbers, trailing \
         commas, comments) are refused rather than rewritten"
            .into(),
    )
}

fn bad_path(p: &str) -> Error {
    Error::TypeMismatch(format!("bad JSON path: '{p}'"))
}

// ---------------------------------------------------------------------------
// The document model
// ---------------------------------------------------------------------------

/// The parser's own result: see [`ParseFail`].
type PResult<T> = std::result::Result<T, ParseFail>;

/// A parsed JSON document.
///
/// Scalars keep their VERBATIM source spelling — `Num` holds the number token
/// as written and `Str` holds the string token *including* its quotes and
/// escapes. Rendering is therefore a concatenation, and a document that is only
/// navigated or partially rewritten comes back out byte-identical to sqlite's
/// (rule 1 in the module docs). `Cow` because `json_set` and friends splice in
/// freshly rendered, owned tokens.
#[derive(Debug, Clone, PartialEq)]
enum Node<'a> {
    Null,
    True,
    False,
    /// A number token, exactly as written (`1.50`, `1e3`, `-0`).
    Num(Cow<'a, str>),
    /// A string token INCLUDING both quotes (`"xå"`, `"a\"b"`).
    Str(Cow<'a, str>),
    Arr(Vec<Node<'a>>),
    /// Key/value pairs in document order, keys stored as raw quoted tokens.
    /// Duplicates are KEPT (sqlite keeps them: `json('{"a":1,"a":2}')` is
    /// unchanged) and the first match wins on lookup.
    Obj(Vec<(Cow<'a, str>, Node<'a>)>),
}

impl Node<'_> {
    /// sqlite's `json_type()` name for this node.
    fn type_name(&self) -> &'static str {
        match self {
            Node::Null => "null",
            Node::True => "true",
            Node::False => "false",
            // sqlite distinguishes integer from real by the token's SHAPE, not
            // its value: `1.0` is real, `1` is integer.
            Node::Num(t) => {
                if is_integral_token(t) {
                    "integer"
                } else {
                    "real"
                }
            }
            Node::Str(_) => "text",
            Node::Arr(_) => "array",
            Node::Obj(_) => "object",
        }
    }

    fn render(&self, out: &mut String) {
        match self {
            Node::Null => out.push_str("null"),
            Node::True => out.push_str("true"),
            Node::False => out.push_str("false"),
            Node::Num(t) | Node::Str(t) => out.push_str(t),
            Node::Arr(items) => {
                out.push('[');
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    it.render(out);
                }
                out.push(']');
            }
            Node::Obj(pairs) => {
                out.push('{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(k);
                    out.push(':');
                    v.render(out);
                }
                out.push('}');
            }
        }
    }

    fn to_text(&self) -> String {
        let mut s = String::new();
        self.render(&mut s);
        s
    }

    /// The SQL value this node UNWRAPS to — what `json_extract()` with one path
    /// and the `->>` operator return.
    ///
    /// A container comes back as its minified JSON text; a JSON string as its
    /// DECODED characters; `true`/`false` as the integers 1/0 (sqlite has no
    /// boolean, and this is observably an integer there); `null` as SQL NULL.
    fn to_sql(&self) -> Result<Value> {
        Ok(match self {
            Node::Null => Value::Null,
            Node::True => Value::Int(1),
            Node::False => Value::Int(0),
            Node::Num(t) => number_to_value(t),
            Node::Str(t) => Value::Text(decode_string(t)?),
            Node::Arr(_) | Node::Obj(_) => Value::Text(self.to_text()),
        })
    }
}

/// Does this number token denote an INTEGER to sqlite (no `.`, no exponent)?
fn is_integral_token(t: &str) -> bool {
    !t.bytes().any(|c| matches!(c, b'.' | b'e' | b'E'))
}

/// A number token as a SQL value, following sqlite's rule exactly: a token with
/// no `.` and no exponent that fits in an i64 is an INTEGER; everything else
/// (including `9223372036854775808`, which overflows) is a REAL.
fn number_to_value(t: &str) -> Value {
    if is_integral_token(t) {
        if let Ok(i) = t.parse::<i64>() {
            return Value::Int(i);
        }
    }
    Value::Float(atof(t.as_bytes()))
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

struct P<'a> {
    b: &'a [u8],
    s: &'a str,
    i: usize,
}

fn is_ws(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

impl<'a> P<'a> {
    fn skip_ws(&mut self) {
        while self.i < self.b.len() && is_ws(self.b[self.i]) {
            self.i += 1;
        }
    }

    fn peek(&self) -> PResult<u8> {
        self.b.get(self.i).copied().ok_or(ParseFail::Malformed)
    }

    /// Consume a `"…"` token, validating its escapes, and return the raw slice
    /// INCLUDING both quotes.
    fn string(&mut self) -> PResult<&'a str> {
        let start = self.i;
        if self.peek()? != b'"' {
            return Err(ParseFail::Malformed);
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
                                if !self.peek()?.is_ascii_hexdigit() {
                                    return Err(ParseFail::Malformed);
                                }
                                self.i += 1;
                            }
                        }
                        _ => return Err(ParseFail::Malformed),
                    }
                }
                // A raw control character is not legal inside a JSON string.
                0x00..=0x1f => return Err(ParseFail::Malformed),
                _ => {}
            }
        }
        // Bounded by two ASCII quotes, so both ends are char boundaries.
        Ok(&self.s[start..self.i])
    }

    /// Consume `-? (0 | [1-9][0-9]*) (. [0-9]+)? ([eE] [+-]? [0-9]+)?` and
    /// return the raw token. Strict RFC 8259: no leading `+`, no bare `.5`, no
    /// trailing `1.`, no hex — all of those are the refused JSON5 forms.
    fn number(&mut self) -> PResult<&'a str> {
        let start = self.i;
        if self.peek()? == b'-' {
            self.i += 1;
        }
        match self.peek()? {
            b'0' => self.i += 1,
            b'1'..=b'9' => {
                while self.b.get(self.i).is_some_and(u8::is_ascii_digit) {
                    self.i += 1;
                }
            }
            _ => return Err(ParseFail::Malformed),
        }
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            if !self.b.get(self.i).is_some_and(u8::is_ascii_digit) {
                return Err(ParseFail::Malformed);
            }
            while self.b.get(self.i).is_some_and(u8::is_ascii_digit) {
                self.i += 1;
            }
        }
        if matches!(self.b.get(self.i), Some(b'e') | Some(b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+') | Some(b'-')) {
                self.i += 1;
            }
            if !self.b.get(self.i).is_some_and(u8::is_ascii_digit) {
                return Err(ParseFail::Malformed);
            }
            while self.b.get(self.i).is_some_and(u8::is_ascii_digit) {
                self.i += 1;
            }
        }
        // ASCII-only token, so slicing is safe.
        Ok(&self.s[start..self.i])
    }

    fn keyword(&mut self, word: &str) -> PResult<()> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(())
        } else {
            Err(ParseFail::Malformed)
        }
    }

    /// Parse one value. Recursive, with `depth` capped at [`MAX_DEPTH`] so a
    /// pathologically nested document is a bounded ERROR, never a blown stack.
    fn value(&mut self, depth: usize) -> PResult<Node<'a>> {
        self.skip_ws();
        // The bound counts CONTAINERS, exactly as sqlite's does: a scalar leaf
        // inside 128 arrays is 128 levels, not 129.
        let container = matches!(self.peek()?, b'{' | b'[');
        if container && depth >= MAX_DEPTH {
            return Err(ParseFail::TooDeep);
        }
        Ok(match self.peek()? {
            b'{' => {
                self.i += 1;
                let mut pairs = Vec::new();
                self.skip_ws();
                if self.peek()? == b'}' {
                    self.i += 1;
                    return Ok(Node::Obj(pairs));
                }
                loop {
                    self.skip_ws();
                    let k = self.string()?;
                    self.skip_ws();
                    if self.peek()? != b':' {
                        return Err(ParseFail::Malformed);
                    }
                    self.i += 1;
                    let v = self.value(depth + 1)?;
                    pairs.push((Cow::Borrowed(k), v));
                    self.skip_ws();
                    match self.peek()? {
                        b',' => self.i += 1,
                        b'}' => {
                            self.i += 1;
                            break;
                        }
                        _ => return Err(ParseFail::Malformed),
                    }
                }
                Node::Obj(pairs)
            }
            b'[' => {
                self.i += 1;
                let mut items = Vec::new();
                self.skip_ws();
                if self.peek()? == b']' {
                    self.i += 1;
                    return Ok(Node::Arr(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.skip_ws();
                    match self.peek()? {
                        b',' => self.i += 1,
                        b']' => {
                            self.i += 1;
                            break;
                        }
                        _ => return Err(ParseFail::Malformed),
                    }
                }
                Node::Arr(items)
            }
            b'"' => Node::Str(Cow::Borrowed(self.string()?)),
            b't' => {
                self.keyword("true")?;
                Node::True
            }
            b'f' => {
                self.keyword("false")?;
                Node::False
            }
            b'n' => {
                self.keyword("null")?;
                Node::Null
            }
            b'-' | b'0'..=b'9' => Node::Num(Cow::Borrowed(self.number()?)),
            _ => return Err(ParseFail::Malformed),
        })
    }
}

/// Parse a complete document: one value, optional surrounding whitespace,
/// nothing after it.
fn parse_raw(s: &str) -> PResult<Node<'_>> {
    let mut p = P {
        b: s.as_bytes(),
        s,
        i: 0,
    };
    let n = p.value(0)?;
    p.skip_ws();
    if p.i != p.b.len() {
        return Err(ParseFail::Malformed);
    }
    Ok(n)
}

/// [`parse_raw`] with the failure turned into the engine's error. Every JSON
/// function except `json_valid` uses this one.
fn parse(s: &str) -> Result<Node<'_>> {
    parse_raw(s).map_err(Error::from)
}

/// Decode a raw string token (quotes included) into its characters.
fn decode_string(raw: &str) -> Result<String> {
    let b = raw.as_bytes();
    // `parse` proved the token is well-formed and quote-delimited.
    debug_assert!(b.len() >= 2 && b[0] == b'"' && b[b.len() - 1] == b'"');
    let inner = &raw[1..raw.len() - 1];
    if !inner.contains('\\') {
        return Ok(inner.to_string());
    }
    let ib = inner.as_bytes();
    let mut out = String::with_capacity(inner.len());
    let mut i = 0usize;
    while i < ib.len() {
        if ib[i] != b'\\' {
            // Copy the whole UTF-8 sequence that starts here.
            let start = i;
            i += 1;
            while i < ib.len() && (ib[i] & 0xc0) == 0x80 {
                i += 1;
            }
            out.push_str(&inner[start..i]);
            continue;
        }
        i += 1;
        let e = ib[i];
        i += 1;
        match e {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let hi = hex4(&ib[i..i + 4]);
                i += 4;
                let cp = if (0xd800..0xdc00).contains(&hi) {
                    // A high surrogate must be followed by `\uDC00..\uDFFF`.
                    if ib.len() >= i + 6 && ib[i] == b'\\' && ib[i + 1] == b'u' {
                        let lo = hex4(&ib[i + 2..i + 6]);
                        if (0xdc00..0xe000).contains(&lo) {
                            i += 6;
                            0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                        } else {
                            return Err(lone_surrogate(hi));
                        }
                    } else {
                        return Err(lone_surrogate(hi));
                    }
                } else if (0xdc00..0xe000).contains(&hi) {
                    return Err(lone_surrogate(hi));
                } else {
                    hi
                };
                // Every non-surrogate scalar below 0x110000 is a valid char.
                out.push(char::from_u32(cp).ok_or_else(malformed)?);
            }
            // `parse` rejected every other escape.
            _ => return Err(malformed()),
        }
    }
    Ok(out)
}

fn hex4(b: &[u8]) -> u32 {
    let mut v = 0u32;
    for &c in b.iter().take(4) {
        v = v * 16 + (c as char).to_digit(16).unwrap_or(0);
    }
    v
}

fn lone_surrogate(cp: u32) -> Error {
    Error::TypeMismatch(format!(
        "json: the string contains the unpaired surrogate escape \\u{cp:04x}; sqlite emits it \
         as three raw bytes that are not valid UTF-8, which mpedb's TEXT cannot hold, so the \
         value is refused rather than silently repaired"
    ))
}

// ---------------------------------------------------------------------------
// Path expressions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Step {
    /// `.key` / `."key"` — the key text VERBATIM (see the module docs on why a
    /// backslash is refused).
    Key(String),
    /// `[N]`
    Index(u32),
    /// `[#-N]` — the Nth element counting back from the end (`#-1` is last).
    FromEnd(u32),
    /// `[#]` — one past the end. Never matches on lookup (sqlite:
    /// `json_extract('[1]','$[#]')` is NULL); it is the append position for
    /// `json_set`/`json_insert`.
    Append,
}

/// Parse a `$`-rooted path, exactly as far as sqlite's grammar goes and no
/// further.
fn parse_path(p: &str) -> Result<Vec<Step>> {
    let b = p.as_bytes();
    if b.first() != Some(&b'$') {
        return Err(bad_path(p));
    }
    let mut steps = Vec::new();
    let mut i = 1usize;
    while i < b.len() {
        match b[i] {
            b'.' => {
                i += 1;
                let key = if b.get(i) == Some(&b'"') {
                    i += 1;
                    let start = i;
                    while i < b.len() && b[i] != b'"' {
                        i += 1;
                    }
                    if i >= b.len() {
                        return Err(bad_path(p));
                    }
                    let k = &p[start..i];
                    i += 1;
                    k
                } else {
                    let start = i;
                    while i < b.len() && b[i] != b'.' && b[i] != b'[' {
                        i += 1;
                    }
                    &p[start..i]
                };
                if key.contains('\\') {
                    return Err(Error::TypeMismatch(format!(
                        "bad JSON path: '{p}' — mpedb refuses a path key containing a \
                         backslash. sqlite 3.45 compares a path key against the DECODED \
                         document label but takes the path key itself verbatim, so `$.\"a\\\"b\"` \
                         matches nothing while `$.a\"b` matches {{\"a\\\"b\":1}}; that asymmetry \
                         is not reproduced"
                    )));
                }
                steps.push(Step::Key(key.to_string()));
            }
            b'[' => {
                i += 1;
                if b.get(i) == Some(&b'#') {
                    i += 1;
                    if b.get(i) == Some(&b']') {
                        i += 1;
                        steps.push(Step::Append);
                        continue;
                    }
                    if b.get(i) != Some(&b'-') {
                        return Err(bad_path(p));
                    }
                    i += 1;
                    let n = read_u32(b, &mut i).ok_or_else(|| bad_path(p))?;
                    if b.get(i) != Some(&b']') {
                        return Err(bad_path(p));
                    }
                    i += 1;
                    steps.push(Step::FromEnd(n));
                } else {
                    let n = read_u32(b, &mut i).ok_or_else(|| bad_path(p))?;
                    if b.get(i) != Some(&b']') {
                        return Err(bad_path(p));
                    }
                    i += 1;
                    steps.push(Step::Index(n));
                }
            }
            _ => return Err(bad_path(p)),
        }
    }
    Ok(steps)
}

/// Read one or more ASCII digits, saturating at `u32::MAX` (an index that large
/// cannot match anything, and saturating keeps a hostile path from wrapping).
fn read_u32(b: &[u8], i: &mut usize) -> Option<u32> {
    let start = *i;
    let mut v: u32 = 0;
    while b.get(*i).is_some_and(u8::is_ascii_digit) {
        v = v.saturating_mul(10).saturating_add((b[*i] - b'0') as u32);
        *i += 1;
    }
    if *i == start {
        None
    } else {
        Some(v)
    }
}

/// Resolve `steps` against `n`, or `None` when the path selects nothing.
fn lookup<'n, 'a>(n: &'n Node<'a>, steps: &[Step]) -> Option<&'n Node<'a>> {
    let mut cur = n;
    for st in steps {
        cur = match (st, cur) {
            (Step::Key(k), Node::Obj(pairs)) => {
                // First match wins — sqlite keeps duplicate keys and takes the
                // first (`json_extract('{"a":1,"a":2}','$.a')` is 1).
                pairs
                    .iter()
                    .find(|(rk, _)| label_eq(rk, k))
                    .map(|(_, v)| v)?
            }
            (Step::Index(idx), Node::Arr(items)) => items.get(*idx as usize)?,
            (Step::FromEnd(back), Node::Arr(items)) => {
                let len = items.len();
                if *back as usize > len || *back == 0 {
                    return None;
                }
                &items[len - *back as usize]
            }
            // `[#]` is one past the end: never a lookup hit.
            (Step::Append, _) => return None,
            // A key step into an array, or an index step into an object, simply
            // finds nothing (sqlite returns NULL rather than erroring).
            _ => return None,
        };
    }
    Some(cur)
}

/// Does the raw document label `raw` (quotes included) denote exactly `key`?
fn label_eq(raw: &str, key: &str) -> bool {
    let inner = &raw[1..raw.len() - 1];
    if !inner.contains('\\') {
        return inner == key;
    }
    decode_string(raw).is_ok_and(|d| d == key)
}

// ---------------------------------------------------------------------------
// SQL value -> JSON
// ---------------------------------------------------------------------------

/// Render a SQL value as a JSON value, exactly as sqlite's `jsonAppendValue`
/// does. This is the "values entering a document" half of the contract.
fn value_to_node<'a>(v: &Value, what: &str) -> Result<Node<'a>> {
    Ok(match v {
        Value::Null => Node::Null,
        Value::Int(i) => Node::Num(Cow::Owned(i.to_string())),
        // mpedb's Timestamp has no sqlite counterpart; it renders as the
        // integer it renders as everywhere else (`quote`, `||`, CAST).
        Value::Timestamp(t) => Node::Num(Cow::Owned(t.to_string())),
        // JSON *has* booleans, and mpedb's Bool is a first-class type with no
        // sqlite counterpart to agree or disagree with, so it becomes a JSON
        // boolean rather than sqlite's 1/0 integer.
        Value::Bool(true) => Node::True,
        Value::Bool(false) => Node::False,
        Value::Float(x) => {
            // A non-finite REAL is the one number sqlite renders INCONSISTENTLY
            // with itself: `json_quote(9e999)` and `json_array(9e999)` give
            // `9.0e+999`, but `json_set('{}','$.a',9e999)` gives `9e999` — the
            // text and JSONB writers disagree. Neither is more right, so mpedb
            // refuses rather than pick one. (`CAST(9e999 AS TEXT)` is a third
            // answer again, `Inf`, which is not even valid JSON.)
            if !x.is_finite() {
                return Err(Error::TypeMismatch(format!(
                    "{what}: the real {x} has no JSON number form — JSON has no infinity or \
                     NaN literal, and sqlite renders it two different ways in two different \
                     JSON writers (`9.0e+999` from json_quote/json_array, `9e999` from \
                     json_set), so mpedb refuses rather than pick one"
                )));
            }
            // Otherwise the JSON writer's rendering IS `CAST(x AS TEXT)`'s
            // (`1e3` -> `1000.0`, `1e300` -> `1.0e+300`, `-0.0` -> `0.0`),
            // which mpedb already reproduces bit-for-bit.
            Node::Num(Cow::Owned(
                String::from_utf8(float_to_text(*x))
                    .map_err(|_| Error::Internal("float_to_text is ASCII".into()))?,
            ))
        }
        Value::Text(s) => Node::Str(Cow::Owned(quote_json_string(s))),
        Value::Blob(_) => {
            return Err(Error::TypeMismatch(format!(
                "{what}: JSON cannot hold BLOB values"
            )))
        }
        Value::List(_) => {
            return Err(Error::TypeMismatch(format!(
                "{what}: a list value has no JSON representation"
            )))
        }
    })
}

/// sqlite's `jsonAppendString`: `"` and `\` are backslash-escaped, the five
/// named control characters get their short escapes, every other character
/// below 0x20 becomes `\u00xx` (lowercase hex), and everything else — including
/// 0x7f and all non-ASCII — passes through verbatim as UTF-8.
fn quote_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// The functions
// ---------------------------------------------------------------------------

/// The TEXT argument of a document-reading function.
fn doc_text<'a>(v: &'a Value, f: &str) -> Result<&'a str> {
    match v {
        Value::Text(s) => Ok(s.as_str()),
        Value::Blob(_) => Err(Error::TypeMismatch(format!(
            "{f}(): a BLOB argument would be read as sqlite 3.45's JSONB binary encoding, \
             which mpedb does not implement"
        ))),
        other => Err(Error::TypeMismatch(format!(
            "{f}() expects JSON text, got {}",
            other.type_name()
        ))),
    }
}

fn path_text<'a>(v: &'a Value, f: &str) -> Result<&'a str> {
    match v {
        Value::Text(s) => Ok(s.as_str()),
        other => Err(Error::TypeMismatch(format!(
            "{f}(): a JSON path must be text, got {}",
            other.type_name()
        ))),
    }
}

/// `json(X)` — validate and minify.
pub(super) fn json(args: &[Value]) -> Result<Value> {
    let s = doc_text(&args[0], "json")?;
    Ok(Value::Text(parse(s)?.to_text()))
}

/// `json_valid(X[, FLAGS])`.
///
/// FLAGS is sqlite 3.45's grammar bitmask: 1 = strict RFC 8259 text,
/// 2 = JSON5 text, 4 = JSONB that "superficially appears" valid, 8 = strictly
/// valid JSONB; the value must be between 1 and 15. mpedb implements grammar
/// 1 and refuses every other bit BY NAME — 2 is the JSON5 that `json()` also
/// refuses, and 4/8 are JSONB, which is out of scope entirely.
pub(super) fn json_valid(args: &[Value]) -> Result<Value> {
    if let Some(f) = args.get(1) {
        let flags = match f {
            Value::Int(i) => *i,
            // A NULL FLAGS is sqlite's out-of-range error, NOT a NULL result:
            // `json_valid('[1]', NULL)` raises. That is why `json_valid` runs
            // ahead of the null gate.
            Value::Null => -1,
            other => {
                return Err(Error::TypeMismatch(format!(
                    "json_valid(): FLAGS must be an integer, got {}",
                    other.type_name()
                )))
            }
        };
        if !(1..=15).contains(&flags) {
            return Err(Error::TypeMismatch(
                "FLAGS parameter to json_valid() must be between 1 and 15".into(),
            ));
        }
        if flags != 1 {
            return Err(Error::TypeMismatch(format!(
                "json_valid(X, {flags}): mpedb implements grammar bit 1 (strict RFC 8259 text) \
                 only. Bit 2 is JSON5, which mpedb refuses rather than accepts; bits 4 and 8 are \
                 sqlite 3.45's JSONB binary encoding, which mpedb does not implement"
            )));
        }
    }
    Ok(match &args[0] {
        Value::Null => Value::Null,
        // A malformed document is 0, matching sqlite — but one that is merely
        // too deep RAISES, because sqlite would have said 1.
        Value::Text(s) => match parse_raw(s) {
            Ok(_) => Value::Int(1),
            Err(ParseFail::Malformed) => Value::Int(0),
            Err(f) => return Err(Error::from(f)),
        },
        // A number's own text IS a JSON document, so sqlite answers 1 without
        // parsing anything (`json_valid(5)` and `json_valid(5.5)` are both 1).
        Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Timestamp(_) => Value::Int(1),
        // sqlite reads a BLOB as JSONB and answers 0 for anything that is not
        // one under grammar bit 1; mpedb agrees on the answer without needing
        // JSONB (`json_valid(x'6162')` is 0 in both).
        Value::Blob(_) => Value::Int(0),
        Value::List(_) => Value::Int(0),
    })
}

/// `json_type(X)` / `json_type(X, PATH)`.
pub(super) fn json_type(args: &[Value]) -> Result<Value> {
    let s = doc_text(&args[0], "json_type")?;
    let doc = parse(s)?;
    let node = match args.get(1) {
        None => Some(&doc),
        Some(p) => {
            let steps = parse_path(path_text(p, "json_type")?)?;
            lookup(&doc, &steps)
        }
    };
    Ok(match node {
        Some(n) => Value::Text(n.type_name().to_string()),
        None => Value::Null,
    })
}

/// `json_array_length(X)` / `json_array_length(X, PATH)`. A non-array is 0; a
/// path that selects nothing is NULL (sqlite's rule, verified).
pub(super) fn json_array_length(args: &[Value]) -> Result<Value> {
    let s = doc_text(&args[0], "json_array_length")?;
    let doc = parse(s)?;
    let node = match args.get(1) {
        None => Some(&doc),
        Some(p) => {
            let steps = parse_path(path_text(p, "json_array_length")?)?;
            lookup(&doc, &steps)
        }
    };
    Ok(match node {
        Some(Node::Arr(items)) => Value::Int(items.len() as i64),
        Some(_) => Value::Int(0),
        None => Value::Null,
    })
}

/// `json_extract(X, PATH, …)`.
///
/// With ONE path the selected node is unwrapped to a SQL value; with more than
/// one the results are wrapped in a JSON array (missing paths become `null`),
/// which is sqlite's documented and verified behaviour.
pub(super) fn json_extract(args: &[Value]) -> Result<Value> {
    let s = doc_text(&args[0], "json_extract")?;
    let doc = parse(s)?;
    // sqlite validates the document and answers NULL when no path is given.
    if args.len() == 1 {
        return Ok(Value::Null);
    }
    if args.len() == 2 {
        let steps = parse_path(path_text(&args[1], "json_extract")?)?;
        return match lookup(&doc, &steps) {
            Some(n) => n.to_sql(),
            None => Ok(Value::Null),
        };
    }
    let mut out = String::from("[");
    for (i, p) in args[1..].iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let steps = parse_path(path_text(p, "json_extract")?)?;
        match lookup(&doc, &steps) {
            Some(n) => n.render(&mut out),
            None => out.push_str("null"),
        }
    }
    out.push(']');
    Ok(Value::Text(out))
}

/// The abbreviated path grammar the `->` and `->>` OPERATORS accept (and
/// `json_extract` does NOT — `json_extract('{"a":1}', 'a')` is an error in
/// sqlite, while `'{"a":1}' -> 'a'` is 1).
///
/// * an INTEGER `N` becomes `$[N]`, and a NEGATIVE one selects nothing (sqlite
///   answers NULL there rather than raising a bad-path error);
/// * text starting with `$` is a path already;
/// * text starting with `[` is prefixed with `$`;
/// * anything else is a whole quoted LABEL — `'a.b'` means `$."a.b"`, not
///   `$.a.b`. Verified against the binary: `'{"a.b":1}' -> 'a.b'` is 1 while
///   `'{"a":{"b":9}}' -> 'a.b'` is NULL.
fn arrow_steps(v: &Value, op: &str) -> Result<Option<Vec<Step>>> {
    match v {
        Value::Int(n) => {
            if *n < 0 {
                return Ok(None);
            }
            let n = u32::try_from(*n).unwrap_or(u32::MAX);
            Ok(Some(vec![Step::Index(n)]))
        }
        Value::Text(t) => {
            if t.starts_with('$') {
                Ok(Some(parse_path(t)?))
            } else if t.starts_with('[') {
                Ok(Some(parse_path(&format!("${t}"))?))
            } else if t.contains('\\') || t.contains('"') {
                Err(Error::TypeMismatch(format!(
                    "`{op}`: the abbreviated label `{t}` contains a quote or backslash; \
                     mpedb refuses it rather than guess how sqlite re-quotes it — write the \
                     full path instead"
                )))
            } else {
                Ok(Some(vec![Step::Key(t.clone())]))
            }
        }
        other => Err(Error::TypeMismatch(format!(
            "`{op}`: the right operand must be a JSON path (text) or an array index (integer), \
             got {}",
            other.type_name()
        ))),
    }
}

/// `X -> Y` — the selected node's JSON TEXT (`'{"a":"s"}' -> '$.a'` is the
/// three characters `"s"`), or SQL NULL when the path selects nothing. Note
/// that a JSON `null` comes back as the four-character text `null`, which is
/// exactly how `->` differs from `->>`.
pub(super) fn json_arrow(args: &[Value]) -> Result<Value> {
    let s = doc_text(&args[0], "->")?;
    let doc = parse(s)?;
    let Some(steps) = arrow_steps(&args[1], "->")? else {
        return Ok(Value::Null);
    };
    Ok(match lookup(&doc, &steps) {
        Some(n) => Value::Text(n.to_text()),
        None => Value::Null,
    })
}

/// `X ->> Y` — the selected node as a SQL value (`'{"a":"s"}' ->> '$.a'` is the
/// one character `s`, and a JSON `null` is SQL NULL).
pub(super) fn json_arrow_text(args: &[Value]) -> Result<Value> {
    let s = doc_text(&args[0], "->>")?;
    let doc = parse(s)?;
    let Some(steps) = arrow_steps(&args[1], "->>")? else {
        return Ok(Value::Null);
    };
    match lookup(&doc, &steps) {
        Some(n) => n.to_sql(),
        None => Ok(Value::Null),
    }
}

/// `json_quote(X)` — X as a JSON value.
///
/// Reached ONLY for an argument that is not already JSON: the binder rewrites
/// `json_quote(<a JSON-producing call>)` to the call itself, because sqlite's
/// `json_quote` passes a JSON-subtyped value straight through and every such
/// call already yields minified JSON text. See `binder::json_ness`.
pub(super) fn json_quote(args: &[Value]) -> Result<Value> {
    let mut out = String::new();
    value_to_node(&args[0], "json_quote()")?.render(&mut out);
    Ok(Value::Text(out))
}

/// The leading bitmask argument the binder prepends to the value-taking JSON
/// writers: bit `k` is set when the `k`-th VALUE argument is already JSON text
/// (sqlite's JSON subtype) and must be spliced raw rather than quoted.
fn json_mask(v: &Value) -> Result<u64> {
    match v {
        Value::Int(i) => Ok(*i as u64),
        _ => Err(Error::Internal(
            "JSON writer called without its binder-supplied subtype mask".into(),
        )),
    }
}

/// Turn one value argument into a node, honouring the subtype mask bit.
fn arg_node<'a>(v: &Value, is_json: bool, what: &str) -> Result<Node<'a>> {
    if !is_json {
        return value_to_node(v, what);
    }
    // A JSON-subtyped argument is a document; splice it in, re-parsed so a
    // corrupt one is an error rather than a document that stops being JSON.
    match v {
        Value::Null => Ok(Node::Null),
        Value::Text(s) => Ok(own(parse(s)?)),
        other => Err(Error::TypeMismatch(format!(
            "{what}: expected JSON text, got {}",
            other.type_name()
        ))),
    }
}

/// Detach a node from the buffer it borrows, so it can outlive it.
fn own<'b>(n: Node<'_>) -> Node<'b> {
    match n {
        Node::Null => Node::Null,
        Node::True => Node::True,
        Node::False => Node::False,
        Node::Num(t) => Node::Num(Cow::Owned(t.into_owned())),
        Node::Str(t) => Node::Str(Cow::Owned(t.into_owned())),
        Node::Arr(items) => Node::Arr(items.into_iter().map(own).collect()),
        Node::Obj(pairs) => Node::Obj(
            pairs
                .into_iter()
                .map(|(k, v)| (Cow::Owned(k.into_owned()), own(v)))
                .collect(),
        ),
    }
}

/// `json_array(MASK, …)` — MASK is the binder-supplied subtype bitmask.
pub(super) fn json_array(args: &[Value]) -> Result<Value> {
    let mask = json_mask(&args[0])?;
    let mut out = String::from("[");
    for (k, a) in args[1..].iter().enumerate() {
        if k > 0 {
            out.push(',');
        }
        arg_node(a, mask >> k & 1 == 1, "json_array()")?.render(&mut out);
    }
    out.push(']');
    Ok(Value::Text(out))
}

/// `json_object(MASK, LABEL, VALUE, …)`. Mask bit `k` covers the `k`-th VALUE.
pub(super) fn json_object(args: &[Value]) -> Result<Value> {
    let mask = json_mask(&args[0])?;
    let rest = &args[1..];
    if !rest.len().is_multiple_of(2) {
        return Err(Error::TypeMismatch(
            "json_object() requires an even number of arguments".into(),
        ));
    }
    let mut out = String::from("{");
    for (k, pair) in rest.chunks_exact(2).enumerate() {
        if k > 0 {
            out.push(',');
        }
        let Value::Text(label) = &pair[0] else {
            return Err(Error::TypeMismatch(
                "json_object() labels must be TEXT".into(),
            ));
        };
        out.push_str(&quote_json_string(label));
        out.push(':');
        arg_node(&pair[1], mask >> k & 1 == 1, "json_object()")?.render(&mut out);
    }
    out.push('}');
    Ok(Value::Text(out))
}

/// `json_remove(X, PATH, …)` — paths applied left to right.
pub(super) fn json_remove(args: &[Value]) -> Result<Value> {
    // A NULL document, or a NULL PATH anywhere, makes the whole call NULL —
    // unlike `json_set`, which silently SKIPS a NULL path (both verified).
    if args.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    let s = doc_text(&args[0], "json_remove")?;
    let mut doc = Some(own(parse(s)?));
    for p in &args[1..] {
        let steps = parse_path(path_text(p, "json_remove")?)?;
        let Some(d) = doc.as_mut() else { break };
        if steps.is_empty() {
            // `json_remove(X, '$')` removes the whole document: NULL.
            doc = None;
            continue;
        }
        remove_at(d, &steps);
    }
    Ok(match doc {
        Some(d) => Value::Text(d.to_text()),
        None => Value::Null,
    })
}

/// Delete the node `steps` selects, if it exists.
fn remove_at(root: &mut Node<'_>, steps: &[Step]) {
    let (last, parents) = steps.split_last().expect("non-empty");
    let Some(parent) = lookup_mut(root, parents) else {
        return;
    };
    match (last, parent) {
        (Step::Key(k), Node::Obj(pairs)) => {
            if let Some(i) = pairs.iter().position(|(rk, _)| label_eq(rk, k)) {
                pairs.remove(i);
            }
        }
        (Step::Index(idx), Node::Arr(items)) => {
            if (*idx as usize) < items.len() {
                items.remove(*idx as usize);
            }
        }
        (Step::FromEnd(back), Node::Arr(items)) => {
            let len = items.len();
            if *back > 0 && (*back as usize) <= len {
                items.remove(len - *back as usize);
            }
        }
        _ => {}
    }
}

fn lookup_mut<'n, 'a>(n: &'n mut Node<'a>, steps: &[Step]) -> Option<&'n mut Node<'a>> {
    let mut cur = n;
    for st in steps {
        cur = match (st, cur) {
            (Step::Key(k), Node::Obj(pairs)) => {
                let i = pairs.iter().position(|(rk, _)| label_eq(rk, k))?;
                &mut pairs[i].1
            }
            (Step::Index(idx), Node::Arr(items)) => items.get_mut(*idx as usize)?,
            (Step::FromEnd(back), Node::Arr(items)) => {
                let len = items.len();
                if *back == 0 || *back as usize > len {
                    return None;
                }
                &mut items[len - *back as usize]
            }
            _ => return None,
        };
    }
    Some(cur)
}

/// Which of the three edit modes a `json_set`/`json_insert`/`json_replace`
/// call is in.
#[derive(Clone, Copy, PartialEq)]
enum Edit {
    /// `json_set`: overwrite if present, create if not.
    Set,
    /// `json_insert`: create only; leave an existing node alone.
    Insert,
    /// `json_replace`: overwrite only; create nothing.
    Replace,
}

/// `json_set/insert/replace(MASK, X, PATH, VALUE, …)`.
fn json_edit(mode: Edit, args: &[Value], name: &str) -> Result<Value> {
    let mask = json_mask(&args[0])?;
    // A NULL document propagates; a NULL VALUE does not (it becomes JSON
    // `null`), and a NULL PATH silently skips its pair — all three verified.
    if args[1].is_null() {
        return Ok(Value::Null);
    }
    let s = doc_text(&args[1], name)?;
    let rest = &args[2..];
    if !rest.len().is_multiple_of(2) {
        return Err(Error::TypeMismatch(format!(
            "{name}() needs an odd number of arguments"
        )));
    }
    let mut doc = own(parse(s)?);
    for (k, pair) in rest.chunks_exact(2).enumerate() {
        if pair[0].is_null() {
            continue;
        }
        let steps = parse_path(path_text(&pair[0], name)?)?;
        let new = arg_node(&pair[1], mask >> k & 1 == 1, name)?;
        if steps.is_empty() {
            // `$` addresses the whole document: set/replace overwrite it,
            // insert leaves it alone (there is nothing to create).
            if mode != Edit::Insert {
                doc = new;
            }
            continue;
        }
        edit_at(&mut doc, &steps, new, mode);
    }
    Ok(Value::Text(doc.to_text()))
}

pub(super) fn json_set(args: &[Value]) -> Result<Value> {
    json_edit(Edit::Set, args, "json_set")
}
pub(super) fn json_insert(args: &[Value]) -> Result<Value> {
    json_edit(Edit::Insert, args, "json_insert")
}
pub(super) fn json_replace(args: &[Value]) -> Result<Value> {
    json_edit(Edit::Replace, args, "json_replace")
}

/// Apply one (path, value) edit.
///
/// `json_set`/`json_insert` create missing intermediate OBJECT levels
/// (`json_set('{"a":1}','$.b.c',9)` is `{"a":1,"b":{"c":9}}`) but never grow an
/// array by more than one element (`json_set('[1,2,3]','$[5]',9)` is unchanged).
fn edit_at<'a>(root: &mut Node<'a>, steps: &[Step], new: Node<'a>, mode: Edit) {
    let (last, parents) = steps.split_last().expect("non-empty");
    let parent = if mode == Edit::Replace {
        match lookup_mut(root, parents) {
            Some(p) => p,
            None => return,
        }
    } else {
        match ensure_path(root, parents) {
            Some(p) => p,
            None => return,
        }
    };
    match (last, parent) {
        (Step::Key(k), Node::Obj(pairs)) => {
            match pairs.iter().position(|(rk, _)| label_eq(rk, k)) {
                Some(i) => {
                    if mode != Edit::Insert {
                        pairs[i].1 = new;
                    }
                }
                None => {
                    if mode != Edit::Replace {
                        pairs.push((Cow::Owned(quote_json_string(k)), new));
                    }
                }
            }
        }
        (Step::Index(idx), Node::Arr(items)) => {
            let i = *idx as usize;
            if i < items.len() {
                if mode != Edit::Insert {
                    items[i] = new;
                }
            } else if i == items.len() && mode != Edit::Replace {
                items.push(new);
            }
        }
        (Step::FromEnd(back), Node::Arr(items)) => {
            let len = items.len();
            if *back > 0 && (*back as usize) <= len && mode != Edit::Insert {
                items[len - *back as usize] = new;
            }
        }
        (Step::Append, Node::Arr(items)) => {
            if mode != Edit::Replace {
                items.push(new);
            }
        }
        _ => {}
    }
}

/// Walk `steps`, creating missing OBJECT levels on the way (what `json_set`
/// and `json_insert` do). Returns `None` when a step cannot be created —
/// an array index that is not exactly one past the end, or a key step into a
/// non-object.
fn ensure_path<'n, 'a>(n: &'n mut Node<'a>, steps: &[Step]) -> Option<&'n mut Node<'a>> {
    let mut cur = n;
    for st in steps {
        cur = match (st, cur) {
            (Step::Key(k), Node::Obj(pairs)) => {
                let i = match pairs.iter().position(|(rk, _)| label_eq(rk, k)) {
                    Some(i) => i,
                    None => {
                        pairs.push((Cow::Owned(quote_json_string(k)), Node::Obj(Vec::new())));
                        pairs.len() - 1
                    }
                };
                &mut pairs[i].1
            }
            (Step::Index(idx), Node::Arr(items)) => {
                let i = *idx as usize;
                if i == items.len() {
                    items.push(Node::Obj(Vec::new()));
                }
                items.get_mut(i)?
            }
            (Step::FromEnd(back), Node::Arr(items)) => {
                let len = items.len();
                if *back == 0 || *back as usize > len {
                    return None;
                }
                &mut items[len - *back as usize]
            }
            (Step::Append, Node::Arr(items)) => {
                items.push(Node::Obj(Vec::new()));
                items.last_mut()?
            }
            _ => return None,
        };
    }
    Some(cur)
}

/// `json_patch(TARGET, PATCH)` — RFC 7396 JSON Merge Patch, sqlite's semantics:
/// a non-object patch REPLACES the target outright, and a `null` member deletes
/// the corresponding key.
pub(super) fn json_patch(args: &[Value]) -> Result<Value> {
    let t = doc_text(&args[0], "json_patch")?;
    let p = doc_text(&args[1], "json_patch")?;
    let mut target = own(parse(t)?);
    let patch = own(parse(p)?);
    merge_patch(&mut target, patch);
    Ok(Value::Text(target.to_text()))
}

fn merge_patch<'a>(target: &mut Node<'a>, patch: Node<'a>) {
    let Node::Obj(members) = patch else {
        *target = patch;
        return;
    };
    if !matches!(target, Node::Obj(_)) {
        *target = Node::Obj(Vec::new());
    }
    let Node::Obj(pairs) = target else {
        unreachable!("just made it an object")
    };
    for (raw_key, val) in members {
        let key = match decode_string(&raw_key) {
            Ok(k) => k,
            // A key mpedb cannot decode (a lone surrogate) cannot be matched
            // against the target either; append it verbatim, which is what a
            // fresh key does anyway.
            Err(_) => {
                if !matches!(val, Node::Null) {
                    pairs.push((raw_key, val));
                }
                continue;
            }
        };
        let at = pairs.iter().position(|(rk, _)| label_eq(rk, &key));
        match (at, matches!(val, Node::Null)) {
            (Some(i), true) => {
                pairs.remove(i);
            }
            (Some(i), false) => merge_patch(&mut pairs[i].1, val),
            (None, true) => {}
            (None, false) => {
                let mut fresh = Node::Obj(Vec::new());
                merge_patch(&mut fresh, val);
                pairs.push((raw_key, fresh));
            }
        }
    }
}

#[cfg(test)]
mod tests;
