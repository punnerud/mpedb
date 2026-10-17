//! Durable whole-request decisions for the mirror outbox. A rejected request
//! writes no application DML and records an EMPTY origin footprint atomically.
use super::*;
use crate::adapter::{NetOp, PushReply};
use crate::oracle_cursor::OraclePosition;

impl OracleSource {
    /// Commit an immutable mirror request's accepted or rejected outcome.
    /// Source-authoritative requests require a checked cursor; MPEdb-authority
    /// requests are unconditional. One conflicting target rejects the whole
    /// batch, preserving local transaction/key-move atomicity. All source
    /// changes count, so callers pull between requests, including own writes.
    /// A committed token replays ORIGINAL flags/images without application DML,
    /// even after its epoch advanced. No receipt/origin garbage collection.
    pub fn write_mirror_batch(
        &mut self,
        tables: &[OracleTable],
        ops: &[NetOp],
        request: &OracleWriteRequest<'_>,
        from: Option<&OraclePosition>,
        max_events: usize,
    ) -> Result<PushReply> {
        if ops.is_empty()
            || ops.len() > 100_000
            || max_events == 0
            || request.origin.is_none()
            || (request.authority == "source") != from.is_some()
        {
            return Err(Error::Config("Oracle mirror write requires bounded operations, origin and an authority-matched cursor".into()));
        }
        let result = (|| {
            // Serialize recovery with writers and authority transitions. Current
            // epoch is checked only AFTER a committed outcome has been sought.
            self.lock_write_incarnation(request)?;
            self.lock_write_ledger(request.ledger_identity)?;
            self.lock_request_origin(request)?;
            let locked = self.lock_write_tables(tables, ops)?;
            if let Some(from) = from {
                OraclePosition::decode(&from.encode(), &self.identity()?, tables)?;
            }
            let mut hash = blake3::Hasher::new();
            hash.update(b"MPEdb Oracle mirror decision v1\0");
            hash.update(&self.receipt_payload(tables, ops, request)?);
            hash.update(&[u8::from(from.is_some())]);
            if let Some(from) = from {
                hash.update(&from.encode());
            }
            let payload = *hash.finalize().as_bytes();
            let full_origin = self
                .request_origin_footprint(tables, ops, request)?
                .expect("required origin");
            let empty_origin = self
                .request_origin_footprint(tables, &[], request)?
                .expect("required origin");
            if let Some(bytes) = self.receipt_bytes(request, &payload)? {
                let reply = super::receipt_codec::decode_mirror(tables, ops, &bytes)?;
                let footprint = if reply.applied.iter().all(|yes| *yes) {
                    &full_origin
                } else {
                    &empty_origin
                };
                if !self.read_request_origin(request, &payload, footprint)? {
                    return Err(Error::Corrupt(
                        "Oracle mirror receipt is missing its committed origin".into(),
                    ));
                }
                return Ok(reply);
            }
            if self.read_request_origin(request, &payload, &full_origin)? {
                return Err(Error::Corrupt(
                    "Oracle mirror origin exists without its receipt".into(),
                ));
            }
            self.verify_write_authority(request)?;
            let conflict = if let Some(from) = from {
                if from.scn() > self.current_scn()? {
                    return Err(Error::Config(
                        "Oracle mirror write cursor is in the future".into(),
                    ));
                }
                self.write_conflicts(tables, ops, from.scn(), max_events)?
            } else {
                false
            };
            let images = if conflict {
                vec![None; ops.len()]
            } else {
                self.write_locked(&locked, ops)?
            };
            let reply = PushReply {
                applied: vec![!conflict; ops.len()],
                postimages: Some(images),
            };
            self.insert_request_origin(
                request,
                &payload,
                if conflict {
                    &empty_origin
                } else {
                    &full_origin
                },
            )?;
            let bytes = super::receipt_codec::encode_mirror(tables, ops, &reply)?;
            self.store_write_receipt(request, &payload, &bytes)?;
            self.check_tracking_triggers("MPEDB_WRITE_RECEIPTS")?;
            self.check_tracking_triggers("MPEDB_WRITE_ORIGINS")?;
            if !conflict {
                for (op, expected) in ops.iter().zip(reply.postimages.as_ref().expect("images")) {
                    let actual = self.read_key(locked[op.table_id as usize], &op.pk, None)?;
                    let same = match (expected, &actual) {
                        (None, None) => true,
                        (Some(a), Some(b)) => crate::adapter::same_carrier_rows(a, b),
                        _ => false,
                    };
                    if !same {
                        return Err(Error::Unsupported("Oracle tracking writes changed an application postimage; transaction rolled back".into()));
                    }
                }
            }
            self.verify_write_authority(request)?;
            Ok(reply)
        })();
        let result = result.and_then(|reply| {
            self.connection
                .commit()
                .map_err(|e| err("mirror write commit", e))?;
            Ok(reply)
        });
        if result.is_err() {
            self.connection
                .rollback()
                .map_err(|e| err("mirror write rollback", e))?;
        }
        result
    }
}
