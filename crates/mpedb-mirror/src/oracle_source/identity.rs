//! Database and PDB incarnation identity from public Oracle views/functions.
use super::*;
use crate::oracle_cursor::OracleIdentity;
impl OracleSource {
    pub(crate) fn import_incarnation(&self) -> Result<[u8; 16]> {
        let bytes: Vec<u8> = self
            .connection
            .query_row_as("SELECT SYS_GUID() FROM dual", &[])
            .map_err(|e| err("import incarnation", e))?;
        let id: [u8; 16] = bytes
            .try_into()
            .map_err(|_| Error::Corrupt("Oracle returned an invalid import GUID".into()))?;
        if id == [0; 16] {
            return Err(Error::Corrupt("Oracle returned a zero import GUID".into()));
        }
        Ok(id)
    }

    /// Requires SELECT on SYS.V_$DATABASE and SYS.V_$PDB_INCARNATION.
    /// Refuses unavailable identity; database names or DSNs are not substitutes.
    pub fn identity(&self) -> Result<OracleIdentity> {
        let row = self
            .connection
            .query_row("SELECT dbid,resetlogs_change# FROM v$database", &[])
            .map_err(|e| {
                err(
                    "database incarnation identity (SELECT on SYS.V_$DATABASE required)",
                    e,
                )
            })?;
        let database_id = get(&row, 0)?;
        let resetlogs_scn = get(&row, 1)?;
        let guid:String=self.connection.query_row_as("SELECT RAWTOHEX(CON_ID_TO_GUID(TO_NUMBER(SYS_CONTEXT('USERENV','CON_ID')))) FROM dual",&[]).map_err(|e|err("container GUID",e))?;
        if guid.len() != 32 || !guid.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Corrupt(
                "Oracle returned an invalid container GUID".into(),
            ));
        }
        let mut container_guid = [0; 16];
        for (i, byte) in container_guid.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&guid[2 * i..2 * i + 2], 16)
                .map_err(|_| Error::Corrupt("Oracle container GUID is not hexadecimal".into()))?;
        }
        let container_id: u64 = self
            .connection
            .query_row_as(
                "SELECT TO_NUMBER(SYS_CONTEXT('USERENV','CON_ID')) FROM dual",
                &[],
            )
            .map_err(|e| err("container id", e))?;
        let pdb_incarnation = if container_id > 1 {
            Some(self.connection.query_row_as("SELECT pdb_incarnation# FROM v$pdb_incarnation WHERE status='CURRENT' AND con_id=TO_NUMBER(SYS_CONTEXT('USERENV','CON_ID'))",&[]).map_err(|e|err("PDB incarnation identity (SELECT on SYS.V_$PDB_INCARNATION required)",e))?)
        } else {
            None
        };
        let session_user = self
            .connection
            .query_row_as(
                "SELECT SYS_CONTEXT('USERENV','SESSION_USER') FROM dual",
                &[],
            )
            .map_err(|e| err("authenticated source account", e))?;
        Ok(OracleIdentity {
            database_id,
            resetlogs_scn,
            container_guid,
            pdb_incarnation,
            owner: self.owner.clone(),
            session_user,
        })
    }
}
