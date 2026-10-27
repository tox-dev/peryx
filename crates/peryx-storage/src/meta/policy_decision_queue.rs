//! A background writer that commits many policy decisions in one redb transaction.
//!
//! Redb admits one writer. A decision committed from the request path took that lock once per
//! listed file, so a burst of cold project pages queued every runtime worker behind it. The queue
//! moves the commit to one thread and amortizes it over a batch.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, channel, sync_channel};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::JoinHandle;

use super::MetaDatabase;
use super::policy_decision::{BatchWriter, MAX_DECISION_HISTORY, OwnedDecision, prune_history};

enum Message {
    Record(OwnedDecision),
    Control(Control),
}

enum Control {
    Flush(Sender<()>),
    Shutdown(Sender<()>),
}

impl Control {
    /// Reports whether the writer must stop, after telling the waiter its earlier records are applied.
    fn acknowledge(self) -> bool {
        match self {
            Self::Flush(done) => {
                _ = done.send(());
                false
            }
            Self::Shutdown(done) => {
                _ = done.send(());
                true
            }
        }
    }
}

/// The writer is gone, so the decision was not queued.
#[derive(Debug)]
pub(super) struct QueueStopped;

pub(super) struct DecisionQueue {
    capacity: usize,
    max_batch: usize,
    history_limit: usize,
    /// Starts on the first decision, so a store that never records one owns no thread.
    sender: OnceLock<SyncSender<Message>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    /// Counts decisions handed to the writer. Equal to `applied` when nothing is pending, which lets
    /// a read skip the round trip to the writer.
    enqueued: AtomicU64,
    /// Counts decisions the writer has finished with, whether it committed them or logged a failure.
    applied: Arc<AtomicU64>,
}

impl DecisionQueue {
    pub(super) fn new() -> Self {
        // The capacity bounds memory while the writer lags request traffic, and a full queue blocks the
        // sender. The batch maximum bounds how long one transaction holds redb's only writer lock.
        Self::with_limits(8_192, 1_024, MAX_DECISION_HISTORY)
    }

    pub(super) fn with_limits(capacity: usize, max_batch: usize, history_limit: usize) -> Self {
        Self {
            capacity,
            max_batch,
            history_limit,
            sender: OnceLock::new(),
            thread: Mutex::new(None),
            enqueued: AtomicU64::new(0),
            applied: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Queues a decision, blocking while the queue is full.
    ///
    /// # Panics
    /// Panics if the OS refuses to spawn the writer thread.
    pub(super) fn enqueue(&self, db: &Arc<MetaDatabase>, decision: OwnedDecision) -> Result<(), QueueStopped> {
        let sender = self.sender.get_or_init(|| self.spawn(db));
        self.enqueued.fetch_add(1, Ordering::AcqRel);
        Self::send(sender, Message::Record(decision)).inspect_err(|_| {
            // The caller writes this decision directly, so count it as handled.
            self.applied.fetch_add(1, Ordering::AcqRel);
        })
    }

    /// Returns once every decision queued before the call is committed.
    ///
    /// The caller must not hold a write transaction: the writer would wait on its lock while the
    /// caller waits on the writer.
    pub(super) fn wait_applied(&self) {
        let target = self.enqueued.load(Ordering::Acquire);
        if self.applied.load(Ordering::Acquire) >= target {
            return;
        }
        if let Some(sender) = self.sender.get() {
            Self::round_trip(sender, Control::Flush);
        }
    }

    /// Commits what is queued, then stops and joins the writer.
    pub(super) fn shutdown(&self) {
        if let Some(sender) = self.sender.get() {
            Self::round_trip(sender, Control::Shutdown);
        }
        self.join();
    }

    fn spawn(&self, db: &Arc<MetaDatabase>) -> SyncSender<Message> {
        let (sender, receiver) = sync_channel(self.capacity);
        let writer = Writer {
            db: Arc::clone(db),
            applied: Arc::clone(&self.applied),
            max_batch: self.max_batch,
            history_limit: self.history_limit,
        };
        // The writer logs from a thread the caller's scoped subscriber does not reach.
        let dispatch = tracing::dispatcher::get_default(tracing::Dispatch::clone);
        let handle = std::thread::Builder::new()
            .name("peryx-policy-decisions".to_owned())
            .spawn(move || tracing::dispatcher::with_default(&dispatch, || writer.run(&receiver)))
            .expect("spawn policy decision writer");
        *self.thread.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);
        sender
    }

    fn send(sender: &SyncSender<Message>, message: Message) -> Result<(), QueueStopped> {
        match sender.try_send(message) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(message)) => {
                tracing::debug!("policy decision queue is full; the sender waits for the writer");
                sender.send(message).map_err(|_| QueueStopped)
            }
            Err(TrySendError::Disconnected(_)) => Err(QueueStopped),
        }
    }

    fn round_trip(sender: &SyncSender<Message>, control: fn(Sender<()>) -> Control) {
        let (done, finished) = channel();
        if Self::send(sender, Message::Control(control(done))).is_ok() {
            // A writer that stopped drops the sender, which ends the wait.
            _ = finished.recv();
        }
    }

    fn join(&self) {
        let handle = self.thread.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(handle) = handle {
            _ = handle.join();
        }
    }
}

impl Drop for DecisionQueue {
    /// The writer holds the database, so the last store handle joins it before a caller reopens the
    /// file, and no thread outlives its store.
    fn drop(&mut self) {
        drop(self.sender.take());
        self.join();
    }
}

struct Writer {
    db: Arc<MetaDatabase>,
    applied: Arc<AtomicU64>,
    max_batch: usize,
    history_limit: usize,
}

/// What one batch took off the queue, kept outside the fallible body so a failure still counts it.
struct Progress {
    processed: usize,
    control: Option<Control>,
}

impl Writer {
    fn run(&self, receiver: &Receiver<Message>) {
        while let Ok(message) = receiver.recv() {
            let control = match message {
                Message::Record(first) => self.commit_batch(&first, receiver),
                Message::Control(control) => Some(control),
            };
            if let Some(control) = control
                && control.acknowledge()
            {
                return;
            }
        }
    }

    /// Commits the first decision with every one already queued behind it, and returns the control
    /// message that ended the batch. The control message ran after the records before it.
    fn commit_batch(&self, first: &OwnedDecision, receiver: &Receiver<Message>) -> Option<Control> {
        let mut progress = Progress {
            processed: 1,
            control: None,
        };
        if let Err(error) = self.write_batch(first, receiver, &mut progress) {
            tracing::error!(decisions = progress.processed, %error, "failed to record policy decisions");
        }
        self.applied.fetch_add(progress.processed as u64, Ordering::AcqRel);
        progress.control
    }

    /// Takes the write lock before draining, so records that arrive while it waits join the batch.
    fn write_batch(
        &self,
        first: &OwnedDecision,
        receiver: &Receiver<Message>,
        progress: &mut Progress,
    ) -> Result<(), super::MetaError> {
        let txn = self.db.begin_write()?;
        let mut batch = BatchWriter::new(&txn)?;
        batch.record(&first.as_new(), first.input_generation)?;
        while progress.processed < self.max_batch {
            match receiver.try_recv() {
                Ok(Message::Record(next)) => {
                    progress.processed += 1;
                    batch.record(&next.as_new(), next.input_generation)?;
                }
                Ok(Message::Control(control)) => {
                    progress.control = Some(control);
                    break;
                }
                Err(_) => break,
            }
        }
        batch.finish()?;
        prune_history(&txn, self.history_limit)?;
        txn.commit()?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/meta/policy_decision_queue_tests.rs"]
mod tests;
