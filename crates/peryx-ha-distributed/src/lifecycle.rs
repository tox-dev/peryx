use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Lifecycle {
    active: watch::Sender<bool>,
    failure: watch::Sender<Option<String>>,
    cancellation: CancellationToken,
}

pub struct FailureReceiver(watch::Receiver<Option<String>>);

impl Lifecycle {
    pub fn new() -> (Self, FailureReceiver) {
        let (active, _) = watch::channel(false);
        let (failure, receiver) = watch::channel(None);
        (
            Self {
                active,
                failure,
                cancellation: CancellationToken::new(),
            },
            FailureReceiver(receiver),
        )
    }

    pub async fn activated(&self) -> bool {
        let mut active = self.active.subscribe();
        loop {
            if *active.borrow_and_update() {
                return true;
            }
            tokio::select! {
                () = self.cancellation.cancelled() => return false,
                _ = active.changed() => {}
            }
        }
    }

    pub fn activate(&self) {
        self.active.send_replace(true);
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub fn fail(&self, message: impl Into<String>) {
        if self.is_cancelled() {
            return;
        }
        let message = message.into();
        self.failure.send_if_modified(|failure| {
            if failure.is_some() {
                return false;
            }
            *failure = Some(message);
            true
        });
    }
}

impl FailureReceiver {
    pub async fn wait(&mut self) -> String {
        loop {
            let failure = self.0.borrow_and_update().clone();
            if let Some(failure) = failure {
                return failure;
            }
            if self.0.changed().await.is_err() {
                return "distributed availability supervision stopped".to_owned();
            }
        }
    }
}
