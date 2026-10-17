//! Versioned Oracle SCN positions bound to database/container incarnations,
//! application owner and original source schema. No connection secret is used.
use crate::oracle::{carrier_tables, OracleTable};
use mpedb_types::{Error, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OracleIdentity {
    pub database_id: u64,
    pub resetlogs_scn: u64,
    pub container_guid: [u8; 16],
    /// None for CDB root; Some(0) is a valid initial PDB incarnation.
    pub pdb_incarnation: Option<u64>,
    pub owner: String,
    pub session_user: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OraclePosition {
    realm: [u8; 32],
    scn: u64,
}
impl OraclePosition {
    pub fn new(identity: &OracleIdentity, tables: &[OracleTable], scn: u64) -> Result<Self> {
        if scn == 0 {
            return Err(Error::Config(
                "Oracle cursor requires a successful snapshot SCN, never zero".into(),
            ));
        }
        Ok(Self {
            realm: realm(identity, tables)?,
            scn,
        })
    }
    pub fn scn(&self) -> u64 {
        self.scn
    }
    /// Format 1: version byte, realm BLAKE3 digest, SCN big-endian u64.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(41);
        bytes.push(1);
        bytes.extend_from_slice(&self.realm);
        bytes.extend_from_slice(&self.scn.to_be_bytes());
        bytes
    }
    pub fn decode(bytes: &[u8], identity: &OracleIdentity, tables: &[OracleTable]) -> Result<Self> {
        if bytes.len() != 41 || bytes[0] != 1 {
            return Err(Error::Corrupt(
                "Oracle cursor has an invalid version or length".into(),
            ));
        }
        let expected = realm(identity, tables)?;
        if bytes[1..33] != expected {
            return Err(Error::Unsupported("Oracle cursor belongs to another database incarnation, owner or schema scope; reimport is required".into()));
        }
        let scn = u64::from_be_bytes(bytes[33..].try_into().expect("exact cursor length checked"));
        if scn == 0 {
            return Err(Error::Corrupt(
                "Oracle cursor contains an invalid zero SCN".into(),
            ));
        }
        Ok(Self {
            realm: expected,
            scn,
        })
    }
}
fn realm(identity: &OracleIdentity, tables: &[OracleTable]) -> Result<[u8; 32]> {
    if identity.database_id == 0
        || identity.container_guid == [0; 16]
        || identity.owner.is_empty()
        || identity.owner.len() > 128
        || identity.session_user.is_empty()
        || identity.session_user.len() > 128
    {
        return Err(Error::Config(
            "Oracle cursor requires a valid database/container identity and owner".into(),
        ));
    }
    let ordered = carrier_tables(tables)?;
    let mut h = blake3::Hasher::new();
    h.update(b"MPEdb Oracle cursor realm v1\0");
    h.update(&identity.database_id.to_be_bytes());
    h.update(&identity.resetlogs_scn.to_be_bytes());
    h.update(&identity.container_guid);
    h.update(&[u8::from(identity.pdb_incarnation.is_some())]);
    h.update(&identity.pdb_incarnation.unwrap_or(0).to_be_bytes());
    frame(&mut h, identity.owner.as_bytes());
    frame(&mut h, identity.session_user.as_bytes());
    h.update(&crate::oracle::build_schema(tables)?.hash());
    h.update(&(ordered.len() as u64).to_be_bytes());
    for table in ordered {
        if table.owner != identity.owner {
            return Err(Error::Config(
                "Oracle cursor scope contains another application owner".into(),
            ));
        }
        frame(&mut h, table.name.as_bytes());
        h.update(&table.object_id.to_be_bytes());
        frame(&mut h, table.ddl_source.as_bytes());
    }
    Ok(*h.finalize().as_bytes())
}
fn frame(h: &mut blake3::Hasher, bytes: &[u8]) {
    h.update(&(bytes.len() as u64).to_be_bytes());
    h.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity() -> OracleIdentity {
        OracleIdentity {
            database_id: 7,
            resetlogs_scn: 9,
            container_guid: [1; 16],
            pdb_incarnation: Some(0),
            owner: "APP".into(),
            session_user: "APP".into(),
        }
    }
    #[test]
    fn cursor_is_strict_monotone_and_bound_to_incarnation_and_owner() {
        let original = identity();
        let p = OraclePosition::new(&original, &[], 100).unwrap();
        let bytes = p.encode();
        assert_eq!(OraclePosition::decode(&bytes, &original, &[]).unwrap(), p);
        assert!(bytes < OraclePosition::new(&original, &[], 101).unwrap().encode());
        for i in 0..bytes.len() {
            assert!(OraclePosition::decode(&bytes[..i], &original, &[]).is_err());
        }
        let mut extended = bytes.clone();
        extended.push(0);
        assert!(OraclePosition::decode(&extended, &original, &[]).is_err());
        let mut invalid = bytes.clone();
        invalid[0] = 2;
        assert!(OraclePosition::decode(&invalid, &original, &[]).is_err());
        invalid = bytes.clone();
        invalid[33..].fill(0);
        assert!(OraclePosition::decode(&invalid, &original, &[]).is_err());
        assert!(OraclePosition::new(&original, &[], 0).is_err());
        for variant in 0..6 {
            let mut changed = original.clone();
            match variant {
                0 => changed.database_id += 1,
                1 => changed.resetlogs_scn += 1,
                2 => changed.container_guid[0] += 1,
                3 => changed.pdb_incarnation = None,
                4 => changed.owner = "OTHER".into(),
                _ => changed.session_user = "OTHER".into(),
            }
            assert!(OraclePosition::decode(&bytes, &changed, &[]).is_err());
        }
    }
}
