//! A Qdrant REST client in plain std — the trimmed sibling of
//! `mpedb-graphbench/src/neo4j.rs` (same HTTP/1.0 + read-to-EOF framing, same
//! minimal JSON), kept separate because the two protocols share nothing but
//! transport.

use std::io::{Read, Write};
use std::net::TcpStream;

pub struct Qdrant {
    addr: String,
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
    pub fn num(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }
}

impl Qdrant {
    pub fn new(addr: &str) -> Qdrant {
        Qdrant { addr: addr.to_string() }
    }

    pub fn call(&self, method: &str, path: &str, body: &str) -> Result<Json, String> {
        let req = format!(
            "{method} {path} HTTP/1.0\r\nHost: {}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\n\r\n{body}",
            self.addr,
            body.len()
        );
        let mut s = TcpStream::connect(&self.addr).map_err(|e| format!("connect: {e}"))?;
        s.write_all(req.as_bytes()).map_err(|e| format!("send: {e}"))?;
        let mut resp = Vec::new();
        s.read_to_end(&mut resp).map_err(|e| format!("recv: {e}"))?;
        let text = String::from_utf8_lossy(&resp);
        let split = text.find("\r\n\r\n").ok_or("no header/body split")?;
        let status = text
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("");
        let json = parse(&text[split + 4..])?;
        if !status.starts_with('2') {
            return Err(format!("qdrant HTTP {status}: {json:?}"));
        }
        Ok(json)
    }

    /// Search: returns the result ids in rank order.
    pub fn search(&self, collection: &str, body: &str) -> Result<Vec<i64>, String> {
        let json = self.call("POST", &format!("/collections/{collection}/points/search"), body)?;
        Ok(json
            .get("result")
            .map(|r| r.arr())
            .unwrap_or(&[])
            .iter()
            .filter_map(|hit| hit.get("id").and_then(|v| v.num()).map(|n| n as i64))
            .collect())
    }
}

// -- minimal JSON (same grammar subset as graphbench's parser) --------------

fn parse(s: &str) -> Result<Json, String> {
    let b = s.as_bytes();
    let mut i = 0usize;
    value(b, &mut i)
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
                let Json::Str(k) = value(b, i)? else { return Err("key not a string".into()) };
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
            let start = *i;
            // Qdrant's strings here (statuses, keys) never need escapes worth
            // decoding; skip to the closing quote honouring backslashes.
            let mut out = String::new();
            while let Some(&c) = b.get(*i) {
                match c {
                    b'"' => {
                        *i += 1;
                        return Ok(Json::Str(out));
                    }
                    b'\\' => {
                        *i += 2;
                        out.push('?');
                    }
                    _ => {
                        out.push(c as char);
                        *i += 1;
                    }
                }
            }
            let _ = start;
            Err("unterminated string".into())
        }
        Some(b't') => lit(b, i, "true", Json::Bool(true)),
        Some(b'f') => lit(b, i, "false", Json::Bool(false)),
        Some(b'n') => lit(b, i, "null", Json::Null),
        Some(_) => {
            let start = *i;
            while *i < b.len() && matches!(b[*i], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') {
                *i += 1;
            }
            std::str::from_utf8(&b[start..*i])
                .ok()
                .and_then(|t| t.parse::<f64>().ok())
                .map(Json::Num)
                .ok_or_else(|| "bad number".into())
        }
        None => Err("unexpected end".into()),
    }
}

fn lit(b: &[u8], i: &mut usize, word: &str, v: Json) -> Result<Json, String> {
    if b.len() - *i >= word.len() && &b[*i..*i + word.len()] == word.as_bytes() {
        *i += word.len();
        Ok(v)
    } else {
        Err(format!("expected {word}"))
    }
}
