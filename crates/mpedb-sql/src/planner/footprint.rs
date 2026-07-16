use super::*;

// ---- footprint ---------------------------------------------------------------

fn access_key_and_indexes(a: &AccessPath) -> (KeyAccess, u64) {
    match a {
        AccessPath::PkPoint(parts) => (KeyAccess::Point(parts.clone()), 1),
        AccessPath::PkRange { lo, hi } => (
            KeyAccess::Range {
                lo: lo.clone(),
                hi: hi.clone(),
            },
            1,
        ),
        // The secondary probe also fetches the row through the PK tree, so
        // both index bits are set. Key access degrades honestly to Full.
        AccessPath::IndexPoint { index_no, .. } => {
            (KeyAccess::Full, 1 | (1u64 << (*index_no).min(63)))
        }
        AccessPath::IndexRange { index_no, .. } => {
            (KeyAccess::Full, 1 | (1u64 << (*index_no).min(63)))
        }
        AccessPath::FullScan => (KeyAccess::Full, 1),
    }
}

/// The footprint of ONE select — shared between a top-level SELECT and each
/// compound arm.
fn select_footprint(sp: &SelectPlan, schema: &Schema) -> Result<Footprint> {
    let table_bit = |id: u32| -> Result<u64> {
        if schema.table(id).is_none() || id >= 64 {
            return Err(Error::Corrupt(format!("table id {id} out of range")));
        }
        Ok(1u64 << id)
    };
    let SelectPlan { table, access, joins, .. } = sp;
    Ok({
        let (key_access, mut indexes_used) = access_key_and_indexes(access);
        // ONE BIT PER TABLE READ. A join that claimed only the outer would
        // under-claim `tables_read`, and `conflicts_with` is a bitmap AND —
        // so a writer to the inner table would not be seen to conflict with
        // this reader, and the commit path would group them as independent.
        let mut tables_read = table_bit(*table)?;
        let mut key_access = key_access;
        for j in joins {
            tables_read |= table_bit(j.table)?;
            let (jkey, jidx) = access_key_and_indexes(&j.access);
            indexes_used |= jidx;
            let _ = jkey;
            // `key_access` is per-STATEMENT, and it names ONE key space. A
            // Point on the outer stops describing what this reads the
            // moment a second table joins in, and a claim narrower than the
            // truth is a claim that rows this statement does read are rows
            // it does not. Full is the only honest answer the type can
            // express — it costs conflict precision, never correctness.
            key_access = KeyAccess::Full;
        }
        Footprint {
            tables_read,
            tables_written: 0,
            indexes_used,
            key_access,
            read_only: true,
        }
    })
}

/// Compute the footprint a statement must carry. Also used by
/// [`CompiledPlan::decode`] to verify that a stored footprint was not forged.
pub(crate) fn compute_footprint(stmt: &PlanStmt, schema: &Schema) -> Result<Footprint> {
    let table_bit = |id: u32| -> Result<u64> {
        if schema.table(id).is_none() || id >= 64 {
            return Err(Error::Corrupt(format!("table id {id} out of range")));
        }
        Ok(1u64 << id)
    };
    let all_secondary_bits = |t: &TableDef| -> Result<u64> {
        let n = secondary_indexes(t).len();
        if n > 63 {
            return Err(Error::Unsupported(
                "more than 63 secondary indexes on one table".into(),
            ));
        }
        let mut bits = 1u64; // PK tree
        for k in 0..n {
            bits |= 1u64 << (k + 1);
        }
        Ok(bits)
    };
    Ok(match stmt {
        PlanStmt::Select(sp) => select_footprint(sp, schema)?,
        // A compound reads the UNION of what its arms read. `key_access` is
        // per-STATEMENT and names ONE key space — with several arms Full is
        // the only honest claim (same argument as the join case below).
        PlanStmt::Compound(c) => {
            let mut tables_read = 0u64;
            let mut indexes_used = 0u64;
            for arm in &c.arms {
                let f = select_footprint(arm, schema)?;
                tables_read |= f.tables_read;
                indexes_used |= f.indexes_used;
            }
            Footprint {
                tables_read,
                tables_written: 0,
                indexes_used,
                key_access: KeyAccess::Full,
                read_only: true,
            }
        }
        PlanStmt::Insert { table, rows, .. } => {
            let t = schema
                .table(*table)
                .ok_or_else(|| Error::Corrupt("table id out of range".into()))?;
            // Single-row insert with every PK column from Param/Const gives an
            // exact point write set; multi-row or defaulted PK degrades to Full.
            let key_access = if rows.len() == 1 {
                let parts: Option<Vec<KeyPart>> = t
                    .primary_key
                    .iter()
                    .map(|&c| match rows[0].get(c as usize) {
                        Some(InsertSource::Param(i)) => Some(KeyPart::Param(*i)),
                        Some(InsertSource::Const(i)) => Some(KeyPart::Const(*i)),
                        _ => None,
                    })
                    .collect();
                parts.map_or(KeyAccess::Full, KeyAccess::Point)
            } else {
                KeyAccess::Full
            };
            Footprint {
                tables_read: 0,
                tables_written: table_bit(*table)?,
                // All unique indexes are maintained by an insert.
                indexes_used: all_secondary_bits(t)?,
                key_access,
                read_only: false,
            }
        }
        PlanStmt::Update {
            table, access, set, ..
        } => {
            let t = schema
                .table(*table)
                .ok_or_else(|| Error::Corrupt("table id out of range".into()))?;
            let (key_access, mut indexes_used) = access_key_and_indexes(access);
            let sec = secondary_indexes(t);
            for (col, _) in set {
                if let Some(pos) = sec.iter().position(|c| c == col) {
                    if pos + 1 > 63 {
                        return Err(Error::Unsupported(
                            "more than 63 secondary indexes on one table".into(),
                        ));
                    }
                    indexes_used |= 1u64 << (pos + 1);
                }
            }
            let bit = table_bit(*table)?;
            Footprint {
                tables_read: bit,
                tables_written: bit,
                indexes_used,
                key_access,
                read_only: false,
            }
        }
        PlanStmt::Delete { table, access, .. } => {
            let t = schema
                .table(*table)
                .ok_or_else(|| Error::Corrupt("table id out of range".into()))?;
            let (key_access, indexes_used) = access_key_and_indexes(access);
            let bit = table_bit(*table)?;
            Footprint {
                tables_read: bit,
                tables_written: bit,
                // A delete unlinks the row from every index.
                indexes_used: indexes_used | all_secondary_bits(t)?,
                key_access,
                read_only: false,
            }
        }
        // Transaction control touches no tables. KeyAccess::Full is the
        // honest "no key claim" value; read_only routes them past nothing —
        // the engine special-cases them anyway.
        PlanStmt::Begin | PlanStmt::Commit | PlanStmt::Rollback => Footprint {
            tables_read: 0,
            tables_written: 0,
            indexes_used: 0,
            key_access: KeyAccess::Full,
            read_only: true,
        },
    })
}
