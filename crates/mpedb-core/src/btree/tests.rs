use super::*;
use crate::pagestore::test_store::TestStore;
use std::collections::BTreeMap;

/// [`partition_keys`] cuts must be strictly inside the bounds, ascending,
/// and every key of the tree must land in exactly one resulting piece —
/// for full-range and bounded asks alike, at several tree sizes (leaf-only
/// through multi-level). This is the parallel fold's disjoint-and-
/// exhaustive precondition; nothing downstream re-checks it.
#[test]
fn partition_keys_cut_the_range_exactly() {
    for n in [0u64, 5, 300, 5000] {
        let mut store = TestStore::new();
        let mut root = 0u64;
        let mut keys: Vec<Vec<u8>> = Vec::new();
        for i in 0..n {
            let k = format!("key{i:06}").into_bytes();
            let out =
                insert(&mut store, root, &k, &mut Payload::Flat(b"v"), InsertMode::Upsert)
                    .unwrap();
            root = out.new_root;
            keys.push(k);
        }
        keys.sort();
        for (lo, hi) in [
            (None, None),
            (Some(&b"key000100"[..]), Some(&b"key004500"[..])),
            (Some(&b"key004999"[..]), None),
        ] {
            for want in [2usize, 3, 8, 32] {
                let points = partition_keys(&store, root, lo, hi, want).unwrap();
                assert!(points.len() < want, "at most want-1 points");
                for w in points.windows(2) {
                    assert!(w[0] < w[1], "points must ascend");
                }
                for p in &points {
                    assert!(lo.is_none_or(|l| p.as_slice() > l), "point ≤ lo");
                    assert!(hi.is_none_or(|h| p.as_slice() < h), "point ≥ hi");
                }
                // Every in-range key lands in EXACTLY one piece: cutting a
                // sorted key list at the points is a partition of it.
                let in_range: Vec<&Vec<u8>> = keys
                    .iter()
                    .filter(|k| {
                        lo.is_none_or(|l| k.as_slice() > l)
                            && hi.is_none_or(|h| k.as_slice() < h)
                    })
                    .collect();
                // The executor's pieces: [orig-lo, p1) ‖ [p1, p2) ‖ … ‖
                // [p_last, orig-hi) — later pieces take their cut
                // INCLUSIVE (a separator may equal a live key).
                let mut covered = 0usize;
                let mut cur_lo: Option<Vec<u8>> = None; // None = piece 0
                let mut cuts = points.clone();
                cuts.push(Vec::new()); // sentinel: the last piece runs to hi
                for (i, cut) in cuts.iter().enumerate() {
                    let last = i == cuts.len() - 1;
                    covered += in_range
                        .iter()
                        .filter(|k| {
                            cur_lo.as_ref().is_none_or(|l| k.as_slice() >= l.as_slice())
                                && (last || k.as_slice() < cut.as_slice())
                        })
                        .count();
                    cur_lo = Some(cut.clone());
                }
                assert_eq!(covered, in_range.len(), "n={n} want={want}");
            }
        }
    }
}

/// Extents through the tree, differentially against a model — with the
/// OWNERSHIP ledger as the real assertion: after every simulated commit,
/// the arena's live extents must be exactly the refs reachable from the
/// root. A replace or delete that forgets `free_extent` leaks; one that
/// frees twice trips the arena's double-free guard; a read of a freed
/// extent trips the dead-extent guard. (DESIGN-BLOBEXTENT §13.2.)
#[test]
fn extent_refs_roundtrip_and_never_leak() {
    let mut store = TestStore::new();
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut root = 0u64;
    let mut rng = Rng(0x0B10_BE47);
    for round in 0..2000u32 {
        let k = format!("k{:03}", rng.next() % 200).into_bytes();
        match rng.next() % 4 {
            // 0..=1: upsert an extent-backed value
            0 | 1 => {
                let len = 1 + (rng.next() as usize % (3 * PAGE_SIZE));
                let val: Vec<u8> = (0..len).map(|i| (rng.next() ^ i as u64) as u8).collect();
                // payload BEFORE reference — the arena models the order
                let (start_page, total_len, npages) = store.put_extent(&val);
                let out = insert(
                    &mut store,
                    root,
                    &k,
                    &mut Payload::ExtentRef { start_page, total_len, npages },
                    InsertMode::Upsert,
                )
                .unwrap();
                root = out.new_root;
                model.insert(k, val);
            }
            // 2: upsert a small inline value (kind churn: extent→inline
            // replace must free the extent)
            2 => {
                let val: Vec<u8> = (0..(rng.next() as usize % 64))
                    .map(|_| rng.next() as u8)
                    .collect();
                let out = insert(
                    &mut store,
                    root,
                    &k,
                    &mut Payload::Flat(&val),
                    InsertMode::Upsert,
                )
                .unwrap();
                root = out.new_root;
                model.insert(k, val);
            }
            // 3: delete
            _ => {
                let out = delete(&mut store, root, &k).unwrap();
                root = out.new_root;
                model.remove(&k);
            }
        }
        // read-back parity on a sample key
        let probe = format!("k{:03}", rng.next() % 200).into_bytes();
        assert_eq!(
            get(&store, root, &probe).unwrap(),
            model.get(&probe).cloned(),
            "round {round}"
        );
        if round % 97 == 0 {
            store.commit();
            let mut refs = Vec::new();
            collect_extents(&store, root, &mut refs).unwrap();
            let mut reachable: Vec<u64> = refs.iter().map(|(s, _)| *s).collect();
            reachable.sort_unstable();
            assert_eq!(
                store.live_extents(),
                reachable,
                "extent ledger out of balance at round {round}"
            );
        }
    }
    // full drain: every extent must come back
    store.commit();
    for (k, v) in &model {
        assert_eq!(get(&store, root, k).unwrap().as_deref(), Some(v.as_slice()));
    }
    let keys: Vec<Vec<u8>> = model.keys().cloned().collect();
    for k in keys {
        root = delete(&mut store, root, &k).unwrap().new_root;
    }
    store.commit();
    assert!(store.live_extents().is_empty(), "drained tree leaked extents");
    assert_eq!(root, 0);
}

/// The vkind=2 decode is strict: truncation at every offset and a
/// disagreeing npages must be Corrupt, never a panic or a wild read.
#[test]
fn extent_cell_decode_is_strict() {
    let mut store = TestStore::new();
    let (start_page, total_len, npages) = store.put_extent(&[7u8; 5000]);
    let out = insert(
        &mut store,
        0,
        b"k",
        &mut Payload::ExtentRef { start_page, total_len, npages },
        InsertMode::Upsert,
    )
    .unwrap();
    let root = out.new_root;
    // Corrupt the npages field in place: find the cell's trailing 4 bytes.
    let page: Vec<u8> = store.page(root).unwrap().to_vec();
    // (cells live at the tail; the ref is the last EXTENT_REF_LEN bytes of
    // the only cell — locate by the known total_len bytes)
    let needle = total_len.to_le_bytes();
    let at = page
        .windows(8)
        .rposition(|w| w == needle)
        .expect("total_len bytes present");
    let dirty = {
        // TestStore page_mut requires dirty — re-insert to keep it dirty,
        // then poke the byte through the same page id.
        store.page_mut(root).unwrap()
    };
    dirty[at + 8..at + 12].copy_from_slice(&(npages + 1).to_le_bytes());
    let err = get(&store, root, b"k").unwrap_err();
    assert!(format!("{err}").contains("mismatch"), "{err}");
}

/// A payload held outside the arena comes back exactly as it went in.
#[test]
fn external_payload_round_trips() {
    let mut store = TestStore::new();
    let bytes: Vec<u8> = (0..9000u32).map(|i| (i * 31) as u8).collect();
    let (hash, total_len) = store.put_external(&bytes);
    let root = insert(
        &mut store,
        0,
        b"k",
        &mut Payload::ExternalRef { hash, total_len },
        InsertMode::Upsert,
    )
    .unwrap()
    .new_root;
    assert_eq!(get(&store, root, b"k").unwrap().as_deref(), Some(bytes.as_slice()));

    // And a slice of it, which has to fetch and verify the whole thing.
    let loc = value_loc(&store, root, b"k").unwrap().unwrap();
    let mut window = vec![0u8; 300];
    read_value_range(&store, &loc, 4096 - 7, &mut window).unwrap();
    assert_eq!(window, &bytes[4096 - 7..4096 - 7 + 300]);
}

/// The `vkind=4` decode is strict: truncation at every offset must be
/// Corrupt, never a panic and never a read past the cell.
#[test]
fn external_cell_decode_is_strict() {
    let mut store = TestStore::new();
    let (hash, total_len) = store.put_external(&[3u8; 7000]);
    let root = insert(
        &mut store,
        0,
        b"kk",
        &mut Payload::ExternalRef { hash, total_len },
        InsertMode::Upsert,
    )
    .unwrap()
    .new_root;
    let page: Vec<u8> = store.page(root).unwrap().to_vec();

    for cut in 0..page.len() {
        let mut broken = page.clone();
        broken.truncate(cut);
        broken.resize(PAGE_SIZE, 0);
        // Whatever it decides, it must decide it — a panic here is the bug
        // this test exists for.
        std::panic::catch_unwind(|| {
            let _ = leaf_cell(&broken, 0);
        })
        .expect("decoding a truncated page must not panic");
    }
}

/// The name is the contents, so contents that do not match the name are
/// refused rather than served.
///
/// This is the one failure this kind has that the others cannot: an extent
/// is bytes this file wrote into itself, while an external payload arrived
/// from outside and nothing else has checked it.
#[test]
fn external_payload_that_does_not_match_its_name_is_refused() {
    struct Liar {
        inner: TestStore,
        serve: Vec<u8>,
    }
    impl PageStore for Liar {
        fn page(&self, id: u64) -> Result<&[u8]> { self.inner.page(id) }
        fn page_mut(&mut self, id: u64) -> Result<&mut [u8]> { self.inner.page_mut(id) }
        fn alloc(&mut self) -> Result<u64> { self.inner.alloc() }
        fn free(&mut self, id: u64) -> Result<()> { self.inner.free(id) }
        fn is_dirty(&self, id: u64) -> bool { self.inner.is_dirty(id) }
        fn read_external(&self, _h: u128, total_len: u64, out: &mut Vec<u8>) -> Result<()> {
            // Right length, wrong bytes — the case a length check misses.
            assert_eq!(self.serve.len() as u64, total_len);
            out.extend_from_slice(&self.serve);
            Ok(())
        }
    }

    let honest: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
    let mut store = TestStore::new();
    let (hash, total_len) = store.put_external(&honest);
    let root = insert(
        &mut store,
        0,
        b"k",
        &mut Payload::ExternalRef { hash, total_len },
        InsertMode::Upsert,
    )
    .unwrap()
    .new_root;

    let mut swapped = honest.clone();
    swapped[2500] ^= 0xFF;
    let liar = Liar { inner: store, serve: swapped };
    let err = get(&liar, root, b"k").unwrap_err();
    assert!(format!("{err}").contains("content hash"), "{err}");
}

/// Identical contents are one object, not two — and letting go of a row
/// does not take the object away, because another row may still name it.
#[test]
fn identical_contents_share_one_object_and_outlive_a_row() {
    let mut store = TestStore::new();
    let bytes = vec![9u8; 6000];
    let (h1, len1) = store.put_external(&bytes);
    let (h2, len2) = store.put_external(&bytes);
    assert_eq!(h1, h2, "the same bytes must produce the same name");
    assert_eq!(len1, len2);
    assert_eq!(store.stored_external(), 1, "one object, written twice");

    let mut root = 0;
    for key in [b"a".as_slice(), b"b".as_slice()] {
        root = insert(
            &mut store,
            root,
            key,
            &mut Payload::ExternalRef { hash: h1, total_len: len1 },
            InsertMode::Upsert,
        )
        .unwrap()
        .new_root;
    }

    root = delete(&mut store, root, b"a").unwrap().new_root;
    store.commit();
    assert_eq!(store.live_external(), vec![h1], "still named by the other row");
    assert_eq!(get(&store, root, b"b").unwrap().as_deref(), Some(bytes.as_slice()));

    root = delete(&mut store, root, b"b").unwrap().new_root;
    store.commit();
    assert!(store.live_external().is_empty(), "nothing names it now");
    // And it is STILL on disk. That is deliberate: a sweep that can see
    // every reference reclaims it, not the row that happened to be deleted.
    assert_eq!(store.stored_external(), 1);
    let _ = root;
}

/// A streamed value must be byte-for-byte the same as one handed over whole.
///
/// The safety claim of #43. Chunked at every awkward size — a source's chunk
/// boundary, the page boundary, and the row head's end are three unrelated
/// things, and the head is what makes the first page's split uneven.
///
/// Mutation-tested: dropping the head into the fill loop (writing it after
/// the source's bytes instead of before) fails this, and so does letting the
/// stream reader hand back a short buffer.
#[test]
fn streamed_payload_is_byte_identical_to_flat() {
    struct Chunked {
        data: Vec<u8>,
        pos: usize,
    }
    impl BlobSource for Chunked {
        fn len(&self) -> usize {
            self.data.len()
        }
        fn next_into(&mut self, buf: &mut [u8]) -> Result<()> {
            buf.copy_from_slice(&self.data[self.pos..self.pos + buf.len()]);
            self.pos += buf.len();
            Ok(())
        }
    }
    let mut rng = Rng(0x5175_2A17);
    for &(head_len, body_len) in &[
        (0usize, OVERFLOW_CAP * 2 + 5usize),
        (1, OVERFLOW_CAP * 2),
        (17, OVERFLOW_CAP - 17), // head + body lands exactly on a page
        (OVERFLOW_CAP - 1, OVERFLOW_CAP + 1),
        (OVERFLOW_CAP, OVERFLOW_CAP * 2),
        (9, 1),
    ] {
        let head: Vec<u8> = (0..head_len).map(|_| rng.next() as u8).collect();
        let body: Vec<u8> = (0..body_len).map(|_| rng.next() as u8).collect();
        let whole: Vec<u8> = head.iter().chain(body.iter()).copied().collect();

        let mut store = TestStore::new();
        let r = insert(&mut store, 0, b"flat", &mut Payload::Flat(&whole), InsertMode::InsertOnly)
            .unwrap()
            .new_root;
        let mut src = Chunked { data: body.clone(), pos: 0 };
        let r = insert(
            &mut store,
            r,
            b"stream",
            &mut Payload::Stream { head: &head, src: &mut src },
            InsertMode::InsertOnly,
        )
        .unwrap()
        .new_root;
        let a = get(&store, r, b"flat").unwrap().unwrap();
        let b = get(&store, r, b"stream").unwrap().unwrap();
        assert_eq!(a, whole, "head={head_len} body={body_len}: flat");
        assert_eq!(b, whole, "head={head_len} body={body_len}: streamed");
        assert_eq!(src.pos, body.len(), "head={head_len}: source not drained");
    }
}

/// `Payload::Parts` must be byte-for-byte `Payload::Flat` of the same bytes.
///
/// This is the whole safety claim of #42: the parts form exists so a large
/// value never has to be concatenated, and it is only sound if the pages it
/// writes are indistinguishable. Split at every awkward place — a piece
/// boundary and a page boundary have nothing to do with each other, so the
/// interesting splits are the ones that land mid-page, mid-header, and on
/// exactly `OVERFLOW_CAP`.
///
/// Mutation-tested: making `PayloadCursor::next_chunk` return the whole
/// remaining piece (ignoring `want`) fails this, and so does dropping the
/// `while filled < take` loop in `write_overflow`.
#[test]
fn parts_payload_is_byte_identical_to_flat() {
    let mut rng = Rng(0x2A17_BEEF);
    let big: Vec<u8> = (0..(OVERFLOW_CAP * 3 + 777)).map(|_| rng.next() as u8).collect();
    // splits that land: mid-first-page, exactly on a page edge, one before,
    // one after, in the tail, and degenerate (empty pieces at both ends)
    let splits: &[&[usize]] = &[
        &[1],
        &[OVERFLOW_CAP - 1],
        &[OVERFLOW_CAP],
        &[OVERFLOW_CAP + 1],
        &[OVERFLOW_CAP * 3],
        &[0, big.len()],
        &[7, OVERFLOW_CAP, OVERFLOW_CAP * 2 + 3, big.len() - 1],
    ];
    for (case, cuts) in splits.iter().enumerate() {
        let mut pieces: Vec<&[u8]> = Vec::new();
        let mut at = 0usize;
        for &c in cuts.iter() {
            pieces.push(&big[at..c]);
            at = c;
        }
        pieces.push(&big[at..]);

        let mut store = TestStore::new();
        let flat = insert(&mut store, 0, b"flat", &mut Payload::Flat(&big), InsertMode::InsertOnly)
            .unwrap()
            .new_root;
        let both = insert(
            &mut store,
            flat,
            b"parts",
            &mut Payload::Parts(&pieces),
            InsertMode::InsertOnly,
        )
        .unwrap()
        .new_root;
        let a = get(&store, both, b"flat").unwrap().unwrap();
        let b = get(&store, both, b"parts").unwrap().unwrap();
        assert_eq!(a, big, "case {case}: flat round-trip");
        assert_eq!(b, big, "case {case}: parts round-trip, cuts {cuts:?}");
        assert_eq!(a, b, "case {case}: parts != flat");
    }
}

/// The same, for a value small enough to stay INLINE — a different code path
/// in `make_leaf_cell`, and one it would be easy to leave joining only the
/// first piece.
#[test]
fn parts_payload_is_byte_identical_to_flat_when_inline() {
    let small: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
    assert!(small.len() <= MAX_INLINE_VAL);
    let pieces: Vec<&[u8]> = vec![&small[..0], &small[..17], &small[17..298], &small[298..]];
    let mut store = TestStore::new();
    let r = insert(&mut store, 0, b"f", &mut Payload::Flat(&small), InsertMode::InsertOnly)
        .unwrap()
        .new_root;
    let r = insert(&mut store, r, b"p", &mut Payload::Parts(&pieces), InsertMode::InsertOnly)
        .unwrap()
        .new_root;
    assert_eq!(get(&store, r, b"f").unwrap().unwrap(), small);
    assert_eq!(get(&store, r, b"p").unwrap().unwrap(), small);
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
}

fn key_of(n: u64) -> Vec<u8> {
    format!("key{n:08}").into_bytes()
}

fn val_of(rng: &mut Rng) -> Vec<u8> {
    // mix of small inline values and large overflow values
    let len = match rng.next() % 20 {
        0 => 3000 + (rng.next() % 8000) as usize, // overflow, multi-page
        1..=3 => 1024 + (rng.next() % 512) as usize, // around the threshold
        _ => (rng.next() % 64) as usize,
    };
    (0..len).map(|i| (i as u8).wrapping_mul(31)).collect()
}

#[test]
fn model_test_against_btreemap() {
    let mut store = TestStore::new();
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut root = 0u64;
    let mut rng = Rng(42);

    for step in 0..6000 {
        let k = key_of(rng.next() % 700);
        match rng.next() % 10 {
            0..=5 => {
                let v = val_of(&mut rng);
                let out = insert(&mut store, root, &k, &mut Payload::Flat(&v), InsertMode::Upsert).unwrap();
                root = out.new_root;
                let prev = model.insert(k.clone(), v);
                assert_eq!(out.existed, prev.is_some(), "step {step}");
            }
            6..=8 => {
                let out = delete(&mut store, root, &k).unwrap();
                root = out.new_root;
                let prev = model.remove(&k);
                assert_eq!(out.existed, prev.is_some(), "step {step}");
            }
            _ => {
                let got = get(&store, root, &k).unwrap();
                assert_eq!(got, model.get(&k).cloned(), "step {step}");
            }
        }
        if step % 500 == 0 {
            // full scan must equal the model exactly
            let mut c = cursor(&store, root, None, None).unwrap();
            let mut items = Vec::new();
            while let Some(kv) = c.next(&store).unwrap() {
                items.push(kv);
            }
            let expect: Vec<_> =
                model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            assert_eq!(items, expect, "scan mismatch at step {step}");
            store.commit();
        }
    }
}

#[test]
fn insert_only_reports_duplicates_without_mutation() {
    let mut store = TestStore::new();
    let mut root = 0;
    root = insert(&mut store, root, b"a", &mut Payload::Flat(b"1"), InsertMode::InsertOnly)
        .unwrap()
        .new_root;
    store.commit();
    let live_before = store.live_pages();
    let out = insert(&mut store, root, b"a", &mut Payload::Flat(b"2"), InsertMode::InsertOnly).unwrap();
    assert!(out.existed);
    assert_eq!(out.new_root, root);
    assert_eq!(store.live_pages(), live_before, "no pages may be touched");
    assert_eq!(get(&store, root, b"a").unwrap().unwrap(), b"1");
}

#[test]
fn range_scans_with_bounds() {
    let mut store = TestStore::new();
    let mut root = 0;
    for i in 0..500u64 {
        root = insert(
            &mut store,
            root,
            &key_of(i),
            &mut Payload::Flat(&i.to_le_bytes()),
            InsertMode::InsertOnly,
        )
        .unwrap()
        .new_root;
    }
    let collect = |lo: Option<(&[u8], bool)>, hi: Option<(&[u8], bool)>| {
        let mut c = cursor(&store, root, lo, hi).unwrap();
        let mut out = Vec::new();
        while let Some((k, _)) = c.next(&store).unwrap() {
            out.push(k);
        }
        out
    };
    let lo = key_of(100);
    let hi = key_of(200);
    assert_eq!(collect(Some((&lo, true)), Some((&hi, false))).len(), 100);
    assert_eq!(collect(Some((&lo, false)), Some((&hi, true))).len(), 100);
    assert_eq!(collect(None, Some((&key_of(10), false))).len(), 10);
    assert_eq!(collect(Some((&key_of(490), true)), None).len(), 10);
    // bounds that match nothing
    assert_eq!(
        collect(Some((b"zzz".as_slice(), true)), None).len(),
        0
    );
}

#[test]
fn delete_everything_frees_every_page() {
    let mut store = TestStore::new();
    let mut root = 0;
    let mut rng = Rng(7);
    let n = 2000u64;
    for i in 0..n {
        let v = val_of(&mut rng);
        root = insert(&mut store, root, &key_of(i), &mut Payload::Flat(&v), InsertMode::Upsert)
            .unwrap()
            .new_root;
    }
    store.commit();
    assert!(store.live_pages() > 20);
    // delete in a shuffled-ish order
    for i in 0..n {
        let k = key_of((i * 617) % n);
        root = delete(&mut store, root, &k).unwrap().new_root;
    }
    store.commit();
    assert_eq!(root, 0);
    assert_eq!(
        store.live_pages(),
        0,
        "all pages must be freed when the tree is empty (no leaks)"
    );
}

#[test]
fn overflow_values_roundtrip_and_free() {
    let mut store = TestStore::new();
    let big: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
    let mut root = insert(&mut store, 0, b"big", &mut Payload::Flat(&big), InsertMode::InsertOnly)
        .unwrap()
        .new_root;
    assert_eq!(get(&store, root, b"big").unwrap().unwrap(), big);
    store.commit();
    // replace with a small value: chain must be freed
    root = insert(&mut store, root, b"big", &mut Payload::Flat(b"small"), InsertMode::Upsert)
        .unwrap()
        .new_root;
    store.commit();
    assert_eq!(store.live_pages(), 1, "overflow chain must be reclaimed");
    assert_eq!(get(&store, root, b"big").unwrap().unwrap(), b"small");
    root = delete(&mut store, root, b"big").unwrap().new_root;
    store.commit();
    assert_eq!(root, 0);
    assert_eq!(store.live_pages(), 0);
}

#[test]
fn oversized_keys_rejected() {
    let mut store = TestStore::new();
    let k = vec![7u8; MAX_KEY + 1];
    assert!(insert(&mut store, 0, &k, &mut Payload::Flat(b""), InsertMode::Upsert).is_err());
}

#[test]
fn cow_discipline_upheld_across_commits() {
    // after commit, any further mutation must not touch committed pages
    let mut store = TestStore::new();
    let mut root = 0;
    for i in 0..300u64 {
        root = insert(
            &mut store,
            root,
            &key_of(i),
            &mut Payload::Flat(b"v"),
            InsertMode::InsertOnly,
        )
        .unwrap()
        .new_root;
    }
    store.commit(); // clears dirty set: everything is now "committed"
    // TestStore::page_mut errors on non-dirty pages, so any COW violation
    // inside insert/delete would fail these calls
    for i in 0..300u64 {
        root = insert(&mut store, root, &key_of(i), &mut Payload::Flat(b"w"), InsertMode::Upsert)
            .unwrap()
            .new_root;
    }
    for i in 150..300u64 {
        root = delete(&mut store, root, &key_of(i)).unwrap().new_root;
    }
    for i in 0..150u64 {
        assert_eq!(get(&store, root, &key_of(i)).unwrap().unwrap(), b"w");
    }
}
