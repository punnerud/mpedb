# mpedbfs — the database as paths (#54)

**Status: v1 BUILT 2026-07-29** (blobs and spliced archives, read-only,
Linux). v2 (lazy ETL) and write support are designed here and refused by
name.

## 0. What it adds, and what it does not

Nothing here is new data. `mpedb rretl get`, `rretl pack-out` and the
Python surface already hand you these bytes. What they cannot do is hand
them to a program that only speaks paths: an image viewer, `grep -r`, a
linker, a build system, an editor's open dialog. **Mounting is the adapter,
and that is the entire contribution.** A design that forgets this grows a
second, worse API for things the first API already does well.

```
/obj/<name>/latest        the newest version's bytes
/obj/<name>/v<N>          that version, exactly
/archive/<id>-<name>/…    a spliced zip's members, as a real directory tree
```

## 1. Read-only, as a decision rather than a stage

A writable mount has to answer a question the database cannot: what is a
partial write? A `cp` that is 40 % through a 2 GB file has produced no
version anybody asked for, and holding the answer outside the database
until `close()` means holding it somewhere that a crash does not protect.

Worse, it would hold mpedb's single writer lock for the duration of a
user's copy — turning a slow `cp` into an outage for every other writer in
every other process. That is not a trade the filesystem layer gets to make
on the database's behalf.

So: every mutating operation returns `EROFS`, the mount carries `-o ro`,
and the permission bits are `r--r--r--` so a tool that checks before
writing gets the same answer as one that tries. `rretl put` remains the way
to write, and it is one call.

## 2. One snapshot per open file

`open` reads the bytes; the handle keeps them until `release`. A `cat` that
takes a minute therefore sees ONE version even if a writer commits three
more meanwhile, and the size the kernel was told at `getattr` stays true
for the whole read — a file whose size changes under a reader is how a FUSE
filesystem returns a truncated answer and calls it success.

The cost is stated rather than hidden: the whole object sits in memory for
the life of the handle. #43's incremental blob API is the way out when a
blob that does not fit becomes a real complaint rather than a hypothetical
one.

## 3. Sizes without reading: the cache, and why it is sound

`getattr` must report a size before anyone reads. For a delta-stored blob
version that means decoding the chain — so `ls -l` on an object directory
would decode every version in it, every time.

The size is therefore cached per path, and the cache never invalidates.
That is sound because of an invariant the blob store already guarantees:
**a version's CONTENT never changes once written.** Its STORAGE does — a
newer version rewrites the previous one as a reverse delta (DESIGN-RRETL
§8.2) — but the bytes it decodes to are the same forever. Archive members
are immutable rows for the same reason.

`VersionInfo.bytes` is NOT that size: it is what the envelope holds, which
for a delta is the delta. Using it would have made `ls -l` disagree with
`wc -c`, and the kernel would have truncated reads at the smaller number.

## 4. Inodes are handed out once

The namespace is rebuilt from the database on every `readdir` — new
versions and new archives appear without remounting. Inodes, though, are
assigned per PATH and never reused: a file that is open cannot have its
identity pulled out from under it by a listing that happens to run.

## 5. The platform gate, and the dependency

FUSE is Linux and macOS; Windows would be WinFsp or Dokan, which is a
different API and not attempted. `mpedb-fs` is its own workspace — like
`mpedb-capi` — so a build machine without `/dev/fuse` is never asked to
compile it:

```sh
cargo build --manifest-path crates/mpedb-fs/Cargo.toml
```

macOS needs macFUSE, which is a system extension the user installs and
approves — the M3 in this project's gate does not have it, so mpedbfs is
**built and mounted on Linux only** so far. That is a coverage statement,
not a portability claim: nothing in the code is Linux-specific, and the
first macOS run is the thing that would prove it.

`fuser` is taken with `default-features = false`, which drops the libfuse C
linkage: mounting goes through the setuid `fusermount3` helper instead, so
the build needs no headers and no `pkg-config` — only the runtime helper
that any FUSE-capable box already has. (Measured on the dev box, which has
`libfuse3.so.3` and `fusermount3` but no `-dev` package: with the feature
on it does not build; with it off it builds and mounts.)

## 6. Staging

| stage | contents | status |
|---|---|---|
| v1 | `/obj` (versioned blobs) + `/archive` (spliced zip members as a tree), read-only | **BUILT** |
| v2 | lazy ETL: a file whose bytes are a lens applied on read | designed below, refused |
| v3 | write support | refused by design (§1), not staged |

### v2, and the question it has to answer first

The shape is obvious — `/lens/<pair>/<table>.csv` produces the forward
image without materializing it — and the hard part is not the plumbing. It
is that a file must have a SIZE before it has content, and a transformed
table's size is not known until the transform runs. The three honest
answers are: materialize on `open` and pay (what v1 does for blobs),
report a size the reader must not trust (what `/proc` does, and what breaks
`cp`), or present a format whose length is computable from the schema and
the row count. Until one of those is chosen with a measurement behind it,
`/lens` does not exist rather than existing badly.
