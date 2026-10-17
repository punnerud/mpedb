//! Oracle PL/SQL trigger bodies. SQL `BEGIN … END` capture stays in `ddl`.
//! A `DECLARE` block is always PL/SQL. A `BEGIN` block is PL/SQL only when
//! it reads `:NEW` or `:OLD`. The stored source is the original bytes, so
//! the PL/SQL compiler sees `:=` and the correlation names.

use super::Parser;
use crate::token::{Kw, Tok};
use mpedb_types::Result;

impl Parser<'_> {
    /// True when a later token is a bare `:NEW` or `:OLD` (not `:NEW:` and
    /// not a string or comment — those never become tokens).
    pub(super) fn rest_has_correlation(&self) -> bool {
        self.toks[self.pos..].iter().any(|tok| bare_correlation(self.src, tok))
    }

    /// Source from the current token (`DECLARE` or `BEGIN`) through the
    /// matching block `END`, with one trailing semicolon. `END IF` and
    /// `END LOOP` do not close the block. `END CASE` does, because `CASE`
    /// opened a level.
    pub(super) fn capture_plsql_block(&mut self) -> Result<String> {
        let start = self.here();
        let mut depth = 0usize;
        loop {
            match self.advance() {
                Some(Tok::Kw(Kw::Begin)) | Some(Tok::Kw(Kw::Case)) => depth += 1,
                Some(Tok::Kw(Kw::End)) => {
                    let closes_control = matches!(
                        self.peek(),
                        Some(Tok::Ident(word))
                            if word.eq_ignore_ascii_case("IF")
                                || word.eq_ignore_ascii_case("LOOP")
                    );
                    if closes_control {
                        continue;
                    }
                    if depth == 0 {
                        return Err(self.err_here("END without BEGIN in trigger body"));
                    }
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Some(_) => {}
                None => return Err(self.err_here("unterminated trigger body: expected END")),
            }
        }
        let end = self
            .toks
            .get(self.pos - 1)
            .map(|tok| tok.end)
            .unwrap_or(self.src.len());
        let mut src = self.src.get(start..end).unwrap_or("").trim().to_string();
        if src.is_empty() {
            return Err(self.err_here("trigger body must contain a statement"));
        }
        if !src.ends_with(';') {
            src.push(';');
        }
        Ok(src)
    }
}

fn bare_correlation(src: &str, tok: &crate::token::SpTok) -> bool {
    let Tok::CustomOp(sym) = &tok.tok else {
        return false;
    };
    if !sym.eq_ignore_ascii_case("NEW") && !sym.eq_ignore_ascii_case("OLD") {
        return false;
    }
    let raw = src.get(tok.pos..tok.end).unwrap_or("");
    raw.len() == sym.len() + 1 && raw.as_bytes().first() == Some(&b':')
}

#[cfg(test)]
mod tests {
    use crate::ddl::{DdlStmt, TriggerBodySpec};
    use crate::parse_ddl;

    #[test]
    fn oracle_trigger_body_keeps_declare_and_correlation() {
        let ddl = parse_ddl(
            "CREATE TRIGGER t AFTER INSERT ON ledger FOR EACH ROW \
             DECLARE n VARCHAR2(40); BEGIN n := :NEW.note; END",
        )
        .unwrap()
        .unwrap();
        match ddl {
            DdlStmt::CreateTrigger(spec) => match spec.body {
                TriggerBodySpec::Oracle(src) => {
                    assert!(src.starts_with("DECLARE"), "{src}");
                    assert!(src.contains(":NEW.note"), "{src}");
                    assert!(src.contains(":="), "{src}");
                    assert!(src.ends_with(';'), "{src}");
                }
                other => panic!("expected Oracle body, got {other:?}"),
            },
            other => panic!("expected CreateTrigger, got {other:?}"),
        }
    }

    #[test]
    fn a_quoted_new_in_a_sql_body_stays_sql() {
        let ddl = parse_ddl(
            "CREATE TRIGGER t AFTER INSERT ON ledger FOR EACH ROW \
             BEGIN INSERT INTO audit (note) VALUES (':NEW'); END",
        )
        .unwrap()
        .unwrap();
        match ddl {
            DdlStmt::CreateTrigger(spec) => match spec.body {
                TriggerBodySpec::Sql(src) => assert!(src.contains("':NEW'"), "{src}"),
                other => panic!("expected SQL body, got {other:?}"),
            },
            other => panic!("expected CreateTrigger, got {other:?}"),
        }
    }
}
