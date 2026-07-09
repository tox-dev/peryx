#![allow(
    clippy::future_not_send,
    reason = "browser fetch futures are single-threaded by nature; callers wrap them in SendWrapper"
)]

use crate::model::UiSnapshot;

/// The dashboard snapshot.
pub async fn load_snapshot() -> UiSnapshot {
    #[cfg(feature = "ssr")]
    {
        crate::ssr::snapshot()
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async {
            super::fetch_json("/+status")
                .await
                .map_or_else(UiSnapshot::default, |value| UiSnapshot::from_status(&value))
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        UiSnapshot::default()
    }
}

/// The admin status snapshot, including bounded metadata summaries.
pub async fn load_admin_snapshot() -> UiSnapshot {
    #[cfg(feature = "ssr")]
    {
        crate::ssr::admin_snapshot()
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async {
            super::fetch_json("/+status?details=admin")
                .await
                .map_or_else(UiSnapshot::default, |value| UiSnapshot::from_status(&value))
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        UiSnapshot::default()
    }
}
