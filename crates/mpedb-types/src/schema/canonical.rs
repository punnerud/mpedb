use super::*;

impl Schema {
    /// Canonical, deterministic serialization — the schema-hash preimage and
    /// the format stored in the database catalog (v2, DESIGN-SCHEMA-V2).
    /// The per-column `unique`/`indexed` flags are NOT serialized (bits 1–7
    /// written zero): `indexes` is the single source of truth on the wire,
    /// and decode reconstructs the in-memory convenience flags from it.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(256);
        buf.push(18u8); // schema encoding version (v18: fts MODULE tag)
        buf.extend_from_slice(&(self.tables.len() as u32).to_le_bytes());
        for t in &self.tables {
            buf.extend_from_slice(&t.id.to_le_bytes());
            buf.push(t.dead as u8); // tombstone marker; a dead slot's rest is empty
            // Table-kind discriminant (v4): 0 = Standard, 1 = FTS ‖ tokenizer
            // ‖ module (v18 — Fts4 carries sqlite's shadow-table catalog).
            match t.kind {
                TableKind::Standard => buf.push(0),
                TableKind::Fts { tokenizer, module } => {
                    buf.push(1);
                    buf.push(tokenizer as u8);
                    buf.push(module as u8);
                }
            }
            // Hidden implicit-rowid flag (v5, #94). Always 0 for a dead slot or
            // an FTS table (validate enforces it).
            buf.push(t.implicit_rowid as u8);
            // AUTOINCREMENT (v17): the never-reuse promise.
            buf.push(t.autoincrement as u8);
            write_str(&mut buf, &t.name);
            buf.extend_from_slice(&(t.columns.len() as u16).to_le_bytes());
            for c in &t.columns {
                write_str(&mut buf, &c.name);
                buf.push(c.ty as u8);
                buf.push(c.nullable as u8);
                // Declared collating sequence (v6). BINARY (0) for every column
                // that did not write `COLLATE`, so a plain schema's bytes grow by
                // exactly one zero byte per column.
                buf.push(c.collation as u8);
                // sqlite type affinity (v7). Pinned by `validate` to the one
                // `ty` implies except on an `Any` column, where `Numeric` vs
                // `Blob` is the store-time-conversion bit `ty` cannot carry.
                buf.push(c.affinity as u8);
                // Verbatim declared-type text (v8). Absent (0) for the config
                // path and synthetic tables, where the canonical name is the
                // answer — a plain schema's bytes grow by one zero per column.
                match &c.decl {
                    None => buf.push(0),
                    Some(d) => {
                        buf.push(1);
                        write_str(&mut buf, d);
                    }
                }
                match &c.default {
                    None => buf.push(0),
                    Some(DefaultExpr::Const(v)) => {
                        buf.push(1);
                        write_value(&mut buf, v);
                    }
                    Some(DefaultExpr::Now) => buf.push(2),
                    Some(DefaultExpr::CurrentTimestamp) => buf.push(3),
                    Some(DefaultExpr::CurrentDate) => buf.push(4),
                    Some(DefaultExpr::CurrentTime) => buf.push(5),
                    Some(DefaultExpr::Expr(d)) => {
                        buf.push(6);
                        write_str(&mut buf, &d.src);
                        d.program.encode_into(&mut buf);
                    }
                }
                match &c.default_text {
                    None => buf.push(0),
                    Some(src) => {
                        buf.push(1);
                        write_str(&mut buf, src);
                    }
                }
                match &c.check {
                    None => buf.push(0),
                    Some(src) => {
                        buf.push(1);
                        write_str(&mut buf, src);
                    }
                }
                // GENERATED ALWAYS AS (…) (v9): source ‖ kind ‖ compiled
                // program. The COMPILED form is on the wire — see `GeneratedCol`
                // for why — so a decoded schema can evaluate the column without
                // the SQL layer.
                match &c.generated {
                    None => buf.push(0),
                    Some(g) => {
                        buf.push(1);
                        write_str(&mut buf, &g.expr);
                        buf.push(g.kind as u8);
                        g.program.encode_into(&mut buf);
                    }
                }
            }
            buf.extend_from_slice(&(t.primary_key.len() as u16).to_le_bytes());
            for &i in &t.primary_key {
                buf.extend_from_slice(&i.to_le_bytes());
            }
            buf.extend_from_slice(&(t.indexes.len() as u16).to_le_bytes());
            for ix in &t.indexes {
                buf.push(ix.unique as u8);
                buf.extend_from_slice(&(ix.columns.len() as u16).to_le_bytes());
                for &ci in &ix.columns {
                    buf.extend_from_slice(&ci.to_le_bytes());
                }
                // Partial-index predicate (v10). Absent (0) for whole-table
                // indexes so a plain schema grows by one zero byte per index.
                match &ix.predicate {
                    None => buf.push(0),
                    Some(src) => {
                        buf.push(1);
                        write_str(&mut buf, src);
                    }
                }
                // Index name (v11). Absent (0) for a flag-derived index, so a
                // config-only schema grows by one zero byte per index.
                match &ix.name {
                    None => buf.push(0),
                    Some(n) => {
                        buf.push(1);
                        write_str(&mut buf, n);
                    }
                }
                // Expression key parts (v13). Empty for a plain-column index,
                // so the common case is one zero u16 per index.
                buf.extend_from_slice(&(ix.exprs.len() as u16).to_le_bytes());
                for e in &ix.exprs {
                    match e {
                        None => buf.push(0),
                        Some(src) => {
                            buf.push(1);
                            write_str(&mut buf, src);
                        }
                    }
                }
                // Per-part COLLATE overrides (v14). Empty for an index that
                // keys by its columns' own collations, so one zero u16.
                buf.extend_from_slice(&(ix.collations.len() as u16).to_le_bytes());
                for c in &ix.collations {
                    buf.push(match c {
                        None => 0,
                        Some(c) => *c as u8 + 1,
                    });
                }
            }
            // FOREIGN KEYs (v12). Almost every table has none, so the common
            // case is a single zero u16 — the same "one length word" price the
            // index list already pays.
            buf.extend_from_slice(&(t.foreign_keys.len() as u16).to_le_bytes());
            for fk in &t.foreign_keys {
                buf.extend_from_slice(&(fk.columns.len() as u16).to_le_bytes());
                for &c in &fk.columns {
                    buf.extend_from_slice(&c.to_le_bytes());
                }
                write_str(&mut buf, &fk.parent);
                // The LENGTH is written rather than assumed equal to
                // `columns.len()`: a decoder must not trust two counts to agree
                // when one byte of a hostile mapping can make them disagree.
                // Zero is legal here and means "the parent's PRIMARY KEY".
                buf.extend_from_slice(&(fk.parent_columns.len() as u16).to_le_bytes());
                for c in &fk.parent_columns {
                    write_str(&mut buf, c);
                }
                buf.push(fk.on_delete.tag());
                buf.push(fk.on_update.tag());
                buf.push(u8::from(fk.deferred));
                match &fk.name {
                    None => buf.push(0),
                    Some(n) => {
                        buf.push(1);
                        write_str(&mut buf, n);
                    }
                }
            }
        }
        buf
    }

    /// Parse [`canonical_bytes`] output (bounds-checked; used when attaching
    /// to an existing database to recover its schema from the catalog). Only
    /// version 5 is accepted — older files refuse loudly and are regenerated
    /// (DESIGN-SCHEMA-V2 §5; the project carries no migration burden).
    pub fn from_canonical_bytes(buf: &[u8]) -> Result<Schema> {
        let err = || Error::Corrupt("truncated schema".into());
        let mut pos = 0usize;
        let version = *buf.get(pos).ok_or_else(err)?;
        pos += 1;
        // v9 = generated columns; v10 = IndexDef.predicate; v11 = IndexDef.name;
        // v12 = TableDef.foreign_keys; v13 = IndexDef.exprs.
        // Older versions refuse loudly (no migration burden — DESIGN-SCHEMA-V2 §5).
        if !(9..=18).contains(&version) {
            return Err(Error::Corrupt(format!(
                "unknown schema version {version} (v1..v8 predate canonical-bytes v9 — \
                 regenerate or re-import)"
            )));
        }
        let has_index_predicate = version >= 10;
        let has_index_name = version >= 11;
        let has_foreign_keys = version >= 12;
        let has_index_exprs = version >= 13;
        let has_index_collations = version >= 14;
        let has_fts_module = version >= 18;
        let ntables = read_u32(buf, &mut pos)? as usize;
        // The CEILING, not the configured cap: this bounds what a FILE can
        // make this decoder believe, and a hostile file must not be able to
        // raise its own limit by naming a bigger one.
        if ntables > crate::MAX_TABLES_CEILING {
            return Err(Error::Corrupt("table count out of range".into()));
        }
        // `.min(256)`: `ntables` comes from untrusted bytes and MAX_TABLES is
        // now 4096, so reserving it outright would let a corrupt count drive a
        // half-megabyte speculative allocation before the first field is read.
        let mut tables = Vec::with_capacity(ntables.min(256));
        for _ in 0..ntables {
            let id = read_u32(buf, &mut pos)?;
            let dead = match *buf.get(pos).ok_or_else(err)? {
                0 => false,
                1 => true,
                _ => return Err(Error::Corrupt("bad table dead flag".into())),
            };
            pos += 1;
            let kind = match *buf.get(pos).ok_or_else(err)? {
                0 => {
                    pos += 1;
                    TableKind::Standard
                }
                1 => {
                    pos += 1;
                    let tok = crate::fts::Tokenizer::from_tag(*buf.get(pos).ok_or_else(err)?)
                        .ok_or_else(|| Error::Corrupt("bad fts tokenizer tag".into()))?;
                    pos += 1;
                    let module = if has_fts_module {
                        let m = crate::fts::FtsModule::from_tag(*buf.get(pos).ok_or_else(err)?)
                            .ok_or_else(|| Error::Corrupt("bad fts module tag".into()))?;
                        pos += 1;
                        m
                    } else {
                        crate::fts::FtsModule::Fts5
                    };
                    TableKind::Fts { tokenizer: tok, module }
                }
                _ => return Err(Error::Corrupt("bad table kind tag".into())),
            };
            let implicit_rowid = match *buf.get(pos).ok_or_else(err)? {
                0 => false,
                1 => true,
                _ => return Err(Error::Corrupt("bad implicit_rowid flag".into())),
            };
            pos += 1;
            let autoincrement = match *buf.get(pos).ok_or_else(err)? {
                0 => false,
                1 => true,
                _ => return Err(Error::Corrupt("bad autoincrement flag".into())),
            };
            pos += 1;
            let name = read_str(buf, &mut pos)?;
            let ncols = read_u16(buf, &mut pos)? as usize;
            if ncols > MAX_COLUMNS {
                return Err(Error::Corrupt("column count out of range".into()));
            }
            let mut columns = Vec::with_capacity(ncols);
            for _ in 0..ncols {
                let cname = read_str(buf, &mut pos)?;
                let ty = ColumnType::from_tag(*buf.get(pos).ok_or_else(err)?)
                    .ok_or_else(|| Error::Corrupt("bad column type".into()))?;
                pos += 1;
                // bits 1–7 are reserved-zero on write and IGNORED on read:
                // the index list is the only wire truth (design §1.5).
                let flags = *buf.get(pos).ok_or_else(err)?;
                pos += 1;
                // Declared collating sequence (v6).
                let collation = Collation::from_tag(*buf.get(pos).ok_or_else(err)?)
                    .ok_or_else(|| Error::Corrupt("bad column collation tag".into()))?;
                pos += 1;
                // sqlite type affinity (v7).
                let affinity = Affinity::from_tag(*buf.get(pos).ok_or_else(err)?)
                    .ok_or_else(|| Error::Corrupt("bad column affinity tag".into()))?;
                pos += 1;
                // Verbatim declared-type text (v8).
                let decl = match *buf.get(pos).ok_or_else(err)? {
                    0 => {
                        pos += 1;
                        None
                    }
                    1 => {
                        pos += 1;
                        Some(read_str(buf, &mut pos)?)
                    }
                    _ => return Err(Error::Corrupt("bad column decl tag".into())),
                };
                let default = match *buf.get(pos).ok_or_else(err)? {
                    0 => {
                        pos += 1;
                        None
                    }
                    1 => {
                        pos += 1;
                        Some(DefaultExpr::Const(read_value(buf, &mut pos)?))
                    }
                    2 => {
                        pos += 1;
                        Some(DefaultExpr::Now)
                    }
                    3 => {
                        pos += 1;
                        Some(DefaultExpr::CurrentTimestamp)
                    }
                    4 => {
                        pos += 1;
                        Some(DefaultExpr::CurrentDate)
                    }
                    5 => {
                        pos += 1;
                        Some(DefaultExpr::CurrentTime)
                    }
                    6 => {
                        pos += 1;
                        let src = read_str(buf, &mut pos)?;
                        let program = ExprProgram::decode(buf, &mut pos)?;
                        Some(DefaultExpr::Expr(Box::new(DefaultProgram { src, program })))
                    }
                    _ => return Err(Error::Corrupt("bad default tag".into())),
                };
                let default_text = match *buf.get(pos).ok_or_else(err)? {
                    0 => {
                        pos += 1;
                        None
                    }
                    1 => {
                        pos += 1;
                        Some(read_str(buf, &mut pos)?)
                    }
                    _ => return Err(Error::Corrupt("bad default-text tag".into())),
                };
                let check = match *buf.get(pos).ok_or_else(err)? {
                    0 => {
                        pos += 1;
                        None
                    }
                    1 => {
                        pos += 1;
                        Some(read_str(buf, &mut pos)?)
                    }
                    _ => return Err(Error::Corrupt("bad check tag".into())),
                };
                // GENERATED ALWAYS AS (…) (v9).
                let generated = match *buf.get(pos).ok_or_else(err)? {
                    0 => {
                        pos += 1;
                        None
                    }
                    1 => {
                        pos += 1;
                        let expr = read_str(buf, &mut pos)?;
                        let kind = GeneratedKind::from_tag(*buf.get(pos).ok_or_else(err)?)
                            .ok_or_else(|| Error::Corrupt("bad generated kind tag".into()))?;
                        pos += 1;
                        let program = ExprProgram::decode(buf, &mut pos)?;
                        Some(GeneratedCol { expr, kind, program })
                    }
                    _ => return Err(Error::Corrupt("bad generated tag".into())),
                };
                columns.push(ColumnDef {
                    name: cname,
                    ty,
                    nullable: flags & 1 != 0,
                    unique: false,
                    indexed: false,
                    default,
                    default_text,
                    check,
                    collation,
                    affinity,
                    decl,
                    generated,
                });
            }
            let npk = read_u16(buf, &mut pos)? as usize;
            if npk > ncols {
                return Err(Error::Corrupt("pk count out of range".into()));
            }
            let mut primary_key = Vec::with_capacity(npk);
            for _ in 0..npk {
                primary_key.push(read_u16(buf, &mut pos)?);
            }
            let nindexes = read_u16(buf, &mut pos)? as usize;
            if nindexes > MAX_INDEXES {
                return Err(Error::Corrupt("index count out of range".into()));
            }
            let mut indexes = Vec::with_capacity(nindexes);
            for _ in 0..nindexes {
                let unique = match *buf.get(pos).ok_or_else(err)? {
                    0 => false,
                    1 => true,
                    _ => return Err(Error::Corrupt("bad index unique tag".into())),
                };
                pos += 1;
                let nic = read_u16(buf, &mut pos)? as usize;
                if nic > MAX_COLUMNS {
                    return Err(Error::Corrupt("index column count out of range".into()));
                }
                let mut cols = Vec::with_capacity(nic);
                for _ in 0..nic {
                    cols.push(read_u16(buf, &mut pos)?);
                }
                let predicate = if has_index_predicate {
                    match *buf.get(pos).ok_or_else(err)? {
                        0 => {
                            pos += 1;
                            None
                        }
                        1 => {
                            pos += 1;
                            Some(read_str(buf, &mut pos)?)
                        }
                        _ => return Err(Error::Corrupt("bad index predicate tag".into())),
                    }
                } else {
                    None
                };
                let name = if has_index_name {
                    match *buf.get(pos).ok_or_else(err)? {
                        0 => {
                            pos += 1;
                            None
                        }
                        1 => {
                            pos += 1;
                            Some(read_str(buf, &mut pos)?)
                        }
                        _ => return Err(Error::Corrupt("bad index name tag".into())),
                    }
                } else {
                    None
                };
                let exprs = if has_index_exprs {
                    let n = read_u16(buf, &mut pos)? as usize;
                    // Parallel to `columns` whenever present: a mismatched
                    // length is a corrupt blob, not a key builder left to read
                    // past the end of one of the two.
                    if n != 0 && n != cols.len() {
                        return Err(Error::Corrupt(
                            "index expression list length differs from the column list".into(),
                        ));
                    }
                    let mut v = Vec::with_capacity(n);
                    for _ in 0..n {
                        v.push(match *buf.get(pos).ok_or_else(err)? {
                            0 => {
                                pos += 1;
                                None
                            }
                            1 => {
                                pos += 1;
                                Some(read_str(buf, &mut pos)?)
                            }
                            _ => return Err(Error::Corrupt("bad index expression tag".into())),
                        });
                    }
                    v
                } else {
                    Vec::new()
                };
                let collations = if has_index_collations {
                    let n = read_u16(buf, &mut pos)? as usize;
                    if n != 0 && n != cols.len() {
                        return Err(Error::Corrupt(
                            "index collation list length differs from the column list".into(),
                        ));
                    }
                    let mut v = Vec::with_capacity(n);
                    for _ in 0..n {
                        let tag = *buf.get(pos).ok_or_else(err)?;
                        pos += 1;
                        v.push(match tag {
                            0 => None,
                            t => Some(
                                crate::value::Collation::from_tag(t - 1)
                                    .ok_or_else(|| Error::Corrupt("bad index collation tag".into()))?,
                            ),
                        });
                    }
                    v
                } else {
                    Vec::new()
                };
                indexes.push(IndexDef {
                    collations,
                    columns: cols,
                    unique,
                    predicate,
                    exprs,
                    name,
                });
            }
            // Reconstruct the in-memory convenience flags from the index
            // list, in one place: a single-column index marks its column.
            for ix in &indexes {
                if let [ci] = ix.columns[..] {
                    if let Some(c) = columns.get_mut(ci as usize) {
                        if ix.unique {
                            c.unique = true;
                        } else {
                            c.indexed = true;
                        }
                    }
                }
            }
            let mut foreign_keys = Vec::new();
            if has_foreign_keys {
                let nfk = read_u16(buf, &mut pos)? as usize;
                if nfk > MAX_COLUMNS {
                    return Err(Error::Corrupt("foreign key count out of range".into()));
                }
                for _ in 0..nfk {
                    let nc = read_u16(buf, &mut pos)? as usize;
                    if nc == 0 || nc > MAX_COLUMNS {
                        return Err(Error::Corrupt("fk column count out of range".into()));
                    }
                    let mut cols = Vec::with_capacity(nc);
                    for _ in 0..nc {
                        cols.push(read_u16(buf, &mut pos)?);
                    }
                    let parent = read_str(buf, &mut pos)?;
                    let npc = read_u16(buf, &mut pos)? as usize;
                    // The two key sides must line up: a foreign key whose child
                    // and parent halves differ in width has no meaning, and
                    // accepting one would hand the write path a zip() that
                    // silently drops columns. Zero is the one legal
                    // disagreement — it means the list was not written.
                    if npc != 0 && npc != nc {
                        return Err(Error::Corrupt(
                            "fk parent column count differs from child".into(),
                        ));
                    }
                    let mut pcols = Vec::with_capacity(npc);
                    for _ in 0..npc {
                        pcols.push(read_str(buf, &mut pos)?);
                    }
                    let on_delete = FkAction::from_tag(*buf.get(pos).ok_or_else(err)?)
                        .ok_or_else(|| Error::Corrupt("bad fk ON DELETE action".into()))?;
                    pos += 1;
                    let on_update = FkAction::from_tag(*buf.get(pos).ok_or_else(err)?)
                        .ok_or_else(|| Error::Corrupt("bad fk ON UPDATE action".into()))?;
                    pos += 1;
                    let deferred = match *buf.get(pos).ok_or_else(err)? {
                        0 => false,
                        1 => true,
                        _ => return Err(Error::Corrupt("bad fk deferred flag".into())),
                    };
                    pos += 1;
                    let fk_name = match *buf.get(pos).ok_or_else(err)? {
                        0 => {
                            pos += 1;
                            None
                        }
                        1 => {
                            pos += 1;
                            Some(read_str(buf, &mut pos)?)
                        }
                        _ => return Err(Error::Corrupt("bad fk name tag".into())),
                    };
                    foreign_keys.push(ForeignKeyDef {
                        columns: cols,
                        parent,
                        parent_columns: pcols,
                        on_delete,
                        on_update,
                        deferred,
                        name: fk_name,
                    });
                }
            }
            tables.push(TableDef {
                id,
                name,
                columns,
                primary_key,
                indexes,
                dead,
                kind,
                implicit_rowid,
                autoincrement,
                foreign_keys,
            });
        }
        if pos != buf.len() {
            return Err(Error::Corrupt("trailing bytes in schema".into()));
        }
        // Re-validate: canonical bytes from a hostile/corrupt mapping must
        // still produce a schema every other invariant can rely on —
        // including the dense-id rule (position == id) that the engine's
        // positional caches depend on.
        let schema = Schema { tables };
        schema.validate().map_err(|e| match e {
            Error::Schema(m) => Error::Corrupt(format!("schema bytes invalid: {m}")),
            other => other,
        })?;
        Ok(schema)
    }

    pub fn hash(&self) -> [u8; 32] {
        *blake3::hash(&self.canonical_bytes()).as_bytes()
    }
}

fn write_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

fn read_u16(buf: &[u8], pos: &mut usize) -> Result<u16> {
    let raw = buf
        .get(*pos..*pos + 2)
        .ok_or_else(|| Error::Corrupt("truncated schema".into()))?;
    *pos += 2;
    Ok(u16::from_le_bytes(raw.try_into().unwrap()))
}

fn read_u32(buf: &[u8], pos: &mut usize) -> Result<u32> {
    let raw = buf
        .get(*pos..*pos + 4)
        .ok_or_else(|| Error::Corrupt("truncated schema".into()))?;
    *pos += 4;
    Ok(u32::from_le_bytes(raw.try_into().unwrap()))
}

fn read_str(buf: &[u8], pos: &mut usize) -> Result<String> {
    let len = read_u32(buf, pos)? as usize;
    if len > 1 << 20 {
        return Err(Error::Corrupt("string too long in schema".into()));
    }
    let raw = buf
        .get(*pos..*pos + len)
        .ok_or_else(|| Error::Corrupt("truncated schema".into()))?;
    *pos += len;
    String::from_utf8(raw.to_vec()).map_err(|_| Error::Corrupt("invalid utf-8 in schema".into()))
}
