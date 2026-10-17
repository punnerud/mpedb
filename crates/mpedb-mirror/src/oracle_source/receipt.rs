//! Durable idempotent write receipts, committed with source data. This is a
//! transport primitive; the mirror protocol must supply stable dirty-generation
//! request tokens and process postimages before enabling adapter write-back.
use super::*;
use crate::adapter::NetOp;
use oracle_client::sql_type::{Blob, ToSql};
use std::io::{Read, Write};
const TABLE: &str = "MPEDB_WRITE_RECEIPTS";
const MARKER: &str = "MPEDB_WRITE_RECEIPTS_V1";
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OracleLedgerIdentity {
    pub object_id: u64,
    pub data_object_id: u64,
    pub schema_ddl: String,
}
/// Token must include a durable local FILE incarnation and dirty generation,
/// not just a path or transaction number that a fresh import can reuse, nor its
/// values: A -> B -> A are different requests. Reusing a committed token with
/// another payload is refused. Keep the ledger until local durable recovery no
/// longer needs that token; this API never deletes receipts.
pub struct OracleWriteRequest<'a> {
    pub mirror_id: &'a str,
    pub token: [u8; 32],
    pub epoch: u64,
    pub authority: &'a str,
    pub state_identity: &'a OracleStateIdentity,
    pub ledger_identity: &'a OracleLedgerIdentity,
}
impl OracleSource {
    fn receipt_target(&self) -> String {
        format!("{}.{}", quote(&self.owner), quote(TABLE))
    }
    fn ledger_identity(&self) -> Result<OracleLedgerIdentity> {
        let (object_id,data_object_id): (u64,Option<u64>) = self.connection.query_row_as("SELECT object_id,data_object_id FROM all_objects WHERE owner=:1 AND object_name=:2 AND object_type='TABLE' AND subobject_name IS NULL", &[&self.owner,&TABLE]).map_err(|e| err("receipt ledger identity",e))?;
        Ok(OracleLedgerIdentity {
            object_id,
            data_object_id: data_object_id.ok_or_else(|| {
                Error::Corrupt("Oracle receipt ledger has no initialized segment".into())
            })?,
            schema_ddl: self.table_ddl(TABLE, true)?,
        })
    }
    pub(crate) fn existing_write_ledger(&mut self) -> Result<Option<OracleLedgerIdentity>> {
        let count: u64 = self
            .connection
            .query_row_as(
                "SELECT COUNT(*) FROM all_tables WHERE owner=:1 AND table_name=:2",
                &[&self.owner, &TABLE],
            )
            .map_err(|e| err("existing receipt ledger", e))?;
        if count == 0 {
            Ok(None)
        } else {
            self.prepare_write_ledger().map(Some)
        }
    }
    pub(super) fn lock_write_ledger(&self, identity: &OracleLedgerIdentity) -> Result<()> {
        self.connection
            .execute(
                &format!(
                    "LOCK TABLE {} IN ROW EXCLUSIVE MODE NOWAIT",
                    self.receipt_target()
                ),
                &[],
            )
            .map_err(|e| err("receipt ledger fence", e))?;
        if self.ledger_identity()? != *identity {
            return Err(Error::Unsupported(
                "Oracle receipt ledger changed; reconcile before retrying".into(),
            ));
        }
        Ok(())
    }
    /// Prepare tracking BEFORE storing a bootstrap boundary. An existing table
    /// must have the exact owned shape; application name collisions are refused.
    pub fn prepare_write_ledger(&mut self) -> Result<OracleLedgerIdentity> {
        let result = (|| {
            let count: u64 = self
                .connection
                .query_row_as(
                    "SELECT COUNT(*) FROM all_tables WHERE owner=:1 AND table_name=:2",
                    &[&self.owner, &TABLE],
                )
                .map_err(|e| err("receipt ledger existence", e))?;
            if count == 0 {
                let ddl=format!("CREATE TABLE {} (mirror_id RAW(128) NOT NULL, request_token RAW(32) NOT NULL, payload_hash RAW(32) NOT NULL, result_hash RAW(32) NOT NULL, result_bytes BLOB NOT NULL, format_version NUMBER(1,0) DEFAULT 1 NOT NULL, PRIMARY KEY(mirror_id,request_token), CONSTRAINT {MARKER} CHECK(format_version=1)) SEGMENT CREATION IMMEDIATE",self.receipt_target());
                self.connection.execute(&format!("BEGIN EXECUTE IMMEDIATE '{}'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -955 THEN RAISE; END IF; END;",ddl.replace('\'',"''")),&[]).map_err(|e|err("receipt ledger create",e))?;
            }
            self.connection
                .execute(
                    &format!(
                        "LOCK TABLE {} IN ROW EXCLUSIVE MODE NOWAIT",
                        self.receipt_target()
                    ),
                    &[],
                )
                .map_err(|e| err("receipt ledger schema fence", e))?;
            let tables = self.introspect(Some(&[TABLE.into()]), &[])?;
            let table = tables
                .first()
                .ok_or_else(|| Error::Corrupt("Oracle receipt ledger is missing".into()))?;
            let expected = [
                ("MIRROR_ID", "RAW", 128),
                ("REQUEST_TOKEN", "RAW", 32),
                ("PAYLOAD_HASH", "RAW", 32),
                ("RESULT_HASH", "RAW", 32),
                ("RESULT_BYTES", "BLOB", 0),
                ("FORMAT_VERSION", "NUMBER", 0),
            ];
            if tables.len() != 1
                || table.columns.len() != expected.len()
                || table.pk != [0, 1]
                || !table.pk_validated
                || table.temporary
                || table.nested
                || table.external
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
                    "Oracle receipt ledger name collides with an incompatible table".into(),
                ));
            }
            let markers: u64=self.connection.query_row_as("SELECT COUNT(*) FROM all_constraints WHERE owner=:1 AND table_name=:2 AND constraint_name=:3 AND constraint_type='C' AND status='ENABLED' AND validated='VALIDATED'", &[&self.owner,&TABLE,&MARKER]).map_err(|e|err("receipt ledger ownership marker",e))?;
            if markers != 1 {
                return Err(Error::Unsupported(
                    "Oracle receipt ledger ownership marker is missing".into(),
                ));
            }
            self.ledger_identity()
        })();
        self.connection
            .rollback()
            .map_err(|e| err("receipt ledger prepare release", e))?;
        result
    }
    fn lock_write_authority(&self, request: &OracleWriteRequest<'_>) -> Result<()> {
        if request.mirror_id.is_empty()
            || request.mirror_id.len() > 128
            || request.token == [0; 32]
            || !matches!(request.authority, "source" | "mpedb")
        {
            return Err(Error::Config(
                "invalid Oracle receipted-write identity or authority".into(),
            ));
        }
        self.lock_state()?;
        let key = request.mirror_id.as_bytes().to_vec();
        let _: Vec<u8> = self
            .connection
            .query_row_as(
                &format!(
                    "SELECT incarnation FROM {}.{} WHERE mirror_id=:1 FOR UPDATE",
                    quote(&self.owner),
                    quote("MPEDB_MIRROR_STATE")
                ),
                &[&key],
            )
            .map_err(|e| err("receipted write authority lock", e))?;
        self.verify_write_authority(request)
    }
    fn verify_write_authority(&self, request: &OracleWriteRequest<'_>) -> Result<()> {
        if self.source_state_identity(request.mirror_id)? != *request.state_identity
            || self.state_row(request.mirror_id, None)?
                != Some((request.epoch, request.authority.into()))
        {
            return Err(Error::Unsupported(
                "Oracle receipted write was fenced by authority or incarnation".into(),
            ));
        }
        Ok(())
    }
    fn receipt_bytes(
        &self,
        request: &OracleWriteRequest<'_>,
        payload: &[u8; 32],
    ) -> Result<Option<Vec<u8>>> {
        let mirror = request.mirror_id.as_bytes().to_vec();
        let token = request.token.to_vec();
        let sql=format!("SELECT payload_hash,result_hash,result_bytes,format_version FROM {} WHERE mirror_id=:1 AND request_token=:2",self.receipt_target());
        let mut stmt = self
            .connection
            .statement(&sql)
            .lob_locator()
            .build()
            .map_err(|e| err("receipt read prepare", e))?;
        let mut rows = stmt
            .query(&[&mirror, &token])
            .map_err(|e| err("receipt read", e))?;
        let Some(row) = rows.next().transpose().map_err(|e| err("receipt row", e))? else {
            return Ok(None);
        };
        if get::<Vec<u8>>(&row, 0)? != payload || get::<u64>(&row, 3)? != 1 || rows.next().is_some()
        {
            return Err(Error::Corrupt(
                "Oracle write token was reused with another payload or invalid receipt".into(),
            ));
        }
        let hash: Vec<u8> = get(&row, 1)?;
        let lob: Blob = get(&row, 2)?;
        let mut bytes = Vec::new();
        lob.take(64 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| Error::Unsupported(format!("Oracle receipt BLOB read failed: {e}")))?;
        if bytes.len() > 64 * 1024 * 1024 || blake3::hash(&bytes).as_bytes() != hash.as_slice() {
            return Err(Error::Corrupt(
                "Oracle write receipt hash or size is invalid".into(),
            ));
        }
        Ok(Some(bytes))
    }
    /// Commit verified source writes and their receipt together. A retry of an
    /// already committed token returns its ORIGINAL postimages without running
    /// DML again. No conflict policy, per-row rejection or echo filtering is
    /// implied by this low-level whole-batch primitive.
    pub fn write_batch_receipted(
        &mut self,
        tables: &[OracleTable],
        ops: &[NetOp],
        request: &OracleWriteRequest<'_>,
    ) -> Result<Vec<Option<Vec<Value>>>> {
        let result = (|| {
            self.lock_write_authority(request)?;
            self.lock_write_ledger(request.ledger_identity)?;
            // Replays must also reject stale application schema metadata.
            let locked = self.lock_write_tables(tables, ops)?;
            let realm =
                crate::oracle_cursor::OraclePosition::new(&self.identity()?, tables, 1)?.encode();
            let mut domain = realm;
            domain.extend_from_slice(&(request.mirror_id.len() as u64).to_be_bytes());
            domain.extend_from_slice(request.mirror_id.as_bytes());
            domain.extend_from_slice(&request.epoch.to_be_bytes());
            domain.push(u8::from(request.authority == "mpedb"));
            domain.extend_from_slice(&request.state_identity.object_id.to_be_bytes());
            domain.extend_from_slice(&request.state_identity.data_object_id.to_be_bytes());
            domain.extend_from_slice(&request.ledger_identity.object_id.to_be_bytes());
            domain.extend_from_slice(&request.ledger_identity.data_object_id.to_be_bytes());
            domain.extend_from_slice(&request.state_identity.incarnation);
            let payload = super::receipt_codec::payload(tables, ops, &domain)?;
            if let Some(bytes) = self.receipt_bytes(request, &payload)? {
                return super::receipt_codec::decode(tables, ops, &bytes);
            }
            let images = self.write_locked(&locked, ops)?;
            let bytes = super::receipt_codec::encode(tables, ops, &images)?;
            let hash = blake3::hash(&bytes).as_bytes().to_vec();
            let mut blob =
                Blob::new(&self.connection).map_err(|e| err("receipt BLOB create", e))?;
            blob.write_all(&bytes).map_err(|e| {
                Error::Unsupported(format!("Oracle receipt BLOB write failed: {e}"))
            })?;
            let mirror = request.mirror_id.as_bytes().to_vec();
            let token = request.token.to_vec();
            let payload = payload.to_vec();
            let sql=format!("INSERT INTO {}(mirror_id,request_token,payload_hash,result_hash,result_bytes,format_version) VALUES(:1,:2,:3,:4,:5,1)",self.receipt_target());
            let refs: [&dyn ToSql; 5] = [&mirror, &token, &payload, &hash, &blob];
            self.connection
                .statement(&sql)
                .lob_locator()
                .build()
                .map_err(|e| err("receipt insert prepare", e))?
                .execute(&refs)
                .map_err(|e| err("receipt insert", e))?;
            let payload: [u8; 32] = payload.try_into().unwrap();
            if self.receipt_bytes(request, &payload)?.as_deref() != Some(bytes.as_slice()) {
                return Err(Error::Corrupt(
                    "Oracle did not retain its write receipt; transaction rolled back".into(),
                ));
            }
            self.verify_write_authority(request)?;
            Ok(images)
        })();
        let result = result.and_then(|images| {
            self.connection
                .commit()
                .map_err(|e| err("receipted write commit", e))?;
            Ok(images)
        });
        if result.is_err() {
            self.connection
                .rollback()
                .map_err(|e| err("receipted write rollback", e))?;
        }
        result
    }
}
