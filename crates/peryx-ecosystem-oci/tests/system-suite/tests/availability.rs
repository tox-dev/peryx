mod harness {
    pub use peryx_test_support::{Cluster, MemberSpec, Node, Role};

    pub struct Topology {
        inner: peryx_test_support::Topology,
        has_index: bool,
    }

    impl Topology {
        pub fn single() -> Self {
            Self {
                inner: peryx_test_support::Topology::single().with_process_harness(process_harness()),
                has_index: false,
            }
        }

        pub fn ha(group: &str, members: Vec<MemberSpec>) -> Self {
            Self {
                inner: peryx_test_support::Topology::ha(group, members).with_process_harness(process_harness()),
                has_index: false,
            }
        }

        pub fn with_admin(self) -> Self {
            Self {
                inner: self.inner.with_admin(),
                ..self
            }
        }

        pub fn with_index_config(self, config: &str) -> Self {
            Self {
                inner: self.inner.with_index_config(config),
                has_index: true,
            }
        }

        pub fn with_write_ack_deadline(self, seconds: u64) -> Self {
            Self {
                inner: self.inner.with_write_ack_deadline(seconds),
                ..self
            }
        }

        pub fn start(&self) -> Result<Cluster, peryx_test_support::HarnessError> {
            if self.has_index {
                self.inner.start()
            } else {
                self.inner.clone().with_index_config("index = []").start()
            }
        }
    }

    fn process_harness() -> peryx_test_support::ProcessHarness {
        peryx_test_support::ProcessHarness::new(peryx_test_support::peryx_binary())
    }
}

#[path = "cases/failover.rs"]
mod failover;
