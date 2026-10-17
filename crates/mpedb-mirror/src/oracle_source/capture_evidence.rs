//! Per-key transaction evidence. An expired version's XID identifies its
//! creator, never the transaction that expired it. Correlate the physical row
//! with a version started at the same SCN; ambiguous evidence stays incomplete.
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OracleKeyTransactions {
    pub xids: Vec<Vec<u8>>,
    /// False means at least one expiration could not be attributed. Consumers
    /// must treat the key as foreign/unknown, regardless of other matching XIDs.
    pub complete: bool,
}
#[derive(Default)]
pub(super) struct KeyEvidence {
    starts: BTreeSet<Vec<u8>>,
    ends: BTreeSet<String>,
}
#[derive(Default)]
pub(super) struct Evidence {
    by_row: BTreeMap<String, BTreeSet<Vec<u8>>>,
    keys: BTreeMap<(u32, Vec<u8>), KeyEvidence>,
}
impl Evidence {
    pub fn start(&mut self, table: u32, pk: Vec<u8>, row: String, xid: Vec<u8>) {
        // ROWID is unique within a table, not across the selected table set.
        let row = format!("{table}:{row}");
        self.by_row.entry(row).or_default().insert(xid.clone());
        self.keys.entry((table, pk)).or_default().starts.insert(xid);
    }
    pub fn end(&mut self, table: u32, pk: Vec<u8>, row: String) {
        self.keys
            .entry((table, pk))
            .or_default()
            .ends
            .insert(format!("{table}:{row}"));
    }
    pub fn resolve(&self, table: u32, pk: &[u8]) -> OracleKeyTransactions {
        let Some(key) = self.keys.get(&(table, pk.to_vec())) else {
            return OracleKeyTransactions {
                xids: vec![],
                complete: false,
            };
        };
        let mut xids = key.starts.clone();
        let mut complete = true;
        for row in &key.ends {
            match self.by_row.get(row) {
                Some(started) if !started.is_empty() => xids.extend(started.iter().cloned()),
                _ => complete = false,
            }
        }
        OracleKeyTransactions {
            complete: complete && !xids.is_empty(),
            xids: xids.into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expiration_uses_the_new_transaction_even_when_the_key_moves() {
        let mut e = Evidence::default();
        e.end(0, vec![1], "row".into());
        e.start(0, vec![2], "row".into(), vec![9]);
        for pk in [1, 2] {
            assert_eq!(
                e.resolve(0, &[pk]),
                OracleKeyTransactions {
                    xids: vec![vec![9]],
                    complete: true
                }
            );
        }
    }
    #[test]
    fn shared_scn_never_assigns_an_unrelated_rows_transaction() {
        let mut e = Evidence::default();
        e.end(0, vec![1], "old".into());
        e.start(0, vec![2], "other".into(), vec![8]);
        e.start(1, vec![3], "old".into(), vec![9]);
        assert_eq!(
            e.resolve(0, &[1]),
            OracleKeyTransactions {
                xids: vec![],
                complete: false
            }
        );
        assert!(!e.resolve(0, &[4]).complete);
    }
    #[test]
    fn all_transactions_and_unmatched_expirations_survive_coalescing() {
        let mut e = Evidence::default();
        e.start(0, vec![1], "a".into(), vec![7]);
        e.start(0, vec![1], "b".into(), vec![8]);
        e.end(0, vec![1], "missing".into());
        assert_eq!(
            e.resolve(0, &[1]),
            OracleKeyTransactions {
                xids: vec![vec![7], vec![8]],
                complete: false
            }
        );
    }
}
