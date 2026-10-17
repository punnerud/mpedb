//! Oracle-side authority state. DDL/shape checks are separate from row CAS;
//! idempotent initialization never overwrites an established source epoch.
use super::*;
const TABLE: &str = "MPEDB_MIRROR_STATE";
const MARKER: &str = "MPEDB_MIRROR_STATE_V1";
fn input(id: &str, authority: &str) -> Result<()> {
    if id.is_empty() || id.len() > 128 || !matches!(authority, "source" | "mpedb") {
        return Err(Error::Config(
            "Oracle state requires a 1..128-byte mirror id and source/mpedb authority".into(),
        ));
    }
    Ok(())
}
impl OracleSource {
    fn state_target(&self) -> String {
        format!("{}.{}", quote(&self.owner), quote(TABLE))
    }
    fn state_exists(&self) -> Result<bool> {
        self.connection
            .query_row_as::<u64>(
                "SELECT COUNT(*) FROM all_tables WHERE owner=:1 AND table_name=:2",
                &[&self.owner, &TABLE],
            )
            .map(|n| n == 1)
            .map_err(|e| err("source state existence", e))
    }
    pub(super) fn lock_state(&self) -> Result<()> {
        self.connection
            .execute(
                &format!(
                    "LOCK TABLE {} IN ROW EXCLUSIVE MODE NOWAIT",
                    self.state_target()
                ),
                &[],
            )
            .map_err(|e| err("source state DDL fence", e))?;
        let rows=self.connection.query("SELECT column_name,data_type,data_length,data_precision,data_scale,nullable FROM all_tab_cols WHERE owner=:1 AND table_name=:2 ORDER BY internal_column_id",&[&self.owner,&TABLE]).map_err(|e|err("source state shape",e))?;
        let expected = [
            ("MIRROR_ID", "RAW", Some(128), None, None),
            ("EPOCH", "NUMBER", None, Some(20), Some(0)),
            ("AUTHORITY", "VARCHAR2", Some(16), None, None),
            ("FORMAT_VERSION", "NUMBER", None, Some(1), Some(0)),
        ];
        let mut n = 0;
        for row in rows {
            let row = row.map_err(|e| err("source state shape row", e))?;
            let Some((name, ty, len, precision, scale)) = expected.get(n) else {
                return Err(Error::Unsupported(
                    "Oracle source state name collides with a different table".into(),
                ));
            };
            if get::<String>(&row, 0)? != *name
                || get::<String>(&row, 1)? != *ty
                || get::<String>(&row, 5)? != "N"
                || len.is_some_and(|l| get::<u64>(&row, 2).ok() != Some(l))
                || get::<Option<u64>>(&row, 3)? != *precision
                || get::<Option<i64>>(&row, 4)? != *scale
            {
                return Err(Error::Unsupported(
                    "Oracle source state table has an incompatible shape".into(),
                ));
            }
            n += 1;
        }
        if n != expected.len() {
            return Err(Error::Unsupported(
                "Oracle source state table has an incompatible shape".into(),
            ));
        }
        let markers:u64=self.connection.query_row_as("SELECT COUNT(*) FROM all_constraints WHERE owner=:1 AND table_name=:2 AND constraint_name=:3 AND constraint_type='C' AND status='ENABLED' AND validated='VALIDATED'",&[&self.owner,&TABLE,&MARKER]).map_err(|e|err("source state format marker",e))?;
        let keys:u64=self.connection.query_row_as("SELECT COUNT(*) FROM all_constraints c JOIN all_cons_columns k ON k.owner=c.owner AND k.constraint_name=c.constraint_name WHERE c.owner=:1 AND c.table_name=:2 AND c.constraint_type='P' AND c.status='ENABLED' AND c.validated='VALIDATED' AND k.column_name='MIRROR_ID' AND k.position=1 AND NOT EXISTS (SELECT 1 FROM all_cons_columns extra WHERE extra.owner=c.owner AND extra.constraint_name=c.constraint_name AND extra.position<>1)",&[&self.owner,&TABLE]).map_err(|e|err("source state identity",e))?;
        if markers != 1 || keys != 1 {
            return Err(Error::Unsupported(
                "Oracle source state ownership marker or validated key is missing".into(),
            ));
        }
        Ok(())
    }
    pub(super) fn state_row(&self, id: &str, scn: Option<u64>) -> Result<Option<(u64, String)>> {
        let snapshot = scn.map_or_else(String::new, |s| format!(" AS OF SCN {s}"));
        let key = id.as_bytes().to_vec();
        let sql = format!(
            "SELECT epoch,authority,format_version FROM {}{snapshot} WHERE mirror_id=:1",
            self.state_target()
        );
        let mut rows = self
            .connection
            .query(&sql, &[&key])
            .map_err(|e| err("source state read", e))?;
        let Some(row) = rows.next() else {
            return Ok(None);
        };
        let row = row.map_err(|e| err("source state row", e))?;
        let epoch: u64 = get(&row, 0)?;
        let authority: String = get(&row, 1)?;
        let version: u64 = get(&row, 2)?;
        if version != 1
            || !matches!(authority.as_str(), "source" | "mpedb")
            || rows.next().is_some()
        {
            return Err(Error::Corrupt(
                "Oracle source authority state is invalid or duplicated".into(),
            ));
        }
        Ok(Some((epoch, authority)))
    }
    pub fn ensure_source_state(&mut self, id: &str, epoch: u64, authority: &str) -> Result<()> {
        input(id, authority)?;
        let result = (|| {
            if !self.state_exists()? {
                let ddl=format!("CREATE TABLE {} (mirror_id RAW(128) NOT NULL, epoch NUMBER(20,0) NOT NULL, authority VARCHAR2(16 BYTE) NOT NULL, format_version NUMBER(1,0) DEFAULT 1 NOT NULL, PRIMARY KEY(mirror_id), CONSTRAINT {MARKER} CHECK(format_version=1), CHECK(authority IN ('source','mpedb')))",self.state_target());
                let sql=format!("BEGIN EXECUTE IMMEDIATE '{}'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -955 THEN RAISE; END IF; END;",ddl.replace('\'',"''"));
                self.connection
                    .execute(&sql, &[])
                    .map_err(|e| err("source state create", e))?;
            }
            self.lock_state()?;
            let key = id.as_bytes().to_vec();
            self.connection.execute(&format!("MERGE INTO {} d USING (SELECT :1 mirror_id,:2 epoch,:3 authority FROM dual) s ON (d.mirror_id=s.mirror_id) WHEN NOT MATCHED THEN INSERT(mirror_id,epoch,authority,format_version) VALUES(s.mirror_id,s.epoch,s.authority,1)",self.state_target()),&[&key,&epoch,&authority]).map_err(|e|err("source state initialize",e))?;
            if self.state_row(id, None)?.is_none() {
                return Err(Error::Corrupt(
                    "Oracle source state initialization did not retain its identity".into(),
                ));
            }
            self.connection
                .commit()
                .map_err(|e| err("source state initialize commit", e))?;
            Ok(())
        })();
        if result.is_err() {
            self.connection
                .rollback()
                .map_err(|e| err("source state initialize rollback", e))?;
        }
        result
    }
    pub fn read_source_state(&mut self, id: &str) -> Result<Option<(u64, String)>> {
        input(id, "source")?;
        let result = (|| {
            if !self.state_exists()? {
                return Ok(None);
            }
            self.lock_state()?;
            self.state_row(id, None)
        })();
        self.connection
            .rollback()
            .map_err(|e| err("source state read release", e))?;
        result
    }
    pub fn read_source_state_at(&mut self, id: &str, scn: u64) -> Result<Option<(u64, String)>> {
        input(id, "source")?;
        let result = (|| {
            if !self.state_exists()? {
                return Ok(None);
            }
            self.lock_state()?;
            self.state_row(id, Some(scn))
        })();
        self.connection
            .rollback()
            .map_err(|e| err("source state snapshot release", e))?;
        result
    }
    pub fn cas_source_state(
        &mut self,
        id: &str,
        expected: u64,
        next: u64,
        authority: &str,
    ) -> Result<bool> {
        input(id, authority)?;
        if expected.checked_add(1) != Some(next) {
            return Err(Error::Config(
                "Oracle source epoch CAS must advance exactly one epoch".into(),
            ));
        }
        let result = (|| {
            self.lock_state()?;
            let key = id.as_bytes().to_vec();
            let statement = self
                .connection
                .execute(
                    &format!(
                        "UPDATE {} SET epoch=:1,authority=:2 WHERE mirror_id=:3 AND epoch=:4",
                        self.state_target()
                    ),
                    &[&next, &authority, &key, &expected],
                )
                .map_err(|e| err("source epoch CAS", e))?;
            let changed = statement
                .row_count()
                .map_err(|e| err("source epoch CAS count", e))?;
            if changed == 0 {
                self.connection
                    .rollback()
                    .map_err(|e| err("source epoch fenced release", e))?;
                return Ok(false);
            }
            if changed != 1 || self.state_row(id, None)? != Some((next, authority.into())) {
                return Err(Error::Corrupt(
                    "Oracle source epoch CAS did not retain the requested state".into(),
                ));
            }
            self.connection
                .commit()
                .map_err(|e| err("source epoch CAS commit", e))?;
            Ok(true)
        })();
        if result.is_err() {
            self.connection
                .rollback()
                .map_err(|e| err("source epoch CAS rollback", e))?;
        }
        result
    }
}
