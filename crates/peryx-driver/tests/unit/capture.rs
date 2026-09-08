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
/// callsite for the tests asserting on it.
///
/// It admits every level because that decision is cached for the binary. Capping it at the level
/// these captures read would resolve a lower callsite to `never` for the whole process, and the
/// tests here that install their own subscriber to read `trace` events would stop seeing them.
pub struct Captured(std::fs::File);

impl Captured {
    pub fn install() -> Self {
        static SUBSCRIBER: OnceLock<()> = OnceLock::new();
        SUBSCRIBER.get_or_init(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_max_level(tracing::Level::TRACE)
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

/// The interleaving that silences a per-test capture: a thread holding no subscriber is the first to
/// execute a callsite, which is when its interest is decided for every thread that follows. A capture
/// that only exists for the length of one test is absent at that moment; the binary's subscriber is
/// not, so the event still reaches the thread asserting on it.
#[test]
fn test_a_capture_survives_a_neighbour_reaching_the_callsite_first() {
    let captured = Captured::install();
    std::thread::spawn(unsubscribed_first_event).join().unwrap();
    unsubscribed_first_event();

    assert!(
        captured.output().contains("callsite registered by a bare thread"),
        "a thread holding no subscriber decided this callsite for every thread that followed"
    );
}

/// A callsite of its own, so the test asserts on a first execution rather than on one some earlier
/// test already resolved.
fn unsubscribed_first_event() {
    tracing::warn!(target: "peryx_driver::capture", "callsite registered by a bare thread");
}
