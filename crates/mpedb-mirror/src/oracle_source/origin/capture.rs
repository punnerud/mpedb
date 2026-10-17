//! Public Flashback Versions ties immutable origin rows to actual source commits.
use super::*;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OracleOriginEvent {
    pub scn: u64,
    pub xid: Vec<u8>,
    pub file_incarnation: [u8; 16],
    pub request_id: [u8; 32],
    pub touches: Vec<OracleOriginTouch>,
}
impl OracleSource {
    /// Read immutable origin INSERTs within `(from_scn,to_scn]`. Any mutation,
    /// expiration, missing undo, malformed footprint or changed ledger fails
    /// without advancing a mirror cursor. No garbage collection is implied.
    pub fn read_write_origins(
        &mut self,
        mirror_id: &str,
        identity: &OracleOriginLedgerIdentity,
        from_scn: u64,
        to_scn: u64,
        max_events: usize,
    ) -> Result<Vec<OracleOriginEvent>> {
        let result = (|| {
            if mirror_id.is_empty() || mirror_id.len() > 128 || from_scn > to_scn || max_events == 0
            {
                return Err(Error::Config(
                    "invalid Oracle origin capture interval/identity/budget".into(),
                ));
            }
            self.lock_origin_ledger(identity)?;
            if from_scn == to_scn {
                return Ok(vec![]);
            }
            if to_scn > self.current_scn()? {
                return Err(Error::Config(
                    "Oracle origin capture cannot read a future SCN".into(),
                ));
            }
            let mirror = mirror_id.as_bytes().to_vec();
            let sql=format!("SELECT versions_startscn,versions_endscn,versions_xid,versions_operation,file_incarnation,request_token,footprint_hash,footprint_bytes,format_version,payload_hash FROM {} VERSIONS BETWEEN SCN :from_scn AND :to_scn WHERE mirror_id=:mirror AND ((versions_startscn>:from_scn AND versions_startscn<=:to_scn) OR (versions_endscn>:from_scn AND versions_endscn<=:to_scn))",self.origin_target());
            let mut stmt = self
                .connection
                .statement(&sql)
                .lob_locator()
                .build()
                .map_err(|e| err("origin versions prepare", e))?;
            let rows = stmt
                .query_named(&[
                    ("from_scn", &from_scn),
                    ("to_scn", &to_scn),
                    ("mirror", &mirror),
                ])
                .map_err(|e| err("origin versions", e))?;
            let mut events = Vec::new();
            let mut total = 0usize;
            for row in rows {
                if events.len() >= max_events {
                    return Err(Error::Unsupported("Oracle origin capture event budget exceeded; narrow the window without advancing".into()));
                }
                let row = row.map_err(|e| err("origin version row", e))?;
                let start: Option<u64> = get(&row, 0)?;
                let end: Option<u64> = get(&row, 1)?;
                let xid: Option<Vec<u8>> = get(&row, 2)?;
                let operation: Option<String> = get(&row, 3)?;
                if end.is_some_and(|scn| scn > from_scn && scn <= to_scn)
                    || operation.as_deref() != Some("I")
                    || !start.is_some_and(|scn| scn > from_scn && scn <= to_scn)
                {
                    return Err(Error::Corrupt(
                        "Oracle origin ledger rows were mutated or expired".into(),
                    ));
                }
                let xid = xid.filter(|id| !id.is_empty()).ok_or_else(|| {
                    Error::Corrupt("Oracle origin is missing its committing transaction".into())
                })?;
                let file_incarnation: [u8; 16] = get::<Vec<u8>>(&row, 4)?
                    .try_into()
                    .map_err(|_| Error::Corrupt("invalid Oracle origin FILE length".into()))?;
                let request_id: [u8; 32] = get::<Vec<u8>>(&row, 5)?
                    .try_into()
                    .map_err(|_| Error::Corrupt("invalid Oracle origin request length".into()))?;
                let hash: Vec<u8> = get(&row, 6)?;
                let blob: Blob = get(&row, 7)?;
                if file_incarnation == [0; 16]
                    || request_id == [0; 32]
                    || get::<u64>(&row, 8)? != 1
                    || get::<Vec<u8>>(&row, 9)?.len() != 32
                {
                    return Err(Error::Corrupt(
                        "invalid Oracle origin identity/version".into(),
                    ));
                }
                let remaining = codec::LIMIT.saturating_sub(total);
                let mut bytes = Vec::new();
                blob.take(remaining as u64 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|e| {
                        Error::Unsupported(format!("Oracle origin version BLOB read failed: {e}"))
                    })?;
                if bytes.len() > remaining {
                    return Err(Error::Unsupported("Oracle origin capture byte budget exceeded; narrow the window without advancing".into()));
                }
                total += bytes.len();
                if blake3::hash(&bytes).as_bytes() != hash.as_slice() {
                    return Err(Error::Corrupt(
                        "Oracle origin version footprint checksum mismatch".into(),
                    ));
                }
                let touches = codec::decode(&bytes)?;
                events.push(OracleOriginEvent {
                    scn: start.unwrap(),
                    xid,
                    file_incarnation,
                    request_id,
                    touches,
                });
            }
            self.lock_origin_ledger(identity)?;
            events.sort_by(|a, b| a.scn.cmp(&b.scn).then(a.request_id.cmp(&b.request_id)));
            Ok(events)
        })();
        self.connection
            .rollback()
            .map_err(|e| err("origin capture release", e))?;
        result
    }
}
