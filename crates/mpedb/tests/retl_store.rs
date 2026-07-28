//! RETL stage 3, the storage half (#52 B2/B3): blob versioning by
//! reverse-delta chains, and zip round-trip by splice.
//!
//! The contract under test: every version ever put materializes to EXACTLY
//! its bytes (newest full, older ones as deltas, every K-th a full anchor);
//! a zip goes in as rows and comes back byte-identical; and every failure —
//! corruption at rest, tampering, incompressibility — is either a NAMED hard
//! error or a documented keep-the-full fallback, never silently wrong bytes.

use mpedb::{Config, Database, Value};

/// Removes the scratch file when the test ends, pass or panic — two full
/// suite runs left 1.4 GB in /dev/shm today, and tmpfs pressure is what
/// SIGBUS-kills linkers (#137).
struct Scratch(String);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn db(tag: &str) -> (Database, Scratch) {
    let path = format!(
        "{}/retl-store-{tag}-{}.mpedb",
        mpedb_testkit::scratch_base_str(),
        std::process::id()
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{path}"
size_mb = 32
max_readers = 8
durability = "none"
"#
    );
    (
        Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(),
        Scratch(path),
    )
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| (self.next() >> 32) as u8).collect()
    }
}

/// Twenty versions of an evolving document — each a small edit of the last,
/// so deltas genuinely compress. EVERY version must materialize to exactly
/// the bytes that were put, and the storage pattern must be: newest full,
/// K-th anchors full, everything else delta.
#[test]
fn twenty_versions_all_materialize_and_anchors_stay_full() {
    let (d, _scratch) = db("chain");
    let mut rng = Rng(0x52b2);
    let mut doc = rng.bytes(4096);
    let mut history = Vec::new();
    for _ in 0..20 {
        // Small edit: overwrite a run, append a little.
        let at = (rng.next() as usize) % (doc.len() - 64);
        let patch = rng.bytes(48);
        doc[at..at + 48].copy_from_slice(&patch);
        doc.extend_from_slice(&rng.bytes(16));
        let ver = d.retl_put_version("doc", &doc).unwrap();
        history.push((ver, doc.clone()));
    }
    assert_eq!(history.len(), 20);
    assert_eq!(history.last().unwrap().0, 20);

    for (ver, bytes) in &history {
        assert_eq!(
            &d.retl_get_version("doc", *ver).unwrap(),
            bytes,
            "version {ver} did not materialize byte-identically"
        );
    }

    let infos = d.retl_versions("doc").unwrap();
    assert_eq!(infos.len(), 20);
    for info in &infos {
        let want = if info.ver == 20 || info.ver % mpedb::retl_store::FULL_EVERY == 0 {
            "full"
        } else {
            "delta"
        };
        assert_eq!(
            info.stored_as, want,
            "version {} stored as {}, expected {want}",
            info.ver, info.stored_as
        );
    }

    // Every put left a lineage row, and none of them is unwindable.
    let log = d.retl_log().unwrap();
    let versioned: Vec<_> = log.iter().filter(|r| r.outcome == "versioned").collect();
    assert_eq!(versioned.len(), 20);
    let err = d.retl_revert(versioned[0].run_id).unwrap_err().to_string();
    assert!(err.contains("versioned"), "revert must name the outcome: {err}");

    assert!(d.retl_fsck().unwrap().is_empty());
}

/// Incompressible versions: completely fresh random bytes each time. The
/// delta cannot be smaller, so every version stays FULL — and the put still
/// succeeds, with the fallback recorded in lineage. Ingest is never hostage
/// to compression.
#[test]
fn incompressible_versions_keep_fulls_and_still_ingest() {
    let (d, _scratch) = db("bloat");
    let mut rng = Rng(0xb10a7);
    let mut history = Vec::new();
    for _ in 0..3 {
        let doc = rng.bytes(2048);
        let ver = d.retl_put_version("noise", &doc).unwrap();
        history.push((ver, doc));
    }
    for info in d.retl_versions("noise").unwrap() {
        assert_eq!(info.stored_as, "full", "version {} should have kept full", info.ver);
    }
    for (ver, bytes) in &history {
        assert_eq!(&d.retl_get_version("noise", *ver).unwrap(), bytes);
    }
    let noted: Vec<_> = d
        .retl_log()
        .unwrap()
        .into_iter()
        .filter(|r| r.outcome == "versioned" && r.error.contains("kept full"))
        .collect();
    assert_eq!(noted.len(), 2, "both rewrites should have recorded the fallback");
}

/// Corruption laundering (adversarial finding 12): if the stored full a put
/// is about to rewrite does not match its recorded hash, the put must HARD
/// error — rewriting would delete the last full copy of already-bad data.
#[test]
fn a_rotted_full_refuses_the_next_put_by_name() {
    let (d, _scratch) = db("launder");
    d.retl_put_version("doc", b"first version, pristine").unwrap();

    // Rot the stored payload directly (still a well-formed raw envelope, so
    // only the hash check can catch it — that is the point).
    let mut s = d.begin().unwrap();
    let mut rotted = vec![1u8]; // kind = raw
    rotted.extend_from_slice(&(4u32).to_le_bytes());
    rotted.extend_from_slice(b"rot!");
    s.query(
        "UPDATE retl_versions SET payload = $1 WHERE obj = $2 AND ver = $3",
        &[Value::Blob(rotted), Value::Text("doc".into()), Value::Int(1)],
    )
    .unwrap();
    s.commit().unwrap();

    let err = d.retl_put_version("doc", b"second version").unwrap_err().to_string();
    assert!(
        err.contains("version 1") && err.contains("hash"),
        "must name the rotted version and the hash mismatch: {err}"
    );
    // The failed put aborted whole: no version 2, and fsck names version 1.
    assert_eq!(d.retl_versions("doc").unwrap().len(), 1);
    let findings = d.retl_fsck().unwrap();
    assert!(
        findings.iter().any(|f| f.contains("version 1") && f.contains("doc")),
        "fsck must surface the rot: {findings:?}"
    );
}

/// Tampering with a DELTA in the middle of the chain: get must refuse with
/// the version named (the reconstruction disagrees with the recorded hash),
/// and fsck must find it — while versions above the tamper stay readable.
#[test]
fn a_tampered_delta_is_named_by_get_and_fsck() {
    let (d, _scratch) = db("tamper");
    let mut doc = b"the quick brown fox jumps over the lazy dog, at length, \
                    repeated enough that a delta is actually smaller than raw"
        .to_vec();
    d.retl_put_version("doc", &doc).unwrap();
    doc.extend_from_slice(b" -- v2");
    d.retl_put_version("doc", &doc).unwrap();
    doc.extend_from_slice(b" -- v3");
    d.retl_put_version("doc", &doc).unwrap();
    assert_eq!(d.retl_versions("doc").unwrap()[0].stored_as, "delta");

    // Corrupt version 1's delta INSERT bytes without breaking the framing:
    // flip one byte near the end of the stored payload.
    let mut s = d.begin().unwrap();
    let row = &mut s
        .query(
            "SELECT payload FROM retl_versions WHERE obj = $1 AND ver = $2",
            &[Value::Text("doc".into()), Value::Int(1)],
        )
        .unwrap();
    let payload = match row {
        mpedb::ExecResult::Rows { rows, .. } => match &rows[0][0] {
            Value::Blob(b) => b.clone(),
            other => panic!("expected blob, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    };
    let mut bent = payload.clone();
    *bent.last_mut().unwrap() ^= 0xff;
    s.query(
        "UPDATE retl_versions SET payload = $1 WHERE obj = $2 AND ver = $3",
        &[Value::Blob(bent), Value::Text("doc".into()), Value::Int(1)],
    )
    .unwrap();
    s.commit().unwrap();

    let err = d.retl_get_version("doc", 1).unwrap_err().to_string();
    assert!(err.contains("version 1") || err.contains("delta"), "named refusal: {err}");
    // Versions 2 and 3 do not depend on version 1's payload.
    assert!(d.retl_get_version("doc", 3).is_ok());
    assert!(d.retl_get_version("doc", 2).is_ok());
    let findings = d.retl_fsck().unwrap();
    assert_eq!(findings.len(), 1, "exactly the tampered version: {findings:?}");
    assert!(findings[0].contains("version 1"));
}

/// Unknown object / unknown version are named refusals, and two objects'
/// chains never interleave.
#[test]
fn versions_are_per_object_and_missing_ones_are_named() {
    let (d, _scratch) = db("objs");
    d.retl_put_version("a", b"alpha").unwrap();
    d.retl_put_version("b", b"beta").unwrap();
    d.retl_put_version("a", b"alpha two").unwrap();
    assert_eq!(d.retl_versions("a").unwrap().len(), 2);
    assert_eq!(d.retl_versions("b").unwrap().len(), 1);
    assert_eq!(d.retl_get_version("b", 1).unwrap(), b"beta");
    let err = d.retl_get_version("a", 3).unwrap_err().to_string();
    assert!(err.contains("no version 3"), "{err}");
    let err = d.retl_get_version("nope", 1).unwrap_err().to_string();
    assert!(err.contains("nope"), "{err}");
    assert_eq!(d.retl_versions("nope").unwrap().len(), 0);
}

// ---------------------------------------------------------------- archives

/// Minimal STORE-method zip builder — same shape as the codec's unit-test
/// fixture: LFH + data per member, then CD, then EOCD. CRCs are zero (we
/// never inflate, so only structure matters).
fn store_zip(members: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cd = Vec::new();
    let mut offsets = Vec::new();
    for (name, data) in members {
        offsets.push(out.len() as u32);
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&[20, 0, 0, 0, 0, 0]); // version, flags, method=store
        out.extend_from_slice(&[0; 8]); // dostime+dosdate+crc
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0, 0]); // extra len
        out.extend_from_slice(name);
        out.extend_from_slice(data);
    }
    let cd_start = out.len() as u32;
    for ((name, data), off) in members.iter().zip(&offsets) {
        cd.extend_from_slice(b"PK\x01\x02");
        cd.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0]); // made-by, needed, flags, method
        cd.extend_from_slice(&[0; 8]); // time+date+crc
        cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
        cd.extend_from_slice(&[0; 12]); // extra, comment, disk, int attrs, ext attrs
        cd.extend_from_slice(&off.to_le_bytes());
        cd.extend_from_slice(name);
    }
    out.extend_from_slice(&cd);
    let cd_len = cd.len() as u32;
    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&[0; 4]); // disk numbers
    out.extend_from_slice(&(members.len() as u16).to_le_bytes());
    out.extend_from_slice(&(members.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_len.to_le_bytes());
    out.extend_from_slice(&cd_start.to_le_bytes());
    out.extend_from_slice(&[0, 0]); // comment len
    out
}

/// Round trip through the DATABASE: members land as queryable rows, and
/// pack-out is byte-identical — including an SFX-style prefix, a non-UTF8
/// name, and an empty member.
#[test]
fn a_zip_round_trips_through_the_database() {
    let (d, _scratch) = db("zip");
    let inner = store_zip(&[
        (b"readme.txt", b"hello from inside the archive"),
        (&[0xC0, 0xFF, 0x80], b"non-utf8 name, binary data \x00\x01\x02"),
        (b"empty.bin", b""),
    ]);
    // SFX quirk: garbage prefix before the first LFH.
    let mut file = b"#!/bin/sh\necho self-extracting stub\n".to_vec();
    file.extend_from_slice(&inner);

    let id = d.retl_pack_in("bundle.zip", &file).unwrap();
    assert_eq!(d.retl_pack_out(id).unwrap(), file);

    // Members are ordinary rows.
    let n = match d
        .query(
            "SELECT count(*) FROM retl_archive_members WHERE archive_id = $1",
            &[Value::Int(id)],
        )
        .unwrap()
    {
        mpedb::ExecResult::Rows { rows, .. } => match rows[0][0] {
            Value::Int(i) => i,
            ref other => panic!("expected int, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    };
    assert_eq!(n, 3);

    let arches = d.retl_archives().unwrap();
    assert_eq!(arches.len(), 1);
    assert_eq!(arches[0].members, 3);
    assert_eq!(arches[0].name, "bundle.zip");

    // The ingest is lineage, and not unwindable as a column run.
    let packed: Vec<_> = d
        .retl_log()
        .unwrap()
        .into_iter()
        .filter(|r| r.outcome == "packed")
        .collect();
    assert_eq!(packed.len(), 1);
    let err = d.retl_revert(packed[0].run_id).unwrap_err().to_string();
    assert!(err.contains("packed"), "{err}");

    assert!(d.retl_fsck().unwrap().is_empty());
}

/// Edit-then-pack-out is the POINT of splice: change a member's data row
/// (same length, so offsets hold), and the reconstruction carries the edit —
/// but then the hash gate must refuse, because the bytes are no longer the
/// original. The escape hatch is deliberate: re-ingest, don't mutate.
#[test]
fn tampering_with_a_member_row_is_refused_by_name() {
    let (d, _scratch) = db("ziptamper");
    let file = store_zip(&[(b"a.txt", b"original contents")]);
    let id = d.retl_pack_in("a.zip", &file).unwrap();

    let mut s = d.begin().unwrap();
    s.query(
        "UPDATE retl_archive_members SET data = $1 WHERE archive_id = $2 AND member_no = $3",
        &[Value::Blob(b"EDITED contents!!".to_vec()), Value::Int(id), Value::Int(0)],
    )
    .unwrap();
    s.commit().unwrap();

    let err = d.retl_pack_out(id).unwrap_err().to_string();
    assert!(err.contains("WRONG bytes"), "named refusal: {err}");
    let findings = d.retl_fsck().unwrap();
    assert!(
        findings.iter().any(|f| f.contains(&format!("archive {id}"))),
        "fsck must surface it: {findings:?}"
    );
}

/// A deleted member row is a NAMED hole, not a silent shrink.
#[test]
fn a_missing_member_row_is_a_named_hole() {
    let (d, _scratch) = db("ziphole");
    let file = store_zip(&[(b"a.txt", b"one"), (b"b.txt", b"two")]);
    let id = d.retl_pack_in("ab.zip", &file).unwrap();
    let mut s = d.begin().unwrap();
    s.query(
        "DELETE FROM retl_archive_members WHERE archive_id = $1 AND member_no = $2",
        &[Value::Int(id), Value::Int(1)],
    )
    .unwrap();
    s.commit().unwrap();
    let err = d.retl_pack_out(id).unwrap_err().to_string();
    assert!(err.contains("member 1") && err.contains("MISSING"), "{err}");
}

/// The codec's named refusals surface through pack_in unchanged: a zip64
/// sentinel refuses ingest, and nothing is left behind.
#[test]
fn refused_zips_leave_nothing_behind() {
    let (d, _scratch) = db("zipnope");
    let mut file = store_zip(&[(b"a.txt", b"data")]);
    // Stamp the EOCD's entry counts to the zip64 sentinel.
    let eocd = file.len() - 22;
    file[eocd + 8] = 0xff;
    file[eocd + 9] = 0xff;
    file[eocd + 10] = 0xff;
    file[eocd + 11] = 0xff;
    let err = d.retl_pack_in("big.zip", &file).unwrap_err().to_string();
    assert!(err.contains("zip64"), "named: {err}");
    assert_eq!(d.retl_archives().unwrap().len(), 0);
    assert!(d.retl_log().unwrap().is_empty());
}

/// Versioning the SAME bytes that live in an archive member does not tangle
/// the two stores; both bookkeeping families coexist in one database.
#[test]
fn versions_and_archives_coexist() {
    let (d, _scratch) = db("coexist");
    let file = store_zip(&[(b"payload.bin", b"shared bytes")]);
    d.retl_put_version("f", &file).unwrap();
    let id = d.retl_pack_in("f.zip", &file).unwrap();
    assert_eq!(d.retl_get_version("f", 1).unwrap(), file);
    assert_eq!(d.retl_pack_out(id).unwrap(), file);
    assert!(d.retl_fsck().unwrap().is_empty());
}
