//! Transactional source origin ledger. Actual commit SCN/XID comes from public
//! Flashback Versions, never from a guessed SCN or a self-declared XID.
use super::*;
use crate::adapter::NetOp;
use oracle_client::sql_type::{Blob, ToSql};
use std::io::{Read, Write};
mod capture;
mod proof;
pub use proof::attest_write_origins;
mod codec;
mod schema;
#[cfg(test)]
mod tests;
pub use capture::OracleOriginEvent;
pub use codec::OracleOriginTouch;
const TABLE: &str = "MPEDB_WRITE_ORIGINS";
const MARKER: &str = "MPEDB_WRITE_ORIGINS_V1";
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OracleOriginLedgerIdentity {
    pub object_id: u64,
    pub data_object_id: u64,
    pub schema_ddl: String,
}
#[derive(Clone, Copy)]
pub struct OracleWriteOrigin<'a> {
    pub file_incarnation: [u8; 16],
    pub ledger_identity: &'a OracleOriginLedgerIdentity,
}
impl OracleSource {
    pub(super) fn lock_request_origin(&self, request: &OracleWriteRequest<'_>) -> Result<()> {
        if let Some(origin) = request.origin {
            if origin.file_incarnation == [0; 16] {
                return Err(Error::Config(
                    "zero Oracle write origin FILE identity".into(),
                ));
            }
            self.lock_origin_ledger(origin.ledger_identity)?;
        }
        Ok(())
    }
    pub(super) fn read_request_origin(
        &self,
        request: &OracleWriteRequest<'_>,
        payload: &[u8; 32],
        expected: &[u8],
    ) -> Result<bool> {
        let Some(origin) = request.origin else {
            return Ok(false);
        };
        let mirror = request.mirror_id.as_bytes().to_vec();
        let token = request.token.to_vec();
        let sql=format!("SELECT file_incarnation,payload_hash,footprint_hash,footprint_bytes,format_version FROM {} WHERE mirror_id=:1 AND request_token=:2",self.origin_target());
        let mut stmt = self
            .connection
            .statement(&sql)
            .lob_locator()
            .build()
            .map_err(|e| err("origin read prepare", e))?;
        let mut rows = stmt
            .query(&[&mirror, &token])
            .map_err(|e| err("origin read", e))?;
        let Some(row) = rows.next().transpose().map_err(|e| err("origin row", e))? else {
            return Ok(false);
        };
        if rows.next().is_some()
            || get::<Vec<u8>>(&row, 0)? != origin.file_incarnation
            || get::<Vec<u8>>(&row, 1)? != payload
            || get::<u64>(&row, 4)? != 1
        {
            return Err(Error::Corrupt(
                "Oracle write origin identity/payload mismatch".into(),
            ));
        }
        let hash: Vec<u8> = get(&row, 2)?;
        let lob: Blob = get(&row, 3)?;
        let mut bytes = Vec::new();
        lob.take(codec::LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| Error::Unsupported(format!("Oracle origin BLOB read failed: {e}")))?;
        if bytes != expected || blake3::hash(&bytes).as_bytes() != hash.as_slice() {
            return Err(Error::Corrupt(
                "Oracle write origin footprint mismatch".into(),
            ));
        }
        Ok(true)
    }
    pub(super) fn request_origin_footprint(
        &self,
        tables: &[OracleTable],
        ops: &[NetOp],
        request: &OracleWriteRequest<'_>,
    ) -> Result<Option<Vec<u8>>> {
        request
            .origin
            .map(|_| codec::encode(tables, ops))
            .transpose()
    }
    pub(super) fn insert_request_origin(
        &self,
        request: &OracleWriteRequest<'_>,
        payload: &[u8; 32],
        bytes: &[u8],
    ) -> Result<()> {
        let origin = request
            .origin
            .ok_or_else(|| Error::Corrupt("missing Oracle write origin context".into()))?;
        let mirror = request.mirror_id.as_bytes().to_vec();
        let file = origin.file_incarnation.to_vec();
        let token = request.token.to_vec();
        let payload = payload.to_vec();
        let hash = blake3::hash(bytes).as_bytes().to_vec();
        let mut blob = Blob::new(&self.connection).map_err(|e| err("origin BLOB create", e))?;
        blob.write_all(bytes)
            .map_err(|e| Error::Unsupported(format!("Oracle origin BLOB write failed: {e}")))?;
        let sql=format!("INSERT INTO {}(mirror_id,file_incarnation,request_token,payload_hash,footprint_hash,footprint_bytes,format_version) VALUES(:1,:2,:3,:4,:5,:6,1)",self.origin_target());
        let binds: [&dyn ToSql; 6] = [&mirror, &file, &token, &payload, &hash, &blob];
        self.connection
            .statement(&sql)
            .lob_locator()
            .build()
            .map_err(|e| err("origin insert prepare", e))?
            .execute(&binds)
            .map_err(|e| err("origin insert", e))?;
        self.lock_origin_ledger(origin.ledger_identity)?;
        if !self.read_request_origin(request, payload.as_slice().try_into().unwrap(), bytes)? {
            return Err(Error::Corrupt(
                "Oracle did not retain its write origin".into(),
            ));
        }
        Ok(())
    }
}
