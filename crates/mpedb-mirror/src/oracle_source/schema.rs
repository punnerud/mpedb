//! Source DDL fences shared by capture and write-back.
use super::*;

impl OracleSource {
    pub(super) fn lock_schema(&self, table: &OracleTable) -> Result<()> {
        if table.owner != self.owner {
            return Err(Error::Unsupported(
                "Oracle write requires the connected application owner".into(),
            ));
        }
        let target = format!("{}.{}", quote(&table.owner), quote(&table.name));
        self.connection
            .execute(
                &format!("LOCK TABLE {target} IN ROW EXCLUSIVE MODE NOWAIT"),
                &[],
            )
            .map_err(|e| err("write schema fence", e))?;
        let object_id: u64 = self.connection.query_row_as("SELECT object_id FROM all_objects WHERE owner=:1 AND object_name=:2 AND object_type='TABLE' AND subobject_name IS NULL", &[&table.owner, &table.name]).map_err(|e| err("write object identity", e))?;
        let ddl: String = self
            .connection
            .query_row_as(
                "SELECT DBMS_METADATA.GET_DDL('TABLE',:1,:2) FROM dual",
                &[&table.name, &table.owner],
            )
            .map_err(|e| err("write schema verification", e))?;
        if object_id != table.object_id || ddl != table.ddl_source {
            return Err(Error::Unsupported(
                "Oracle source schema changed; reimport before writing".into(),
            ));
        }
        Ok(())
    }
}
