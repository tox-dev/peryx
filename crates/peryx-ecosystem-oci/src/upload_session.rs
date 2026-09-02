use peryx_storage::meta::{DriverCommit, DriverTxn, MetaError, MetaStore, QuotaError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const PREFIX: &str = "oci/upload-session/";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadRecord {
    pub offset: u64,
    pub index: String,
    pub name: String,
    pub updated_at_unix: i64,
}

pub trait UploadStore {
    fn begin_upload(&self, session: &str, index: &str, name: &str, now: i64) -> Result<(), MetaError>;
    fn advance_upload(&self, session: &str, offset: u64, now: i64) -> Result<bool, MetaError>;
    fn upload_record(&self, session: &str) -> Result<Option<UploadRecord>, MetaError>;
    fn remove_upload(&self, session: &str) -> Result<bool, MetaError>;
    /// Sessions untouched since `cutoff`, at most `limit` of them, left where they are.
    ///
    /// Selection deletes nothing. The row is the only durable link from an upload id back to its
    /// staged bytes, so a caller that removed it before discarding the stage would strand those bytes
    /// with nothing left to find them by, across a restart included.
    fn expired_uploads(&self, cutoff: i64, limit: usize) -> Result<Vec<String>, MetaError>;
    /// Remove `session` while it is still untouched since `cutoff`, reporting whether it went.
    ///
    /// A request that touched the session after it was selected has made it active again, and an
    /// active session keeps its row.
    fn remove_expired_upload(&self, session: &str, cutoff: i64) -> Result<bool, MetaError>;
    /// Reports the journal serial the transaction committed at, which a write acknowledgement waits on
    /// as its metadata evidence.
    fn commit_driver_txn_closing_upload<T, E: From<MetaError>>(
        &self,
        session: Option<&str>,
        body: impl FnOnce(&mut DriverTxn) -> Result<(T, Vec<Vec<u8>>), E>,
    ) -> Result<DriverCommit<T>, E>;
    fn commit_driver_txn_with_quota_if_closing_upload<T, E>(
        &self,
        id: Uuid,
        session: Option<&str>,
        commit: impl FnOnce(&T) -> bool,
        body: impl FnOnce(&mut DriverTxn) -> Result<(T, Vec<Vec<u8>>), E>,
    ) -> Result<DriverCommit<T>, E>
    where
        E: From<MetaError> + From<QuotaError>;
}

impl UploadStore for MetaStore {
    fn begin_upload(&self, session: &str, index: &str, name: &str, now: i64) -> Result<(), MetaError> {
        self.put_driver_value(
            &key(session),
            &serde_json::to_vec(&UploadRecord {
                offset: 0,
                index: index.to_owned(),
                name: name.to_owned(),
                updated_at_unix: now,
            })?,
        )
    }

    fn advance_upload(&self, session: &str, offset: u64, now: i64) -> Result<bool, MetaError> {
        self.update_driver_value(&key(session), |value| {
            let Some(value) = value else {
                return Ok((None, false));
            };
            let mut record: UploadRecord = serde_json::from_slice(value)?;
            record.offset = offset;
            record.updated_at_unix = now;
            Ok((Some(serde_json::to_vec(&record)?), true))
        })
    }

    fn upload_record(&self, session: &str) -> Result<Option<UploadRecord>, MetaError> {
        self.get_driver_value(&key(session))?
            .map(|value| serde_json::from_slice(&value).map_err(MetaError::from))
            .transpose()
    }

    fn remove_upload(&self, session: &str) -> Result<bool, MetaError> {
        self.delete_driver_value(&key(session))
    }

    fn expired_uploads(&self, cutoff: i64, limit: usize) -> Result<Vec<String>, MetaError> {
        let mut expired: Vec<String> = Vec::new();
        self.scan_driver_prefix::<MetaError>(PREFIX, |key, value| {
            if expired.len() < limit && serde_json::from_slice::<UploadRecord>(value)?.updated_at_unix <= cutoff {
                expired.push(key.strip_prefix(PREFIX).unwrap_or(key).to_owned());
            }
            Ok(())
        })?;
        Ok(expired)
    }

    fn remove_expired_upload(&self, session: &str, cutoff: i64) -> Result<bool, MetaError> {
        self.update_driver_value(&key(session), |value| {
            let Some(value) = value else {
                return Ok((None, false));
            };
            if serde_json::from_slice::<UploadRecord>(value)?.updated_at_unix > cutoff {
                return Ok((Some(value.to_vec()), false));
            }
            Ok((None, true))
        })
    }

    fn commit_driver_txn_closing_upload<T, E: From<MetaError>>(
        &self,
        session: Option<&str>,
        body: impl FnOnce(&mut DriverTxn) -> Result<(T, Vec<Vec<u8>>), E>,
    ) -> Result<DriverCommit<T>, E> {
        self.commit_driver_txn_with_commit(close_session(session, body))
    }

    fn commit_driver_txn_with_quota_if_closing_upload<T, E>(
        &self,
        id: Uuid,
        session: Option<&str>,
        commit: impl FnOnce(&T) -> bool,
        body: impl FnOnce(&mut DriverTxn) -> Result<(T, Vec<Vec<u8>>), E>,
    ) -> Result<DriverCommit<T>, E>
    where
        E: From<MetaError> + From<QuotaError>,
    {
        self.commit_driver_txn_with_quota_if_commit(id, commit, close_session(session, body))
    }
}

fn key(session: &str) -> String {
    format!("{PREFIX}{session}")
}

fn close_session<'a, T, E: From<MetaError>>(
    session: Option<&'a str>,
    body: impl FnOnce(&mut DriverTxn) -> Result<(T, Vec<Vec<u8>>), E> + 'a,
) -> impl FnOnce(&mut DriverTxn) -> Result<(T, Vec<Vec<u8>>), E> + 'a {
    move |txn| {
        let result = body(txn)?;
        if let Some(session) = session {
            txn.remove_local(&key(session))?;
        }
        Ok(result)
    }
}
