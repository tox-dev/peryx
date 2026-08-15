//! Authority-transfer audits survive consensus restarts. Keys combine authority and zero-padded commit
//! index so prefix scans preserve commit order and retries overwrite the same record.

use peryx_ha::{TransferAudit, TransferAuditStore};

use super::{MetaError, MetaStore, TRANSFER_AUDIT, open_optional_table};

fn range_end(authority: &str) -> String {
    format!("{authority}\u{1}")
}

impl MetaStore {
    /// # Errors
    /// Returns an error when the store write fails.
    pub fn record_transfer_audit(&self, audit: &TransferAudit) -> Result<(), MetaError> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(TRANSFER_AUDIT)?;
            table.insert(audit_key(audit).as_str(), serde_json::to_vec(audit)?.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Returns commits in index order or an empty list when the authority never moved.
    ///
    /// # Errors
    /// Returns an error when the store read fails or a record cannot be decoded.
    pub fn transfer_audits(&self, authority: &str) -> Result<Vec<TransferAudit>, MetaError> {
        let txn = self.db.begin_read()?;
        let Some(table) = open_optional_table(&txn, TRANSFER_AUDIT)? else {
            return Ok(Vec::new());
        };
        let prefix = format!("{authority}\u{0}");
        let mut audits = Vec::new();
        for entry in table.range(prefix.as_str()..range_end(authority).as_str())? {
            let (_, value) = entry?;
            audits.push(serde_json::from_slice(value.value())?);
        }
        Ok(audits)
    }
}

impl TransferAuditStore for MetaStore {
    type Error = MetaError;

    fn record_transfer_audit(&self, audit: &TransferAudit) -> Result<(), Self::Error> {
        Self::record_transfer_audit(self, audit)
    }

    fn transfer_audits(&self, authority: &str) -> Result<Vec<TransferAudit>, Self::Error> {
        Self::transfer_audits(self, authority)
    }
}

fn audit_key(audit: &TransferAudit) -> String {
    format!("{}\u{0}{:020}", audit.authority, audit.commit_index)
}
