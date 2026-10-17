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
        let ddl = self.table_ddl(&table.name, true)?;
        if object_id != table.object_id || ddl != table.schema_ddl {
            return Err(Error::Unsupported(
                "Oracle source schema changed; reimport before writing".into(),
            ));
        }
        Ok(())
    }

    /// Keep original DDL for provenance, but compare logical DDL for fences.
    /// First INSERT allocates deferred segments and changes physical GET_DDL
    /// output without changing columns or constraints. This dedicated session
    /// resets the metadata transform even when extraction fails.
    pub(super) fn table_ddl(&self, name: &str, logical: bool) -> Result<String> {
        const RESET: &str = "BEGIN DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM,'DEFAULT',TRUE); END;";
        self.connection
            .execute(RESET, &[])
            .map_err(|e| err("metadata transform reset", e))?;
        let result = (|| {
            if logical {
                self.connection.execute("BEGIN DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM,'SEGMENT_ATTRIBUTES',FALSE); END;", &[]).map_err(|e| err("logical metadata transform", e))?;
            }
            self.connection
                .query_row_as(
                    "SELECT DBMS_METADATA.GET_DDL('TABLE',:1,:2) FROM dual",
                    &[&name, &self.owner],
                )
                .map_err(|e| err("application table DDL", e))
        })();
        self.connection
            .execute(RESET, &[])
            .map_err(|e| err("metadata transform restore", e))?;
        result
    }
}
