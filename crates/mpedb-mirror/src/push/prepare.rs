//! Capture one immutable request under the local writer, durably BEFORE RPC.
use super::*;
use mpedb::WriteSession;
use mpedb_core::DirtyOp;
pub(super) fn prepare<A: SourceAdapter + ?Sized>(
    db: &Database,
    adapter: &A,
) -> Result<PreparedPush> {
    let identity = adapter.push_outbox_identity();
    if identity == Some([0; 16]) {
        return Err(Error::Config(
            "durable push requires a nonzero FILE incarnation".into(),
        ));
    }
    let mut s = db.begin()?;
    s.set_capture(false);
    if let Some(prepared) = outbox::load(&mut s, identity)? {
        s.rollback();
        return Ok(prepared);
    }
    let mut prepared = scan(&mut s)?;
    if let Some(identity) = identity.filter(|_| !prepared.ops.is_empty()) {
        if prepared.cursor.is_none() {
            prepared.cursor = Some(adapter.zero_cursor());
        }
        prepared.outbox_tag = Some(outbox::store(&mut s, identity, &prepared)?);
        s.commit()?;
    } else {
        s.rollback();
    }
    Ok(prepared)
}
fn scan(s: &mut WriteSession<'_>) -> Result<PreparedPush> {
    let schema = mpedb_types::Schema {
        tables: s.schema().tables.clone(),
    };
    let mut prepared = PreparedPush {
        epoch_bytes: s.sys_record_get(state::MIR_NS, state::KEY_EPOCH)?,
        cursor: s.sys_record_get(state::MIR_NS, state::KEY_CUR)?,
        cfg_bytes: s.sys_record_get(state::MIR_NS, state::KEY_CFG)?,
        schema,
        ops: Vec::new(),
        pending: Vec::new(),
        outbox_tag: None,
    };
    for (subkey, val) in s.sys_record_scan_range("cdc", b"d/", b"d0")? {
        if subkey.len() < 6 {
            return Err(Error::Corrupt("short cdc dirty key".into()));
        }
        let de = DirtyEntry::decode(&val)?;
        let table_id = u32::from_be_bytes(subkey[2..6].try_into().unwrap());
        let table = prepared
            .schema
            .tables
            .get(table_id as usize)
            .ok_or_else(|| Error::Corrupt(format!("dirty entry for unknown table {table_id}")))?;
        let pk = keycode::decode_key(&de.pk_keycode, &table.pk_types())?;
        let kind = match de.op {
            DirtyOp::Upsert => s
                .get_by_pk(table_id, &pk)?
                .map_or(NetOpKind::Delete, NetOpKind::Upsert),
            DirtyOp::Delete => NetOpKind::Delete,
        };
        // Keep the entire coalesced dirty set in one source transaction.
        // Arbitrary byte-prefix batching could split key moves/local commits.
        // Oversized durable requests fail BEFORE RPC rather than truncate.
        prepared.ops.push(NetOp { table_id, pk, kind });
        prepared.pending.push(Pending {
            subkey,
            last_txn: de.last_txn,
            table_id,
            pk_keycode: de.pk_keycode,
        });
    }
    Ok(prepared)
}
