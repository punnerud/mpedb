//! mpedb-proc — PySpell-style stored procedures for mpedb.
//!
//! User logic written in a small **Python subset** or **Rust subset** is
//! parsed exactly once, host-side, at [`ProcEngine::define`] time; compiled
//! to a compact, sandboxed, content-hashed IR; stored *inside* the database;
//! and executed by hash with a hard instruction budget.
//!
//! # The security model (exactly PySpell's)
//!
//! > The parser stays on the host; the runtime only ever sees IR — that is
//! > the security boundary.
//!
//! Concretely:
//!
//! - `rustpython-parser` and `syn` run only inside `define`. No code path
//!   reachable from [`ProcEngine::call`] parses Python, Rust or SQL.
//! - SQL appears in proc source exclusively as **string literals** inside
//!   `db.query("...", [...])` / `db.execute("...", [...])`. Each is
//!   compiled once at define time via the facade's `prepare` (published to
//!   the shared plan registry, design/DESIGN.md §7.2) and only its `PlanHash` is
//!   embedded in the proc — injection has nowhere to happen at run time.
//! - Stored blobs are treated as hostile on load: bounds-checked decode,
//!   jump-target and stack-depth analysis, plan-arity checks; anything off
//!   is [`mpedb::Error::Corrupt`], never a panic (same discipline as
//!   `mpedb_types::expr`). Hash-keyed blobs additionally verify
//!   `blake3(blob) == key` before decoding.
//! - The interpreter has no ambient authority: no I/O, no imports, no
//!   attribute access, no clock — only arithmetic, locals and the embedded
//!   plan hashes. Runaway procs die on a per-call budget (default
//!   1_000_000 instructions + 10_000 db calls + 10_000_000 cursor rows;
//!   every executed instruction — in particular every backward jump —
//!   costs budget). All three dimensions are settable
//!   ([`ProcEngine::set_budget`], `mpedb proc call --budget`).
//!
//! # Streaming cursors (read-only procs)
//!
//! `db.query` materializes the whole result as an interpreter list —
//! O(result) memory. For near-data analytics both frontends also offer a
//! **cursor** that pulls ONE row at a time from the engine scan
//! (`Database::stream_query` under the hood; O(1) interpreter memory in
//! the result size):
//!
//! - Python: `for row in db.rows("SELECT ...", [args]):` — the only `for`
//!   form; general iteration stays rejected.
//! - Rust: `let c = db.rows("SELECT ...", &[args]);` then
//!   `while db.cursor_next(c) { ... db.cursor_col(c, i) ... }`.
//!
//! v1 rule: cursors are allowed only in procedures that do not write
//! (enforced at define time with a located error AND structurally in IR
//! validation, so hostile blobs cannot bypass it). Each advanced row costs
//! one unit of the row budget; cursor opens cost one db call.
//!
//! # Value semantics: Python/Rust, not SQL
//!
//! Inside a proc, [`Value::Null`] *is* Python's `None` (and stands in for
//! Rust's `()`), and ordinary language semantics apply — **not** SQL 3VL:
//! `None == None` is true, `not None` is true, comparisons never yield
//! "unknown". NULLs read from the database arrive as `None`, and `None`
//! passed to a db call is stored as SQL NULL. The SQL *inside* the plans
//! keeps full 3VL, evaluated by the SQL executor — the boundary is the
//! `db.query`/`db.execute` call.
//!
//! Per-frontend semantics are preserved where the languages differ: the
//! Python `/` on ints yields a float and `//`/`%` floor toward the divisor;
//! the Rust `/`/`%` truncate toward zero. The same algorithm written in
//! both languages therefore behaves identically only where the languages
//! agree — and its two blobs may legitimately hash differently.
//!
//! # Example
//!
//! ```no_run
//! use mpedb::{params, Database};
//! use mpedb_proc::{Lang, ProcEngine};
//!
//! let db = Database::open(std::path::Path::new("config.toml")).unwrap();
//! let engine = ProcEngine::new(&db);
//! let hash = engine.define(r#"
//! def transfer(src, dst, amount):
//!     rows = db.query("SELECT balance FROM accounts WHERE id = $1", [src])
//!     if len(rows) == 0 or rows[0][0] < amount:
//!         return -1
//!     db.execute("UPDATE accounts SET balance = balance - $2 WHERE id = $1", [src, amount])
//!     db.execute("UPDATE accounts SET balance = balance + $2 WHERE id = $1", [dst, amount])
//!     return rows[0][0] - amount
//! "#, Lang::Python).unwrap();
//! // Any attached process can now call by name or by hash:
//! let v = engine.call(&hash.to_string(), &params![1, 2, 50]).unwrap();
//! println!("new balance: {v}");
//! ```

mod engine;

pub use engine::{Lang, ProcEngine, ProcInfo, NS_PROC, NS_PROC_HASH};
pub use mpedb_spell::{Budget, ProcHash, ProcValue};

// Re-exported for callers that match on errors / build params.
pub use mpedb::{Error, Result, Value};


