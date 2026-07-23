//! A Neo4j transactional-endpoint client in plain std.
//!
//! The whole dependency argument: this talks JSON over HTTP/1.0 to
//! 127.0.0.1, and the two things it needs — post statements, read back rows —
//! are a few hundred lines of std. A TLS/HTTP stack in the workspace to reach
//! localhost would be a poor trade. HTTP/1.0 on purpose: the server answers
//! with `Connection: close` framing and never chunks, so the body is simply
//! "the rest of the stream".

use std::io::{Read, Write};
use std::net::TcpStream;

pub struct Neo4j {
    addr: String,
    auth: String, // pre-computed `Basic <base64>`
}

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn arr(&self) -> &[Json] {
        match self {
            Json::Arr(v) => v,
            _ => &[],
        }
    }
}

impl Neo4j {
    pub fn new(addr: &str, user: &str, pass: &str) -> Neo4j {
        Neo4j {
            addr: addr.to_string(),
            auth: format!("Basic {}", base64(format!("{user}:{pass}").as_bytes())),
        }
    }

    /// POST one statement (with a raw JSON parameters object) to
    /// `/db/neo4j/tx/commit` and return the parsed response. Any entry in the
    /// response's `errors` array is an `Err` — a failed statement must never
    /// read as an empty result.
    pub fn call(&self, statement: &str, params: &str) -> Result<Json, String> {
        let body = format!(
            "{{\"statements\":[{{\"statement\":{},\"parameters\":{params}}}]}}",
            json_str(statement)
        );
        let req = format!(
            "POST /db/neo4j/tx/commit HTTP/1.0\r\nHost: {}\r\nAuthorization: {}\r\n\
             Content-Type: application/json\r\nAccept: application/json\r\n\
             Content-Length: {}\r\n\r\n{body}",
            self.addr,
            self.auth,
            body.len()
        );
        let mut s = TcpStream::connect(&self.addr).map_err(|e| format!("connect: {e}"))?;
        s.write_all(req.as_bytes()).map_err(|e| format!("send: {e}"))?;
        let mut resp = Vec::new();
        s.read_to_end(&mut resp).map_err(|e| format!("recv: {e}"))?;
        let text = String::from_utf8_lossy(&resp);
        let split = text.find("\r\n\r\n").ok_or("no header/body split in response")?;
        let json = parse(&text[split + 4..])?;
        let errors = json.get("errors").map(|e| e.arr().len()).unwrap_or(0);
        if errors > 0 {
            return Err(format!("neo4j error: {:?}", json.get("errors")));
        }
        Ok(json)
    }

    /// The `row` arrays of the first result, rendered canonically (sorted
    /// lines, `|` cells, floats to 2 decimals) — the same shape the mpedb side
    /// renders, so agreement is a string compare.
    pub fn rows(&self, statement: &str, params: &str) -> Result<String, String> {
        let json = self.call(statement, params)?;
        let results = json.get("results").ok_or("no results")?;
        let first = results.arr().first().ok_or("empty results")?;
        let mut out: Vec<String> = first
            .get("data")
            .map(|d| d.arr())
            .unwrap_or(&[])
            .iter()
            .map(|row| {
                row.get("row")
                    .map(|r| r.arr())
                    .unwrap_or(&[])
                    .iter()
                    .map(render)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect();
        out.sort();
        Ok(out.join("\n"))
    }
}

fn render(v: &Json) -> String {
    match v {
        Json::Null => "NULL".into(),
        Json::Bool(b) => b.to_string(),
        // Integral numbers render as integers — a count must compare equal to
        // mpedb's Int rendering, and every value this bench compares is far
        // inside f64's exact-integer range.
        Json::Num(n) if n.fract() == 0.0 && n.abs() < 9e15 => format!("{}", *n as i64),
        Json::Num(n) => format!("{n:.2}"),
        Json::Str(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Minimal JSON
// ---------------------------------------------------------------------------

fn parse(s: &str) -> Result<Json, String> {
    let b = s.as_bytes();
    let mut i = 0usize;
    let v = value(b, &mut i)?;
    Ok(v)
}

fn ws(b: &[u8], i: &mut usize) {
    while *i < b.len() && matches!(b[*i], b' ' | b'\t' | b'\r' | b'\n') {
        *i += 1;
    }
}

fn value(b: &[u8], i: &mut usize) -> Result<Json, String> {
    ws(b, i);
    match b.get(*i) {
        Some(b'{') => {
            *i += 1;
            let mut kv = Vec::new();
            ws(b, i);
            if b.get(*i) == Some(&b'}') {
                *i += 1;
                return Ok(Json::Obj(kv));
            }
            loop {
                ws(b, i);
                let Json::Str(k) = value(b, i)? else { return Err("object key not a string".into()) };
                ws(b, i);
                if b.get(*i) != Some(&b':') {
                    return Err("expected :".into());
                }
                *i += 1;
                kv.push((k, value(b, i)?));
                ws(b, i);
                match b.get(*i) {
                    Some(b',') => *i += 1,
                    Some(b'}') => {
                        *i += 1;
                        return Ok(Json::Obj(kv));
                    }
                    _ => return Err("expected , or }".into()),
                }
            }
        }
        Some(b'[') => {
            *i += 1;
            let mut a = Vec::new();
            ws(b, i);
            if b.get(*i) == Some(&b']') {
                *i += 1;
                return Ok(Json::Arr(a));
            }
            loop {
                a.push(value(b, i)?);
                ws(b, i);
                match b.get(*i) {
                    Some(b',') => *i += 1,
                    Some(b']') => {
                        *i += 1;
                        return Ok(Json::Arr(a));
                    }
                    _ => return Err("expected , or ]".into()),
                }
            }
        }
        Some(b'"') => {
            *i += 1;
            let mut out = String::new();
            while let Some(&c) = b.get(*i) {
                *i += 1;
                match c {
                    b'"' => return Ok(Json::Str(out)),
                    b'\\' => {
                        let e = *b.get(*i).ok_or("truncated escape")?;
                        *i += 1;
                        match e {
                            b'"' => out.push('"'),
                            b'\\' => out.push('\\'),
                            b'/' => out.push('/'),
                            b'n' => out.push('\n'),
                            b't' => out.push('\t'),
                            b'r' => out.push('\r'),
                            b'b' => out.push('\u{8}'),
                            b'f' => out.push('\u{c}'),
                            b'u' => {
                                let hex = s_get(b, *i, 4).ok_or("truncated \\u")?;
                                *i += 4;
                                let cp = u32::from_str_radix(hex, 16).map_err(|e| e.to_string())?;
                                // Surrogate pairs: peek for the low half.
                                if (0xD800..0xDC00).contains(&cp) {
                                    if s_get(b, *i, 2) == Some("\\u") {
                                        let lo_hex = s_get(b, *i + 2, 4).ok_or("truncated pair")?;
                                        let lo = u32::from_str_radix(lo_hex, 16)
                                            .map_err(|e| e.to_string())?;
                                        *i += 6;
                                        let c = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                        out.push(char::from_u32(c).unwrap_or('\u{fffd}'));
                                    } else {
                                        out.push('\u{fffd}');
                                    }
                                } else {
                                    out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                                }
                            }
                            _ => return Err(format!("bad escape \\{}", e as char)),
                        }
                    }
                    _ => {
                        // Raw UTF-8 continuation bytes pass through unchanged.
                        let start = *i - 1;
                        let len = utf8_len(c);
                        let chunk = &s_all(b)[start..(start + len).min(b.len())];
                        out.push_str(chunk);
                        *i = start + chunk.len();
                    }
                }
            }
            Err("unterminated string".into())
        }
        Some(b't') => lit(b, i, "true", Json::Bool(true)),
        Some(b'f') => lit(b, i, "false", Json::Bool(false)),
        Some(b'n') => lit(b, i, "null", Json::Null),
        Some(_) => {
            let start = *i;
            while *i < b.len()
                && matches!(b[*i], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
            {
                *i += 1;
            }
            s_all(b)[start..*i]
                .parse::<f64>()
                .map(Json::Num)
                .map_err(|e| format!("bad number: {e}"))
        }
        None => Err("unexpected end".into()),
    }
}

fn lit(b: &[u8], i: &mut usize, word: &str, v: Json) -> Result<Json, String> {
    if s_get(b, *i, word.len()) == Some(word) {
        *i += word.len();
        Ok(v)
    } else {
        Err(format!("expected {word}"))
    }
}

fn s_all(b: &[u8]) -> &str {
    std::str::from_utf8(b).unwrap_or("")
}

fn s_get(b: &[u8], i: usize, n: usize) -> Option<&str> {
    s_all(b).get(i..i + n)
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// A JSON string literal (quotes included).
pub fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}
