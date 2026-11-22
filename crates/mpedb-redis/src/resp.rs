//! RESP2, the byte format Redis clients already speak.
//!
//! The only request shape accepted is an array of bulk strings, or one
//! inline line. Replies
//! cover the five RESP2 types those commands return. A bulk string is
//! binary-safe: keys and values are bytes, and a CRLF inside a value is
//! just more bytes.

use std::fmt;

/// One RESP2 value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resp {
    Simple(String),
    /// The text after the leading `-`, without CR/LF.
    Error(String),
    Int(i64),
    /// `None` is the null bulk (`$-1`).
    Bulk(Option<Vec<u8>>),
    Array(Vec<Resp>),
}

impl Resp {
    pub fn ok() -> Self {
        Resp::Simple("OK".into())
    }

    pub fn err(message: impl Into<String>) -> Self {
        Resp::Error(message.into())
    }

    pub fn pong() -> Self {
        Resp::Simple("PONG".into())
    }

    /// Bytes of bulk payload held, framing not counted.
    pub fn payload_len(&self) -> usize {
        match self {
            Resp::Bulk(Some(bytes)) => bytes.len(),
            Resp::Array(items) => items.iter().map(Resp::payload_len).sum(),
            _ => 0,
        }
    }
}

#[derive(Debug)]
pub struct ProtocolError(pub String);

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

/// Append the RESP encoding of `value` to `out`.
pub fn encode(value: &Resp, out: &mut Vec<u8>) {
    match value {
        Resp::Simple(s) => {
            out.push(b'+');
            push_line(out, s.as_bytes());
        }
        Resp::Error(s) => {
            out.push(b'-');
            push_line(out, s.as_bytes());
        }
        Resp::Int(n) => {
            out.push(b':');
            push_line(out, n.to_string().as_bytes());
        }
        Resp::Bulk(None) => out.extend_from_slice(b"$-1\r\n"),
        Resp::Bulk(Some(bytes)) => {
            out.push(b'$');
            push_line(out, bytes.len().to_string().as_bytes());
            out.extend_from_slice(bytes);
            out.extend_from_slice(b"\r\n");
        }
        Resp::Array(items) => {
            out.push(b'*');
            push_line(out, items.len().to_string().as_bytes());
            for item in items {
                encode(item, out);
            }
        }
    }
}

fn push_line(out: &mut Vec<u8>, bytes: &[u8]) {
    for &b in bytes {
        if b != b'\r' && b != b'\n' {
            out.push(b);
        }
    }
    out.extend_from_slice(b"\r\n");
}

/// Reads commands off a connection without parsing any byte twice.
///
/// [`parse_one`] starts from the front of the buffer on every call. A
/// 64 KiB value arrives in eight reads, so the command was parsed eight
/// times and every argument before the unfinished one was copied and thrown
/// away each time. Two 16 MiB arguments cost about 32 GiB of copying. This
/// keeps the arguments it has finished and resumes at the first byte it has
/// not consumed.
pub struct CommandReader {
    buf: Vec<u8>,
    /// First unconsumed byte of `buf`.
    at: usize,
    /// Arguments still owed by the command being read. `None` between
    /// commands.
    want: Option<usize>,
    args: Vec<Vec<u8>>,
    /// Bytes held in `args`, against `max_command`.
    held: usize,
    max_bulk: usize,
    max_command: usize,
}

/// What Redis allows for an inline command line.
const MAX_INLINE: usize = 64 * 1024;
/// A command is a short list of arguments. A longer array is a client bug or
/// a memory bomb.
const MAX_ARGS: i64 = 100_000;
/// A buffer that grew for one large value is given back once it is empty.
const KEEP_CAPACITY: usize = 64 * 1024;

impl CommandReader {
    pub fn new(max_bulk: usize) -> Self {
        CommandReader {
            buf: Vec::new(),
            at: 0,
            want: None,
            args: Vec::new(),
            held: 0,
            max_bulk,
            // One command is a max-sized value plus its key and framing, and
            // HSET may carry more than one value.
            max_command: max_bulk.saturating_mul(2).saturating_add(64 * 1024),
        }
    }

    /// Append bytes read from the socket.
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.at > 0 {
            self.buf.drain(..self.at);
            self.at = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete command, or `None` when more bytes are needed.
    /// `Err` means the frame boundary is lost and the connection must close.
    pub fn next_command(&mut self) -> Result<Option<Vec<Vec<u8>>>, ProtocolError> {
        loop {
            let Some(want) = self.want else {
                if self.at >= self.buf.len() {
                    self.release();
                    return Ok(None);
                }
                if self.buf[self.at] != b'*' {
                    match self.inline()? {
                        Some(args) if args.is_empty() => continue,
                        Some(args) => return Ok(Some(args)),
                        None => return Ok(None),
                    }
                }
                let Some(count) = self.header(b'*', "multibulk")? else {
                    return Ok(None);
                };
                if count > MAX_ARGS {
                    return Err(ProtocolError("invalid multibulk length".into()));
                }
                // `*0` and `*-1` are not commands. Redis reads past them.
                if count > 0 {
                    self.want = Some(count as usize);
                }
                continue;
            };
            if want == 0 {
                self.want = None;
                self.held = 0;
                return Ok(Some(std::mem::take(&mut self.args)));
            }
            if self.at >= self.buf.len() {
                return Ok(None);
            }
            if self.buf[self.at] != b'$' {
                return Err(ProtocolError(format!(
                    "expected '$', got '{}'",
                    self.buf[self.at].escape_ascii()
                )));
            }
            let before = self.at;
            let Some(len) = self.header(b'$', "bulk")? else {
                return Ok(None);
            };
            if len < 0 || len as u64 > self.max_bulk as u64 {
                return Err(ProtocolError("invalid bulk length".into()));
            }
            let len = len as usize;
            if self.held + len > self.max_command {
                return Err(ProtocolError("command exceeds the value size limit".into()));
            }
            let end = self.at + len;
            if self.buf.len() < end + 2 {
                // Wait for the rest. Only this header is read again.
                let missing = end + 2 - self.buf.len();
                self.buf.reserve(missing);
                self.at = before;
                return Ok(None);
            }
            if &self.buf[end..end + 2] != b"\r\n" {
                return Err(ProtocolError(
                    "bulk string is not terminated by CRLF".into(),
                ));
            }
            self.args.push(self.buf[self.at..end].to_vec());
            self.held += len;
            self.at = end + 2;
            self.want = Some(want - 1);
        }
    }

    /// `<tag><integer>\r\n` at `self.at`. Consumes it when complete.
    fn header(&mut self, tag: u8, what: &str) -> Result<Option<i64>, ProtocolError> {
        debug_assert_eq!(self.buf[self.at], tag);
        let rest = &self.buf[self.at..];
        let Some(rel) = find_crlf(rest) else {
            if rest.len() > 32 {
                return Err(ProtocolError(format!("invalid {what} length")));
            }
            return Ok(None);
        };
        let digits = &rest[1..rel];
        // Redis takes `-1` and plain digits. `+5` and an empty length are
        // not lengths.
        let plain = !digits.is_empty()
            && digits
                .iter()
                .enumerate()
                .all(|(i, b)| b.is_ascii_digit() || (i == 0 && *b == b'-' && digits.len() > 1));
        let value = std::str::from_utf8(digits)
            .ok()
            .filter(|_| plain)
            .and_then(|s| s.parse::<i64>().ok())
            .ok_or_else(|| ProtocolError(format!("invalid {what} length")))?;
        self.at += rel + 2;
        Ok(Some(value))
    }

    /// `PING\r\n` from a health check or a person with netcat. Split on
    /// spaces. An empty line is skipped.
    fn inline(&mut self) -> Result<Option<Vec<Vec<u8>>>, ProtocolError> {
        let rest = &self.buf[self.at..];
        let Some(end) = rest.iter().position(|b| *b == b'\n') else {
            if rest.len() > MAX_INLINE {
                return Err(ProtocolError("too big inline request".into()));
            }
            return Ok(None);
        };
        let line = rest[..end].strip_suffix(b"\r").unwrap_or(&rest[..end]);
        let args: Vec<Vec<u8>> = line
            .split(|b| *b == b' ' || *b == b'\t')
            .filter(|part| !part.is_empty())
            .map(<[u8]>::to_vec)
            .collect();
        self.at += end + 1;
        Ok(Some(args))
    }

    fn release(&mut self) {
        self.buf.clear();
        self.at = 0;
        if self.buf.capacity() > KEEP_CAPACITY {
            self.buf = Vec::new();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_encode_and_nil_is_not_empty() {
        let mut buf = Vec::new();
        encode(&Resp::ok(), &mut buf);
        encode(&Resp::Bulk(None), &mut buf);
        encode(&Resp::Bulk(Some(Vec::new())), &mut buf);
        encode(&Resp::Int(-2), &mut buf);
        assert_eq!(buf, b"+OK\r\n$-1\r\n$0\r\n\r\n:-2\r\n");
    }

    fn read_all(reader: &mut CommandReader) -> Vec<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        while let Some(cmd) = reader.next_command().unwrap() {
            out.push(cmd);
        }
        out
    }

    #[test]
    fn reader_resumes_at_every_split_point() {
        let wire = b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$5\r\na\r\nbc\r\n*1\r\n$4\r\nPING\r\n";
        let expect = vec![
            vec![b"SET".to_vec(), b"k".to_vec(), b"a\r\nbc".to_vec()],
            vec![b"PING".to_vec()],
        ];
        for split in 0..=wire.len() {
            let mut reader = CommandReader::new(1024);
            let mut got = Vec::new();
            reader.feed(&wire[..split]);
            got.extend(read_all(&mut reader));
            reader.feed(&wire[split..]);
            got.extend(read_all(&mut reader));
            assert_eq!(got, expect, "split at {split}");
        }
        // One byte at a time.
        let mut reader = CommandReader::new(1024);
        let mut got = Vec::new();
        for byte in wire {
            reader.feed(&[*byte]);
            got.extend(read_all(&mut reader));
        }
        assert_eq!(got, expect);
    }

    #[test]
    fn reader_takes_inline_and_skips_empty_arrays() {
        let mut reader = CommandReader::new(1024);
        reader.feed(b"*0\r\n*-1\r\n\r\nPING\r\nget  a\n");
        assert_eq!(
            read_all(&mut reader),
            vec![vec![b"PING".to_vec()], vec![b"get".to_vec(), b"a".to_vec()]]
        );
    }

    #[test]
    fn reader_refuses_what_is_not_a_command() {
        for wire in [
            &b"*1\r\n*1\r\n"[..],
            b"*1\r\n:1\r\n",
            b"*1\r\n$-1\r\n",
            b"*1\r\n$+4\r\nPING\r\n",
            b"*1\r\n$2000\r\n",
            b"*100001\r\n",
            b"*1\r\n$4\r\nPINGxx",
        ] {
            let mut reader = CommandReader::new(1024);
            reader.feed(wire);
            assert!(
                reader.next_command().is_err(),
                "{:?}",
                wire.escape_ascii().to_string()
            );
        }
        // Nesting cannot recurse: there is no recursion to do.
        let mut reader = CommandReader::new(1024);
        reader.feed(&b"*1\r\n".repeat(200_000));
        assert!(reader.next_command().is_err());
    }

    #[test]
    fn reader_gives_a_large_buffer_back() {
        let mut reader = CommandReader::new(1 << 20);
        let mut wire = b"*1\r\n$500000\r\n".to_vec();
        wire.extend(std::iter::repeat_n(b'x', 500_000));
        wire.extend(b"\r\n");
        reader.feed(&wire);
        assert_eq!(read_all(&mut reader)[0][0].len(), 500_000);
        assert!(reader.buf.capacity() <= KEEP_CAPACITY);
    }
}
