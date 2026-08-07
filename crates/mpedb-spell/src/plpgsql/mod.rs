//! PL/pgSQL frontend: PostgreSQL's procedural language, parsed **on the host at
//! define time only**, compiled to the SAME IR the Python and Rust frontends
//! emit. The runtime never sees PL/pgSQL source — that is the PySpell security
//! boundary, and a third language must not become a third way around it.
//!
//! It exists for MIGRATION. `pg_dump` writes `CREATE FUNCTION … AS $$ … $$
//! LANGUAGE plpgsql;` and this compiles that statement whole, which is why the
//! unit here is the statement and not the bare body (see `head`).
//!
//! # What compiles
//!
//! The header: `CREATE [OR REPLACE] FUNCTION [schema.]name(params) RETURNS type
//! AS $$ … $$ LANGUAGE plpgsql [hints…]`. Named and unnamed parameters (an
//! unnamed one is addressable as `$n`), multi-word/qualified/modified type
//! names, and the volatility/security/cost/parallel hints, which are skipped
//! because they change no answer.
//!
//! The body: `[DECLARE …] BEGIN … END`, with
//!
//! - assignment (`v := e`, and PostgreSQL's `v = e`),
//! - `IF/ELSIF/ELSE/END IF`, `WHILE … LOOP`, bare `LOOP`, `FOR v IN [REVERSE]
//!   lo..hi [BY step] LOOP`, `EXIT [WHEN]`, `CONTINUE [WHEN]`, `RETURN`, `NULL`,
//! - expressions: literals, parameters, variables, `+ - * / %`, unary `-`,
//!   comparisons, `AND`/`OR`/`NOT` (value-preserving short circuit), `IS [NOT]
//!   NULL`, parentheses, and `::int` casts as identities.
//!
//! # What refuses, and the shape of every refusal
//!
//! Everything else, **by name and with a reason** — never a bare "unsupported".
//! They fall into three groups, and the groups matter more than the list:
//!
//! 1. **A different feature.** `RETURNS SETOF` / `RETURNS TABLE` make the
//!    function a row source; `RETURNS trigger` is a trigger body; `CREATE
//!    PROCEDURE` is invoked with `CALL`. Each says which feature it is.
//! 2. **The stored-function contract.** `SELECT … INTO`, `PERFORM`, `INSERT`/
//!    `UPDATE`/`DELETE` and `FOR … IN SELECT` all touch the database, and a
//!    stored SQL function is evaluated per row inside a statement that is
//!    already scanning (`mpedb/src/spellfn.rs`). Those bodies belong in a
//!    stored PROCEDURE, and the message says so.
//! 3. **The IR cannot say it, and pretending would be a wrong answer.** `RAISE`
//!    has no opcode that raises. `||` has no concatenation opcode. A converting
//!    `::text` has no cast opcode, so the value would keep the type it already
//!    had. Dynamic `EXECUTE` is precisely the define-time-compilation hole the
//!    security boundary exists to close. An `EXCEPTION` block needs a
//!    subtransaction inside the caller's statement.
//!
//! The one place a compiled body could MEAN something different from
//! PostgreSQL is NULL comparison, and it is closed rather than documented away:
//! see `body`'s module docs.

mod body;
mod head;
mod lex;

use crate::emit::Skeleton;
use mpedb_types::Result;

/// Compile one `CREATE FUNCTION … LANGUAGE plpgsql` statement.
pub fn compile(src: &str) -> Result<Skeleton> {
    let toks = lex::lex(src)?;
    let h = head::parse_head(src, &toks)?;

    // The body is lexed on its own, but every error location is reported
    // against the ORIGINAL source: the lexer records byte offsets, and
    // `body_at` shifts them back onto the file the user wrote. Without the
    // shift, a mistake on the body's third line reports line 3 of a string that
    // appears nowhere.
    let mut btoks = lex::lex(&h.body)?;
    for l in &mut btoks {
        l.at += h.body_at;
    }
    let mut b = body::Body::new(src, &btoks, &h.params)?;
    b.compile_block()?;
    let fb = b.b;
    Ok(Skeleton {
        name: h.name,
        argc: h.params.len() as u16,
        nlocals: fb.nlocals(),
        consts: fb.consts,
        instrs: fb.instrs,
        calls: fb.calls,
    })
}

#[cfg(test)]
mod tests;
