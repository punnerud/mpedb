//! Composite row identities and bounded lexicographic scan SQL. Single-column
//! identities retain their original residual digest, so stored runs still work.
use super::{Error, Result, Value};

#[derive(Clone)]
pub(super) struct RowKey(pub Vec<Value>);
impl std::fmt::Debug for RowKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.len() == 1 {
            std::fmt::Debug::fmt(&self.0[0], f)
        } else {
            std::fmt::Debug::fmt(&self.0, f)
        }
    }
}
impl RowKey {
    pub fn from_row(row: &[Value], arity: usize) -> Result<Self> {
        if arity == 0 || row.len() != arity + 1 {
            return Err(Error::Corrupt("rretl: invalid identity projection".into()));
        }
        Ok(Self(row[..arity].to_vec()))
    }
    pub fn reference(&self) -> Vec<u8> {
        if self.0.len() == 1 {
            return super::pk_ref(&self.0[0]);
        }
        // 0xff is not a scalar value_bits tag. Framing prevents either a
        // scalar Blob or differently partitioned compound values colliding.
        let mut hash = blake3::Hasher::new();
        hash.update(&[0xff]);
        hash.update(&(self.0.len() as u64).to_le_bytes());
        for value in &self.0 {
            let bytes = crate::lens::value_bits(value);
            hash.update(&(bytes.len() as u64).to_le_bytes());
            hash.update(&bytes);
        }
        hash.finalize().as_bytes().to_vec()
    }
    pub fn update_args(&self, value: Value) -> Vec<Value> {
        let mut args = Vec::with_capacity(self.0.len() + 1);
        args.push(value);
        args.extend(self.0.iter().cloned());
        args
    }
}

fn q(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub(super) fn scan_sql(
    table: &str,
    keys: &[String],
    column: &str,
    chunk: usize,
) -> (String, String) {
    let cols = keys.iter().map(|k| q(k)).collect::<Vec<_>>();
    let order = cols.join(",");
    let select = format!("SELECT {},{} FROM {}", order, q(column), q(table));
    let predicate = if cols.len() == 1 {
        format!("{} > $1", cols[0])
    } else {
        let alternatives = (0..cols.len())
            .map(|i| {
                let mut terms = (0..i)
                    .map(|j| format!("{} = ${}", cols[j], j + 1))
                    .collect::<Vec<_>>();
                terms.push(format!("{} > ${}", cols[i], i + 1));
                format!("({})", terms.join(" AND "))
            })
            .collect::<Vec<_>>()
            .join(" OR ");
        // Expose the leading range to the planner as well as expressing the
        // exact strict tuple order; no tuple comparison syntax is required.
        format!("{} >= $1 AND ({alternatives})", cols[0])
    };
    (
        format!("{select} ORDER BY {order} LIMIT {chunk}"),
        format!("{select} WHERE {predicate} ORDER BY {order} LIMIT {chunk}"),
    )
}

pub(super) fn update_sql(table: &str, keys: &[String], column: &str) -> String {
    let predicate = keys
        .iter()
        .enumerate()
        .map(|(i, k)| format!("{} = ${}", q(k), i + 2))
        .collect::<Vec<_>>()
        .join(" AND ");
    format!(
        "UPDATE {} SET {} = $1 WHERE {predicate}",
        q(table),
        q(column)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compound_identity_is_framed_and_keeps_scalar_compatibility() {
        let value = Value::Int(7);
        assert_eq!(
            RowKey(vec![value.clone()]).reference(),
            super::super::pk_ref(&value)
        );
        let a = RowKey(vec![Value::Text("ab".into()), Value::Text("c".into())]);
        let b = RowKey(vec![Value::Text("a".into()), Value::Text("bc".into())]);
        assert_ne!(a.reference(), b.reference());
        assert_ne!(
            a.reference(),
            RowKey(vec![Value::Blob(a.reference())]).reference()
        );
    }
}
