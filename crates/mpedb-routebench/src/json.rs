//! Minimal read-only JSON — the vecbench parser, shared shape (a third copy
//! is the signal to factor a benchutil crate; noted, not yet worth the crate).

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
    pub fn str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
}

// -- minimal JSON (same grammar subset as graphbench's parser) --------------

pub fn parse(s: &str) -> Result<Json, String> {
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
