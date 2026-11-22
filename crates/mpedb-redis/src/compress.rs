//! zstd in front of the row, not inside the engine.
//!
//! Repetitive JSON is the payload this server is for. Storing it compressed
//! is what keeps a multi-megabyte chart layout in a handful of pages. Values
//! that do not shrink are stored raw, so a small or already-compressed blob
//! does not pay a header for nothing. The codec byte lives in its own column:
//! a reader that does not know which is which would return compressed bytes
//! as if they were the value.

use std::cell::RefCell;
use std::io::{self, Read};

/// Stored as the bytes the client sent.
pub const CODEC_RAW: i64 = 0;
/// zstd frame.
pub const CODEC_ZSTD: i64 = 1;

/// Below this size zstd rarely wins and the frame can be larger than the
/// input. Skipping the attempt keeps a `SET` of a few bytes on the cheap path.
const MIN_COMPRESS: usize = 64;

/// Compress `raw` when that makes it smaller.
pub fn compress(raw: &[u8], level: i32) -> io::Result<(i64, Vec<u8>)> {
    if raw.len() < MIN_COMPRESS {
        return Ok((CODEC_RAW, raw.to_vec()));
    }
    // One compression context per thread, kept between calls. A fresh one
    // per value cost more than writing the row. Measured in-process on
    // tmpfs with a 64-byte value: a `SET` alone went from 28.5 to 9.8 µs,
    // a `SET` in a batch of 128 from 17.8 to 3.8 µs, and the engine's
    // share of that is about 3 µs.
    thread_local! {
        static CTX: RefCell<Option<(i32, zstd::bulk::Compressor<'static>)>> =
            const { RefCell::new(None) };
    }
    let compressed = CTX.with(|cell| {
        let mut slot = cell.borrow_mut();
        if !matches!(&*slot, Some((have, _)) if *have == level) {
            *slot = Some((level, zstd::bulk::Compressor::new(level)?));
        }
        match slot.as_mut() {
            Some((_, ctx)) => ctx.compress(raw),
            None => Err(io::Error::other("no zstd context")),
        }
    })?;
    if compressed.len() < raw.len() {
        Ok((CODEC_ZSTD, compressed))
    } else {
        Ok((CODEC_RAW, raw.to_vec()))
    }
}

/// Reverse [`compress`]. A frame that expands past `max` is refused: a small
/// stored blob must not be able to allocate an unbounded buffer on read.
pub fn decompress(codec: i64, data: &[u8], max: usize) -> Result<Vec<u8>, String> {
    match codec {
        CODEC_RAW => {
            if data.len() > max {
                return Err(format!("stored value is {} bytes (max {max})", data.len()));
            }
            Ok(data.to_vec())
        }
        CODEC_ZSTD => {
            // A frame this server wrote states its content size. With that
            // known, one call into a kept context decodes it. A frame that
            // does not state it takes the bounded streaming loop below.
            if let Ok(Some(size)) = zstd::zstd_safe::get_frame_content_size(data) {
                let size = usize::try_from(size).unwrap_or(usize::MAX);
                if size > max {
                    return Err(format!("decompressed value exceeds max of {max} bytes"));
                }
                thread_local! {
                    static CTX: RefCell<Option<zstd::bulk::Decompressor<'static>>> =
                        const { RefCell::new(None) };
                }
                return CTX.with(|cell| {
                    let mut slot = cell.borrow_mut();
                    if slot.is_none() {
                        *slot = Some(
                            zstd::bulk::Decompressor::new().map_err(|e| format!("zstd: {e}"))?,
                        );
                    }
                    match slot.as_mut() {
                        Some(ctx) => ctx.decompress(data, size).map_err(|e| format!("zstd: {e}")),
                        None => Err("zstd: no context".to_string()),
                    }
                });
            }
            let mut decoder =
                zstd::stream::read::Decoder::with_buffer(data).map_err(|e| format!("zstd: {e}"))?;
            let mut out = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = decoder.read(&mut buf).map_err(|e| format!("zstd: {e}"))?;
                if n == 0 {
                    break;
                }
                if out.len().saturating_add(n) > max {
                    return Err(format!("decompressed value exceeds max of {max} bytes"));
                }
                out.extend_from_slice(&buf[..n]);
            }
            Ok(out)
        }
        other => Err(format!("unknown blob codec {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repetitive_json_shrinks_and_round_trips() {
        let raw = br#"{"layout":"chart","draw":"line"}"#.repeat(20);
        let (codec, stored) = compress(&raw, 3).unwrap();
        assert_eq!(codec, CODEC_ZSTD);
        assert!(stored.len() < raw.len());
        let back = decompress(codec, &stored, raw.len()).unwrap();
        assert_eq!(back, raw);
    }

    #[test]
    fn tiny_and_empty_values_stay_raw() {
        let (codec, stored) = compress(b"", 3).unwrap();
        assert_eq!((codec, stored.as_slice()), (CODEC_RAW, b"" as &[u8]));
        let (codec, stored) = compress(b"abcd", 3).unwrap();
        assert_eq!(codec, CODEC_RAW);
        assert_eq!(stored, b"abcd");
    }

    #[test]
    fn a_lying_frame_is_an_error() {
        let err = decompress(CODEC_ZSTD, b"not-zstd", 100).unwrap_err();
        assert!(err.contains("zstd"), "{err}");
        assert!(decompress(9, b"x", 100).unwrap_err().contains("codec 9"));
    }
}
