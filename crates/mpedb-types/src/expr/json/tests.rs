//! Unit tests for the JSON engine. The behavioural surface is pinned
//! differentially against the real `sqlite3` in `crates/mpedb/tests/json_fn.rs`;
//! what is tested HERE is the parser's safety contract — no panic, no stack
//! overflow, no wrong slice — on inputs the differential cannot reach because
//! sqlite would refuse them too.

use super::*;

fn t(s: &str) -> Value {
    Value::Text(s.to_string())
}

fn json_of(s: &str) -> String {
    match json(&[t(s)]).unwrap() {
        Value::Text(out) => out,
        other => panic!("json({s:?}) -> {other:?}"),
    }
}

/// Truncation at EVERY byte offset of a rich document: each prefix is either a
/// complete document or a clean error, never a panic and never a partial value.
#[test]
fn truncation_at_every_offset_is_an_error_not_a_panic() {
    let doc = r#"{"a":1,"b":[true,false,null,-1.5e-7],"c":{"d":"xåå\"","e":[]}}"#;
    let full = json_of(doc);
    for i in 0..doc.len() {
        if !doc.is_char_boundary(i) {
            continue;
        }
        let prefix = &doc[..i];
        match json(&[t(prefix)]) {
            // The only prefixes that parse are complete documents, and they
            // must minify to themselves.
            Ok(Value::Text(out)) => {
                assert_eq!(out, json_of(prefix), "prefix {i} is not idempotent");
            }
            Ok(other) => panic!("prefix {i} produced {other:?}"),
            Err(_) => {}
        }
    }
    assert_eq!(full, doc, "the sample document is already minified");
}

/// Every single-byte corruption of a document is an error or a valid document —
/// never a panic, and never a value that disagrees with a re-parse.
#[test]
fn single_byte_corruption_never_panics() {
    let doc = r#"[{"k":[1,2.5,true,null,"s"]},"t",-3]"#;
    for i in 0..doc.len() {
        for b in [b'"', b'\\', b'{', b'}', b'[', b']', b',', b':', b'0', b'e', 0x01, 0x7f] {
            let mut bytes = doc.as_bytes().to_vec();
            bytes[i] = b;
            let Ok(s) = String::from_utf8(bytes) else {
                continue;
            };
            if let Ok(Value::Text(out)) = json(&[t(&s)]) {
                // Idempotence must survive any accepted corruption.
                assert_eq!(json_of(&out), out, "not idempotent after corrupting byte {i}");
            }
        }
    }
}

/// The nesting bound is a bounded ERROR, not a blown stack — in both
/// directions, and for every entry point that walks a document.
#[test]
fn depth_bound_is_an_error_in_every_entry_point() {
    let ok = format!("{}1{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
    let deep = format!("{}1{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
    assert!(json(&[t(&ok)]).is_ok());
    assert_eq!(json_valid(&[t(&ok)]).unwrap(), Value::Int(1));
    for f in [
        json as fn(&[Value]) -> Result<Value>,
        json_valid,
        json_type,
        json_array_length,
    ] {
        let e = f(&[t(&deep)]).unwrap_err().to_string();
        assert!(e.contains("nests deeper"), "unexpected error: {e}");
    }
    // `json_valid` must RAISE rather than answer 0 — sqlite answers 1 there,
    // so a 0 would be a wrong answer instead of a refusal.
    assert!(json_valid(&[t(&deep)]).is_err());
    // ... but a genuinely malformed shallow document is still a plain 0.
    assert_eq!(json_valid(&[t("{")]).unwrap(), Value::Int(0));
}

/// A very wide (not deep) document is fine: width costs heap, not stack.
#[test]
fn width_is_unbounded() {
    let wide = format!(
        "[{}]",
        (0..50_000).map(|i| i.to_string()).collect::<Vec<_>>().join(",")
    );
    assert_eq!(
        json_array_length(&[t(&wide)]).unwrap(),
        Value::Int(50_000)
    );
}

/// Escape decoding, including the surrogate pair rules. A LONE surrogate is
/// refused by name: sqlite emits it as three raw bytes that are not UTF-8.
#[test]
fn string_escapes_decode_or_refuse() {
    let ex = |doc: &str| json_extract(&[t(doc), t("$.a")]);
    assert_eq!(
        ex(r#"{"a":"xå\n\t\r\b\f\/\\\""}"#).unwrap(),
        t("xå\n\t\r\u{8}\u{c}/\\\"")
    );
    // A surrogate PAIR is one astral character.
    assert_eq!(ex(r#"{"a":"😀"}"#).unwrap(), t("😀"));
    // Lone high, lone low, and a high followed by a non-surrogate.
    for bad in [
        r#"{"a":"\ud800"}"#,
        r#"{"a":"\udc00"}"#,
        r#"{"a":"\ud800A"}"#,
        r#"{"a":"\ud800x"}"#,
    ] {
        let e = ex(bad).unwrap_err().to_string();
        assert!(e.contains("unpaired surrogate"), "{bad}: {e}");
        // The document is still VALID JSON — only extracting the string is
        // refused, exactly as documented.
        assert_eq!(json_valid(&[t(bad)]).unwrap(), Value::Int(1));
        assert!(json(&[t(bad)]).is_ok());
    }
}

/// A path key is compared against the DECODED document label, and a backslash
/// in the path itself is refused rather than guessed.
#[test]
fn path_keys_compare_against_decoded_labels() {
    let ex = |doc: &str, p: &str| json_extract(&[t(doc), t(p)]).unwrap();
    assert_eq!(ex(r#"{"ab":1}"#, "$.ab"), Value::Int(1));
    assert_eq!(ex(r#"{"a\"b":1}"#, r#"$.a"b"#), Value::Int(1));
    assert_eq!(ex(r#"{"a.b":1}"#, r#"$."a.b""#), Value::Int(1));
    assert_eq!(ex(r#"{"a.b":1}"#, "$.a.b"), Value::Null);
    let e = json_extract(&[t(r#"{"a":1}"#), t(r#"$."a\"b""#)])
        .unwrap_err()
        .to_string();
    assert!(e.contains("backslash"), "{e}");
}

/// A path index is bounded: a huge one saturates and simply finds nothing,
/// rather than wrapping into a valid index.
#[test]
fn huge_path_indexes_saturate() {
    for p in [
        "$[99999999999999999999]",
        "$[#-99999999999999999999]",
        "$[4294967296]",
    ] {
        assert_eq!(json_extract(&[t("[1,2,3]"), t(p)]).unwrap(), Value::Null);
    }
}

/// The one place a `Value` is rendered INTO a document: every type has an
/// exact JSON form, a BLOB is the documented error, and mpedb's two extra
/// first-class types name themselves honestly.
#[test]
fn value_rendering_covers_every_type() {
    let quoted = |v: Value| match json_quote(&[v]).unwrap() {
        Value::Text(s) => s,
        other => panic!("{other:?}"),
    };
    assert_eq!(quoted(Value::Null), "null");
    assert_eq!(quoted(Value::Int(-7)), "-7");
    assert_eq!(quoted(Value::Float(1e3)), "1000.0");
    for x in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
        let e = json_quote(&[Value::Float(x)]).unwrap_err().to_string();
        assert!(e.contains("no JSON number form"), "{x}: {e}");
    }
    assert_eq!(quoted(t("a\"b")), r#""a\"b""#);
    assert_eq!(quoted(t("\u{1}\u{1f}\u{7f}")), "\"\\u0001\\u001f\u{7f}\"");
    // sqlite has no boolean, so there is nothing to disagree with; JSON does,
    // so mpedb's Bool becomes a JSON boolean.
    assert_eq!(quoted(Value::Bool(true)), "true");
    assert_eq!(quoted(Value::Bool(false)), "false");
    assert_eq!(quoted(Value::Timestamp(42)), "42");
    let e = json_quote(&[Value::Blob(vec![1])]).unwrap_err().to_string();
    assert!(e.contains("JSON cannot hold BLOB"), "{e}");
}

/// Minifying is not re-rendering: an accepted token comes back byte-identical.
#[test]
fn token_spellings_survive() {
    for doc in [
        "1.50", "1e3", "1E3", "1e+3", "-0", "0.0", "1.500000",
        r#"{"a":1.50,"b":1e3}"#,
        r#""å""#,
    ] {
        assert_eq!(json_of(doc), doc, "{doc} was re-rendered");
    }
    assert_eq!(json_of("  [ 1 , 2 ]  "), "[1,2]");
    // ... and a document that is only PARTIALLY rewritten keeps the rest.
    let set = json_set(&[
        Value::Int(0),
        t(r#"{"a":1.50,"b":1e3}"#),
        t("$.c"),
        Value::Int(1),
    ])
    .unwrap();
    assert_eq!(set, t(r#"{"a":1.50,"b":1e3,"c":1}"#));
}

/// The subtype mask really is what decides splice-vs-quote — the whole reason
/// the binder computes it.
#[test]
fn subtype_mask_selects_splice_or_quote() {
    let plain = json_array(&[Value::Int(0), t("[1,2]")]).unwrap();
    assert_eq!(plain, t(r#"["[1,2]"]"#));
    let as_json = json_array(&[Value::Int(1), t("[1,2]")]).unwrap();
    assert_eq!(as_json, t("[[1,2]]"));
    // Bit k selects the k-th VALUE, not the k-th argument.
    let obj = json_object(&[Value::Int(0b10), t("a"), t("[1]"), t("b"), t("[2]")]).unwrap();
    assert_eq!(obj, t(r#"{"a":"[1]","b":[2]}"#));
    // A JSON-flagged argument is RE-PARSED, so a corrupt one is an error
    // rather than a document that silently stops being JSON.
    assert!(json_array(&[Value::Int(1), t("[1,")]).is_err());
}

/// A mask argument that is not an integer is an internal error, never a
/// silently-plain reading — a decoded plan cannot forge one.
#[test]
fn missing_mask_is_an_internal_error() {
    for bad in [Value::Null, t("1"), Value::Float(1.0)] {
        assert!(json_array(&[bad.clone(), Value::Int(1)]).is_err());
        assert!(json_object(&[bad, t("a"), Value::Int(1)]).is_err());
    }
}
