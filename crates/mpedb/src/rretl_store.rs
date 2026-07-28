//! rRETL stage 3, the storage half: blob VERSIONING (reverse-delta chains with
//! full anchors — the Bennett knob made concrete) and zip ROUND-TRIP by
//! splice. The codecs live in [`crate::rretl_codec`]; this module owns the
//! tables, the transactions, and the verification discipline.
//!
//! Versioning (design/DESIGN-RRETL §11 stage 3, chain algebra per the
//! adversarial check's finding 11): the NEWEST version is always stored FULL;
//! when a new version arrives, the previous newest is REWRITTEN as a delta
//! whose base is EXACTLY the version above it — except every K-th version,
//! which stays full forever (git packfiles' depth limit). Materializing any
//! version walks down from the nearest full through at most K−1 deltas,
//! blake3-verified at EVERY step. Only the current newest is ever rewritten,
//! exactly once, when it stops being newest — older rows never change, so no
//! chain can dangle. **Nothing here deletes a version, and any future prune
//! must respect the base chains: deleting a full anchor orphans everything
//! below it to the next anchor.**
//!
//! Archives: a zip is ingested by SPLICING (§8.4) — members become rows, the
//! residual is the original minus the data segments — and reconstruction is
//! verified byte-identically against the original BEFORE the ingest commits,
//! then re-verified against the stored hash on every pack-out.

use mpedb_types::{Error, Result, Value};

use crate::rretl::{
    as_int, as_text, next_run_id, now_micros, rows_of, shape_gate, LineageRow,
};
use crate::rretl_codec::{
    delta_apply, delta_encode, envelope, open_envelope, zip_join, zip_split, Kind,
};
use crate::WriteSession;

pub const T_VERSIONS: &str = "rretl_versions";
pub const T_ARCHIVES: &str = "rretl_archives";
pub const T_MEMBERS: &str = "rretl_archive_members";

/// Every K-th version stays full forever — the Bennett pebbling knob
/// (commitment 6): residual space traded against recomputation depth, and
/// K−1 is the longest delta walk any get can take.
pub const FULL_EVERY: i64 = 8;

const VERSIONS_SHAPE: [&str; 5] = ["obj", "ver", "payload", "content_hash", "ts_micros"];
const ARCHIVES_SHAPE: [&str; 5] = ["archive_id", "name", "residual", "content_hash", "ts_micros"];
const MEMBERS_SHAPE: [&str; 5] = ["archive_id", "member_no", "name", "data", "method"];

/// One version, as `rretl versions` lists it.
#[derive(Debug)]
pub struct VersionInfo {
    pub ver: i64,
    /// "full" or "delta" — read from the stored envelope's kind byte.
    pub stored_as: &'static str,
    pub bytes: u64,
    pub content_hash: String,
}

/// One archive, as `rretl archives`/fsck sees it.
#[derive(Debug)]
pub struct ArchiveInfo {
    pub archive_id: i64,
    pub name: String,
    pub members: i64,
    pub content_hash: String,
}

// Bookkeeping tables come from SPECS with rigid types, not SQL text — see
// `rretl::spec_col` for why (`BLOB` in DDL means the TYPELESS column, which
// takes neither point probes nor range bounds).
fn ensure_version_table(
    s: &mut WriteSession<'_>,
    have: &[(String, Vec<String>)],
) -> Result<()> {
    use mpedb_types::ColumnType::{Blob, Int64, Text};
    if !shape_gate(have, T_VERSIONS, &VERSIONS_SHAPE)? {
        crate::rretl::create_bookkeeping(
            s,
            T_VERSIONS,
            vec![
                crate::rretl::spec_col("obj", Text),
                crate::rretl::spec_col("ver", Int64),
                crate::rretl::spec_col("payload", Blob),
                crate::rretl::spec_col("content_hash", Text),
                crate::rretl::spec_col("ts_micros", Int64),
            ],
            &["obj", "ver"],
        )?;
    }
    Ok(())
}

fn ensure_archive_tables(
    s: &mut WriteSession<'_>,
    have: &[(String, Vec<String>)],
) -> Result<()> {
    use mpedb_types::ColumnType::{Blob, Int64, Text};
    if !shape_gate(have, T_ARCHIVES, &ARCHIVES_SHAPE)? {
        crate::rretl::create_bookkeeping(
            s,
            T_ARCHIVES,
            vec![
                crate::rretl::spec_col("archive_id", Int64),
                crate::rretl::spec_col("name", Text),
                crate::rretl::spec_col("residual", Blob),
                crate::rretl::spec_col("content_hash", Text),
                crate::rretl::spec_col("ts_micros", Int64),
            ],
            &["archive_id"],
        )?;
    }
    if !shape_gate(have, T_MEMBERS, &MEMBERS_SHAPE)? {
        crate::rretl::create_bookkeeping(
            s,
            T_MEMBERS,
            vec![
                crate::rretl::spec_col("archive_id", Int64),
                crate::rretl::spec_col("member_no", Int64),
                crate::rretl::spec_col("name", Blob),
                crate::rretl::spec_col("data", Blob),
                crate::rretl::spec_col("method", Int64),
            ],
            &["archive_id", "member_no"],
        )?;
    }
    Ok(())
}

fn as_blob(v: &Value) -> Result<Vec<u8>> {
    match v {
        Value::Blob(b) => Ok(b.clone()),
        other => Err(Error::Corrupt(format!("rretl store: expected blob, got {other:?}"))),
    }
}

fn hash_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[allow(clippy::too_many_arguments)]
fn builtin_lineage(
    run_id: i64,
    lens: &str,
    obj: &str,
    table: &'static str,
    content_hash: &str,
    rows: i64,
    // Never "applied": revert/putback/stacking all key on that outcome, and
    // neither a version put nor a prune is a column run they could unwind.
    outcome: &'static str,
    error: String,
) -> LineageRow {
    LineageRow {
        run_id,
        lens: lens.into(),
        // Engine-coded transforms have no ProcHash triple; the envelope
        // kind+version IS the code identity (§7, amended by the adversarial
        // check's finding 7). Empty hashes stay the failed-run convention.
        forward_hash: lens.into(),
        rex_hash: String::new(),
        inverse_hash: lens.into(),
        table: table.into(),
        column: obj.into(),
        source_hash: content_hash.into(),
        output_hash: content_hash.into(),
        residual_hash: String::new(),
        rows,
        outcome,
        error,
    }
}

impl crate::Database {
    /// Store `bytes` as the next version of `obj`. The new version is stored
    /// FULL; the previous newest is rewritten as a reverse delta against it —
    /// verified AS PERSISTED, byte-identically, before the commit — unless it
    /// is a K-th anchor (kept full) or the delta would not be smaller (kept
    /// full; ingest is never hostage to compression). Returns the version.
    pub fn rretl_put_version(&self, obj: &str, bytes: &[u8]) -> Result<i64> {
        if bytes.len() > crate::rretl_codec::MAX_PAYLOAD {
            return Err(Error::Unsupported(format!(
                "{} bytes exceeds the {}-byte envelope cap",
                bytes.len(),
                crate::rretl_codec::MAX_PAYLOAD
            )));
        }
        let have = self.committed_tables()?;
        let mut s = self.begin()?;
        let out = put_version_in(&mut s, &have, obj, bytes);
        match out {
            Ok(ver) => {
                s.commit()?;
                Ok(ver)
            }
            Err(e) => {
                s.rollback();
                Err(e)
            }
        }
    }

    /// Delete the OLDEST versions of `obj`, keeping the newest `keep`.
    /// Returns how many were deleted. Chain-safe by construction (deltas
    /// base upward; see `prune_in`); recorded as lineage outcome `pruned`.
    /// `keep = 0` is refused — keeping nothing is a drop, not a prune.
    pub fn rretl_prune_versions(&self, obj: &str, keep: u64) -> Result<u64> {
        if keep == 0 {
            return Err(Error::Unsupported(
                "keeping ZERO versions is a drop, not a prune — refused; if you mean \
                 it, delete the rows from rretl_versions yourself"
                    .into(),
            ));
        }
        let have = self.committed_tables()?;
        let mut s = self.begin()?;
        let out = prune_in(&mut s, &have, obj, keep);
        match out {
            Ok(n) => {
                s.commit()?;
                Ok(n)
            }
            Err(e) => {
                s.rollback();
                Err(e)
            }
        }
    }

    /// Materialize version `ver` of `obj`: walk down from the nearest full,
    /// applying reverse deltas, blake3-verifying EVERY intermediate against
    /// its recorded hash — a hash mismatch anywhere is a hard error naming
    /// the version, never silently wrong bytes.
    pub fn rretl_get_version(&self, obj: &str, ver: i64) -> Result<Vec<u8>> {
        // The nearest full at-or-above `ver` sits at the next FULL_EVERY
        // anchor or at the newest version, whichever comes first — both are
        // full BY INVARIANT (anchors are never rewritten; only the newest is,
        // when it stops being newest). So the fetch is bounded to that window
        // (≤ FULL_EVERY rows) instead of materialising the whole tail of a
        // long history. A corrupted anchor inside the window is an error
        // either way: any later full's delta walk would cross it too.
        let maxv = rows_of(self.query(
            "SELECT max(ver) FROM rretl_versions WHERE obj = $1",
            &[Value::Text(obj.into())],
        )?)?;
        let maxv = match maxv.first().and_then(|r| r.first()) {
            Some(Value::Int(m)) => *m,
            _ => {
                return Err(Error::Unsupported(format!(
                    "no version {ver} of `{obj}` — `rretl versions` lists what exists"
                )))
            }
        };
        let anchor = ver + (FULL_EVERY - ver.rem_euclid(FULL_EVERY)) % FULL_EVERY;
        let bound = anchor.min(maxv);
        let rows = rows_of(self.query(
            "SELECT ver, payload, content_hash FROM rretl_versions WHERE obj = $1 \
             AND ver >= $2 AND ver <= $3 ORDER BY ver",
            &[Value::Text(obj.into()), Value::Int(ver), Value::Int(bound)],
        )?)?;
        if rows.is_empty() || as_int(&rows[0][0])? != ver {
            return Err(Error::Unsupported(format!("no version {ver} of `{obj}`")));
        }
        // Find the nearest full at-or-above `ver`.
        let mut anchor_idx = None;
        for (i, r) in rows.iter().enumerate() {
            let payload = as_blob(&r[1])?;
            let (kind, _) = open_envelope(&payload)?;
            if kind == Kind::Raw {
                anchor_idx = Some(i);
                break;
            }
        }
        let Some(ai) = anchor_idx else {
            return Err(Error::Corrupt(format!(
                "no full version at or above {ver} of `{obj}` — the chain is broken \
                 (was an anchor deleted?)"
            )));
        };
        let a_payload = as_blob(&rows[ai][1])?;
        let (_, full) = open_envelope(&a_payload)?;
        let mut bytes = full.to_vec();
        let a_ver = as_int(&rows[ai][0])?;
        if hash_hex(&bytes) != as_text(&rows[ai][2]) {
            return Err(Error::Corrupt(format!(
                "version {a_ver} of `{obj}` does not match its recorded hash — \
                 corruption at rest, refusing to build on it"
            )));
        }
        for i in (0..ai).rev() {
            let w = as_int(&rows[i][0])?;
            let payload = as_blob(&rows[i][1])?;
            let (kind, delta) = open_envelope(&payload)?;
            if kind != Kind::DeltaV1 {
                return Err(Error::Corrupt(format!(
                    "version {w} of `{obj}` is neither full nor delta-v1"
                )));
            }
            bytes = delta_apply(&bytes, delta)?;
            if hash_hex(&bytes) != as_text(&rows[i][2]) {
                return Err(Error::Corrupt(format!(
                    "version {w} of `{obj}` reconstructs to the WRONG bytes — its \
                     recorded hash disagrees; refusing"
                )));
            }
        }
        Ok(bytes)
    }

    /// Every version of `obj`, oldest first, with how each is stored.
    pub fn rretl_versions(&self, obj: &str) -> Result<Vec<VersionInfo>> {
        let bundle = self.engine.schema();
        if !bundle.schema.tables.iter().any(|t| t.name == T_VERSIONS && !t.dead) {
            return Ok(Vec::new());
        }
        let rows = rows_of(self.query(
            "SELECT ver, payload, content_hash FROM rretl_versions WHERE obj = $1 ORDER BY ver",
            &[Value::Text(obj.into())],
        )?)?;
        rows.into_iter()
            .map(|r| {
                let payload = as_blob(&r[1])?;
                let (kind, body) = open_envelope(&payload)?;
                Ok(VersionInfo {
                    ver: as_int(&r[0])?,
                    stored_as: if kind == Kind::Raw { "full" } else { "delta" },
                    bytes: body.len() as u64,
                    content_hash: as_text(&r[2]),
                })
            })
            .collect()
    }

    /// Ingest a zip archive by SPLICE: members become queryable rows, the
    /// residual keeps every non-data byte, and the reconstruction is verified
    /// byte-identically — from the PERSISTED rows, inside the ingest
    /// transaction — before the commit. Returns the archive id.
    pub fn rretl_pack_in(&self, name: &str, file: &[u8]) -> Result<i64> {
        let have = self.committed_tables()?;
        let mut s = self.begin()?;
        let out = pack_in_in(&mut s, &have, name, file);
        match out {
            Ok(id) => {
                s.commit()?;
                Ok(id)
            }
            Err(e) => {
                s.rollback();
                Err(e)
            }
        }
    }

    /// Reconstruct archive `archive_id` byte-identically, hash-gated against
    /// the stored original hash — a mismatch is "changed outside the
    /// pipeline", named, never silently wrong bytes.
    pub fn rretl_pack_out(&self, archive_id: i64) -> Result<Vec<u8>> {
        let arch = rows_of(self.query(
            "SELECT residual, content_hash FROM rretl_archives WHERE archive_id = $1",
            &[Value::Int(archive_id)],
        )?)?;
        let Some(arch) = arch.into_iter().next() else {
            return Err(Error::Unsupported(format!("no archive {archive_id}")));
        };
        let residual = as_blob(&arch[0])?;
        let want_hash = as_text(&arch[1]);
        let (kind, payload) = open_envelope(&residual)?;
        if kind != Kind::ZipSpliceV1 {
            return Err(Error::Corrupt(format!(
                "archive {archive_id}'s residual is not zip-splice-v1"
            )));
        }
        let file = zip_join(payload, &|member_no| {
            let d = rows_of(self.query(
                "SELECT data FROM rretl_archive_members WHERE archive_id = $1 \
                 AND member_no = $2",
                &[Value::Int(archive_id), Value::Int(member_no as i64)],
            )?)?;
            match d.into_iter().next() {
                Some(mut row) if !row.is_empty() => as_blob(&row.remove(0)),
                _ => Err(Error::Corrupt(format!(
                    "archive {archive_id}: member {member_no}'s data row is MISSING — \
                     the archive cannot be reconstructed"
                ))),
            }
        })?;
        if hash_hex(&file) != want_hash {
            return Err(Error::Corrupt(format!(
                "archive {archive_id} reconstructs to the WRONG bytes — a member or the \
                 residual changed outside the pipeline; refusing to hand back a fake"
            )));
        }
        Ok(file)
    }

    /// Every archive, oldest first.
    pub fn rretl_archives(&self) -> Result<Vec<ArchiveInfo>> {
        let bundle = self.engine.schema();
        if !bundle.schema.tables.iter().any(|t| t.name == T_ARCHIVES && !t.dead) {
            return Ok(Vec::new());
        }
        let rows = rows_of(self.query(
            "SELECT a.archive_id, a.name, a.content_hash, \
             (SELECT count(*) FROM rretl_archive_members m \
              WHERE m.archive_id = a.archive_id) \
             FROM rretl_archives a ORDER BY a.archive_id",
            &[],
        )?)?;
        rows.into_iter()
            .map(|r| {
                Ok(ArchiveInfo {
                    archive_id: as_int(&r[0])?,
                    name: as_text(&r[1]),
                    content_hash: as_text(&r[2]),
                    members: as_int(&r[3])?,
                })
            })
            .collect()
    }
}

fn put_version_in(
    s: &mut WriteSession<'_>,
    have: &[(String, Vec<String>)],
    obj: &str,
    bytes: &[u8],
) -> Result<i64> {
    ensure_version_table(s, have)?;
    crate::rretl::ensure_lineage_tables(s, have)?;

    let cur = rows_of(s.query(
        "SELECT max(ver) FROM rretl_versions WHERE obj = $1",
        &[Value::Text(obj.into())],
    )?)?;
    let cur_ver = match cur.first().and_then(|r| r.first()) {
        Some(Value::Int(v)) => Some(*v),
        _ => None,
    };
    let new_ver = cur_ver.map(|v| v + 1).unwrap_or(1);
    let content_hash = hash_hex(bytes);

    s.query(
        "INSERT INTO rretl_versions (obj, ver, payload, content_hash, ts_micros) \
         VALUES ($1, $2, $3, $4, $5)",
        &[
            Value::Text(obj.into()),
            Value::Int(new_ver),
            Value::Blob(envelope(Kind::Raw, bytes)?),
            Value::Text(content_hash.clone()),
            Value::Int(now_micros()),
        ],
    )?;

    let mut note = String::new();
    if let Some(cv) = cur_ver {
        if cv % FULL_EVERY != 0 {
            note = rewrite_as_delta(s, obj, cv, new_ver, bytes)?;
        }
    }

    let run_id = next_run_id(s)?;
    builtin_lineage(
        run_id,
        "builtin:delta-v1",
        obj,
        T_VERSIONS,
        &content_hash,
        1,
        "versioned",
        note,
    )
    .insert(s)?;
    Ok(new_ver)
}

/// Delete the OLDEST versions of `obj`, keeping the newest `keep` — the
/// retention story "nothing is ever deleted" deliberately left open. It is
/// chain-safe BY CONSTRUCTION: every delta bases on the version ABOVE it and
/// `retl_get_version` walks downward from an anchor at or above the target,
/// so deleting a prefix of history can never orphan anything that remains.
/// (The dangerous prune — an anchor from the MIDDLE — stays impossible: only
/// a contiguous oldest-first prefix ever goes.) The prune itself is
/// first-class lineage (outcome `pruned`, the deleted range in the note).
fn prune_in(
    s: &mut WriteSession<'_>,
    have: &[(String, Vec<String>)],
    obj: &str,
    keep: u64,
) -> Result<u64> {
    if !shape_gate(have, T_VERSIONS, &VERSIONS_SHAPE)? {
        return Err(Error::Unsupported(format!(
            "no versions of `{obj}` — nothing to prune"
        )));
    }
    crate::rretl::ensure_lineage_tables(s, have)?;
    let bounds = rows_of(s.query(
        "SELECT min(ver), max(ver) FROM rretl_versions WHERE obj = $1",
        &[Value::Text(obj.into())],
    )?)?;
    let (minv, maxv) = match bounds.first().map(|r| (&r[0], &r[1])) {
        Some((Value::Int(lo), Value::Int(hi))) => (*lo, *hi),
        _ => {
            return Err(Error::Unsupported(format!(
                "no versions of `{obj}` — nothing to prune"
            )))
        }
    };
    let cutoff = maxv.saturating_sub(keep as i64);
    if cutoff < minv {
        return Ok(0);
    }
    s.query(
        "DELETE FROM rretl_versions WHERE obj = $1 AND ver <= $2",
        &[Value::Text(obj.into()), Value::Int(cutoff)],
    )?;
    let pruned = (cutoff - minv + 1) as u64;
    let run_id = next_run_id(s)?;
    builtin_lineage(
        run_id,
        "builtin:prune",
        obj,
        T_VERSIONS,
        "",
        pruned as i64,
        "pruned",
        format!("versions {minv}..={cutoff} deleted, newest {keep} kept"),
    )
    .insert(s)?;
    Ok(pruned)
}

/// Rewrite the previous newest (`cv`) as a reverse delta against the bytes
/// just stored at `cv + 1`. Two independent failure disciplines
/// (adversarial-check findings 12 and 13):
///
/// - The old full must match its RECORDED hash before it is replaced — a
///   rotted full that "verifies" against its own rot would launder the
///   corruption into a clean-looking chain and delete the last good copy.
///   HARD error; the whole put aborts, because the data was already bad and
///   proceeding destroys the evidence.
/// - A delta that fails its own persisted-round-trip verification, or is not
///   smaller than the full, keeps the FULL instead and the put still
///   succeeds — ingest is never hostage to compression.
fn rewrite_as_delta(
    s: &mut WriteSession<'_>,
    obj: &str,
    cv: i64,
    new_ver: i64,
    new_bytes: &[u8],
) -> Result<String> {
    let old = rows_of(s.query(
        "SELECT payload, content_hash FROM rretl_versions WHERE obj = $1 AND ver = $2",
        &[Value::Text(obj.into()), Value::Int(cv)],
    )?)?;
    let old = old.into_iter().next().ok_or_else(|| {
        Error::Corrupt(format!("version {cv} of `{obj}` vanished mid-transaction"))
    })?;
    let old_payload = as_blob(&old[0])?;
    let (kind, old_bytes) = open_envelope(&old_payload)?;
    if kind != Kind::Raw {
        // Only the newest is ever rewritten, and the newest is always full;
        // anything else means the invariant is already broken — leave it be.
        return Ok(format!("version {cv} was not stored full; left untouched"));
    }
    if hash_hex(old_bytes) != as_text(&old[1]) {
        return Err(Error::Corrupt(format!(
            "version {cv} of `{obj}` does not match its recorded hash — corruption at \
             rest; rewriting it as a delta would LAUNDER the corruption and delete the \
             last full copy, so the whole put is refused"
        )));
    }
    let old_bytes = old_bytes.to_vec();

    let Some(delta) = delta_encode(new_bytes, &old_bytes) else {
        return Ok(format!(
            "version {cv} kept full: the delta would not be smaller"
        ));
    };
    let delta_env = envelope(Kind::DeltaV1, &delta)?;
    if delta_env.len() >= old_payload.len() {
        return Ok(format!(
            "version {cv} kept full: the delta would not be smaller"
        ));
    }
    s.query(
        "UPDATE rretl_versions SET payload = $1 WHERE obj = $2 AND ver = $3",
        &[Value::Blob(delta_env), Value::Text(obj.into()), Value::Int(cv)],
    )?;

    // Verify AS PERSISTED (finding 14): re-read BOTH rows inside this
    // transaction and reconstruct — the row codec is part of what is being
    // trusted, so it is part of what is verified.
    let mut reread = |ver: i64| -> Result<Vec<u8>> {
        let r = rows_of(s.query(
            "SELECT payload FROM rretl_versions WHERE obj = $1 AND ver = $2",
            &[Value::Text(obj.into()), Value::Int(ver)],
        )?)?;
        as_blob(&r[0][0])
    };
    let new_persisted = reread(new_ver)?;
    let delta_persisted = reread(cv)?;
    let (_, new_full) = open_envelope(&new_persisted)?;
    let (_, d) = open_envelope(&delta_persisted)?;
    let back = delta_apply(new_full, d)?;
    if back != old_bytes {
        // Keep the full: put back the original payload and carry on.
        s.query(
            "UPDATE rretl_versions SET payload = $1 WHERE obj = $2 AND ver = $3",
            &[Value::Blob(old_payload), Value::Text(obj.into()), Value::Int(cv)],
        )?;
        return Ok(format!(
            "version {cv} kept full: the persisted delta did not reconstruct \
             byte-identically (encoder bug — bloat, not corruption, by design)"
        ));
    }
    Ok(String::new())
}

fn pack_in_in(
    s: &mut WriteSession<'_>,
    have: &[(String, Vec<String>)],
    name: &str,
    file: &[u8],
) -> Result<i64> {
    ensure_archive_tables(s, have)?;
    crate::rretl::ensure_lineage_tables(s, have)?;

    let parts = zip_split(file)?;
    let content_hash = hash_hex(file);

    let prev = rows_of(s.query("SELECT max(archive_id) FROM rretl_archives", &[])?)?;
    let archive_id = match prev.first().and_then(|r| r.first()) {
        Some(Value::Int(m)) => m + 1,
        _ => 1,
    };
    s.query(
        "INSERT INTO rretl_archives (archive_id, name, residual, content_hash, ts_micros) \
         VALUES ($1, $2, $3, $4, $5)",
        &[
            Value::Int(archive_id),
            Value::Text(name.into()),
            Value::Blob(envelope(Kind::ZipSpliceV1, &parts.residual_payload)?),
            Value::Text(content_hash.clone()),
            Value::Int(now_micros()),
        ],
    )?;
    let n_members = parts.members.len() as i64;
    for m in &parts.members {
        s.query(
            "INSERT INTO rretl_archive_members (archive_id, member_no, name, data, method) \
             VALUES ($1, $2, $3, $4, $5)",
            &[
                Value::Int(archive_id),
                Value::Int(m.member_no as i64),
                Value::Blob(m.name.clone()),
                Value::Blob(file[m.offset as usize..(m.offset + m.len) as usize].to_vec()),
                Value::Int(m.method as i64),
            ],
        )?;
    }

    // Verify AS PERSISTED, before the commit: re-read the residual and every
    // member row inside this transaction, re-splice, and byte-compare against
    // the original. "By construction" is an argument; this is the check.
    let stored = rows_of(s.query(
        "SELECT residual FROM rretl_archives WHERE archive_id = $1",
        &[Value::Int(archive_id)],
    )?)?;
    let residual = as_blob(&stored[0][0])?;
    let (_, payload) = open_envelope(&residual)?;
    let mut member_data = std::collections::HashMap::new();
    let mrows = rows_of(s.query(
        "SELECT member_no, data FROM rretl_archive_members WHERE archive_id = $1",
        &[Value::Int(archive_id)],
    )?)?;
    for r in mrows {
        member_data.insert(as_int(&r[0])? as u32, as_blob(&r[1])?);
    }
    let back = zip_join(payload, &|no| {
        member_data
            .get(&no)
            .cloned()
            .ok_or_else(|| Error::Corrupt(format!("member {no} not persisted")))
    })?;
    if back != file {
        return Err(Error::Corrupt(format!(
            "archive `{name}`: the PERSISTED rows do not re-splice to the original — \
             aborting the ingest before anything commits"
        )));
    }

    let run_id = next_run_id(s)?;
    LineageRow {
        run_id,
        lens: "builtin:zip-splice-v1".into(),
        forward_hash: "builtin:zip-splice-v1".into(),
        rex_hash: String::new(),
        inverse_hash: "builtin:zip-splice-v1".into(),
        table: T_ARCHIVES.into(),
        column: name.into(),
        source_hash: content_hash.clone(),
        output_hash: content_hash,
        residual_hash: String::new(),
        rows: n_members,
        outcome: "packed",
        error: String::new(),
    }
    .insert(s)?;
    Ok(archive_id)
}

/// fsck extension for the stage-3 stores: re-materialize EVERY version of
/// every object and re-splice every archive, against their recorded hashes —
/// the read paths already hash-verify, so fsck just drives them and collects
/// the refusals as findings.
pub(crate) fn fsck_stores(db: &crate::Database, findings: &mut Vec<String>) -> Result<()> {
    let bundle = db.engine.schema();
    let has = |n: &str| bundle.schema.tables.iter().any(|t| t.name == n && !t.dead);
    if has(T_VERSIONS) {
        let objs = rows_of(db.query(
            "SELECT DISTINCT obj FROM rretl_versions ORDER BY obj",
            &[],
        )?)?;
        for o in objs {
            let obj = as_text(&o[0]);
            let vers = rows_of(db.query(
                "SELECT ver FROM rretl_versions WHERE obj = $1 ORDER BY ver",
                &[Value::Text(obj.clone())],
            )?)?;
            for v in vers {
                let ver = as_int(&v[0])?;
                if let Err(e) = db.rretl_get_version(&obj, ver) {
                    findings.push(format!("version {ver} of `{obj}`: {e}"));
                }
            }
        }
    }
    if has(T_ARCHIVES) {
        let ids = rows_of(db.query(
            "SELECT archive_id FROM rretl_archives ORDER BY archive_id",
            &[],
        )?)?;
        for r in ids {
            let id = as_int(&r[0])?;
            if let Err(e) = db.rretl_pack_out(id) {
                findings.push(format!("archive {id}: {e}"));
            }
        }
    }
    Ok(())
}
