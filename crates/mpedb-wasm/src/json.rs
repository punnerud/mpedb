//! A minimal JSON writer.
//!
//! Hand-rolled rather than `serde_json`: the playground emits one small,
//! fixed-shape document per query, and a dependency-free writer keeps the wasm
//! binary small and the build a plain `cargo build`.

/// Append `s` as a quoted JSON string, escaped per RFC 8259.
///
/// Engine output is arbitrary user data — table text, and above all **error
/// messages**, which the page is required to show verbatim. Anything that is
/// not escapable to valid JSON here would corrupt the document and silently
/// lose the very message the demo exists to display, so control characters go
/// out as `\uXXXX` rather than being dropped.
pub fn push_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A `f64` as JSON. Non-finite values have no JSON spelling; emit them as
/// strings so the page can render what the engine actually produced instead
/// of a parse error.
pub fn push_f64(out: &mut String, v: f64) {
    if v.is_finite() {
        // `{:?}` round-trips f64 exactly, unlike `{}`.
        out.push_str(&format!("{v:?}"));
    } else if v.is_nan() {
        out.push_str("\"NaN\"");
    } else if v > 0.0 {
        out.push_str("\"Infinity\"");
    } else {
        out.push_str("\"-Infinity\"");
    }
}

/// Comma helper: writes `,` on every call but the first.
pub struct Sep(bool);

impl Sep {
    pub fn new() -> Sep {
        Sep(true)
    }
    pub fn sep(&mut self, out: &mut String) {
        if self.0 {
            self.0 = false;
        } else {
            out.push(',');
        }
    }
}

impl Default for Sep {
    fn default() -> Self {
        Sep::new()
    }
}
