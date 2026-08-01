use super::*;

// ================================================================= runner

pub(super) fn run_file(path: &Path, engines: &[&str]) -> FileReport {
    let mut rep = FileReport {
        name: path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
        ..FileReport::default()
    };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            rep.fatal = Some(format!("cannot read: {e}"));
            return rep;
        }
    };
    let records = match parse_slt(&text, engines) {
        Ok(r) => r,
        Err(e) => {
            rep.fatal = Some(format!("slt parse: {e}"));
            return rep;
        }
    };

    // No pass 1, and no synthetic schema. The corpus's own DDL is executed.


    // In-memory: path = ":memory:" → Shm::open_memory (memfd + ftruncate only;
    // no fallocate / pre-zero). size_mb is a virtual high-water cap (pages
    // demand-fault). durability omitted → none. max_readers = 2.
    let size_mb = SIZE_MB.load(std::sync::atomic::Ordering::Relaxed);
    let mut cfg = format!(
        "[database]\npath = \":memory:\"\nsize_mb = {size_mb}\nmax_readers = 2\n"
    );
    if JOIN_CELLS_SET.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = write!(
            cfg,
            "\n[runtime]\nmax_join_cells = {}\n",
            JOIN_CELLS.load(std::sync::atomic::Ordering::Relaxed)
        );
    }
    // A ZERO-table seed is legal (CLAUDE.md: "tables arrive via live DDL"), and
    // it is what lets the corpus's own `CREATE TABLE` be the thing under test
    // rather than a thing the runner works around.
    let db = match Config::from_toml_str(&cfg).and_then(Database::open_with_config) {
        Ok(db) => db,
        Err(e) => {
            rep.fatal = Some(format!("open: {e}"));
            return rep;
        }
    };

    // Expected-ok statements that failed and might have changed state in
    // sqlite (INSERT/UPDATE/DELETE/DDL): once nonzero, later query mismatches
    // may be state-divergence cascades rather than answer bugs.
    let mut failed_writes = 0usize;
    for rec in &records {
        match &rec.kind {
            Kind::Statement { expect_error } => {
                rep.total += 1;
                if rec.skip {
                    rep.skipped += 1;
                    continue;
                }
                let result: Result<(), String> =
                    exec_sql(&db, &prepare_statement(&rec.sql)).map(|_| ());
                match (result, expect_error) {
                    (Ok(()), false) => rep.stmt_pass += 1,
                    (Err(_), true) => rep.stmt_pass += 1,
                    (Ok(()), true) => {
                        rep.errmis_total += 1;
                        if rep.errmis.len() < MAX_ERRMIS_STORED {
                            rep.errmis.push((rec.line, truncate_sql(&rec.sql, 120)));
                        }
                    }
                    (Err(e), false) => {
                        let cats = categories(&rec.sql, &e);
                        *rep.unsupported.entry(cats[0]).or_default() += 1;
                        for c in &cats {
                            *rep.co_counts.entry(c).or_default() += 1;
                        }
                        failed_writes += 1;
                        rep.sample(cats[0], rec.line, &rec.sql, &e);
                    }
                }
            }
            Kind::Query { types, sort, expected } => {
                rep.total += 1;
                if rec.skip {
                    rep.skipped += 1;
                    continue;
                }
                // A dropped table is now the ENGINE's "no such table", not a
                // simulated one, so the query goes straight through.
                let sql = strip_select_all(&rec.sql);
                let rows = match exec_sql(&db, &sql) {
                    Ok(ExecResult::Rows { rows, .. }) => rows,
                    Ok(_) => {
                        *rep.unsupported.entry("other").or_default() += 1;
                        *rep.co_counts.entry("other").or_default() += 1;
                        rep.sample("other", rec.line, &rec.sql, "query returned no row set");
                        continue;
                    }
                    Err(e) => {
                        let cats = categories(&rec.sql, &e);
                        *rep.unsupported.entry(cats[0]).or_default() += 1;
                        for c in &cats {
                            *rep.co_counts.entry(c).or_default() += 1;
                        }
                        rep.sample(cats[0], rec.line, &rec.sql, &e);
                        continue;
                    }
                };
                // Render per typestring.
                let mut arity_note = None;
                let mut rendered: Vec<Vec<String>> = Vec::with_capacity(rows.len());
                for row in &rows {
                    if row.len() != types.len() && arity_note.is_none() {
                        arity_note = Some(format!(
                            "column count differs: typestring {} vs {} returned",
                            types.len(),
                            row.len()
                        ));
                    }
                    let mut r = Vec::with_capacity(row.len());
                    for (i, v) in row.iter().enumerate() {
                        let tc = types.as_bytes().get(i).copied().unwrap_or(b'T');
                        r.push(render_value(v, tc));
                    }
                    rendered.push(r);
                }
                if let Some(note) = arity_note {
                    rep.wrong_total += 1;
                    if rep.wrong.len() < MAX_WRONG_STORED {
                        rep.wrong.push(Wrong {
                            line: rec.line,
                            sql: truncate_sql(&rec.sql, 200),
                            detail: note,
                            failed_writes_before: failed_writes,
                        });
                    }
                    continue;
                }
                let values: Vec<String> = match sort {
                    SortMode::No => rendered.into_iter().flatten().collect(),
                    SortMode::Row => {
                        let mut r = rendered;
                        r.sort();
                        r.into_iter().flatten().collect()
                    }
                    SortMode::Value => {
                        let mut v: Vec<String> =
                            rendered.into_iter().flatten().collect();
                        v.sort();
                        v
                    }
                };
                match expected {
                    Expected::Hash { count, md5 } => {
                        let mut buf = String::new();
                        for v in &values {
                            buf.push_str(v);
                            buf.push('\n');
                        }
                        let got_md5 = md5_hex(buf.as_bytes());
                        if values.len() != *count || got_md5 != *md5 {
                            rep.wrong_total += 1;
                            if rep.wrong.len() < MAX_WRONG_STORED {
                                let preview: Vec<&str> =
                                    values.iter().take(10).map(|s| s.as_str()).collect();
                                rep.wrong.push(Wrong {
                                    line: rec.line,
                                    sql: truncate_sql(&rec.sql, 200),
                                    detail: format!(
                                        "expected {count} values md5={md5}; got {} values md5={got_md5} (first: [{}])",
                                        values.len(),
                                        preview.join(", ")
                                    ),
                                    failed_writes_before: failed_writes,
                                });
                            }
                        } else {
                            rep.query_pass += 1;
                            rep.hash_verified += 1;
                        }
                    }
                    Expected::Literal(exp) => {
                        if &values == exp {
                            rep.query_pass += 1;
                        } else {
                            rep.wrong_total += 1;
                            if rep.wrong.len() < MAX_WRONG_STORED {
                                rep.wrong.push(Wrong {
                                    line: rec.line,
                                    sql: truncate_sql(&rec.sql, 200),
                                    detail: format!(
                                        "expected [{}] got [{}]",
                                        preview_vals(exp),
                                        preview_vals(&values)
                                    ),
                                    failed_writes_before: failed_writes,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    if DO_VERIFY.load(std::sync::atomic::Ordering::Relaxed) {
        if let Err(e) = db.verify() {
            rep.verify_failed = Some(e.to_string());
        }
    }
    rep
}

fn preview_vals(v: &[String]) -> String {
    let shown: Vec<&str> = v.iter().take(16).map(|s| s.as_str()).collect();
    if v.len() > 16 {
        format!("{} … ({} total)", shown.join(", "), v.len())
    } else {
        shown.join(", ")
    }
}
