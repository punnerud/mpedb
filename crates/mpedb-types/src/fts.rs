//! Native full-text-search primitives shared by the storage engine (posting
//! maintenance, `mpedb-core`) and the SQL front-end (query-term normalization
//! and set algebra, `mpedb-sql`/`mpedb`): the tokenizers, the posting-list
//! (doclist) wire codec, and the inverted-index key layout.
//!
//! This is a **new on-disk wire structure** (design/DESIGN-FTS.md §7): the
//! [`Doclist`] decoder treats its input as hostile — every read is
//! bounds-checked and every malformed byte sequence yields [`Error::Corrupt`],
//! never a panic. Truncation at every offset is tested below.

use crate::error::{Error, Result};

/// The tokenizer frozen into an FTS table's schema bytes (design/DESIGN-FTS.md
/// §2). The choice is content-hashed with the plan, so a query can never
/// tokenize differently than the index was built with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Tokenizer {
    /// sqlite's default: split on Unicode non-alphanumeric, casefold, and strip
    /// diacritics (common Latin set — see [`fold_diacritic`]).
    Unicode61 = 0,
    /// ASCII: token characters are `[0-9A-Za-z]` plus every byte `>= 0x80`;
    /// only ASCII `A-Z` casefold, no diacritic stripping (matches sqlite's
    /// `ascii` tokenizer, which keeps high bytes verbatim).
    Ascii = 1,
}

impl Tokenizer {
    pub fn from_tag(t: u8) -> Option<Tokenizer> {
        match t {
            0 => Some(Tokenizer::Unicode61),
            1 => Some(Tokenizer::Ascii),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Tokenizer::Unicode61 => "unicode61",
            Tokenizer::Ascii => "ascii",
        }
    }

    /// Parse a `tokenize='…'` option value. Only the stage-1 tokenizers are
    /// accepted; `porter`/`trigram` are stage 3 and refuse by name at the
    /// call site.
    pub fn parse(s: &str) -> Option<Tokenizer> {
        match s.trim().to_ascii_lowercase().as_str() {
            "unicode61" => Some(Tokenizer::Unicode61),
            "ascii" => Some(Tokenizer::Ascii),
            _ => None,
        }
    }
}

/// Fold a lowercase Latin accented character to its base letter, drop a
/// combining diacritical mark, or keep the character unchanged.
///
/// This covers the common Latin-1 Supplement and Latin Extended-A letters plus
/// the combining-marks block — enough for real-world accented text (café,
/// crème, naïve, Zürich) to fold exactly as sqlite's `unicode61` does. It is a
/// deliberately bounded table, not sqlite's full Unicode fold; stage 1
/// documents the scope.
fn fold_diacritic(c: char) -> Option<char> {
    // Combining diacritical marks: dropped entirely.
    if ('\u{0300}'..='\u{036F}').contains(&c) {
        return None;
    }
    Some(match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => 'a',
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => 'c',
        'ð' | 'ď' | 'đ' => 'd',
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => 'g',
        'ĥ' | 'ħ' => 'h',
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => 'i',
        'ĵ' => 'j',
        'ķ' => 'k',
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => 'l',
        'ñ' | 'ń' | 'ņ' | 'ň' | 'ŋ' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => 'o',
        'ŕ' | 'ŗ' | 'ř' => 'r',
        'ś' | 'ŝ' | 'ş' | 'š' => 's',
        'ţ' | 'ť' | 'ŧ' => 't',
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => 'u',
        'ŵ' => 'w',
        'ý' | 'ÿ' | 'ŷ' => 'y',
        'ź' | 'ż' | 'ž' => 'z',
        other => other,
    })
}

/// Split `text` into `(token, position)` pairs, in ascending position order.
/// Positions are 0-based token offsets within `text`. Repeated tokens keep
/// distinct ascending positions (needed for `^initial` and future phrases).
pub fn tokenize(tk: Tokenizer, text: &str) -> Vec<(String, u32)> {
    match tk {
        Tokenizer::Unicode61 => tokenize_unicode61(text),
        Tokenizer::Ascii => tokenize_ascii(text),
    }
}

fn tokenize_unicode61(text: &str) -> Vec<(String, u32)> {
    let mut out: Vec<(String, u32)> = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                if let Some(folded) = fold_diacritic(lc) {
                    cur.push(folded);
                }
            }
        } else if !cur.is_empty() {
            out.push((std::mem::take(&mut cur), 0));
        }
    }
    if !cur.is_empty() {
        out.push((cur, 0));
    }
    // A token whose characters were ALL combining marks folds to empty; drop it,
    // then number positions densely 0..n.
    out.retain(|(t, _)| !t.is_empty());
    for (i, (_, p)) in out.iter_mut().enumerate() {
        *p = i as u32;
    }
    out
}

fn tokenize_ascii(text: &str) -> Vec<(String, u32)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let mut pos = 0u32;
    let is_tok = |b: u8| b.is_ascii_alphanumeric() || b >= 0x80;
    for i in 0..=bytes.len() {
        let tok = i < bytes.len() && is_tok(bytes[i]);
        match (start, tok) {
            (None, true) => start = Some(i),
            (Some(s), false) => {
                // ASCII-casefold the run; high bytes pass through untouched.
                let mut buf: Vec<u8> = bytes[s..i].to_vec();
                for b in &mut buf {
                    b.make_ascii_lowercase();
                }
                // Valid UTF-8: separators are single ASCII bytes and every byte
                // of a multibyte char is >= 0x80 (a token char), so a run never
                // splits a code point.
                if let Ok(t) = std::str::from_utf8(&buf) {
                    out.push((t.to_string(), pos));
                    pos += 1;
                }
                start = None;
            }
            _ => {}
        }
    }
    out
}

/// Normalize one query term the way indexing does, so it matches the stored
/// postings. Returns the canonical token, or `None` if the term has no token
/// characters at all. If the term spans a separator (`foo.bar`), only the
/// FIRST token is returned — the query grammar is responsible for splitting.
pub fn normalize_term(tk: Tokenizer, term: &str) -> Option<String> {
    tokenize(tk, term).into_iter().next().map(|(t, _)| t)
}

// ---- inverted-index key layout (design/DESIGN-FTS.md §7) -------------------
//
// key := term_utf8 ‖ 0x00 ‖ colno_be_u16
//
// Tokens never contain 0x00 (a NUL is neither alphanumeric nor a byte the ASCII
// tokenizer keeps), so the separator unambiguously ends the term. Fixed-width
// big-endian colno keeps the key memcmp-ordered by (term, column).

/// The full posting key for an exact `term` in FTS column `colno`.
pub fn posting_key(term: &str, colno: u16) -> Vec<u8> {
    let mut k = Vec::with_capacity(term.len() + 3);
    k.extend_from_slice(term.as_bytes());
    k.push(0);
    k.extend_from_slice(&colno.to_be_bytes());
    k
}

/// Prefix selecting exactly `term` across ALL columns (`term ‖ 0x00`): a
/// range scan from here yields one entry per column that indexed the term.
pub fn posting_key_exact_prefix(term: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(term.len() + 1);
    k.extend_from_slice(term.as_bytes());
    k.push(0);
    k
}

/// Prefix selecting every term that starts with `prefix` (for `term*`), across
/// all columns. A scan from here stops at the first key not starting with it.
pub fn posting_key_scan_prefix(prefix: &str) -> Vec<u8> {
    prefix.as_bytes().to_vec()
}

/// Recover the column ordinal from a posting key (its trailing 2 bytes, BE).
pub fn posting_key_colno(key: &[u8]) -> Option<u16> {
    if key.len() < 3 {
        return None;
    }
    let tail = &key[key.len() - 2..];
    Some(u16::from_be_bytes([tail[0], tail[1]]))
}

// ---- doclist wire codec (design/DESIGN-FTS.md §7) --------------------------

/// A decoded posting list for one `(term, column)`: doc entries in ascending
/// docid order, each carrying that document's ascending token positions in the
/// column.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Doclist {
    pub docs: Vec<(i64, Vec<u32>)>,
}

fn write_uvarint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(b);
            break;
        }
        buf.push(b | 0x80);
    }
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

fn read_uvarint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        let b = *buf
            .get(*pos)
            .ok_or_else(|| Error::Corrupt("truncated doclist varint".into()))?;
        *pos += 1;
        // 10 groups of 7 bits cover 64 bits; a longer sequence, or a 10th group
        // with bits above bit 63, is malformed.
        if shift >= 64 || (shift == 63 && b > 1) {
            return Err(Error::Corrupt("doclist varint overflows u64".into()));
        }
        result |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    Ok(result)
}

impl Doclist {
    /// Canonical, deterministic serialization (design/DESIGN-FTS.md §7):
    /// `n:uvarint`, then per doc `zigzag(docid-delta), n_pos:uvarint,
    /// (uvarint pos-delta)*`. The leading count makes every proper-prefix
    /// truncation detectable — a decode of a short buffer runs out mid-entry
    /// rather than silently yielding a valid shorter list.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(4 + self.docs.len() * 3);
        write_uvarint(&mut buf, self.docs.len() as u64);
        let mut prev_doc: i64 = 0;
        for (docid, positions) in &self.docs {
            write_uvarint(&mut buf, zigzag(docid.wrapping_sub(prev_doc)));
            prev_doc = *docid;
            write_uvarint(&mut buf, positions.len() as u64);
            let mut prev_pos: u32 = 0;
            for &p in positions {
                write_uvarint(&mut buf, (p - prev_pos) as u64);
                prev_pos = p;
            }
        }
        buf
    }

    /// Bounds-checked decode of [`encode`](Self::encode) output. A hostile or
    /// truncated byte string yields [`Error::Corrupt`], never a panic. Enforces
    /// strictly-ascending docids, strictly-ascending positions, at least one
    /// position per entry, and no trailing bytes.
    pub fn decode(buf: &[u8]) -> Result<Doclist> {
        let mut pos = 0usize;
        let n = read_uvarint(buf, &mut pos)? as usize;
        // Each entry is at least 3 bytes (docid, npos>=1, one pos): a count
        // larger than the remaining buffer can never be satisfied.
        if n > buf.len() {
            return Err(Error::Corrupt("doclist doc count exceeds buffer".into()));
        }
        let mut docs = Vec::with_capacity(n.min(1 << 16));
        let mut prev_doc: i64 = 0;
        for i in 0..n {
            let delta = unzigzag(read_uvarint(buf, &mut pos)?);
            let docid = prev_doc
                .checked_add(delta)
                .ok_or_else(|| Error::Corrupt("doclist docid delta overflows i64".into()))?;
            if i > 0 && docid <= prev_doc {
                return Err(Error::Corrupt("doclist docids not strictly ascending".into()));
            }
            prev_doc = docid;
            let npos = read_uvarint(buf, &mut pos)? as usize;
            if npos == 0 {
                return Err(Error::Corrupt("doclist entry has no positions".into()));
            }
            let mut positions = Vec::with_capacity(npos.min(1 << 16));
            let mut prev_pos: i64 = -1;
            for _ in 0..npos {
                let d = read_uvarint(buf, &mut pos)?;
                if prev_pos >= 0 && d == 0 {
                    return Err(Error::Corrupt(
                        "doclist positions not strictly ascending".into(),
                    ));
                }
                // A delta between two u32 positions is itself <= u32::MAX. Reject
                // a larger one BEFORE the arithmetic: otherwise `d as i64` can wrap
                // negative (bypassing the `cur > u32::MAX` guard, a silent bad
                // decode in release) and `prev_pos + d as i64` can overflow (a
                // panic in checked builds — corrupt input must yield Corrupt).
                if d > u32::MAX as u64 {
                    return Err(Error::Corrupt("doclist position delta exceeds u32".into()));
                }
                let cur = if prev_pos < 0 { d as i64 } else { prev_pos + d as i64 };
                if cur > u32::MAX as i64 {
                    return Err(Error::Corrupt("doclist position exceeds u32".into()));
                }
                positions.push(cur as u32);
                prev_pos = cur;
            }
            docs.push((docid, positions));
        }
        if pos != buf.len() {
            return Err(Error::Corrupt("trailing bytes in doclist".into()));
        }
        Ok(Doclist { docs })
    }

    /// Insert or replace `docid`'s positions, keeping `docs` sorted ascending by
    /// docid. `positions` must be ascending and non-empty.
    pub fn upsert_doc(&mut self, docid: i64, positions: Vec<u32>) {
        match self.docs.binary_search_by_key(&docid, |(d, _)| *d) {
            Ok(i) => self.docs[i].1 = positions,
            Err(i) => self.docs.insert(i, (docid, positions)),
        }
    }

    /// Remove `docid`'s entry. Returns whether it was present.
    pub fn remove_doc(&mut self, docid: i64) -> bool {
        match self.docs.binary_search_by_key(&docid, |(d, _)| *d) {
            Ok(i) => {
                self.docs.remove(i);
                true
            }
            Err(_) => false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode61_casefold_and_diacritics() {
        let toks = |s: &str| -> Vec<String> {
            tokenize(Tokenizer::Unicode61, s).into_iter().map(|(t, _)| t).collect()
        };
        assert_eq!(toks("The Quick Brown FOX"), vec!["the", "quick", "brown", "fox"]);
        // Diacritics stripped, casefolded: café → cafe, CRÈME → creme.
        assert_eq!(toks("Café CRÈME"), vec!["cafe", "creme"]);
        assert_eq!(toks("naïve Zürich"), vec!["naive", "zurich"]);
        // Digits attach to letters (one token), underscore separates.
        assert_eq!(toks("abc123 foo_bar"), vec!["abc123", "foo", "bar"]);
        // Positions are dense 0..n.
        let p: Vec<u32> =
            tokenize(Tokenizer::Unicode61, "a b a").into_iter().map(|(_, p)| p).collect();
        assert_eq!(p, vec![0, 1, 2]);
    }

    #[test]
    fn ascii_keeps_high_bytes_and_folds_only_ascii() {
        let toks = |s: &str| -> Vec<String> {
            tokenize(Tokenizer::Ascii, s).into_iter().map(|(t, _)| t).collect()
        };
        assert_eq!(toks("Hello WORLD"), vec!["hello", "world"]);
        // 'é' is a high byte: kept verbatim, ASCII case folded → "café".
        assert_eq!(toks("Café"), vec!["café"]);
        assert_eq!(toks("foo_bar.baz"), vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn doclist_rejects_oversized_position_delta() {
        // n=1 doc; zigzag docid-delta=1; npos=1; position delta = 2^40 (> u32::MAX).
        // Must be Corrupt — never a panic (overflowing add in a checked build) and
        // never a silent wrap-negative that slips past the `cur > u32::MAX` guard.
        fn uv(mut v: u64, out: &mut Vec<u8>) {
            loop {
                let b = (v & 0x7f) as u8;
                v >>= 7;
                if v == 0 {
                    out.push(b);
                    break;
                }
                out.push(b | 0x80);
            }
        }
        let mut buf = Vec::new();
        uv(1, &mut buf); // 1 doc
        uv(2, &mut buf); // zigzag(1) docid delta
        uv(1, &mut buf); // npos = 1
        uv(1u64 << 40, &mut buf); // position delta 2^40 > u32::MAX
        assert!(matches!(Doclist::decode(&buf), Err(Error::Corrupt(_))));
        // And a first-position delta that would wrap negative under `d as i64`.
        let mut buf2 = Vec::new();
        uv(1, &mut buf2);
        uv(2, &mut buf2);
        uv(1, &mut buf2);
        uv(1u64 << 63, &mut buf2); // first position 2^63 (was silently -> 0)
        assert!(matches!(Doclist::decode(&buf2), Err(Error::Corrupt(_))));
    }

    #[test]
    fn posting_key_layout_and_colno() {
        let k = posting_key("brown", 3);
        assert_eq!(&k[..5], b"brown");
        assert_eq!(k[5], 0);
        assert_eq!(&k[6..], &[0, 3]);
        assert_eq!(posting_key_colno(&k), Some(3));
        // Exact-term prefix is a strict prefix of the full key.
        assert!(k.starts_with(&posting_key_exact_prefix("brown")));
        // A longer term with the same prefix does NOT share the exact-term
        // prefix (the NUL delimits): "brownie" vs "brown".
        assert!(!posting_key("brownie", 3).starts_with(&posting_key_exact_prefix("brown")));
        // But both share the scan prefix "brow" (for `brow*`).
        assert!(k.starts_with(&posting_key_scan_prefix("brow")));
        assert!(posting_key("brownie", 3).starts_with(&posting_key_scan_prefix("brow")));
    }

    fn sample() -> Doclist {
        Doclist {
            docs: vec![
                (1, vec![0, 3, 9]),
                (2, vec![5]),
                (1000, vec![0, 1, 2, 7]),
                (i64::MAX, vec![42]),
            ],
        }
    }

    #[test]
    fn doclist_roundtrip() {
        let d = sample();
        let enc = d.encode();
        let back = Doclist::decode(&enc).unwrap();
        assert_eq!(d, back);
        // Empty doclist round-trips too.
        let empty = Doclist::default();
        assert_eq!(Doclist::decode(&empty.encode()).unwrap(), empty);
    }

    #[test]
    fn doclist_negative_and_explicit_docids() {
        let d = Doclist {
            docs: vec![(-100, vec![0]), (-1, vec![2, 4]), (0, vec![1]), (77, vec![0])],
        };
        assert_eq!(Doclist::decode(&d.encode()).unwrap(), d);
    }

    #[test]
    fn doclist_truncation_at_every_offset_is_corrupt_never_panics() {
        let enc = sample().encode();
        for i in 0..enc.len() {
            // Every PROPER prefix must be rejected (the leading count makes a
            // short buffer run out mid-entry), and must never panic.
            assert!(Doclist::decode(&enc[..i]).is_err(), "offset {i} decoded");
        }
        // The full buffer decodes.
        assert!(Doclist::decode(&enc).is_ok());
    }

    #[test]
    fn doclist_hostile_bytes_refuse() {
        // Trailing garbage after a valid list.
        let mut enc = sample().encode();
        enc.push(0xff);
        assert!(Doclist::decode(&enc).is_err());
        // A count far larger than the buffer.
        assert!(Doclist::decode(&[0xff, 0xff, 0xff, 0x7f]).is_err());
        // npos == 0 is rejected: n=1, docid delta=2 (zigzag of 1), npos=0.
        assert!(Doclist::decode(&[1, 2, 0]).is_err());
        // Non-ascending docids: n=2, entry0 docid=1 (delta zigzag(1)=2), then
        // entry1 delta zigzag(0)=0 → docid stays 1, not strictly ascending.
        assert!(Doclist::decode(&[2, 2, 1, 0, 0, 1, 0]).is_err());
    }

    #[test]
    fn upsert_and_remove_keep_sorted() {
        let mut d = Doclist::default();
        d.upsert_doc(5, vec![1]);
        d.upsert_doc(2, vec![0]);
        d.upsert_doc(8, vec![3]);
        d.upsert_doc(2, vec![9]); // replace
        assert_eq!(d.docs, vec![(2, vec![9]), (5, vec![1]), (8, vec![3])]);
        assert!(d.remove_doc(5));
        assert!(!d.remove_doc(5));
        assert_eq!(d.docs, vec![(2, vec![9]), (8, vec![3])]);
    }
}
