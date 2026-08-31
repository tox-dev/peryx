//! Finalization commits authoritative rows, replication journal entries, the retry outcome, and the
//! staging-intent transition in one transaction. Checking the outcome inside that transaction prevents
//! concurrent retries from publishing twice.

use redb::ReadableTable as _;

use super::ingress_intent::{IntentPhase, StagedIntent};
use super::operation_outcome::{OperationOutcomeRecord, OperationState};
use super::{DriverTxn, INGRESS_INTENT, MetaError, MetaStore, OPERATION_OUTCOME};

#[derive(Debug, Clone, Copy)]
pub struct FinalizedWrite<'a> {
    pub operation: &'a str,
    /// An empty key leaves the intent ledger unchanged.
    pub intent_key: &'a str,
    pub response: &'a [u8],
    /// `None` retains the outcome without a time bound.
    pub expiry_unix: Option<i64>,
    pub now: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalizeOutcome {
    /// The transaction committed the write for the first time.
    Published,
    /// Replays a prior terminal outcome without another write.
    Replayed(OperationOutcomeRecord),
}

pub(super) enum FinalizeFlow<E> {
    User(E),
    /// Carries the outcome the transaction observed. Retention prunes expired terminal rows, so reading
    /// the row again after the transaction ends can find it already deleted.
    RaceReplay(OperationOutcomeRecord),
}

impl<E: From<MetaError>> From<MetaError> for FinalizeFlow<E> {
    fn from(err: MetaError) -> Self {
        Self::User(E::from(err))
    }
}

impl MetaStore {
    /// Commits `body`, its journal, the published outcome, and the admitted intent transition atomically.
    /// A terminal outcome discards staged changes and returns [`FinalizeOutcome::Replayed`].
    ///
    /// # Errors
    /// Returns the body's error, or a store error mapped into it, if the transaction fails to open,
    /// read, write, or commit.
    pub fn commit_finalized_write<E: From<MetaError>>(
        &self,
        write: FinalizedWrite<'_>,
        body: impl FnOnce(&mut DriverTxn) -> Result<Vec<Vec<u8>>, E>,
    ) -> Result<FinalizeOutcome, E> {
        let committed = self.commit_driver_txn_at(
            None,
            None,
            true,
            |txn, ()| stamp_finalized(txn, &write),
            |driver| body(driver).map(|journal| ((), journal)).map_err(FinalizeFlow::User),
        );
        resolve_finalize(committed)
    }
}

fn resolve_finalize<E>(committed: Result<(), FinalizeFlow<E>>) -> Result<FinalizeOutcome, E> {
    match committed {
        Ok(()) => Ok(FinalizeOutcome::Published),
        Err(FinalizeFlow::User(err)) => Err(err),
        Err(FinalizeFlow::RaceReplay(record)) => Ok(FinalizeOutcome::Replayed(record)),
    }
}

pub(super) fn stamp_finalized<E: From<MetaError>>(
    txn: &redb::WriteTransaction,
    write: &FinalizedWrite<'_>,
) -> Result<(), FinalizeFlow<E>> {
    let mut outcomes = txn.open_table(OPERATION_OUTCOME).map_err(MetaError::from)?;
    let existing = outcomes
        .get(write.operation)
        .map_err(MetaError::from)?
        .map(|value| serde_json::from_slice::<OperationOutcomeRecord>(value.value()))
        .transpose()
        .map_err(MetaError::from)?;
    if let Some(record) = existing.filter(|record| record.state.is_terminal()) {
        return Err(FinalizeFlow::RaceReplay(record));
    }
    let record = OperationOutcomeRecord {
        state: OperationState::Published,
        response: write.response.to_vec(),
        expiry_unix: write.expiry_unix,
        updated_at_unix: write.now,
    };
    let encoded = serde_json::to_vec(&record).map_err(MetaError::from)?;
    outcomes
        .insert(write.operation, encoded.as_slice())
        .map_err(MetaError::from)?;
    advance_intent_to_admitted(txn, write.intent_key, write.now)
}

fn advance_intent_to_admitted<E: From<MetaError>>(
    txn: &redb::WriteTransaction,
    intent_key: &str,
    now: i64,
) -> Result<(), FinalizeFlow<E>> {
    if intent_key.is_empty() {
        return Ok(());
    }
    let mut intents = txn.open_table(INGRESS_INTENT).map_err(MetaError::from)?;
    let existing = intents
        .get(intent_key)
        .map_err(MetaError::from)?
        .map(|value| serde_json::from_slice::<StagedIntent>(value.value()))
        .transpose()
        .map_err(MetaError::from)?;
    if let Some(mut record) = existing.filter(|record| IntentPhase::Admitted > record.phase) {
        record.phase = IntentPhase::Admitted;
        record.updated_at_unix = now;
        let encoded = serde_json::to_vec(&record).map_err(MetaError::from)?;
        intents
            .insert(intent_key, encoded.as_slice())
            .map_err(MetaError::from)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/meta/finalize_fault_tests.rs"]
mod fault_tests;

#[cfg(test)]
#[path = "../../tests/unit/meta/finalize_race_tests.rs"]
mod race_tests;
