//! Owned origin ledger preparation and recreation/DDL fencing.
use super::*;
impl OracleSource {
    pub(super) fn origin_target(&self) -> String {
        format!("{}.{}", quote(&self.owner), quote(TABLE))
    }
    fn origin_identity(&self) -> Result<OracleOriginLedgerIdentity> {
        let (object_id,data_object_id): (u64,Option<u64>) = self.connection.query_row_as("SELECT object_id,data_object_id FROM all_objects WHERE owner=:1 AND object_name=:2 AND object_type='TABLE' AND subobject_name IS NULL", &[&self.owner,&TABLE]).map_err(|e| err("origin ledger identity",e))?;
        Ok(OracleOriginLedgerIdentity {
            object_id,
            data_object_id: data_object_id.ok_or_else(|| {
                Error::Corrupt("Oracle origin ledger has no initialized segment".into())
            })?,
            schema_ddl: self.table_ddl(TABLE, true)?,
        })
    }
    pub(crate) fn existing_origin_ledger(&mut self) -> Result<Option<OracleOriginLedgerIdentity>> {
        let count: u64 = self
            .connection
            .query_row_as(
                "SELECT COUNT(*) FROM all_tables WHERE owner=:1 AND table_name=:2",
                &[&self.owner, &TABLE],
            )
            .map_err(|e| err("existing origin ledger", e))?;
        if count == 0 {
            Ok(None)
        } else {
            self.prepare_origin_ledger().map(Some)
        }
    }
    pub(in crate::oracle_source) fn lock_origin_ledger(
        &self,
        identity: &OracleOriginLedgerIdentity,
    ) -> Result<()> {
        self.connection
            .execute(
                &format!(
                    "LOCK TABLE {} IN ROW EXCLUSIVE MODE NOWAIT",
                    self.origin_target()
                ),
                &[],
            )
            .map_err(|e| err("origin ledger fence", e))?;
        if self.origin_identity()? != *identity {
            return Err(Error::Unsupported(
                "Oracle origin ledger changed; reconcile before retrying".into(),
            ));
        }
        self.check_tracking_triggers(TABLE)
    }
    /// Prepare before a bootstrap boundary. Existing application collisions are
    /// refused; creation is explicit and never performed as part of a DML retry.
    pub fn prepare_origin_ledger(&mut self) -> Result<OracleOriginLedgerIdentity> {
        let result = (|| {
            let count: u64 = self
                .connection
                .query_row_as(
                    "SELECT COUNT(*) FROM all_tables WHERE owner=:1 AND table_name=:2",
                    &[&self.owner, &TABLE],
                )
                .map_err(|e| err("origin ledger existence", e))?;
            if count == 0 {
                let ddl=format!("CREATE TABLE {} (mirror_id RAW(128) NOT NULL, file_incarnation RAW(16) NOT NULL, request_token RAW(32) NOT NULL, payload_hash RAW(32) NOT NULL, footprint_hash RAW(32) NOT NULL, footprint_bytes BLOB NOT NULL, format_version NUMBER(1,0) DEFAULT 1 NOT NULL, PRIMARY KEY(mirror_id,request_token), CONSTRAINT {MARKER} CHECK(format_version=1)) SEGMENT CREATION IMMEDIATE",self.origin_target());
                self.connection.execute(&format!("BEGIN EXECUTE IMMEDIATE '{}'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -955 THEN RAISE; END IF; END;",ddl.replace('\'',"''")),&[]).map_err(|e|err("origin ledger create",e))?;
            }
            self.connection
                .execute(
                    &format!(
                        "LOCK TABLE {} IN ROW EXCLUSIVE MODE NOWAIT",
                        self.origin_target()
                    ),
                    &[],
                )
                .map_err(|e| err("origin ledger schema fence", e))?;
            let tables = self.introspect(Some(&[TABLE.into()]), &[])?;
            let table = tables
                .first()
                .ok_or_else(|| Error::Corrupt("Oracle origin ledger missing".into()))?;
            let expected = [
                ("MIRROR_ID", "RAW", 128),
                ("FILE_INCARNATION", "RAW", 16),
                ("REQUEST_TOKEN", "RAW", 32),
                ("PAYLOAD_HASH", "RAW", 32),
                ("FOOTPRINT_HASH", "RAW", 32),
                ("FOOTPRINT_BYTES", "BLOB", 0),
                ("FORMAT_VERSION", "NUMBER", 0),
            ];
            if tables.len() != 1
                || table.columns.len() != expected.len()
                || table.pk != [0, 2]
                || !table.pk_validated
                || table.temporary
                || table.external
                || table.nested
                || table
                    .columns
                    .iter()
                    .zip(expected)
                    .any(|(c, (name, ty, len))| {
                        c.name != name
                            || c.data_type != ty
                            || !c.not_null
                            || c.generated
                            || c.identity
                            || (len != 0 && c.data_length != len)
                            || (ty == "NUMBER" && (c.precision != Some(1) || c.scale != Some(0)))
                    })
            {
                return Err(Error::Unsupported(
                    "Oracle origin ledger name collides with an incompatible table".into(),
                ));
            }
            let markers:u64=self.connection.query_row_as("SELECT COUNT(*) FROM all_constraints WHERE owner=:1 AND table_name=:2 AND constraint_name=:3 AND constraint_type='C' AND status='ENABLED' AND validated='VALIDATED'", &[&self.owner,&TABLE,&MARKER]).map_err(|e|err("origin ledger ownership marker",e))?;
            if markers != 1 {
                return Err(Error::Unsupported(
                    "Oracle origin ledger ownership marker is missing".into(),
                ));
            }
            let identity = self.origin_identity()?;
            self.lock_origin_ledger(&identity)?;
            Ok(identity)
        })();
        self.connection
            .rollback()
            .map_err(|e| err("origin ledger prepare release", e))?;
        result
    }
}
