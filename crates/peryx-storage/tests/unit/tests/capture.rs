use std::cell::RefCell;
use std::io::{Read as _, Seek as _};
use std::sync::OnceLock;

thread_local! {
    /// Where this thread's events go while it holds a [`Captured`], and nowhere otherwise.
    static CAPTURE: RefCell<Option<std::fs::File>> = const { RefCell::new(None) };
}

/// Hands each event to the capture belonging to the thread that raised it, so one subscriber serves
/// every test at once. Both writers it returns are standard ones, which keeps the routing decision as
/// the only behaviour here.
struct ThreadCapture;

impl tracing_subscriber::fmt::MakeWriter<'_> for ThreadCapture {
    type Writer = Box<dyn std::io::Write>;

    fn make_writer(&self) -> Self::Writer {
        CAPTURE.with_borrow(|capture| match capture {
            Some(file) => Box::new(file.try_clone().expect("a capture file clones")) as Self::Writer,
            None => Box::new(std::io::sink()),
        })
    }
}

/// The events this thread raises, until it drops.
///
/// The subscriber is installed once for the whole binary rather than per test, because a callsite
/// decides its interest the first time any thread executes it and every thread reads that decision
/// afterwards. A per-test subscriber leaves that decision to whichever thread arrives first: one
/// holding no subscriber resolves the callsite against nothing, caches `never`, and silences the
/// callsite for the tests that are asserting on it. A subscriber that outlives every test cannot be
/// the one missing when that question is asked.
pub struct Captured(std::fs::File);

impl Captured {
    pub fn install() -> Self {
        static SUBSCRIBER: OnceLock<()> = OnceLock::new();
        SUBSCRIBER.get_or_init(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_writer(ThreadCapture)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("the test binary installs one global subscriber");
        });
        let file = tempfile::tempfile().expect("a capture file opens");
        CAPTURE.with_borrow_mut(|slot| *slot = Some(file.try_clone().expect("a capture file clones")));
        Self(file)
    }

    pub fn output(&self) -> String {
        let mut reader = self.0.try_clone().expect("a capture file clones");
        reader.rewind().expect("a capture file rewinds");
        let mut output = String::new();
        reader
            .read_to_string(&mut output)
            .expect("the fmt subscriber writes utf-8");
        output
    }
}

impl Drop for Captured {
    fn drop(&mut self) {
        CAPTURE.with_borrow_mut(|slot| *slot = None);
    }
}
