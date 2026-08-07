//! The PostgreSQL v3 frontend/backend protocol, as bytes.
//!
//! Hand-written, synchronous, and deliberately dependency-free — the same
//! discipline `mpedb-sqlitefmt` applies to sqlite's file format. The protocol is
//! a length-prefixed byte format; a client library would give us a client, which
//! is the wrong end.
//!
//! # Frame shape
//!
//! Every message except the startup packet is:
//!
//! ```text
//! u8 tag ‖ i32 length ‖ body      (length COUNTS ITSELF, excludes the tag)
//! ```
//!
//! The startup packet has no tag: `i32 length ‖ i32 version ‖ body`. That
//! asymmetry is why [`read_startup`] exists separately from [`read_message`] —
//! and why a server that guesses wrong reads the protocol version as a length
//! and hangs waiting for 196608 bytes.
//!
//! # Byte order
//!
//! Network order (big-endian) everywhere. Not host order, not little-endian
//! like the rest of mpedb — this is someone else's format.

use std::io::{self, Read, Write};

/// Protocol version 3.0, as the startup packet spells it: major 3, minor 0.
pub const PROTOCOL_V3: i32 = 196_608;
/// The magic "length" a client sends to ask for TLS before anything else.
pub const SSL_REQUEST_CODE: i32 = 80_877_103;
/// The same, for GSSAPI encryption.
pub const GSSENC_REQUEST_CODE: i32 = 80_877_104;
/// The magic code of a CancelRequest, which arrives on its OWN connection.
pub const CANCEL_REQUEST_CODE: i32 = 80_877_102;

/// A message body is bounded so a hostile or confused client cannot make the
/// server allocate arbitrarily. PostgreSQL's own limit for most messages is
/// well under this; a 1 GiB `Bind` is not a real workload, it is an attack.
pub const MAX_MESSAGE_LEN: usize = 512 * 1024 * 1024;

/// What the frontend can send.
#[derive(Debug, Clone, PartialEq)]
pub enum Frontend {
    /// `Q` — simple query: one string, possibly several statements.
    Query(String),
    /// `P` — parse: (destination statement name, SQL, parameter type OIDs).
    Parse {
        name: String,
        sql: String,
        param_types: Vec<u32>,
    },
    /// `B` — bind a statement to a portal.
    Bind {
        portal: String,
        statement: String,
        /// Per-parameter format codes: 0 text, 1 binary. Zero entries means
        /// "all text"; ONE entry means "this format for ALL parameters" — a
        /// rule that is easy to miss and produces garbage for every parameter
        /// after the first when it is.
        param_formats: Vec<i16>,
        /// `None` is SQL NULL, which is NOT the same as an empty byte string.
        params: Vec<Option<Vec<u8>>>,
        result_formats: Vec<i16>,
    },
    /// `D` — describe a statement (`S`) or a portal (`P`).
    Describe { kind: u8, name: String },
    /// `E` — execute a portal, up to `max_rows` (0 = unlimited).
    Execute { portal: String, max_rows: i32 },
    /// `C` — close a statement (`S`) or portal (`P`).
    Close { kind: u8, name: String },
    /// `S` — sync: end the implicit transaction, send ReadyForQuery.
    Sync,
    /// `H` — flush.
    Flush,
    /// `X` — terminate.
    Terminate,
    /// `d` — CopyData.
    CopyData(Vec<u8>),
    /// `c` — CopyDone.
    CopyDone,
    /// `f` — CopyFail with a message.
    CopyFail(String),
    /// `p` — the password / SASL response message. One tag, several meanings,
    /// decided by what the server last asked for.
    PasswordMessage(Vec<u8>),
    /// Anything else, kept so the session can refuse it BY TAG rather than by
    /// dropping the connection and leaving the client guessing.
    Unknown(u8, Vec<u8>),
}

/// One column in a `RowDescription`.
#[derive(Debug, Clone)]
pub struct FieldDesc {
    pub name: String,
    /// OID of the table this column came from, or 0 when it is computed.
    pub table_oid: u32,
    /// 1-based attribute number within that table, or 0.
    pub column_id: i16,
    pub type_oid: u32,
    /// `pg_type.typlen`: byte width, or -1 for a varlena.
    pub type_len: i16,
    pub type_mod: i32,
    /// 0 text, 1 binary.
    pub format: i16,
}

/// The transaction state byte a `ReadyForQuery` carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnStatus {
    /// `I` — idle, no transaction.
    Idle,
    /// `T` — in a transaction block.
    InTransaction,
    /// `E` — in a FAILED transaction block: everything but ROLLBACK is refused.
    Failed,
}

impl TxnStatus {
    fn byte(self) -> u8 {
        match self {
            TxnStatus::Idle => b'I',
            TxnStatus::InTransaction => b'T',
            TxnStatus::Failed => b'E',
        }
    }
}

// ------------------------------------------------------------------- reading

fn read_exact(r: &mut impl Read, n: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn be_i32(b: &[u8], at: usize) -> io::Result<i32> {
    b.get(at..at + 4)
        .and_then(|s| s.try_into().ok())
        .map(i32::from_be_bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated i32"))
}

fn be_i16(b: &[u8], at: usize) -> io::Result<i16> {
    b.get(at..at + 2)
        .and_then(|s| s.try_into().ok())
        .map(i16::from_be_bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated i16"))
}

/// Read a NUL-terminated string starting at `*at`, advancing past the NUL.
fn cstr(b: &[u8], at: &mut usize) -> io::Result<String> {
    let start = *at;
    let end = b[start..]
        .iter()
        .position(|&c| c == 0)
        .map(|p| start + p)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "unterminated string in message body",
            )
        })?;
    *at = end + 1;
    // Lossy rather than an error: a client that sends a non-UTF-8 identifier is
    // wrong, but dropping the connection tells them nothing. The lossy form
    // reaches the binder and comes back as "no such table `??`".
    Ok(String::from_utf8_lossy(&b[start..end]).into_owned())
}

/// What the very first packet on a connection turned out to be.
#[derive(Debug, Clone, PartialEq)]
pub enum Startup {
    /// A real startup packet with its parameter list.
    Params(Vec<(String, String)>),
    /// `SSLRequest` — the client is asking whether TLS is available.
    SslRequest,
    /// `GSSENCRequest`.
    GssEncRequest,
    /// `CancelRequest` for another backend's (pid, secret).
    Cancel { pid: i32, secret: i32 },
}

/// Read the untagged first packet.
///
/// A client may send `SSLRequest` (or `GSSENCRequest`) first, take the one-byte
/// answer, and then send the REAL startup packet on the same connection — so
/// this is called more than once per connection in the common case.
pub fn read_startup(r: &mut impl Read) -> io::Result<Startup> {
    let len = be_i32(&read_exact(r, 4)?, 0)?;
    if !(8..=(MAX_MESSAGE_LEN as i32)).contains(&len) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("implausible startup packet length {len}"),
        ));
    }
    let body = read_exact(r, len as usize - 4)?;
    let code = be_i32(&body, 0)?;
    match code {
        SSL_REQUEST_CODE => return Ok(Startup::SslRequest),
        GSSENC_REQUEST_CODE => return Ok(Startup::GssEncRequest),
        CANCEL_REQUEST_CODE => {
            return Ok(Startup::Cancel {
                pid: be_i32(&body, 4)?,
                secret: be_i32(&body, 8)?,
            })
        }
        PROTOCOL_V3 => {}
        other => {
            // Version 2 is the common wrong answer here (code 131072). Naming
            // the number is what turns a hang into a fixable message.
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported protocol version {other} — mpedb speaks v3 ({PROTOCOL_V3}) only"
                ),
            ));
        }
    }
    let mut at = 4usize;
    let mut params = Vec::new();
    while at < body.len() {
        let k = cstr(&body, &mut at)?;
        if k.is_empty() {
            break;
        }
        let v = cstr(&body, &mut at)?;
        params.push((k, v));
    }
    Ok(Startup::Params(params))
}

/// Read one tagged frontend message.
pub fn read_message(r: &mut impl Read) -> io::Result<Frontend> {
    let tag = read_exact(r, 1)?[0];
    let len = be_i32(&read_exact(r, 4)?, 0)?;
    if !(4..=(MAX_MESSAGE_LEN as i32)).contains(&len) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("implausible message length {len} for tag {}", tag as char),
        ));
    }
    let body = read_exact(r, len as usize - 4)?;
    let mut at = 0usize;
    Ok(match tag {
        b'Q' => Frontend::Query(cstr(&body, &mut at)?),
        b'P' => {
            let name = cstr(&body, &mut at)?;
            let sql = cstr(&body, &mut at)?;
            let n = be_i16(&body, at)? as usize;
            at += 2;
            let mut param_types = Vec::with_capacity(n);
            for _ in 0..n {
                param_types.push(be_i32(&body, at)? as u32);
                at += 4;
            }
            Frontend::Parse {
                name,
                sql,
                param_types,
            }
        }
        b'B' => {
            let portal = cstr(&body, &mut at)?;
            let statement = cstr(&body, &mut at)?;
            let nf = be_i16(&body, at)? as usize;
            at += 2;
            let mut param_formats = Vec::with_capacity(nf);
            for _ in 0..nf {
                param_formats.push(be_i16(&body, at)?);
                at += 2;
            }
            let np = be_i16(&body, at)? as usize;
            at += 2;
            let mut params = Vec::with_capacity(np);
            for _ in 0..np {
                let n = be_i32(&body, at)?;
                at += 4;
                if n < 0 {
                    // -1 is SQL NULL. A zero-length value is an EMPTY string,
                    // which is a different value; conflating them is the classic
                    // wire bug and it never errors, it just answers wrongly.
                    params.push(None);
                } else {
                    let n = n as usize;
                    let v = body
                        .get(at..at + n)
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::UnexpectedEof, "truncated bind parameter")
                        })?
                        .to_vec();
                    at += n;
                    params.push(Some(v));
                }
            }
            let nr = be_i16(&body, at)? as usize;
            at += 2;
            let mut result_formats = Vec::with_capacity(nr);
            for _ in 0..nr {
                result_formats.push(be_i16(&body, at)?);
                at += 2;
            }
            Frontend::Bind {
                portal,
                statement,
                param_formats,
                params,
                result_formats,
            }
        }
        b'D' => {
            let kind = *body
                .first()
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "empty Describe"))?;
            at = 1;
            Frontend::Describe {
                kind,
                name: cstr(&body, &mut at)?,
            }
        }
        b'E' => {
            let portal = cstr(&body, &mut at)?;
            Frontend::Execute {
                portal,
                max_rows: be_i32(&body, at)?,
            }
        }
        b'C' => {
            let kind = *body
                .first()
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "empty Close"))?;
            at = 1;
            Frontend::Close {
                kind,
                name: cstr(&body, &mut at)?,
            }
        }
        b'S' => Frontend::Sync,
        b'H' => Frontend::Flush,
        b'X' => Frontend::Terminate,
        b'd' => Frontend::CopyData(body),
        b'c' => Frontend::CopyDone,
        b'f' => Frontend::CopyFail(cstr(&body, &mut at)?),
        b'p' => Frontend::PasswordMessage(body),
        other => Frontend::Unknown(other, body),
    })
}

// ------------------------------------------------------------------- writing

/// Accumulates backend messages, so a whole response is one `write_all`.
///
/// Batching is not just for speed: `Execute` can produce thousands of DataRows,
/// and a per-message syscall on a per-connection process is the difference
/// between a scan that streams and one that stalls.
#[derive(Default)]
pub struct Out(Vec<u8>);

impl Out {
    pub fn new() -> Out {
        Out(Vec::with_capacity(8 * 1024))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn flush_to(&mut self, w: &mut impl Write) -> io::Result<()> {
        if self.0.is_empty() {
            return Ok(());
        }
        w.write_all(&self.0)?;
        w.flush()?;
        self.0.clear();
        Ok(())
    }

    /// Start a tagged message; returns the offset of its length field so
    /// [`Self::end`] can fill it in once the body is known.
    fn begin(&mut self, tag: u8) -> usize {
        self.0.push(tag);
        let at = self.0.len();
        self.0.extend_from_slice(&0i32.to_be_bytes());
        at
    }

    fn end(&mut self, at: usize) {
        // The length COUNTS ITSELF and excludes the tag — the single most
        // commonly mis-implemented rule in this protocol.
        let n = (self.0.len() - at) as i32;
        self.0[at..at + 4].copy_from_slice(&n.to_be_bytes());
    }

    fn cstr(&mut self, s: &str) {
        self.0.extend_from_slice(s.as_bytes());
        self.0.push(0);
    }

    fn i16(&mut self, v: i16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }

    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }

    /// `R` with a 0 payload — authentication succeeded.
    pub fn auth_ok(&mut self) {
        let at = self.begin(b'R');
        self.i32(0);
        self.end(at);
    }

    /// `R` asking for a cleartext password.
    pub fn auth_cleartext(&mut self) {
        let at = self.begin(b'R');
        self.i32(3);
        self.end(at);
    }

    /// `S` — a runtime parameter the client should track.
    pub fn parameter_status(&mut self, key: &str, value: &str) {
        let at = self.begin(b'S');
        self.cstr(key);
        self.cstr(value);
        self.end(at);
    }

    /// `K` — the (pid, secret) a CancelRequest must quote.
    pub fn backend_key_data(&mut self, pid: i32, secret: i32) {
        let at = self.begin(b'K');
        self.i32(pid);
        self.i32(secret);
        self.end(at);
    }

    /// `Z` — ready for the next query, carrying the transaction state.
    pub fn ready_for_query(&mut self, status: TxnStatus) {
        let at = self.begin(b'Z');
        self.0.push(status.byte());
        self.end(at);
    }

    /// `T` — the shape of the rows that follow.
    pub fn row_description(&mut self, fields: &[FieldDesc]) {
        let at = self.begin(b'T');
        self.i16(fields.len() as i16);
        for f in fields {
            self.cstr(&f.name);
            self.i32(f.table_oid as i32);
            self.i16(f.column_id);
            self.i32(f.type_oid as i32);
            self.i16(f.type_len);
            self.i32(f.type_mod);
            self.i16(f.format);
        }
        self.end(at);
    }

    /// `D` — one row. `None` is SQL NULL (length -1), which is NOT an empty
    /// value (length 0).
    pub fn data_row(&mut self, values: &[Option<Vec<u8>>]) {
        let at = self.begin(b'D');
        self.i16(values.len() as i16);
        for v in values {
            match v {
                None => self.i32(-1),
                Some(b) => {
                    self.i32(b.len() as i32);
                    self.0.extend_from_slice(b);
                }
            }
        }
        self.end(at);
    }

    /// `C` — the command completed; the tag carries the row count for the verbs
    /// that report one.
    pub fn command_complete(&mut self, tag: &str) {
        let at = self.begin(b'C');
        self.cstr(tag);
        self.end(at);
    }

    /// `I` — the query was empty (a lone `;`, or whitespace).
    ///
    /// It replaces CommandComplete rather than accompanying it, and a client
    /// that never receives either will wait forever.
    pub fn empty_query_response(&mut self) {
        let at = self.begin(b'I');
        self.end(at);
    }

    /// `n` — this statement returns no rows (the Describe answer for a DML
    /// statement without RETURNING).
    pub fn no_data(&mut self) {
        let at = self.begin(b'n');
        self.end(at);
    }

    /// `1` — Parse completed.
    pub fn parse_complete(&mut self) {
        let at = self.begin(b'1');
        self.end(at);
    }

    /// `2` — Bind completed.
    pub fn bind_complete(&mut self) {
        let at = self.begin(b'2');
        self.end(at);
    }

    /// `3` — Close completed.
    pub fn close_complete(&mut self) {
        let at = self.begin(b'3');
        self.end(at);
    }

    /// `s` — the portal ran to `max_rows` and has more.
    pub fn portal_suspended(&mut self) {
        let at = self.begin(b's');
        self.end(at);
    }

    /// `t` — the parameter types a prepared statement wants.
    pub fn parameter_description(&mut self, oids: &[u32]) {
        let at = self.begin(b't');
        self.i16(oids.len() as i16);
        for o in oids {
            self.i32(*o as i32);
        }
        self.end(at);
    }

    /// `E` — an error. The connection stays open; the client sees it at Sync.
    pub fn error(&mut self, severity: &str, code: &str, message: &str) {
        let at = self.begin(b'E');
        self.field(b'S', severity);
        // `V` is the NON-localised severity. Clients that parse severity read
        // this one, because `S` is translated in a real PostgreSQL.
        self.field(b'V', severity);
        self.field(b'C', code);
        self.field(b'M', message);
        self.0.push(0);
        self.end(at);
    }

    /// `N` — a notice. Same shape as an error, but not a failure.
    pub fn notice(&mut self, severity: &str, code: &str, message: &str) {
        let at = self.begin(b'N');
        self.field(b'S', severity);
        self.field(b'V', severity);
        self.field(b'C', code);
        self.field(b'M', message);
        self.0.push(0);
        self.end(at);
    }

    /// `A` — an asynchronous notification (LISTEN/NOTIFY).
    pub fn notification(&mut self, pid: i32, channel: &str, payload: &str) {
        let at = self.begin(b'A');
        self.i32(pid);
        self.cstr(channel);
        self.cstr(payload);
        self.end(at);
    }

    /// `G` — the server is ready to receive COPY data.
    pub fn copy_in_response(&mut self, columns: usize) {
        let at = self.begin(b'G');
        // 0 = textual format.
        self.0.push(0);
        self.i16(columns as i16);
        for _ in 0..columns {
            self.i16(0);
        }
        self.end(at);
    }

    /// `H` — the server is about to send COPY data.
    pub fn copy_out_response(&mut self, columns: usize) {
        let at = self.begin(b'H');
        self.0.push(0);
        self.i16(columns as i16);
        for _ in 0..columns {
            self.i16(0);
        }
        self.end(at);
    }

    /// `d` — one COPY payload.
    pub fn copy_data(&mut self, bytes: &[u8]) {
        let at = self.begin(b'd');
        self.0.extend_from_slice(bytes);
        self.end(at);
    }

    /// `c` — COPY is finished.
    pub fn copy_done(&mut self) {
        let at = self.begin(b'c');
        self.end(at);
    }

    fn field(&mut self, tag: u8, value: &str) {
        self.0.push(tag);
        self.cstr(value);
    }

    #[cfg(test)]
    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip helper: encode with `Out`, then read it back with the
    /// FRONTEND reader. Only works for messages that exist in both directions
    /// by tag; used below for the tags that do.
    fn framed(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![tag];
        v.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn message_length_counts_itself_and_excludes_the_tag() {
        // The single most commonly mis-implemented rule in this protocol. An
        // off-by-one desynchronises the stream and every later message is read
        // at the wrong offset — which looks like a corrupt client, not a bug
        // here.
        let mut o = Out::new();
        o.ready_for_query(TxnStatus::Idle);
        let b = o.bytes();
        assert_eq!(b[0], b'Z');
        assert_eq!(i32::from_be_bytes(b[1..5].try_into().unwrap()), 5);
        assert_eq!(b[5], b'I');
        assert_eq!(b.len(), 6);
    }

    #[test]
    fn a_null_parameter_is_minus_one_and_an_empty_one_is_zero() {
        // Conflating these is the classic wire bug: it never errors, it just
        // turns every NULL into '' or the other way round.
        let mut body = Vec::new();
        body.extend_from_slice(b"\0"); // portal
        body.extend_from_slice(b"\0"); // statement
        body.extend_from_slice(&0i16.to_be_bytes()); // no format codes
        body.extend_from_slice(&2i16.to_be_bytes()); // two parameters
        body.extend_from_slice(&(-1i32).to_be_bytes()); // NULL
        body.extend_from_slice(&0i32.to_be_bytes()); // empty string
        body.extend_from_slice(&0i16.to_be_bytes()); // no result formats
        let msg = framed(b'B', &body);
        let Frontend::Bind { params, .. } = read_message(&mut &msg[..]).unwrap() else {
            panic!("not a Bind")
        };
        assert_eq!(params, vec![None, Some(Vec::new())]);
    }

    #[test]
    fn data_row_distinguishes_null_from_empty_the_same_way() {
        let mut o = Out::new();
        o.data_row(&[None, Some(Vec::new()), Some(b"hi".to_vec())]);
        let b = o.bytes();
        assert_eq!(b[0], b'D');
        // 2-byte count, then (-1), (0), (2 ‖ "hi")
        let mut at = 5;
        assert_eq!(i16::from_be_bytes(b[at..at + 2].try_into().unwrap()), 3);
        at += 2;
        assert_eq!(i32::from_be_bytes(b[at..at + 4].try_into().unwrap()), -1);
        at += 4;
        assert_eq!(i32::from_be_bytes(b[at..at + 4].try_into().unwrap()), 0);
        at += 4;
        assert_eq!(i32::from_be_bytes(b[at..at + 4].try_into().unwrap()), 2);
    }

    #[test]
    fn the_startup_packet_is_untagged_and_ssl_request_is_told_apart_by_its_code() {
        // Reading the version as a length is the failure that hangs a server
        // for 196608 bytes, so both shapes get a test.
        let mut pkt = Vec::new();
        let body = {
            let mut b = PROTOCOL_V3.to_be_bytes().to_vec();
            b.extend_from_slice(b"user\0morten\0database\0app\0\0");
            b
        };
        pkt.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
        pkt.extend_from_slice(&body);
        let got = read_startup(&mut &pkt[..]).unwrap();
        assert_eq!(
            got,
            Startup::Params(vec![
                ("user".into(), "morten".into()),
                ("database".into(), "app".into()),
            ])
        );

        let mut ssl = 8i32.to_be_bytes().to_vec();
        ssl.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
        assert_eq!(read_startup(&mut &ssl[..]).unwrap(), Startup::SslRequest);

        let mut cancel = 16i32.to_be_bytes().to_vec();
        cancel.extend_from_slice(&CANCEL_REQUEST_CODE.to_be_bytes());
        cancel.extend_from_slice(&4242i32.to_be_bytes());
        cancel.extend_from_slice(&99i32.to_be_bytes());
        assert_eq!(
            read_startup(&mut &cancel[..]).unwrap(),
            Startup::Cancel {
                pid: 4242,
                secret: 99
            }
        );
    }

    #[test]
    fn an_unsupported_protocol_version_is_named_rather_than_hung_on() {
        // Protocol v2 is code 131072 and still shows up from old drivers.
        let mut pkt = 8i32.to_be_bytes().to_vec();
        pkt.extend_from_slice(&131_072i32.to_be_bytes());
        let e = read_startup(&mut &pkt[..]).unwrap_err();
        assert!(format!("{e}").contains("131072"), "{e}");
    }

    #[test]
    fn parse_reads_its_parameter_type_oids() {
        let mut body = Vec::new();
        body.extend_from_slice(b"st1\0");
        body.extend_from_slice(b"SELECT $1\0");
        body.extend_from_slice(&1i16.to_be_bytes());
        body.extend_from_slice(&20i32.to_be_bytes());
        let msg = framed(b'P', &body);
        let Frontend::Parse {
            name,
            sql,
            param_types,
        } = read_message(&mut &msg[..]).unwrap()
        else {
            panic!()
        };
        assert_eq!(name, "st1");
        assert_eq!(sql, "SELECT $1");
        assert_eq!(param_types, vec![20]);
    }

    #[test]
    fn an_implausible_length_is_refused_before_any_allocation() {
        // A hostile client must not be able to make the server reserve a
        // gigabyte by lying in five bytes.
        let mut msg = vec![b'Q'];
        msg.extend_from_slice(&i32::MAX.to_be_bytes());
        let e = read_message(&mut &msg[..]).unwrap_err();
        assert!(format!("{e}").contains("implausible"), "{e}");

        // …and a length below the minimum is equally not a message.
        let mut msg = vec![b'Q'];
        msg.extend_from_slice(&1i32.to_be_bytes());
        assert!(read_message(&mut &msg[..]).is_err());
    }

    #[test]
    fn an_unterminated_string_is_an_error_not_a_read_past_the_body() {
        let msg = framed(b'Q', b"SELECT 1"); // no NUL
        assert!(read_message(&mut &msg[..]).is_err());
    }

    #[test]
    fn unknown_tags_are_kept_so_the_session_can_refuse_them_by_name() {
        let msg = framed(b'!', b"whatever");
        let got = read_message(&mut &msg[..]).unwrap();
        assert_eq!(got, Frontend::Unknown(b'!', b"whatever".to_vec()));
    }

    #[test]
    fn error_carries_both_severity_fields_because_clients_parse_v_not_s() {
        let mut o = Out::new();
        o.error("ERROR", "42P01", "no such table");
        let b = o.bytes();
        assert_eq!(b[0], b'E');
        let body = &b[5..];
        assert!(body.starts_with(b"SERROR\0VERROR\0C42P01\0Mno such table\0\0"));
    }
}
