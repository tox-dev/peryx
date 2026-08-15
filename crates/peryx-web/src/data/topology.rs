use crate::model::TopologySnapshot;

/// Uses the same role-filtered projection as the one-shot snapshot.
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
const TOPOLOGY_STREAM_URL: &str = "/+availability/topology/stream";

/// Must match the server's named event rather than the default `message` event.
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
const TOPOLOGY_STREAM_EVENT: &str = "topology";

/// Avoids badge flicker during the browser's normal reconnect window.
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
const RECONNECT_GRACE_MS: i32 = 400;

/// The availability topology snapshot, projected to the caller's class.
///
/// The server reads and projects `AppState`; the hydrated browser fetches `/+availability/topology`,
/// which projects the same fields. Both sides yield the identical `TopologySnapshot`.
///
/// # Errors
///
/// Returns a message when the snapshot cannot be reached or does not parse.
pub async fn load_topology() -> Result<TopologySnapshot, String> {
    #[cfg(feature = "ssr")]
    {
        Ok(crate::ssr::topology().await)
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        let request = async {
            let value = super::fetch_json_required("/+availability/topology")
                .await
                .map_err(|_| "The availability topology could not be reached.".to_owned())?;
            serde_json::from_value(value).map_err(|_| "The availability topology returned invalid data.".to_owned())
        };
        send_wrapper::SendWrapper::new(request).await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        Err("The availability topology is unavailable.".to_owned())
    }
}

/// Deserialize one streamed snapshot event body, so the browser hands the page the same
/// [`TopologySnapshot`] the one-shot loader would.
///
/// # Errors
///
/// Returns a message when the event body does not parse as a snapshot.
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
fn parse_topology_snapshot(data: &str) -> Result<TopologySnapshot, String> {
    serde_json::from_str(data).map_err(|_| "The availability topology stream sent invalid data.".to_owned())
}

/// A live subscription to the availability topology stream. Dropping it closes the underlying
/// `EventSource`, so a page that navigates away stops the browser reconnecting in the background.
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub struct TopologyStream {
    source: web_sys::EventSource,
    _on_snapshot: wasm_bindgen::closure::Closure<dyn FnMut(web_sys::MessageEvent)>,
    _on_open: wasm_bindgen::closure::Closure<dyn FnMut()>,
    _on_error: wasm_bindgen::closure::Closure<dyn FnMut()>,
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
impl Drop for TopologyStream {
    fn drop(&mut self) {
        self.source.close();
    }
}

/// Open the bounded topology stream and report snapshots and connection status.
///
/// Returns `None` when the browser cannot open an `EventSource`. Invalid events report `Stale`; reconnects
/// report `Connecting` until the browser gives up and reports `Offline`.
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
#[must_use]
pub fn subscribe_topology(
    on_snapshot: impl Fn(TopologySnapshot) + 'static,
    on_status: impl Fn(crate::model::StreamStatus) + 'static,
) -> Option<TopologyStream> {
    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::closure::Closure;

    use crate::model::StreamStatus;

    let source = web_sys::EventSource::new(TOPOLOGY_STREAM_URL).ok()?;
    let on_status: std::rc::Rc<dyn Fn(StreamStatus)> = std::rc::Rc::new(on_status);

    // Defer the `Reconnecting` badge behind a grace window so a routine, quickly-recovered drop never
    // flickers it; a definite status (live, stale, offline) cancels any deferred flip and applies at once.
    let window = web_sys::window()?;
    let pending = std::rc::Rc::new(std::cell::RefCell::new(None::<i32>));

    let cancel = {
        let window = window.clone();
        let pending = std::rc::Rc::clone(&pending);
        std::rc::Rc::new(move || {
            if let Some(handle) = pending.borrow_mut().take() {
                window.clear_timeout_with_handle(handle);
            }
        })
    };

    let apply: std::rc::Rc<dyn Fn(StreamStatus)> = {
        let cancel = std::rc::Rc::clone(&cancel);
        let on_status = std::rc::Rc::clone(&on_status);
        std::rc::Rc::new(move |status| {
            cancel();
            on_status(status);
        })
    };

    let fire_connecting = {
        let on_status = std::rc::Rc::clone(&on_status);
        let pending = std::rc::Rc::clone(&pending);
        Closure::<dyn FnMut()>::new(move || {
            *pending.borrow_mut() = None;
            on_status(StreamStatus::Connecting);
        })
    };

    let defer_connecting: std::rc::Rc<dyn Fn()> = {
        let cancel = std::rc::Rc::clone(&cancel);
        let pending = std::rc::Rc::clone(&pending);
        std::rc::Rc::new(move || {
            cancel();
            if let Ok(handle) = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                fire_connecting.as_ref().unchecked_ref(),
                RECONNECT_GRACE_MS,
            ) {
                *pending.borrow_mut() = Some(handle);
            }
        })
    };

    let snapshot_apply = std::rc::Rc::clone(&apply);
    let on_snapshot = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
        let Some(data) = event.data().as_string() else {
            return;
        };
        match parse_topology_snapshot(&data) {
            // A valid event proves the stream is delivering, so the badge turns live even if `onopen` has
            // not fired yet; a body that will not decode marks the render stale rather than dropping it
            // silently, so a protocol error can never freeze under a live badge.
            Ok(snapshot) => {
                snapshot_apply(StreamStatus::Live);
                on_snapshot(snapshot);
            }
            Err(_) => snapshot_apply(StreamStatus::Stale),
        }
    });
    source
        .add_event_listener_with_callback(TOPOLOGY_STREAM_EVENT, on_snapshot.as_ref().unchecked_ref())
        .ok()?;

    let open_apply = std::rc::Rc::clone(&apply);
    let on_open = Closure::<dyn FnMut()>::new(move || open_apply(StreamStatus::Live));
    source.set_onopen(Some(on_open.as_ref().unchecked_ref()));

    let errored_source = source.clone();
    let error_apply = std::rc::Rc::clone(&apply);
    let on_error = Closure::<dyn FnMut()>::new(move || {
        // `CLOSED` means the browser stopped retrying, so the feed is frozen and reported at once; any
        // other state is a transient drop it is already reconnecting through, so hold the badge and flip
        // to `Reconnecting` only if the grace window passes without a recovered event.
        if errored_source.ready_state() == web_sys::EventSource::CLOSED {
            error_apply(StreamStatus::Offline);
        } else {
            defer_connecting();
        }
    });
    source.set_onerror(Some(on_error.as_ref().unchecked_ref()));

    Some(TopologyStream {
        source,
        _on_snapshot: on_snapshot,
        _on_open: on_open,
        _on_error: on_error,
    })
}

#[cfg(test)]
#[path = "../../tests/unit/data/topology/tests.rs"]
mod tests;
