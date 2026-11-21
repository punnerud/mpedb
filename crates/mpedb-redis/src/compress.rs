//! zstd in front of the row, not inside the engine.
//!
//! Repetitive JSON is the payload this server is for. Storing it compressed
//! is what keeps a multi-megabyte chart layout in a handful of pages. Values
//! that do not shrink are stored raw, so a small or already-compressed blob
//! does not pay a header for nothing. The codec byte lives in its own column:
//! a reader that does not know which is which would return compressed bytes
//! as if they were the value.

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
    let compressed = zstd::encode_all(std::io::Cursor::new(raw), level)?;
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
