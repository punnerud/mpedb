    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static UNIQ: AtomicU64 = AtomicU64::new(0);

    fn test_paths(name: &str) -> PathBuf {
        let dir = mpedb_testkit::scratch_base();
        dir.join(format!(
            "mpedb-facade-unit-{name}-{}-{}.mpedb",
            std::process::id(),
            UNIQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn test_config(name: &str, size_mb: u64) -> (Config, PathBuf) {
        let path = test_paths(name);
        let _ = std::fs::remove_file(&path);
        let toml = format!(
            r#"
[database]
path = "{}"
size_mb = {size_mb}
max_readers = 64

[[table]]
name = "users"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "email"
  type = "text"
  nullable = false
  unique = true
"#,
            path.display()
        );
        (Config::from_toml_str(&toml).unwrap(), path)
    }

    /// Two independent tables in one file — the shape every other write test
    /// here lacks.
    fn two_table_config(name: &str) -> (Config, PathBuf) {
        let dir = mpedb_testkit::scratch_base();
        let path = dir.join(format!("mpedb-{name}-{}.mpedb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let toml = format!(
            r#"
[database]
path = "{}"
size_mb = 8

[[table]]
name = "a"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "v"
  type = "int64"

[[table]]
name = "b"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "v"
  type = "int64"
"#,
            path.display()
        );
        (Config::from_toml_str(&toml).unwrap(), path)
    }

    /// Host scalar UDF dispatch through the facade (design/DESIGN-UDF.md): a
    /// registered `create_function`-style closure runs in SELECT/WHERE, with a
    /// bound param, and over text; an unregistered name errors; unregistering
    /// restores the error. Also verifies the plan-sharing bypass — a UDF plan is
    /// never published to the shared registry.
    #[test]
    fn host_scalar_udf_dispatch_and_registry_bypass() {
        let (cfg, path) = test_config("host-udf", 8);
        let _g = FileGuard(path);
        let db = Database::open_with_config(cfg).unwrap();
        db.query(
            "INSERT INTO users (id, email) VALUES (1, 'a'), (2, 'b'), (3, 'c')",
            &[],
        )
        .unwrap();

        let ints = |r: ExecResult| -> Vec<i64> {
            let ExecResult::Rows { rows, .. } = r else { panic!("want rows") };
            rows.iter()
                .map(|row| match row[0] {
                    Value::Int(x) => x,
                    ref v => panic!("want int, got {v:?}"),
                })
                .collect()
        };

        // Unregistered → bind error.
        assert!(db.query("SELECT plus1(id) FROM users", &[]).is_err());

        db.register_host_function("plus1", 1, |a| match a {
            [Value::Int(x)] => Ok(Value::Int(x + 1)),
            _ => Err(Error::Unsupported("plus1 wants one int".into())),
        });
        db.register_host_function("addk", 2, |a| match a {
            [Value::Int(x), Value::Int(k)] => Ok(Value::Int(x + k)),
            _ => Err(Error::Unsupported("addk wants two ints".into())),
        });
        db.register_host_function("shout", 1, |a| match a {
            [Value::Text(s)] => Ok(Value::Text(s.to_uppercase())),
            _ => Err(Error::Unsupported("shout wants text".into())),
        });

        // SELECT plus1(id)
        assert_eq!(
            ints(db.query("SELECT plus1(id) FROM users ORDER BY id", &[]).unwrap()),
            vec![2, 3, 4]
        );
        // WHERE plus1(id) = 3  → id 2
        assert_eq!(
            ints(db.query("SELECT id FROM users WHERE plus1(id) = 3", &[]).unwrap()),
            vec![2]
        );
        // Bound param: addk(id, $1)
        assert_eq!(
            ints(
                db.query("SELECT addk(id, $1) FROM users ORDER BY id", &[Value::Int(10)])
                    .unwrap()
            ),
            vec![11, 12, 13]
        );
        // Text UDF.
        let ExecResult::Rows { rows, .. } =
            db.query("SELECT shout(email) FROM users ORDER BY id", &[]).unwrap()
        else {
            panic!("want rows")
        };
        assert_eq!(rows[0][0], Value::Text("A".into()));

        // Plan-sharing bypass: a UDF plan must NOT be in the shared registry, so a
        // FRESH handle to the same file (which registered no UDF) cannot execute
        // it by hash — it is UnknownPlan, not a silently-shared plan.
        let hash = db.prepare("SELECT plus1(id) FROM users").unwrap();
        let db2 = Database::open_from_file(_g.0.as_path()).unwrap();
        assert!(matches!(
            db2.execute(&hash, &[]),
            Err(Error::UnknownPlan(_))
        ));
        // The registering handle still runs it by hash (local cache).
        assert_eq!(ints(db.execute(&hash, &[]).unwrap()), vec![2, 3, 4]);

        // Unregister → unknown again.
        assert!(db.unregister_host_function("plus1", 1));
        assert!(db.query("SELECT plus1(id) FROM users", &[]).is_err());
    }

    /// Host AGGREGATE dispatch through the facade (design/DESIGN-UDF.md stage 2):
    /// a registered `xStep`/`xFinal`-style factory accumulates per group, sees
    /// NULL arguments (unlike a built-in), honors `FILTER`, produces one value
    /// for an EMPTY group, and — like a scalar UDF — keeps its plan out of the
    /// shared registry.
    #[test]
    fn host_aggregate_dispatch_and_registry_bypass() {
        /// `mysum(x)`: sums ints, and COUNTS the NULLs it is handed — the visible
        /// proof that a host aggregate is stepped for every row, where a built-in
        /// `sum` would have skipped them.
        #[derive(Default)]
        struct MySum {
            total: i64,
            nulls: i64,
        }
        impl mpedb_types::HostAggState for MySum {
            fn step(&mut self, args: &[Value]) -> Result<()> {
                match args {
                    [Value::Int(x)] => self.total += x,
                    [Value::Null] => self.nulls += 1,
                    _ => return Err(Error::Unsupported("mysum wants one int".into())),
                }
                Ok(())
            }
            fn finish(self: Box<Self>) -> Result<Value> {
                Ok(Value::Int(self.total * 1000 + self.nulls))
            }
        }

        let (cfg, path) = test_config("host-agg", 8);
        let _g = FileGuard(path);
        let db = Database::open_with_config(cfg).unwrap();

        let ints = |r: ExecResult| -> Vec<i64> {
            let ExecResult::Rows { rows, .. } = r else { panic!("want rows") };
            rows.iter()
                .map(|row| match row[0] {
                    Value::Int(x) => x,
                    ref v => panic!("want int, got {v:?}"),
                })
                .collect()
        };

        // Unregistered → bind error (an unknown function, not a silent NULL).
        assert!(db.query("SELECT mysum(id) FROM users", &[]).is_err());

        db.register_host_aggregate("mysum", 1, || Box::<MySum>::default());

        // EMPTY table: one group, `xFinal` on a never-stepped state.
        assert_eq!(ints(db.query("SELECT mysum(id) FROM users", &[]).unwrap()), vec![0]);

        db.query(
            "INSERT INTO users (id, email) VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd')",
            &[],
        )
        .unwrap();

        // Bare aggregate over the whole table: 1+2+3+4 = 10, no NULLs.
        assert_eq!(ints(db.query("SELECT mysum(id) FROM users", &[]).unwrap()), vec![10_000]);
        // GROUP BY: odd ids 1+3 = 4, even 2+4 = 6.
        assert_eq!(
            ints(
                db.query("SELECT mysum(id) FROM users GROUP BY id % 2 ORDER BY id % 2", &[])
                    .unwrap()
            ),
            vec![6_000, 4_000]
        );
        // FILTER (WHERE …) applies to a host aggregate exactly as to a built-in.
        assert_eq!(
            ints(
                db.query("SELECT mysum(id) FILTER (WHERE id > 2) FROM users", &[])
                    .unwrap()
            ),
            vec![7_000]
        );
        // NULL arguments REACH `xStep` (sqlite's rule; a built-in would skip
        // them): `mysum(NULL)` sees 4 NULLs and no ints.
        assert_eq!(ints(db.query("SELECT mysum(NULL) FROM users", &[]).unwrap()), vec![4]);
        // The result is dynamically typed (`ColumnType::Any`), exactly as a host
        // SCALAR's is. Arithmetic over it USED to be a bind error and no longer
        // is: `Any` now unifies for arithmetic the same way it always did for
        // comparison, with the runtime `arith` refusing a non-numeric operand.
        // (The relaxation arrived with the JSON functions, where `doc ->> '$.n'`
        // is `Any` and arithmetic over it is the obvious thing to write; it
        // changes both together, as this comment used to promise.)
        db.register_host_function("plus1", 1, |a| match a {
            [Value::Int(x)] => Ok(Value::Int(x + 1)),
            _ => Err(Error::Unsupported("plus1 wants one int".into())),
        });
        assert_eq!(
            ints(db.query("SELECT mysum(id) + 1 FROM users", &[]).unwrap()),
            vec![10_001]
        );
        assert_eq!(
            ints(db.query("SELECT sum(plus1(id)) FROM users", &[]).unwrap()),
            vec![14]
        );
        // Both are fine on their own, and a host scalar INSIDE a host
        // aggregate's argument composes (two registries, one execution).
        assert_eq!(
            ints(db.query("SELECT mysum(plus1(id)) FROM users", &[]).unwrap()),
            vec![14_000]
        );

        // Plan-sharing bypass, exactly as for a host scalar: a fresh handle to
        // the same file cannot execute the plan by hash.
        let hash = db.prepare("SELECT mysum(id) FROM users").unwrap();
        let db2 = Database::open_from_file(_g.0.as_path()).unwrap();
        assert!(matches!(db2.execute(&hash, &[]), Err(Error::UnknownPlan(_))));
        assert_eq!(ints(db.execute(&hash, &[]).unwrap()), vec![10_000]);

        // MULTI-ARGUMENT host aggregates (PLAN_FORMAT 51): sqlite's
        // `create_aggregate(name, N, cls)` is an N-ary contract, and CPython's
        // own suite registers 2-ary and variadic ones. `xStep` gets the WHOLE
        // argument list, in source order.
        /// Sums `a * b` over the group, and reports how many arguments each
        /// step was handed — so a dropped or reordered argument is visible.
        #[derive(Default)]
        struct Dot {
            total: i64,
            argc: usize,
        }
        impl mpedb_types::HostAggState for Dot {
            fn step(&mut self, args: &[Value]) -> Result<()> {
                self.argc = args.len();
                let n = |v: &Value| match v {
                    Value::Int(x) => *x,
                    _ => 0,
                };
                self.total += args.iter().map(n).product::<i64>();
                Ok(())
            }
            fn finish(self: Box<Self>) -> Result<Value> {
                Ok(Value::Int(self.total * 10 + self.argc as i64))
            }
        }
        db.register_host_aggregate("dot", 2, || Box::<Dot>::default());
        // 1*1 + 2*2 + 3*3 + 4*4 = 30, two arguments per step.
        assert_eq!(ints(db.query("SELECT dot(id, id) FROM users", &[]).unwrap()), vec![302]);
        // Arguments are positional and evaluated over the same base row.
        assert_eq!(ints(db.query("SELECT dot(id, 2) FROM users", &[]).unwrap()), vec![202]);
        // Wrong arity is a clean compile error, not a call with a missing slot.
        assert!(db.query("SELECT dot(id) FROM users", &[]).is_err());
        assert!(db.query("SELECT dot(id, id, id) FROM users", &[]).is_err());
        // A variadic (`-1`) registration takes whatever it is written with.
        db.register_host_aggregate("dotv", -1, || Box::<Dot>::default());
        assert_eq!(ints(db.query("SELECT dotv(id, id, id) FROM users", &[]).unwrap()), vec![1003]);
        // The no-publish gate covers a multi-argument call too: the plan names a
        // host aggregate, so it stays in THIS connection's cache.
        let h2 = db.prepare("SELECT dot(id, id) FROM users").unwrap();
        assert!(matches!(db2.execute(&h2, &[]), Err(Error::UnknownPlan(_))));
        assert_eq!(ints(db.execute(&h2, &[]).unwrap()), vec![302]);

        // A built-in name can never be shadowed by a registration.
        db.register_host_aggregate("sum", 1, || Box::<MySum>::default());
        assert_eq!(ints(db.query("SELECT sum(id) FROM users", &[]).unwrap()), vec![10]);

        // Unregister → unknown again.
        assert!(db.unregister_host_aggregate("mysum", 1));
        assert!(db.query("SELECT mysum(id) FROM users", &[]).is_err());
    }

    /// Host COLLATING SEQUENCES (design/DESIGN-UDF.md stage 3) and — just as
    /// load-bearing — the boundary of what they may do.
    ///
    /// A registered collation is a COMPARATOR, so it orders `ORDER BY … COLLATE
    /// <name>`. It cannot change a KEY ENCODING, so a host collation on a
    /// column's declared `COLLATE`, or as a `GROUP BY`/`DISTINCT` fold, refuses
    /// by name rather than being answered under BINARY. The refusals are what
    /// keep this from being a wrong answer with no error.
    #[test]
    fn host_collation_orders_sorts_and_refuses_every_key_position() {
        let (cfg, path) = test_config("host-coll", 8);
        let _g = FileGuard(path);
        let db = Database::open_with_config(cfg).unwrap();

        let texts = |r: ExecResult| -> Vec<String> {
            let ExecResult::Rows { rows, .. } = r else { panic!("want rows") };
            rows.iter()
                .map(|row| match &row[0] {
                    Value::Text(s) => s.clone(),
                    v => panic!("want text, got {v:?}"),
                })
                .collect()
        };

        db.query(
            "INSERT INTO users (id, email) VALUES (1, 'a'), (2, 'b'), (3, 'c')",
            &[],
        )
        .unwrap();

        // Unregistered → a clean bind error naming the collation, never BINARY.
        let err = db
            .query("SELECT email FROM users ORDER BY email COLLATE rev", &[])
            .unwrap_err();
        assert!(format!("{err}").contains("no such collation sequence: rev"), "{err}");

        db.register_host_collation("rev", |a, b| b.cmp(a));
        assert_eq!(
            texts(db.query("SELECT email FROM users ORDER BY email COLLATE rev", &[]).unwrap()),
            ["c", "b", "a"]
        );
        // A plain ORDER BY is untouched by the registration.
        assert_eq!(
            texts(db.query("SELECT email FROM users ORDER BY email", &[]).unwrap()),
            ["a", "b", "c"]
        );
        // Re-registering REPLACES (sqlite's rule).
        db.register_host_collation("rev", |a, b| a.cmp(b));
        assert_eq!(
            texts(db.query("SELECT email FROM users ORDER BY email COLLATE rev", &[]).unwrap()),
            ["a", "b", "c"]
        );
        // A built-in can never be shadowed.
        db.register_host_collation("nocase", |a, b| b.cmp(a));
        assert_eq!(
            texts(db.query("SELECT email FROM users ORDER BY email COLLATE NOCASE", &[]).unwrap()),
            ["a", "b", "c"]
        );

        // THE BOUNDARY. Each of these would need the collation to decide KEY
        // BYTES, which a callback cannot produce — so each refuses by name.
        for sql in [
            // A comparison's collation (rung 1) — no sort, a predicate.
            "SELECT email FROM users WHERE email = 'a' COLLATE rev",
            // A GROUP BY fold.
            "SELECT email FROM users GROUP BY email COLLATE rev",
        ] {
            let err = db.query(sql, &[]).unwrap_err();
            assert!(
                format!("{err}").contains("no such collation sequence: rev"),
                "{sql}: {err}"
            );
        }
        // …and a column DECLARED under it: the index/PK encoding is bytewise.
        let err = db
            .query("CREATE TABLE c (id INTEGER PRIMARY KEY, s TEXT COLLATE rev)", &[])
            .unwrap_err();
        assert!(format!("{err}").contains("no such collation sequence: rev"), "{err}");

        // A plan naming a host collation is CONNECTION-LOCAL, exactly like one
        // naming a host function: a fresh handle to the same file cannot run it.
        let hash = db.prepare("SELECT email FROM users ORDER BY email COLLATE rev").unwrap();
        let db2 = Database::open_from_file(_g.0.as_path()).unwrap();
        assert!(matches!(db2.execute(&hash, &[]), Err(Error::UnknownPlan(_))));
        assert_eq!(texts(db.execute(&hash, &[]).unwrap()), ["a", "b", "c"]);

        // Deregistering makes an EXISTING plan fail by name — never a silent
        // fallback to BINARY, which would be a different order with no error.
        assert!(db.unregister_host_collation("rev"));
        let err = db
            .query("SELECT email FROM users ORDER BY email COLLATE rev", &[])
            .unwrap_err();
        assert!(format!("{err}").contains("no such collation sequence: rev"), "{err}");
    }

    struct FileGuard(PathBuf);
    impl Drop for FileGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn corrupt_registry_entries_degrade_to_unknown_plan() {
        let (cfg, path) = test_config("corrupt", 8);
        let _guard = FileGuard(path);
        let db = Database::open_with_config(cfg.clone()).unwrap();
        let h = db.prepare("SELECT * FROM users WHERE id = $1").unwrap();
        db.query(
            "INSERT INTO users (id, email) VALUES (1, 'a@x')",
            &params![],
        )
        .unwrap();

        // 1. Garbage bytes under the plan's own subkey.
        let mut w = db.engine.begin_write().unwrap();
        w.sys_put(&plan_subkey(&h, mpedb_types::Dialect::Sqlite), b"total garbage, not a record")
            .unwrap();
        w.commit().unwrap();
        // Fresh handle (empty local cache) must hit the registry and reject.
        let db2 = Database::open_with_config(cfg.clone()).unwrap();
        assert!(matches!(
            db2.execute(&h, &params![1]),
            Err(Error::UnknownPlan(x)) if x == h
        ));

        // 2. Structurally valid record whose blob is a DIFFERENT plan
        //    (content-hash mismatch on load).
        let other = mpedb_sql::prepare("SELECT * FROM users WHERE id = $1 AND id > 0", &db.schema())
            .unwrap();
        assert_ne!(other.hash(), h);
        let rec = registry::encode_record("mismatch", &other.encode(), mpedb_types::Dialect::Sqlite, 1);
        let mut w = db.engine.begin_write().unwrap();
        w.sys_put(&plan_subkey(&h, mpedb_types::Dialect::Sqlite), &rec).unwrap();
        w.commit().unwrap();
        let db3 = Database::open_with_config(cfg.clone()).unwrap();
        assert!(matches!(
            db3.execute(&h, &params![1]),
            Err(Error::UnknownPlan(x)) if x == h
        ));

        // 3. Blob compiled against a different schema → PlanInvalidated.
        let other_schema = Schema::new(vec![TableDef {
            id: 0,
            name: "users".into(),
            columns: vec![ColumnDef { pg_enum: None, generated: None, default_text: None, decl: None,
                name: "id".into(),
                ty: ColumnType::Int64,
                nullable: false,
                unique: false,
                indexed: false,
                default: None,
                check: None,
                collation: mpedb_types::Collation::Binary,
                affinity: mpedb_types::Affinity::Integer,
            }],
            primary_key: vec![0],
            indexes: vec![],
            dead: false, pk_name: None,
            implicit_rowid: false, autoincrement: false,
            kind: mpedb_types::TableKind::Standard,
            foreign_keys: Vec::new(),
        }])
        .unwrap();
        let foreign = mpedb_sql::prepare("SELECT * FROM users WHERE id = $1", &other_schema).unwrap();
        let fh = foreign.hash();
        let rec = registry::encode_record("foreign", &foreign.encode(), mpedb_types::Dialect::Sqlite, 1);
        let mut w = db.engine.begin_write().unwrap();
        w.sys_put(&plan_subkey(&fh, mpedb_types::Dialect::Sqlite), &rec).unwrap();
        w.commit().unwrap();
        let db4 = Database::open_with_config(cfg.clone()).unwrap();
        assert!(matches!(
            db4.execute(&fh, &params![1]),
            Err(Error::PlanInvalidated)
        ));

        // 4. A re-prepare on the corrupted hash overwrites and heals.
        let db5 = Database::open_with_config(cfg.clone()).unwrap();
        let h2 = db5.prepare("SELECT * FROM users WHERE id = $1").unwrap();
        assert_eq!(h2, h);
        let db6 = Database::open_with_config(cfg).unwrap();
        assert!(matches!(
            db6.execute(&h, &params![1]).unwrap(),
            ExecResult::Rows { rows, .. } if rows.len() == 1
        ));

        db6.verify().unwrap();
    }

    #[test]
    fn decode_registry_plan_rejects_garbage_unit_level() {
        let (cfg, path) = test_config("decode-unit", 8);
        let _guard = FileGuard(path);
        let db = Database::open_with_config(cfg).unwrap();
        let h = PlanHash([0x5A; 32]);
        for garbage in [
            &b""[..],
            &b"short"[..],
            &registry::encode_record("sql", b"not a plan blob", mpedb_types::Dialect::Sqlite, 3)[..],
        ] {
            assert!(matches!(
                decode_registry_plan(garbage, &h, &db.schema(), mpedb_types::Dialect::Sqlite),
                Err(Error::UnknownPlan(x)) if x == h
            ));
        }
    }

    #[test]
    fn sys_records_roundtrip_and_stay_clear_of_the_plan_registry() {
        let (cfg, path) = test_config("sysrec", 8);
        let _guard = FileGuard(path);
        let db = Database::open_with_config(cfg.clone()).unwrap();

        // Namespace/key validation.
        assert!(db.sys_record_put("", b"k", b"v").is_err());
        assert!(db.sys_record_put("ns\0evil", b"k", b"v").is_err());
        assert!(db.sys_record_put(&"n".repeat(65), b"k", b"v").is_err());
        assert!(db.sys_record_put("ns", b"", b"v").is_err());
        assert!(db.sys_record_put("ns", &[7u8; 1025], b"v").is_err());
        assert!(db
            .sys_record_put("ns", b"k", &vec![0u8; SYS_RECORD_MAX_VALUE + 1])
            .is_err());
        assert!(db.sys_record_scan("").is_err());

        // Roundtrip, overwrite, scan with prefix stripping.
        db.sys_record_put("proc", b"transfer", b"blob-1").unwrap();
        db.sys_record_put("proc", b"transfer", b"blob-2").unwrap();
        db.sys_record_put("proc", b"other", b"blob-3").unwrap();
        db.sys_record_put("proch", b"\x01\x02", b"blob-2").unwrap();
        assert_eq!(
            db.sys_record_get("proc", b"transfer").unwrap().as_deref(),
            Some(&b"blob-2"[..])
        );
        assert_eq!(db.sys_record_get("proc", b"missing").unwrap(), None);
        let scan = db.sys_record_scan("proc").unwrap();
        assert_eq!(
            scan,
            vec![
                (b"other".to_vec(), b"blob-3".to_vec()),
                (b"transfer".to_vec(), b"blob-2".to_vec()),
            ]
        );
        // `proch` does not leak into a `proc` scan, nor vice versa.
        assert_eq!(db.sys_record_scan("proch").unwrap().len(), 1);

        // A plan whose registry subkey starts with the bytes `plan` must be
        // untouched by a `plan`-namespace record (0x00 vs '/' separator).
        let h = db.prepare("SELECT * FROM users WHERE id = $1").unwrap();
        db.sys_record_put("plan", &h.0, b"not a plan record").unwrap();
        let db2 = Database::open_with_config(cfg).unwrap();
        assert!(db2.execute(&h, &params![1]).is_ok());
        db2.verify().unwrap();
    }

    /// #124: publication reads a `plancount` record instead of ranking the whole
    /// registry, so the counter's failure modes are the fix's risk surface. A
    /// counter that reads HIGH (here: forged to the cap, with three plans
    /// present) must fall through to the authoritative walk, evict nothing, and
    /// leave the truth behind — otherwise one bad 8-byte record would evict a
    /// live registry on every insert, forever.
    #[test]
    fn a_forged_plan_counter_self_heals_and_evicts_nothing() {
        let (cfg, path) = test_config("plancount", 8);
        let _guard = FileGuard(path);
        let db = Database::open_with_config(cfg).unwrap();
        let hashes: Vec<PlanHash> = (0..3)
            .map(|i| db.prepare(&format!("SELECT * FROM users WHERE id = {i}")).unwrap())
            .collect();

        let count = |db: &Database| -> u64 {
            let r = db.engine.begin_read().unwrap();
            let n = u64::from_le_bytes(
                r.sys_get(registry::PLAN_COUNT_KEY).unwrap().unwrap().try_into().unwrap(),
            );
            r.finish().unwrap();
            n
        };
        assert_eq!(count(&db), 3);

        // Forge it to the cap, then publish one more plan.
        let mut w = db.engine.begin_write().unwrap();
        w.sys_put(
            registry::PLAN_COUNT_KEY,
            &(registry::MAX_REGISTRY_PLANS as u64).to_le_bytes(),
        )
        .unwrap();
        w.commit().unwrap();
        let fourth = db.prepare("SELECT * FROM users WHERE id = 99").unwrap();

        // Recounted from the tree: 3 live + the new one, nothing evicted.
        assert_eq!(count(&db), 4);
        let r = db.engine.begin_read().unwrap();
        for h in hashes.iter().chain(std::iter::once(&fourth)) {
            assert!(r.sys_get(&plan_subkey(h, mpedb_types::Dialect::Sqlite)).unwrap().is_some(), "{h:?} evicted");
        }
        r.finish().unwrap();

        // A malformed counter takes the same authoritative path.
        let mut w = db.engine.begin_write().unwrap();
        w.sys_put(registry::PLAN_COUNT_KEY, b"nope").unwrap();
        w.commit().unwrap();
        db.prepare("SELECT * FROM users WHERE id = 100").unwrap();
        assert_eq!(count(&db), 5);
        db.verify().unwrap();
    }

    #[test] // ~10 s in debug: fills the 4096-entry registry once
    fn registry_eviction_caps_entries() {
        let (cfg, path) = test_config("evict", 64);
        let _guard = FileGuard(path);
        let db = Database::open_with_config(cfg).unwrap();

        let total = registry::MAX_REGISTRY_PLANS + 1;
        let mut hashes = Vec::with_capacity(total);
        for i in 0..total {
            hashes.push(
                db.prepare(&format!("SELECT * FROM users WHERE id = {i}"))
                    .unwrap(),
            );
        }

        // Inspect the registry through `db`'s OWN engine.
        //
        // This test used to open a SECOND `Database` on the same path here (the
        // obvious "a cold-cache handle reads the shared registry" check). That is
        // flaky: a second attach opens the file BY PATH, and `Shm::open` passes
        // `create(true)`, so if anything removes the /dev/shm file between the
        // fill and the second open — a concurrent process sweeping the shared
        // tmpfs, another test run, disk pressure — the second handle silently
        // CREATES AND FORMATS A FRESH, EMPTY database at that path and then
        // observes zero plans, and the "survivors still resolve" assertions blow
        // up even though eviction was perfectly correct. `db` already holds the
        // mmap of the real inode, so reading through `db.engine` is immune to any
        // such path churn. (The cross-handle "a fresh Database can load a plan
        // from the shared registry" behaviour is covered independently by
        // `sys_records_roundtrip_and_stay_clear_of_the_plan_registry`.)
        let r = db.engine.begin_read().unwrap();
        let present: std::collections::HashSet<Vec<u8>> = r
            .sys_scan()
            .unwrap()
            .into_iter()
            .filter(|(k, _)| k.starts_with(registry::PLAN_PREFIX))
            .map(|(k, _)| k)
            .collect();
        let n_plans = present.len();

        // EXACT count, not a range. This is a single writer that inserts exactly
        // one plan per commit, so the registry grows 0,1,2,…,MAX and
        // `evict_if_full` (registry.rs) trips exactly ONCE — at the (MAX+1)-th
        // insert, when the tree holds precisely MAX entries — dropping
        // EVICT_BATCH of them before adding the last, leaving MAX-EVICT_BATCH+1.
        // (A post-eviction *range* only arises with concurrent writers that can
        // push the count past MAX between eviction checks; there are none here,
        // so the deterministic value is the correct invariant — do not weaken it.)
        assert_eq!(
            n_plans,
            registry::MAX_REGISTRY_PLANS - registry::EVICT_BATCH + 1
        );

        // Eviction drops the OLDEST EVICT_BATCH by last_used_txn. Here
        // last_used_txn is strictly monotonic with prepare order (one commit per
        // prepare, txn_id += 1 each; verified: all stamps distinct), so the
        // evicted set is EXACTLY the first EVICT_BATCH prepared and the survivors
        // are EXACTLY the rest — a fixed partition with no sort tie-break.
        for &i in &[0usize, 1, registry::EVICT_BATCH - 1] {
            assert!(
                !present.contains(&plan_subkey(&hashes[i], mpedb_types::Dialect::Sqlite)),
                "plan #{i} (among the oldest {}) should have been evicted",
                registry::EVICT_BATCH
            );
        }
        // ...and the survivors still load through the COLD registry path —
        // `decode_registry_plan` is exactly what a cache-miss `execute` runs:
        // parse the record, re-validate the blob against the live schema, and
        // verify the recomputed content hash.
        let schema = db.schema();
        for &i in &[registry::EVICT_BATCH, registry::EVICT_BATCH + 1, total - 1] {
            let key = plan_subkey(&hashes[i], mpedb_types::Dialect::Sqlite);
            assert!(present.contains(&key), "plan #{i} should have survived");
            let rec = r.sys_get(&key).unwrap().expect("survivor record present");
            decode_registry_plan(&rec, &hashes[i], &schema, mpedb_types::Dialect::Sqlite)
                .unwrap_or_else(|e| panic!("survivor plan #{i} must cold-load, got {e:?}"));
        }
        // An evicted hash leaves no record, so the cold path reports UnknownPlan.
        assert!(r.sys_get(&plan_subkey(&hashes[0], mpedb_types::Dialect::Sqlite)).unwrap().is_none());
        r.finish().unwrap();
        db.verify().unwrap();
    }

    // -------------------------------------------- detached (client-borne) plans

    #[test]
    fn detached_plan_encode_decode_roundtrip_and_truncation() {
        let dp = DetachedPlan {
            hash: PlanHash([0x37; 32]),
            blob: b"a compiled plan blob \x00\xff".to_vec(),
            sql: "SELECT * FROM users WHERE id = $1".to_owned(),
        };
        let bytes = dp.encode();
        assert_eq!(DetachedPlan::decode(&bytes).unwrap(), dp);

        // Every truncation fails cleanly (Corrupt), never panics.
        for cut in 0..bytes.len() {
            assert!(
                matches!(DetachedPlan::decode(&bytes[..cut]), Err(Error::Corrupt(_))),
                "truncation at {cut} must be Corrupt"
            );
        }
        // Trailing garbage and a bad format byte are rejected too.
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(matches!(DetachedPlan::decode(&trailing), Err(Error::Corrupt(_))));
        let mut bad_fmt = bytes.clone();
        bad_fmt[0] = 9;
        assert!(matches!(DetachedPlan::decode(&bad_fmt), Err(Error::Corrupt(_))));
        // Invalid utf-8 in the sql field.
        let mut bad_sql = DetachedPlan {
            hash: PlanHash([1; 32]),
            blob: vec![],
            sql: String::new(),
        }
        .encode();
        // append u32 sql_len = 1 then a lone 0xff (rewrite trailing sql section)
        bad_sql.truncate(1 + 32 + 4); // fmt + hash + blob_len(=0)
        bad_sql.extend_from_slice(&1u32.to_le_bytes());
        bad_sql.push(0xff);
        assert!(matches!(DetachedPlan::decode(&bad_sql), Err(Error::Corrupt(_))));
    }

    /// A detached plan from an OLDER binary (a different PLAN_FORMAT byte) is
    /// version drift, not tampering: the client must get `PlanInvalidated` —
    /// the documented re-prepare-from-`DetachedPlan::sql` path — not a
    /// `Corrupt` that reads as "your blob was forged". Pinned after the 7→8
    /// bump surfaced exactly this (adversarial review find).
    #[test]
    fn detached_plan_from_old_format_is_invalidated_not_corrupt() {
        let (cfg, path) = test_config("detached-format", 8);
        let _guard = FileGuard(path);
        let db = Database::open_with_config(cfg).unwrap();
        let mut dp = db.prepare_detached("SELECT * FROM users WHERE id = $1").unwrap();
        // The format byte is byte 0 of the canonical encoding; regress it.
        assert_ne!(dp.blob[0], 7, "bump the test constant when the format moves");
        dp.blob[0] = 7;
        assert!(
            matches!(db.execute_detached(&dp, &params![1]), Err(Error::PlanInvalidated)),
            "an old-format blob must invalidate, not report corruption"
        );
    }

    #[test]
    fn detached_hash_matches_prepare_and_runs_without_registry() {
        let (cfg, path) = test_config("detached-basic", 8);
        let _guard = FileGuard(path);
        let db = Database::open_with_config(cfg.clone()).unwrap();

        let sql = "SELECT * FROM users WHERE id = $1";
        // prepare_detached hash == prepare() hash (same plan, same schema).
        let published = db.prepare(sql).unwrap();
        let dp = db.prepare_detached(sql).unwrap();
        assert_eq!(dp.hash, published);
        assert_eq!(dp.sql, sql);

        // A DML detached plan that was NEVER prepared/published.
        let ins = db
            .prepare_detached("INSERT INTO users (id, email) VALUES ($1, $2)")
            .unwrap();
        // Fresh handle (empty local cache) on the same file: the plan has no
        // registry entry, yet execute_detached runs it.
        let db2 = Database::open_with_config(cfg.clone()).unwrap();
        // Prove absence from the shared registry directly. Only `ins` is
        // detached-only; `dp`'s SQL was also published above via prepare() for
        // the hash-equality check, so it legitimately IS in the registry.
        {
            let r = db2.engine.begin_read().unwrap();
            let rec = r.sys_get(&plan_subkey(&ins.hash, mpedb_types::Dialect::Sqlite)).unwrap();
            r.finish().unwrap();
            assert!(rec.is_none(), "detached plan leaked into the registry");
        }
        // ...and execute(hash) (registry path) genuinely can't find it.
        assert!(matches!(
            db2.execute(&ins.hash, &params![9, "z@x"]),
            Err(Error::UnknownPlan(_))
        ));

        // execute_detached runs the DML and the SELECT correctly.
        assert_eq!(
            db2.execute_detached(&ins, &params![9, "z@x"]).unwrap(),
            ExecResult::Affected(1)
        );
        let sel = db2.prepare_detached(sql).unwrap();
        assert!(matches!(
            db2.execute_detached(&sel, &params![9]).unwrap(),
            ExecResult::Rows { rows, .. } if rows.len() == 1
        ));
        // Still not in the registry after executing.
        let r = db2.engine.begin_read().unwrap();
        assert!(r.sys_get(&plan_subkey(&ins.hash, mpedb_types::Dialect::Sqlite)).unwrap().is_none());
        r.finish().unwrap();
        db2.verify().unwrap();
    }

    #[test]
    fn detached_tampered_blob_or_hash_is_corrupt() {
        let (cfg, path) = test_config("detached-tamper", 8);
        let _guard = FileGuard(path);
        let db = Database::open_with_config(cfg).unwrap();
        db.query("INSERT INTO users (id, email) VALUES (1, 'a@x')", &params![])
            .unwrap();

        let sql = "SELECT * FROM users WHERE id = $1";
        let good = db.prepare_detached(sql).unwrap();
        assert!(db.execute_detached(&good, &params![1]).is_ok());

        // (a) Flip a byte in the blob PAST the schema_hash region (bytes 1..33)
        // so this is a genuine content tamper, not a schema-hash edit: it must
        // be Corrupt (structural decode failure or hash mismatch), never a
        // silent wrong execution.
        let mut tampered = good.clone();
        let last = tampered.blob.len() - 1;
        assert!(last >= 33);
        tampered.blob[last] ^= 1;
        assert!(matches!(
            db.execute_detached(&tampered, &params![1]),
            Err(Error::Corrupt(_))
        ));

        // (b) A blob that decodes cleanly but is paired with the WRONG hash
        // exercises the separate integrity check → Corrupt (not PlanInvalidated,
        // not UnknownPlan).
        let mut wrong_hash = good.clone();
        wrong_hash.hash.0[0] ^= 0xFF;
        assert!(matches!(
            db.execute_detached(&wrong_hash, &params![1]),
            Err(Error::Corrupt(m)) if m.contains("does not match its carried hash")
        ));
        db.verify().unwrap();
    }

    #[test]
    fn detached_schema_change_returns_plan_invalidated_and_reprepare_recovers() {
        let (cfg, path) = test_config("detached-schema", 8);
        let _guard = FileGuard(path);
        let db = Database::open_with_config(cfg).unwrap();
        db.query("INSERT INTO users (id, email) VALUES (5, 'e@x')", &params![])
            .unwrap();

        let sql = "SELECT * FROM users WHERE id = $1";
        // A plan compiled against a DIFFERENT schema (users has only `id`), so
        // its embedded schema_hash will not match the live one — exactly what a
        // client holding a stale detached plan across a schema migration has.
        let foreign_schema = Schema::new(vec![TableDef {
            id: 0,
            name: "users".into(),
            columns: vec![ColumnDef { pg_enum: None, generated: None, default_text: None, decl: None,
                name: "id".into(),
                ty: ColumnType::Int64,
                nullable: false,
                unique: false,
                indexed: false,
                default: None,
                check: None,
                collation: mpedb_types::Collation::Binary,
                affinity: mpedb_types::Affinity::Integer,
            }],
            primary_key: vec![0],
            indexes: vec![],
            dead: false, pk_name: None,
            implicit_rowid: false, autoincrement: false,
            kind: mpedb_types::TableKind::Standard,
            foreign_keys: Vec::new(),
        }])
        .unwrap();
        let foreign = mpedb_sql::prepare(sql, &foreign_schema).unwrap();
        let stale = DetachedPlan {
            hash: foreign.hash(),
            blob: foreign.encode(),
            sql: sql.to_owned(),
        };
        assert!(matches!(
            db.execute_detached(&stale, &params![5]),
            Err(Error::PlanInvalidated)
        ));

        // Recovery: re-prepare from the carried sql against the live schema.
        let fresh = db.prepare_detached(&stale.sql).unwrap();
        assert_ne!(fresh.hash, stale.hash);
        assert!(matches!(
            db.execute_detached(&fresh, &params![5]).unwrap(),
            ExecResult::Rows { rows, .. } if rows.len() == 1
        ));
        db.verify().unwrap();
    }

    /// **Cross-table atomic commit** — DESIGN-MULTIDB §21 lists it as a headline
    /// property of the single-file option ("one writer lock, one meta flip"),
    /// and nothing tested it. Every write test in this repo touched exactly one
    /// table, so an advertised guarantee rested on the reader believing the
    /// architecture rather than on evidence.
    ///
    /// It follows from the design (a commit is one atomic meta flip regardless
    /// of how many trees the txn dirtied) — but "it follows" is what people say
    /// right before a regression. This pins both halves: both tables land
    /// together on commit, and NEITHER lands on rollback.
    #[test]
    fn one_transaction_writing_two_tables_is_atomic() {
        let (cfg, path) = two_table_config("xtable");
        let _guard = FileGuard(path);
        let db = Database::open_with_config(cfg).unwrap();

        let count = |t: &str| -> usize {
            match db.query(&format!("SELECT id FROM {t}"), &params![]).unwrap() {
                ExecResult::Rows { rows, .. } => rows.len(),
                o => panic!("{o:?}"),
            }
        };

        // Rollback: neither table may keep its row.
        let mut s = db.begin().unwrap();
        s.insert_row(0, &[Value::Int(1), Value::Int(10)]).unwrap();
        s.insert_row(1, &[Value::Int(1), Value::Int(20)]).unwrap();
        s.rollback();
        assert_eq!((count("a"), count("b")), (0, 0), "rollback must undo BOTH tables");

        // Commit: both land, in one meta flip.
        let mut s = db.begin().unwrap();
        s.insert_row(0, &[Value::Int(1), Value::Int(10)]).unwrap();
        s.insert_row(1, &[Value::Int(1), Value::Int(20)]).unwrap();
        s.commit().unwrap();
        assert_eq!((count("a"), count("b")), (1, 1), "commit must land BOTH tables");

        // And a failure on the SECOND table must not leave the first behind —
        // the case that would make "atomic" a lie.
        let mut s = db.begin().unwrap();
        s.insert_row(0, &[Value::Int(2), Value::Int(11)]).unwrap();
        let dup = s.insert_row(1, &[Value::Int(1), Value::Int(99)]); // PK 1 exists
        assert!(dup.is_err(), "expected a PK violation on table b");
        s.rollback();
        assert_eq!(
            (count("a"), count("b")),
            (1, 1),
            "a failure on the second table must not leave the first table's row"
        );
    }
