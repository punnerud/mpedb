// EXACT py-sequence mirror: bootstrap seed + live DDL + the A-case.
fn main() -> Result<(), mpedb::Error> {
    let path = ":memory:";
    
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 64\nmax_readers = 64\n\n\
         [[table]]\nname = \"_mpedb_py_bootstrap\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = mpedb::Database::open_with_config(mpedb::Config::from_toml_str(&toml)?)?;
    db.query("CREATE TABLE t (v INTEGER PRIMARY KEY)", &[])?;
    let mut s = db.begin()?;
    s.query("INSERT INTO t VALUES (1)", &[])?;
    s.commit()?;
    let mut s = db.begin()?;
    s.query("SAVEPOINT rep", &[])?;
    s.query("INSERT INTO t VALUES (30)", &[])?;
    s.query("ROLLBACK TO SAVEPOINT rep", &[])?;
    println!("i txn: {:?}", s.query("SELECT count(*) FROM t WHERE v = 30", &[])?);
    s.commit()?;
    println!("committed: {:?}", db.query("SELECT count(*) FROM t WHERE v = 30", &[])?);
    Ok(())
}
