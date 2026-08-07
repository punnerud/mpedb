//! The protocol, end to end, over a pipe.
//!
//! No socket and no `psql`: a `Session` reads and writes anything that is
//! `Read + Write`, so a canned byte stream in and a byte stream out is the whole
//! harness. That keeps these tests runnable on a machine with no PostgreSQL
//! installed, in CI, in milliseconds — and it exercises exactly the code a real
//! client reaches, because the socket is the only thing swapped out.
//!
//! What is deliberately NOT tested here: whether `psql` is happy. That is a
//! different question (it depends on psql's own SQL, not on mpedb's protocol)
//! and it belongs in the measured differential suite, not in a unit test that
//! would silently start passing for the wrong reason.

use mpedb::{Config, Database};
use std::io::{Cursor, Read, Write};

/// A fake socket: reads from a canned buffer, writes into another.
struct Pipe {
    input: Cursor<Vec<u8>>,
    output: Vec<u8>,
}

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.input.read(buf)
    }
}

impl mpedb_pg::session::AsBytes for Pipe {
    fn written(&self) -> &[u8] {
        &self.output
    }
}

impl Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.output.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn startup_packet() -> Vec<u8> {
    let mut body = 196_608i32.to_be_bytes().to_vec();
    body.extend_from_slice(b"user\0tester\0database\0app\0\0");
    let mut pkt = ((body.len() + 4) as i32).to_be_bytes().to_vec();
    pkt.extend_from_slice(&body);
    pkt
}

fn msg(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![tag];
    v.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    v.extend_from_slice(body);
    v
}

fn query(sql: &str) -> Vec<u8> {
    let mut b = sql.as_bytes().to_vec();
    b.push(0);
    msg(b'Q', &b)
}

fn terminate() -> Vec<u8> {
    msg(b'X', &[])
}

fn db() -> Database {
    let toml = "\
[database]
path = \":memory:\"
size_mb = 16
durability = \"none\"

[[table]]
name = \"users\"
primary_key = [\"id\"]

  [[table.column]]
  name = \"id\"
  type = \"int64\"

  [[table.column]]
  name = \"nick\"
  type = \"text\"

  [[table.column]]
  name = \"score\"
  type = \"float64\"
  nullable = true
";
    let db = Database::open_in_memory(Config::from_toml_str(toml).unwrap()).unwrap();
    db.query(
        "INSERT INTO users VALUES (1,'ada',9.5),(2,'linus',NULL)",
        &[],
    )
    .unwrap();
    db
}

/// Drive a session with a script of frontend messages and return everything the
/// backend wrote.
fn run(script: Vec<Vec<u8>>) -> Vec<u8> {
    let mut input = startup_packet();
    for m in script {
        input.extend_from_slice(&m);
    }
    input.extend_from_slice(&terminate());
    let pipe = Pipe {
        input: Cursor::new(input),
        output: Vec::new(),
    };
    let mut s = mpedb_pg::Session::new(
        pipe,
        mpedb_pg::Options {
            db: db(),
            server_version: mpedb_pg::server_version(),
            require_password: false,
        },
    );
    s.run_for_test()
}

/// Split a backend byte stream into `(tag, body)` frames.
fn frames(bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 5 <= bytes.len() {
        let tag = bytes[i];
        let len = i32::from_be_bytes(bytes[i + 1..i + 5].try_into().unwrap()) as usize;
        assert!(len >= 4, "frame {} claims length {len}", tag as char);
        let end = i + 1 + len;
        assert!(end <= bytes.len(), "frame {} runs past the buffer", tag as char);
        out.push((tag, bytes[i + 5..end].to_vec()));
        i = end;
    }
    assert_eq!(i, bytes.len(), "trailing bytes are not a frame");
    out
}

fn tags(bytes: &[u8]) -> String {
    frames(bytes).iter().map(|(t, _)| *t as char).collect()
}

/// The text of every DataRow field, row by row.
fn rows(bytes: &[u8]) -> Vec<Vec<Option<String>>> {
    frames(bytes)
        .iter()
        .filter(|(t, _)| *t == b'D')
        .map(|(_, b)| {
            let n = i16::from_be_bytes(b[0..2].try_into().unwrap()) as usize;
            let mut at = 2usize;
            (0..n)
                .map(|_| {
                    let len = i32::from_be_bytes(b[at..at + 4].try_into().unwrap());
                    at += 4;
                    if len < 0 {
                        None
                    } else {
                        let v = String::from_utf8_lossy(&b[at..at + len as usize]).into_owned();
                        at += len as usize;
                        Some(v)
                    }
                })
                .collect()
        })
        .collect()
}

#[test]
fn a_connection_completes_the_startup_handshake_in_the_right_order() {
    // Every client waits for these, in this order. A missing ReadyForQuery is
    // the classic hang: the server has answered and the client is still
    // waiting, with nothing to see in either log.
    let out = run(vec![]);
    let t = tags(&out);
    assert!(t.starts_with('R'), "authentication first: {t}");
    assert!(t.contains('K'), "BackendKeyData: {t}");
    assert!(t.ends_with('Z'), "ReadyForQuery last: {t}");
    // server_version has to be among the ParameterStatus messages, because
    // SQLAlchemy and Django parse it at CONNECT.
    let sv = frames(&out)
        .into_iter()
        .filter(|(t, _)| *t == b'S')
        .map(|(_, b)| String::from_utf8_lossy(&b).into_owned())
        .find(|s| s.starts_with("server_version"))
        .expect("server_version must be sent");
    assert!(sv.contains("16."), "{sv}");
}

#[test]
fn a_simple_query_returns_a_description_then_rows_then_completion() {
    let out = run(vec![query("SELECT id, nick FROM users ORDER BY id")]);
    let t = tags(&out);
    assert!(t.contains("TDDCZ"), "expected T,D,D,C,Z in order: {t}");
    assert_eq!(
        rows(&out),
        vec![
            vec![Some("1".into()), Some("ada".into())],
            vec![Some("2".into()), Some("linus".into())],
        ]
    );
}

#[test]
fn a_null_arrives_as_a_null_and_not_as_an_empty_string() {
    // The whole reason DataRow has a -1 length. Conflating the two never
    // errors; it just turns every NULL into '' on the client.
    let out = run(vec![query("SELECT nick, score FROM users ORDER BY id")]);
    let r = rows(&out);
    assert_eq!(r[0], vec![Some("ada".into()), Some("9.5".into())]);
    assert_eq!(r[1], vec![Some("linus".into()), None]);
}

#[test]
fn a_write_reports_the_row_count_in_the_tag_postgresql_uses() {
    let out = run(vec![query("INSERT INTO users VALUES (3,'grace',7.25)")]);
    let tag = frames(&out)
        .into_iter()
        .find(|(t, _)| *t == b'C')
        .map(|(_, b)| String::from_utf8_lossy(&b).trim_end_matches('\0').to_string())
        .expect("CommandComplete");
    // The OID field is 0 and POSITIONAL: clients split on spaces, so `INSERT 1`
    // reads as an OID of 1 and no row count at all.
    assert_eq!(tag, "INSERT 0 1");
}

#[test]
fn several_statements_in_one_simple_query_each_get_their_own_completion() {
    let out = run(vec![query(
        "SELECT 1; INSERT INTO users VALUES (4,'hopper',NULL); SELECT 2",
    )]);
    let completions: Vec<String> = frames(&out)
        .into_iter()
        .filter(|(t, _)| *t == b'C')
        .map(|(_, b)| String::from_utf8_lossy(&b).trim_end_matches('\0').to_string())
        .collect();
    assert_eq!(completions.len(), 3, "{completions:?}");
    assert_eq!(completions[1], "INSERT 0 1");
    // …and exactly ONE ReadyForQuery for the whole simple-query message.
    assert_eq!(tags(&out).matches('Z').count(), 2, "startup + the query");
}

#[test]
fn an_error_is_reported_with_a_sqlstate_and_the_connection_survives() {
    // A failed statement must not close the connection: the client is expected
    // to see the error and carry on. `psql` prints it and stays at the prompt.
    let out = run(vec![
        query("SELECT * FROM no_such_table"),
        query("SELECT 1 AS ok"),
    ]);
    let err = frames(&out)
        .into_iter()
        .find(|(t, _)| *t == b'E')
        .map(|(_, b)| String::from_utf8_lossy(&b).into_owned())
        .expect("ErrorResponse");
    // 42P01 = undefined_table. Reporting a syntax error here would make every
    // migration tool's existence probe unrecoverable.
    assert!(err.contains("42P01"), "{err}");
    // The later statement still ran.
    assert!(rows(&out).iter().any(|r| r == &vec![Some("1".into())]));
}

#[test]
fn the_extended_query_protocol_round_trips_a_parameter() {
    // psycopg uses this path for everything; a server that only implements the
    // simple query answers nothing it sends.
    let mut parse = b"st\0".to_vec();
    parse.extend_from_slice(b"SELECT nick FROM users WHERE id = $1\0");
    parse.extend_from_slice(&1i16.to_be_bytes());
    parse.extend_from_slice(&20i32.to_be_bytes()); // int8

    let mut bind = b"\0st\0".to_vec(); // unnamed portal, statement "st"
    bind.extend_from_slice(&0i16.to_be_bytes()); // no format codes -> all text
    bind.extend_from_slice(&1i16.to_be_bytes()); // one parameter
    bind.extend_from_slice(&1i32.to_be_bytes());
    bind.extend_from_slice(b"2");
    bind.extend_from_slice(&0i16.to_be_bytes()); // no result formats

    let mut exec = b"\0".to_vec();
    exec.extend_from_slice(&0i32.to_be_bytes());

    let out = run(vec![
        msg(b'P', &parse),
        msg(b'B', &bind),
        msg(b'E', &exec),
        msg(b'S', &[]),
    ]);
    let t = tags(&out);
    assert!(t.contains('1'), "ParseComplete: {t}");
    assert!(t.contains('2'), "BindComplete: {t}");
    assert_eq!(rows(&out), vec![vec![Some("linus".into())]]);
}

#[test]
fn a_null_bind_parameter_is_a_null_and_not_the_string_null() {
    let mut parse = b"st\0".to_vec();
    parse.extend_from_slice(b"SELECT $1 IS NULL AS is_null\0");
    parse.extend_from_slice(&1i16.to_be_bytes());
    parse.extend_from_slice(&25i32.to_be_bytes()); // text

    let mut bind = b"\0st\0".to_vec();
    bind.extend_from_slice(&0i16.to_be_bytes());
    bind.extend_from_slice(&1i16.to_be_bytes());
    bind.extend_from_slice(&(-1i32).to_be_bytes()); // NULL
    bind.extend_from_slice(&0i16.to_be_bytes());

    let mut exec = b"\0".to_vec();
    exec.extend_from_slice(&0i32.to_be_bytes());

    let out = run(vec![
        msg(b'P', &parse),
        msg(b'B', &bind),
        msg(b'E', &exec),
        msg(b'S', &[]),
    ]);
    assert_eq!(rows(&out), vec![vec![Some("t".into())]]);
}

#[test]
fn a_transaction_block_commits_as_one_and_reports_its_state() {
    // The block is buffered and replayed as ONE mpedb transaction at COMMIT —
    // the resolution of PostgreSQL's interactive transactions against mpedb's
    // single writer. What must hold from the client's side: BEGIN reports 'T',
    // COMMIT reports 'I', and the write is visible afterwards.
    let out = run(vec![
        query("BEGIN"),
        query("INSERT INTO users VALUES (9,'turing',NULL)"),
        query("COMMIT"),
        query("SELECT nick FROM users WHERE id = 9"),
    ]);
    let states: Vec<u8> = frames(&out)
        .into_iter()
        .filter(|(t, _)| *t == b'Z')
        .map(|(_, b)| b[0])
        .collect();
    // startup, BEGIN, INSERT, COMMIT, SELECT
    assert_eq!(states, vec![b'I', b'T', b'T', b'I', b'I'], "{states:?}");
    assert_eq!(rows(&out), vec![vec![Some("turing".into())]]);
}

#[test]
fn a_rolled_back_block_leaves_nothing_behind() {
    let out = run(vec![
        query("BEGIN"),
        query("INSERT INTO users VALUES (9,'turing',NULL)"),
        query("ROLLBACK"),
        query("SELECT count(*) FROM users WHERE id = 9"),
    ]);
    assert_eq!(rows(&out).last().unwrap(), &vec![Some("0".into())]);
}

#[test]
fn a_failed_statement_inside_a_block_poisons_it_the_way_postgresql_does() {
    // Everything but ROLLBACK is refused until the block ends. Not reproducing
    // this lets a client commit a block whose middle failed.
    let out = run(vec![
        query("BEGIN"),
        query("SELECT * FROM no_such_table"),
        query("SELECT 1"),
        query("ROLLBACK"),
    ]);
    let states: Vec<u8> = frames(&out)
        .into_iter()
        .filter(|(t, _)| *t == b'Z')
        .map(|(_, b)| b[0])
        .collect();
    assert_eq!(states, vec![b'I', b'T', b'E', b'E', b'I'], "{states:?}");
    let errs = frames(&out).iter().filter(|(t, _)| *t == b'E').count();
    assert_eq!(errs, 2, "the failure, then the refusal of the next statement");
}

#[test]
fn a_binary_parameter_is_refused_by_name_rather_than_decoded_wrongly() {
    // Binary is where a wrong answer hides best: a misencoded int8 is eight
    // bytes that decode to a plausible number with no error anywhere.
    let mut parse = b"st\0".to_vec();
    parse.extend_from_slice(b"SELECT $1\0");
    parse.extend_from_slice(&0i16.to_be_bytes());

    let mut bind = b"\0st\0".to_vec();
    bind.extend_from_slice(&1i16.to_be_bytes());
    bind.extend_from_slice(&1i16.to_be_bytes()); // ONE format code, binary
    bind.extend_from_slice(&1i16.to_be_bytes());
    bind.extend_from_slice(&8i32.to_be_bytes());
    bind.extend_from_slice(&[0u8; 8]);
    bind.extend_from_slice(&0i16.to_be_bytes());

    let out = run(vec![msg(b'P', &parse), msg(b'B', &bind), msg(b'S', &[])]);
    let err = frames(&out)
        .into_iter()
        .find(|(t, _)| *t == b'E')
        .map(|(_, b)| String::from_utf8_lossy(&b).into_owned())
        .expect("a binary parameter must be refused");
    assert!(err.contains("binary parameter format"), "{err}");
}

#[test]
fn the_catalog_answers_a_join_across_several_relations() {
    // The load-bearing design claim, over the wire this time: catalog relations
    // are real tables in a session-private database, so the ordinary planner
    // joins them.
    let out = run(vec![query(
        "SELECT c.relname, n.nspname FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind = 'r' ORDER BY c.relname",
    )]);
    assert_eq!(
        rows(&out),
        vec![vec![Some("users".into()), Some("public".into())]]
    );
}

#[test]
fn information_schema_is_reachable_under_its_qualified_name() {
    let out = run(vec![query(
        "SELECT column_name, data_type, is_nullable FROM information_schema.columns \
         WHERE table_name = 'users' ORDER BY ordinal_position",
    )]);
    assert_eq!(
        rows(&out),
        vec![
            vec![Some("id".into()), Some("bigint".into()), Some("NO".into())],
            // `nick` is declared without `nullable = true` in this schema, so
            // NOT NULL is the right answer — the assertion was wrong here
            // first, which is the direction one wants.
            // `id` is NOT NULL because it is the primary key; `nick` and
            // `score` are both nullable because mpedb's TOML `nullable`
            // defaults to TRUE (`config.rs`, `default = "default_true"`) — so
            // NOT declaring it is not the same as declaring it false. The first
            // version of this assertion assumed the opposite and the catalog
            // corrected it, which is the direction one wants.
            vec![Some("nick".into()), Some("text".into()), Some("YES".into())],
            vec![
                Some("score".into()),
                Some("double precision".into()),
                Some("YES".into())
            ],
        ]
    );
}

#[test]
fn the_postgres_dialect_is_live_on_the_wire() {
    // The session sets it per connection; these three all fail to LEX under the
    // sqlite dialect, so they are proof the dialect reached the tokenizer.
    let out = run(vec![
        query("SELECT 42::text AS c"),
        query("SELECT $$dollar$$ AS d"),
        query("SELECT 'abc' ~ '^a' AS m"),
    ]);
    let r = rows(&out);
    assert_eq!(r[0], vec![Some("42".into())]);
    assert_eq!(r[1], vec![Some("dollar".into())]);
    assert_eq!(r[2], vec![Some("t".into())]);
}

#[test]
fn an_empty_query_gets_an_empty_query_response_rather_than_silence() {
    // A client that receives neither EmptyQueryResponse nor CommandComplete
    // waits forever.
    let out = run(vec![query("")]);
    assert!(tags(&out).contains('I'), "{}", tags(&out));
}

#[test]
fn show_answers_the_settings_a_driver_asks_for_at_connect() {
    let out = run(vec![query("SHOW server_version"), query("SET timezone='UTC'")]);
    let r = rows(&out);
    assert!(r[0][0].as_deref().unwrap().starts_with("16."), "{r:?}");
    // SET must SUCCEED — a driver whose SET fails gives up on the connection.
    let completions: Vec<String> = frames(&out)
        .into_iter()
        .filter(|(t, _)| *t == b'C')
        .map(|(_, b)| String::from_utf8_lossy(&b).trim_end_matches('\0').to_string())
        .collect();
    assert!(completions.contains(&"SET".to_string()), "{completions:?}");
}
