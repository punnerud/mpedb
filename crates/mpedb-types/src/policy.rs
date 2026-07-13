//! Row-level-security policy definitions (DESIGN-MULTIDB.md §3).
//!
//! Policies are stored in the catalog sys-keyspace (NOT the schema bytes, so an
//! edit is an online COW commit and never registers as config drift) and
//! re-bound by the planner against each statement at prepare time. This module
//! is the shared, dependency-light representation + a bounds-checked codec used
//! by both the engine (storage) and the SQL planner (injection).
//!
//! **Honesty (DESIGN-MULTIDB.md §6):** in-file RLS is COOPERATIVE
//! defense-in-depth, not a boundary against a process that reads raw pages.

use crate::{Error, Result};

/// The command(s) a policy governs. `All` applies to every command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyCmd {
    All,
    Select,
    Insert,
    Update,
    Delete,
}

impl PolicyCmd {
    pub fn tag(self) -> u8 {
        match self {
            PolicyCmd::All => 0,
            PolicyCmd::Select => 1,
            PolicyCmd::Insert => 2,
            PolicyCmd::Update => 3,
            PolicyCmd::Delete => 4,
        }
    }

    pub fn from_tag(t: u8) -> Option<PolicyCmd> {
        Some(match t {
            0 => PolicyCmd::All,
            1 => PolicyCmd::Select,
            2 => PolicyCmd::Insert,
            3 => PolicyCmd::Update,
            4 => PolicyCmd::Delete,
            _ => return None,
        })
    }

    pub fn parse(s: &str) -> Option<PolicyCmd> {
        Some(match s.to_ascii_uppercase().as_str() {
            "ALL" => PolicyCmd::All,
            "SELECT" => PolicyCmd::Select,
            "INSERT" => PolicyCmd::Insert,
            "UPDATE" => PolicyCmd::Update,
            "DELETE" => PolicyCmd::Delete,
            _ => return None,
        })
    }

    /// Whether this policy governs `cmd` (an `All` policy governs everything;
    /// otherwise the commands must match).
    pub fn governs(self, cmd: PolicyCmd) -> bool {
        self == PolicyCmd::All || self == cmd
    }
}

/// One row-level-security policy. `name` is carried in the storage key; the
/// encoded value holds the rest. `using_src` gates *reads* (row visibility for
/// SELECT and the target set of UPDATE/DELETE); `check_src` gates *writes* (the
/// new row of INSERT/UPDATE). Both are SQL predicate SOURCE — re-bound by the
/// planner so their `current_setting()` refs share the statement's parameter
/// space (DESIGN-MULTIDB.md §3.2).
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyDef {
    pub name: String,
    pub command: PolicyCmd,
    /// `true` = PERMISSIVE (OR-combined), `false` = RESTRICTIVE (AND-combined).
    pub permissive: bool,
    pub using_src: Option<String>,
    pub check_src: Option<String>,
}

impl PolicyDef {
    /// Encode the stored value (the policy `name` lives in the key, not here).
    pub fn encode_value(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(8);
        b.push(self.command.tag());
        b.push(u8::from(self.permissive));
        put_opt_str(&mut b, self.using_src.as_deref());
        put_opt_str(&mut b, self.check_src.as_deref());
        b
    }

    /// Decode a stored value; `name` comes from the key. Bounds-checked:
    /// corrupt input yields [`Error::Corrupt`], never a panic.
    pub fn decode_value(name: String, bytes: &[u8]) -> Result<PolicyDef> {
        let mut pos = 0usize;
        let command = PolicyCmd::from_tag(take1(bytes, &mut pos)?)
            .ok_or_else(|| Error::Corrupt("bad policy command tag".into()))?;
        let permissive = take1(bytes, &mut pos)? != 0;
        let using_src = get_opt_str(bytes, &mut pos)?;
        let check_src = get_opt_str(bytes, &mut pos)?;
        if pos != bytes.len() {
            return Err(Error::Corrupt("trailing bytes in policy record".into()));
        }
        Ok(PolicyDef {
            name,
            command,
            permissive,
            using_src,
            check_src,
        })
    }
}

fn put_opt_str(b: &mut Vec<u8>, s: Option<&str>) {
    match s {
        None => b.push(0),
        Some(s) => {
            b.push(1);
            b.extend_from_slice(&(s.len() as u32).to_le_bytes());
            b.extend_from_slice(s.as_bytes());
        }
    }
}

fn take1(b: &[u8], pos: &mut usize) -> Result<u8> {
    let v = *b.get(*pos).ok_or_else(|| Error::Corrupt("policy record truncated".into()))?;
    *pos += 1;
    Ok(v)
}

fn get_opt_str(b: &[u8], pos: &mut usize) -> Result<Option<String>> {
    let present = take1(b, pos)?;
    if present == 0 {
        return Ok(None);
    }
    if *pos + 4 > b.len() {
        return Err(Error::Corrupt("policy record truncated (str len)".into()));
    }
    let len = u32::from_le_bytes(b[*pos..*pos + 4].try_into().unwrap()) as usize;
    *pos += 4;
    let end = pos.checked_add(len).ok_or_else(|| Error::Corrupt("policy str len overflow".into()))?;
    if end > b.len() {
        return Err(Error::Corrupt("policy record truncated (str body)".into()));
    }
    let s = std::str::from_utf8(&b[*pos..end])
        .map_err(|_| Error::Corrupt("policy source is not valid utf-8".into()))?
        .to_string();
    *pos = end;
    Ok(Some(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_value_round_trips() {
        let p = PolicyDef {
            name: "tenant_isolation".into(),
            command: PolicyCmd::All,
            permissive: false,
            using_src: Some("tenant = current_setting('app.tenant')".into()),
            check_src: None,
        };
        let bytes = p.encode_value();
        let back = PolicyDef::decode_value(p.name.clone(), &bytes).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn truncated_policy_is_corrupt_not_panic() {
        let p = PolicyDef {
            name: "x".into(),
            command: PolicyCmd::Select,
            permissive: true,
            using_src: Some("a = 1".into()),
            check_src: Some("b = 2".into()),
        };
        let bytes = p.encode_value();
        for cut in 0..bytes.len() {
            assert!(PolicyDef::decode_value("x".into(), &bytes[..cut]).is_err());
        }
        // A trailing byte is also rejected.
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(PolicyDef::decode_value("x".into(), &extra).is_err());
    }

    #[test]
    fn governs_matches_all_and_exact() {
        assert!(PolicyCmd::All.governs(PolicyCmd::Insert));
        assert!(PolicyCmd::Select.governs(PolicyCmd::Select));
        assert!(!PolicyCmd::Select.governs(PolicyCmd::Update));
    }
}
