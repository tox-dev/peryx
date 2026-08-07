use peryx_storage::meta::{DriverTxn, MetaError, MetaStore, QuotaError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const PREFIX: &str = "oci/upload-session/";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UploadRecord {
    pub offset: u64,
    pub index: String,
    pub name: String,
    pub updated_at_unix: i64,
}

pub(crate) trait UploadStore {
    fn begin_upload(&self, session: &str, index: &str, name: &str, now: i64) -> Result<(), MetaError>;
    fn advance_upload(&self, session: &str, offset: u64, now: i64) -> Result<bool, MetaError>;
    fn upload_record(&self, session: &str) -> Result<Option<UploadRecord>, MetaError>;
    fn remove_upload(&self, session: &str) -> Result<bool, MetaError>;
    fn reclaim_uploads(&self, cutoff: i64, limit: usize) -> Result<Vec<String>, MetaError>;
    fn commit_driver_txn_closing_upload<T, E: From<MetaError>>(
        &self,
        session: Option<&str>,
        body: impl FnOnce(&mut DriverTxn) -> Result<(T, Vec<Vec<u8>>), E>,
    ) -> Result<T, E>;
    fn commit_driver_txn_with_quota_closing_upload<T, E>(
        &self,
        id: Uuid,
        session: Option<&str>,
        body: impl FnOnce(&mut DriverTxn) -> Result<(T, Vec<Vec<u8>>), E>,
    ) -> Result<T, E>
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

    fn reclaim_uploads(&self, cutoff: i64, limit: usize) -> Result<Vec<String>, MetaError> {
        self.remove_driver_values_if(PREFIX, limit, |value| {
            Ok(serde_json::from_slice::<UploadRecord>(value)?.updated_at_unix <= cutoff)
        })
        .map(|keys| {
            keys.into_iter()
                .map(|key| key.strip_prefix(PREFIX).unwrap_or(&key).to_owned())
                .collect()
        })
    }

    fn commit_driver_txn_closing_upload<T, E: From<MetaError>>(
        &self,
        session: Option<&str>,
        body: impl FnOnce(&mut DriverTxn) -> Result<(T, Vec<Vec<u8>>), E>,
    ) -> Result<T, E> {
        self.commit_driver_txn(close_session(session, body))
    }

    fn commit_driver_txn_with_quota_closing_upload<T, E>(
        &self,
        id: Uuid,
        session: Option<&str>,
        body: impl FnOnce(&mut DriverTxn) -> Result<(T, Vec<Vec<u8>>), E>,
    ) -> Result<T, E>
    where
        E: From<MetaError> + From<QuotaError>,
    {
        self.commit_driver_txn_with_quota(id, close_session(session, body))
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
