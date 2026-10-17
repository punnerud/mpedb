    use super::*;

    #[test]
    fn statements_split_on_top_level_semicolons_and_drop_psql_meta_commands() {
        // A `\d` in a corpus file is psql's SQL, not the corpus's. Sending it
        // would measure psql rather than mpedb.
        let sql = "SELECT 1;\n\\d foo\nSELECT 2;\n-- a comment;\nSELECT 3;";
        assert_eq!(statements(sql), vec!["SELECT 1", "SELECT 2", "SELECT 3"]);
    }

    #[test]
    fn a_semicolon_inside_a_literal_is_not_a_separator() {
        assert_eq!(statements("SELECT 'a;b';"), vec!["SELECT 'a;b'"]);
    }

    #[test]
    fn a_doubled_quote_inside_a_string_does_not_end_it() {
        // The bug this pins: two single-quotes read as "close then open"
        // leave the splitter thinking it is OUTSIDE a string, so the next
        // `;` tears a statement in half and the tail becomes a nonsense
        // "statement". 1 047 refusals were exactly that.
        assert_eq!(
            statements("SELECT 'it''s; fine' AS a;"),
            vec!["SELECT 'it''s; fine' AS a"]
        );
        assert_eq!(
            statements("SELECT 'a''';\nSELECT 2;"),
            vec!["SELECT 'a'''", "SELECT 2"]
        );
    }

    #[test]
    fn a_copy_payload_is_data_and_never_read_as_sql() {
        // `COPY t FROM stdin;` is followed by raw rows ended by `\.`. Reading
        // those as statements produced 1 030 "unexpected character `\`"
        // refusals AND split every `;` inside a data row into another bogus
        // statement — inflating both the corpus size and the failure count with
        // text that was never SQL.
        let sql = "\
CREATE TABLE t (a int, b text);
COPY t FROM stdin;
1\tone; with a semicolon
2\ttwo \\ with a backslash
\\.
SELECT * FROM t;";
        assert_eq!(
            statements(sql),
            vec![
                "CREATE TABLE t (a int, b text)",
                "COPY t FROM stdin",
                "SELECT * FROM t",
            ]
        );
    }

    /// The one cause kept at full resolution, and its neighbours still shaped.
    /// The name extractor decides which of two buckets a refusal lands in, so
    /// the spellings the corpus actually uses have to work — and the ones that
    /// are NOT a table create must not be mistaken for one.
    /// `\g` and friends end a statement the way `;` does — that is what they
    /// mean. Without this, `SELECT 1 \g SELECT 2` arrived as one blob with a
    /// backslash in the middle, and neither engine could run it.
    #[test]
    fn a_psql_send_command_terminates_a_statement_like_a_semicolon() {
        assert_eq!(
            statements("SELECT 1 as one \\g SELECT 2 \\gx SELECT 3;"),
            vec!["SELECT 1 as one", "SELECT 2", "SELECT 3"]
        );
        assert_eq!(statements("select 10 as a \\gset
SELECT 1;"), vec!["select 10 as a", "SELECT 1"]);
    }

    /// The two commands whose answer is NOT the row set drop their buffer:
    /// `\gdesc` describes the columns instead of producing them, so comparing
    /// our rows against PostgreSQL's description scores a difference neither
    /// engine disagrees about.
    #[test]
    fn a_describe_or_pivot_command_drops_the_statement_it_would_have_sent() {
        assert_eq!(statements("SELECT 1 \\gdesc SELECT 2;"), vec!["SELECT 2"]);
        assert_eq!(statements("SELECT 1 \\crosstabview SELECT 2;"), vec!["SELECT 2"]);
    }

    /// `\r` RESETS the buffer: psql never ran what came before it, so the
    /// harness must not either. Running it would compare an answer PostgreSQL
    /// was never asked for.
    #[test]
    fn a_buffer_reset_discards_the_statement_before_it() {
        assert_eq!(statements("SELECT 2 \\r SELECT 3;"), vec!["SELECT 3"]);
    }

    /// A backslash inside a STRING is content, not a command. `\ud83d\ude04`
    /// is a surrogate pair in a JSON value and appears 39 times in the corpus.
    #[test]
    fn a_backslash_inside_a_string_literal_is_not_a_meta_command() {
        assert_eq!(
            statements("SELECT '\\ud83d\\ude04' AS emoji;"),
            vec!["SELECT '\\ud83d\\ude04' AS emoji"]
        );
        // …and one that merely looks like a send command.
        assert_eq!(statements("SELECT 'a \\g b';"), vec!["SELECT 'a \\g b'"]);
    }

    #[test]
    fn a_created_table_name_is_read_out_of_every_spelling_the_corpus_uses() {
        for (sql, want) in [
            ("CREATE TABLE t (a int)", "t"),
            ("create table t(a int)", "t"),
            ("CREATE TEMP TABLE t (a int)", "t"),
            ("CREATE TEMPORARY TABLE IF NOT EXISTS t (a int)", "t"),
            ("CREATE UNLOGGED TABLE public.t (a int)", "t"),
            ("CREATE TABLE \"MixedCase\" (a int)", "mixedcase"),
            ("CREATE GLOBAL TEMPORARY TABLE t (a int)", "t"),
        ] {
            assert_eq!(created_table_name(sql).as_deref(), Some(want), "{sql}");
        }
        // Not a table create — a false positive here would silently move
        // refusals into the "consequence" bucket and flatter the split.
        for sql in [
            "CREATE INDEX i ON t (a)",
            "CREATE VIEW v AS SELECT 1",
            "CREATE FUNCTION f() RETURNS int AS $$ $$ LANGUAGE sql",
            "SELECT 1",
            "",
        ] {
            assert_eq!(created_table_name(sql), None, "{sql}");
        }
    }

    #[test]
    fn the_missing_table_name_is_read_from_both_message_shapes() {
        assert_eq!(
            missing_table_name("bind error: unknown table `MinMaxTest1`").as_deref(),
            Some("minmaxtest1")
        );
        assert_eq!(
            missing_table_name("bind error: DROP TABLE: no such table `t3`").as_deref(),
            Some("t3")
        );
        assert_eq!(missing_table_name("bind error: unknown column `x`"), None);
    }

    #[test]
    fn an_unknown_function_keeps_its_name_while_everything_else_is_shaped() {
        // The REAL message shape, prefix and all — the first version of this
        // test used a stripped message and passed while the code did nothing.
        assert_eq!(
            cause_key(
                "bind error: unknown function `pg_advisory_xact_lock()`; available: lower, upper"
            ),
            "unknown function `pg_advisory_xact_lock()`"
        );
        assert_eq!(
            cause_key("bind error: unknown function `unnest()`; available: lower"),
            "unknown function `unnest()`"
        );
        assert_eq!(
            cause_key("SQL parse error at byte 22: unexpected trailing input `Ident(\"cascade\")`"),
            "unexpected trailing input `Ident(\"cascade\")`"
        );
        // The table-function bucket keeps its name for the same reason — three
        // different jobs (an array type, a join-side row source, a corpus-local
        // function nobody will implement) were one line.
        assert_eq!(
            cause_key(
                "SQL parse error at byte 14: `unnest(…)` is a table function in FROM \
                 position, and mpedb has no table-function planner — it produces rows \
                 from an array or JSON value"
            ),
            "table function in FROM `unnest(…)`"
        );
        assert_eq!(
            cause_key(
                "SQL parse error at byte 14: `generate_series(…)` is a table function in \
                 FROM position, and mpedb has no table-function planner — mpedb generates \
                 it in the FIRST `FROM` position only"
            ),
            "table function in FROM `generate_series(…)`"
        );
        // Everything else still collapses, or the list is 20 000 lines long.
        assert_eq!(
            cause_key("bind error: unknown table `minmaxtest1`"),
            cause_key("bind error: unknown table `other`")
        );
    }

    /// The progress counter counts FRAMES, not bytes that look like frames.
    ///
    /// The claim in `Pipe::write` is that a `Z` inside a row VALUE must not be
    /// counted, because a data-dependent progress counter would name the wrong
    /// statement exactly when a hang makes it matter. Asserted rather than
    /// asserted-in-a-comment.
    #[test]
    fn the_hang_watchdogs_progress_counter_counts_frames_not_bytes() {
        use std::io::Write as _;
        let done = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut p = Pipe {
            input: std::io::Cursor::new(Vec::new()),
            output: Vec::new(),
            done: done.clone(),
            scanned: 0,
        };
        let frame = |tag: u8, body: &[u8]| {
            let mut v = vec![tag];
            v.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
            v.extend_from_slice(body);
            v
        };
        // (This test drives `Pipe` directly, so there is no startup handshake
        // and no off-by-one to subtract — the subtraction lives in
        // `run_with_timeout`, which is where the handshake happens.)
        //
        // A DataRow whose single value is the text "Z" — one byte that would
        // fool a scanner and must not fool this one.
        let mut d = 1i16.to_be_bytes().to_vec();
        d.extend_from_slice(&1i32.to_be_bytes());
        d.push(b'Z');
        p.write_all(&frame(b'D', &d)).unwrap();
        assert_eq!(done.load(std::sync::atomic::Ordering::Relaxed), 0);

        // Now a real ReadyForQuery.
        p.write_all(&frame(b'Z', b"I")).unwrap();
        assert_eq!(done.load(std::sync::atomic::Ordering::Relaxed), 1);

        // And one arriving SPLIT across two writes — the wire does that, and a
        // walker that restarted or gave up on a partial frame would either
        // miscount or count twice.
        let z = frame(b'Z', b"I");
        p.write_all(&z[..3]).unwrap();
        assert_eq!(done.load(std::sync::atomic::Ordering::Relaxed), 1);
        p.write_all(&z[3..]).unwrap();
        assert_eq!(done.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[test]
    fn explain_is_recognised_and_a_column_called_explain_is_not() {
        assert!(is_explain("EXPLAIN SELECT 1"));
        assert!(is_explain("explain (costs off) select 1"));
        assert!(is_explain("  EXPLAIN ANALYZE SELECT 1"));
        assert!(!is_explain("SELECT explain FROM t"));
        assert!(!is_explain("SELECT 1"));
    }

    #[test]
    fn copy_from_stdin_is_recognised_in_the_spellings_the_corpus_uses() {
        assert!(is_copy_from_stdin("COPY t FROM stdin"));
        assert!(is_copy_from_stdin("copy testnl from stdin csv"));
        assert!(is_copy_from_stdin("COPY t (a, b) FROM STDIN WITH (FORMAT csv)"));
        // A COPY that reads a FILE has no payload to skip.
        assert!(!is_copy_from_stdin("COPY t FROM '/tmp/x.csv'"));
        // …and COPY TO sends data the other way.
        assert!(!is_copy_from_stdin("COPY t TO stdout"));
        assert!(!is_copy_from_stdin("SELECT 1"));
    }

    #[test]
    fn a_dollar_quoted_body_may_contain_semicolons_and_comments() {
        // Function bodies are where both live, and splitting inside one turns a
        // single statement into several syntax errors.
        assert_eq!(
            statements("CREATE FUNCTION f() RETURNS int AS $$BEGIN; -- hi\nEND$$ LANGUAGE x;"),
            vec!["CREATE FUNCTION f() RETURNS int AS $$BEGIN; -- hi\nEND$$ LANGUAGE x"]
        );
    }

    #[test]
    fn an_order_by_at_paren_depth_zero_is_told_apart_from_one_inside() {
        assert!(has_top_level_order_by("SELECT a FROM t ORDER BY a"));
        assert!(has_top_level_order_by("select a from t order by a"));
        // A subquery's ORDER BY does NOT order the outer result — counting it
        // as ordered would put every such statement in the divergence column
        // for a sequence neither engine promised.
        assert!(!has_top_level_order_by(
            "SELECT * FROM (SELECT a FROM t ORDER BY a) s"
        ));
        // A window's OVER (ORDER BY …) is inside parens too.
        assert!(!has_top_level_order_by(
            "SELECT rank() OVER (ORDER BY salary) FROM t"
        ));
        // …but a statement with both keeps the outer one.
        assert!(has_top_level_order_by(
            "SELECT rank() OVER (ORDER BY salary) FROM t ORDER BY empno"
        ));
        assert!(!has_top_level_order_by("SELECT a FROM t"));
        // A string literal containing the words is not a clause.
        assert!(!has_top_level_order_by("SELECT 'order by a' FROM t"));
        // …and `REORDER` is not `ORDER`.
        assert!(!has_top_level_order_by("SELECT reorder FROM t"));
    }

    #[test]
    fn same_rows_in_a_different_order_is_agreement_only_when_no_order_was_asked_for() {
        // The case this exists for: two engines scan a table in different
        // physical orders. SQL promises nothing about that, so both are right.
        let a = "b|2\na|1";
        let b = "a|1\nb|2";
        assert!(matches!(
            compare(a, b, "SELECT x, y FROM t"),
            Verdict::OrderOnly
        ));
        // But if the statement DID ask for an order, getting it wrong is a bug.
        assert!(matches!(
            compare(a, b, "SELECT x, y FROM t ORDER BY x"),
            Verdict::Different
        ));
        // Identical output is a plain match either way.
        assert!(matches!(compare(a, a, "SELECT x FROM t"), Verdict::Same));
        // Different ROWS are different, order rule or not.
        assert!(matches!(
            compare("a|1", "a|2", "SELECT x FROM t"),
            Verdict::Different
        ));
        // A different row COUNT is never an ordering question.
        assert!(matches!(
            compare("a|1\nb|2", "a|1", "SELECT x FROM t"),
            Verdict::Different
        ));
    }

    /// One empty row is not no rows.
    ///
    /// psql spells the difference exactly — `SELECT ''` prints `"\n"` and
    /// `SELECT 1 WHERE false` prints `""` — and the old normaliser rejoined
    /// rows with `"\n"`, which maps both to `""`. The harness then scored
    /// them as a MATCH. Narrow, and in the one direction that matters: it
    /// could only ever turn a wrong answer into agreement, never the reverse.
    #[test]
    fn a_single_empty_row_does_not_match_no_rows_at_all() {
        assert!(matches!(compare("\n", "", "SELECT ''"), Verdict::Different));
        assert!(matches!(compare("", "\n", "SELECT ''"), Verdict::Different));
        // Two empty rows against one is the same question one row further on.
        assert!(matches!(compare("\n\n", "\n", "SELECT ''"), Verdict::Different));
        // …and the trailing newline psql always writes is still not an answer.
        assert!(matches!(compare("a|1\n", "a|1", "SELECT x FROM t"), Verdict::Same));
        assert!(matches!(compare("", "", "SELECT 1 WHERE false"), Verdict::Same));
    }

    #[test]
    fn the_baseline_round_trips_and_any_movement_is_reported() {
        let dir = mpedb_testkit::scratch_base().join(format!("pgregress-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("baseline.tsv");
        let a = vec![(
            "int4".to_string(),
            Counts {
                matched: 10,
                order_only: 0,
                both_refused: 2,
                refused: 1,
                diverged: 0,
            },
        )];
        write_baseline_tsv(&path, &a).unwrap();
        assert_eq!(compare_baseline(&path, &a).unwrap(), 0);

        // An IMPROVEMENT is movement too, and must be reported. A number that
        // drifts upward unnoticed is how a regression hides next to a win.
        let better = vec![(
            "int4".to_string(),
            Counts {
                matched: 11,
                order_only: 0,
                both_refused: 2,
                refused: 0,
                diverged: 0,
            },
        )];
        assert_eq!(compare_baseline(&path, &better).unwrap(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_missing_from_the_baseline_counts_as_movement() {
        let dir = mpedb_testkit::scratch_base().join(format!("pgregress2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("baseline.tsv");
        write_baseline_tsv(&path, &[]).unwrap();
        let got = vec![("new_file".to_string(), Counts::default())];
        assert_eq!(compare_baseline(&path, &got).unwrap(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
