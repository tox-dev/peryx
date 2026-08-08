#![allow(
    clippy::must_use_candidate,
    reason = "the #[component] macro consumes attributes, so #[must_use] cannot reach the generated functions"
)]

use leptos::prelude::*;
use peryx_core::NodeRole;

use crate::data::load_topology;
use crate::model::{
    HealthLabel, LocalNode, RoleFilter, StreamStatus, TopologyNode, TopologySnapshot, format_instant, liveness_health,
    mode_label, role_label, stream_status_label,
};

#[component]
pub fn AvailabilityTopology() -> impl IntoView {
    let topology = Resource::new(|| (), |()| load_topology());
    view! {
        <section class="page topology-page">
            <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
                {move || Suspend::new(async move {
                    match topology.await {
                        Ok(snapshot) => view! { <TopologyBody snapshot /> }.into_any(),
                        Err(error) => view! { <p class="error" role="alert">{error}</p> }.into_any(),
                    }
                })}
            </Suspense>
        </section>
    }
}

/// The snapshot-derived values the page shows around the roster: the mode, group, and roster size in
/// the summary, the per-snapshot cap, whether any node exposes an address, and the roster's roles. Every
/// field comes from one snapshot, so a streamed roster change recomputes them together and never strands
/// a stale count, group, or column above fresh rows.
struct TopologySummary {
    mode: &'static str,
    group: Option<String>,
    node_count: usize,
    shown: usize,
    capped: bool,
    show_address: bool,
    node_roles: Vec<NodeRole>,
}

impl TopologySummary {
    fn derive(snapshot: &TopologySnapshot) -> Self {
        let shown = snapshot.nodes.len();
        Self {
            mode: mode_label(snapshot.mode),
            group: snapshot.group.clone(),
            node_count: snapshot.node_count,
            shown,
            capped: snapshot.node_count > shown,
            show_address: snapshot.nodes.iter().any(|node| node.address.is_some()),
            node_roles: snapshot.nodes.iter().map(|node| node.role).collect(),
        }
    }
}

#[component]
fn TopologyBody(snapshot: TopologySnapshot) -> impl IntoView {
    let (filter, set_filter) = signal(RoleFilter::All);
    let live = RwSignal::new(snapshot);
    // A feed claim is only true once the stream opens or delivers a valid event, so the badge starts
    // connecting and turns live from `subscribe_topology`, never on the strength of a pending connection.
    let status = RwSignal::new(StreamStatus::Connecting);
    // The static render carries no feed badge: a claim about the live connection is meaningful only once
    // the browser opens the stream, so the badge appears after hydration rather than on the server render.
    let streaming = RwSignal::new(false);
    watch_topology(live, status, streaming);
    topology_view(live, filter, set_filter, status, streaming)
}

fn topology_view(
    live: RwSignal<TopologySnapshot>,
    filter: ReadSignal<RoleFilter>,
    set_filter: WriteSignal<RoleFilter>,
    status: RwSignal<StreamStatus>,
    streaming: RwSignal<bool>,
) -> impl IntoView {
    // Read every summary and filtering value from the live signal so a streamed roster change moves the
    // count, group, mode, cap notice, and address column with the table, never from the loader's snapshot.
    let summary = move || TopologySummary::derive(&live.get());
    // Gate the standalone/roster split on the roster's presence alone, so an ordinary snapshot that keeps
    // the roster non-empty refreshes the rows and columns without rebuilding the filter select underneath
    // the operator's current selection.
    let has_roster = Memo::new(move |_| live.get().node_count > 0);
    let rows = move || {
        let show_address = summary().show_address;
        let current = filter.get();
        live.get()
            .nodes
            .iter()
            .filter(|node| current.matches(node.role))
            .map(|node| node_row(node, show_address))
            .collect_view()
    };
    let visible = move || {
        let current = filter.get();
        summary()
            .node_roles
            .iter()
            .filter(|role| current.matches(**role))
            .count()
    };
    view! {
        <div class="ops-title">
            <h1>"Availability topology"</h1>
            <span class="badge">"read-only"</span>
            {move || streaming.get().then(|| view! { <StreamBadge status /> })}
            <a href="/+availability/topology"><code>"/+availability/topology"</code></a>
        </div>
        <p class="dim">
            "A single role-filtered picture of the availability group that updates live as this node's own \
             frontier and liveness move. Peer liveness stays "<em>"unknown"</em>" until a consensus layer \
             observes it, so stale peer data never reads as healthy. Fields above your access are withheld \
             rather than shown, and a paused feed shows as such so stale data cannot look fresh."
        </p>
        <div class="stat-row topology-summary">
            <div class="stat"><strong>{move || summary().mode}</strong><span>"mode"</span></div>
            <div class="stat"><strong>{move || summary().group.unwrap_or_else(|| "-".to_owned())}</strong><span>"group"</span></div>
            <div class="stat"><strong>{move || summary().node_count}</strong><span>"roster nodes"</span></div>
            <div class="stat"><strong>{move || format_instant(live.get().captured_at)}</strong><span>"snapshot taken (UTC)"</span></div>
        </div>
        {move || {
            let summary = summary();
            summary.capped.then(|| view! {
                <p class="usage-retention" role="note">
                    {format!("Showing {} of {} nodes. The roster is capped per snapshot; the count above is the full size.", summary.shown, summary.node_count)}
                </p>
            })
        }}
        <h2>"This node"</h2>
        {move || {
            let snapshot = live.get();
            let captured = format_instant(snapshot.captured_at);
            view! { <LocalNodePanel local=snapshot.local captured /> }
        }}
        <h2>"Roster"</h2>
        {move || {
            if has_roster.get() {
                view! {
                    <form class="policy-filters topology-filters" on:submit=|event| event.prevent_default()>
                        <label for="topology-role">"Role"</label>
                        <select
                            id="topology-role"
                            on:change:target=move |event| set_filter.set(RoleFilter::from_value(&event.target().value()))
                        >
                            <option value="all">"All roles"</option>
                            <option value="writer">"Writer"</option>
                            <option value="replica">"Replica"</option>
                        </select>
                    </form>
                    <p class="result-count" role="status" aria-live="polite">
                        {move || format!("Showing {} of {} roster nodes.", visible(), live.get().node_count)}
                    </p>
                    <div class="table-scroll">
                        <table class="files ops-table topology-table">
                            <caption>"Configured availability roster, one row per node."</caption>
                            <thead>
                                <tr>
                                    <th scope="col">"Node"</th>
                                    <th scope="col">"Datacenter"</th>
                                    <th scope="col">"Role"</th>
                                    <th scope="col">"Health"</th>
                                    <th scope="col" class="num">"Frontier"</th>
                                    {move || summary().show_address.then(|| view! { <th scope="col">"Address"</th> })}
                                </tr>
                            </thead>
                            <tbody>{rows}</tbody>
                        </table>
                    </div>
                }.into_any()
            } else {
                view! {
                    <p class="dim" role="status">
                        "This node runs standalone. No availability roster is configured, so there are no peers to show."
                    </p>
                }.into_any()
            }
        }}
    }
}

#[component]
fn LocalNodePanel(local: LocalNode, captured: String) -> impl IntoView {
    let health = liveness_health(local.liveness);
    view! {
        <div class="stat-row topology-local">
            <div class="stat"><strong>{role_label(local.role)}</strong><span>"role"</span></div>
            <div class="stat">
                <strong><HealthBadge health /></strong>
                <span>"liveness"</span>
            </div>
            <div class="stat">
                <strong>{local.frontier.map_or_else(|| "restricted".to_owned(), |value| value.to_string())}</strong>
                <span>"committed frontier"</span>
            </div>
            <div class="stat"><strong>{captured}</strong><span>"observed at (UTC)"</span></div>
        </div>
    }
}

fn node_row(node: &TopologyNode, show_address: bool) -> AnyView {
    let health = liveness_health(node.liveness);
    let node_cell = if node.local {
        view! { <td><code>{node.node.clone()}</code>" "<span class="badge topology-self">"this node"</span></td> }
            .into_any()
    } else {
        view! { <td><code>{node.node.clone()}</code></td> }.into_any()
    };
    view! {
        <tr>
            {node_cell}
            <td>{node.dc.clone()}</td>
            <td><span class=format!("badge role-{}", role_label(node.role).to_lowercase())>{role_label(node.role)}</span></td>
            <td><HealthBadge health /></td>
            <td class="num">{node.frontier.map_or_else(|| "-".to_owned(), |value| value.to_string())}</td>
            {show_address.then(|| view! {
                <td>{node.address.clone().unwrap_or_else(|| "-".to_owned())}</td>
            })}
        </tr>
    }
    .into_any()
}

#[component]
fn HealthBadge(health: HealthLabel) -> impl IntoView {
    view! { <span class=format!("badge {}", health.class)>{health.text}</span> }
}

#[component]
fn StreamBadge(status: RwSignal<StreamStatus>) -> impl IntoView {
    view! {
        {move || {
            let label = stream_status_label(status.get());
            view! {
                <span class=format!("badge {}", label.class) role="status" aria-live="polite">
                    "feed: "{label.text}
                </span>
            }
        }}
    }
}

/// Subscribe the page to live snapshot deltas, holding the stream for the page's lifetime. The browser
/// half opens an `EventSource`; the server render and the script-free fallback keep the one-shot
/// snapshot, so the page stays complete without the stream.
fn watch_topology(live: RwSignal<TopologySnapshot>, status: RwSignal<StreamStatus>, streaming: RwSignal<bool>) {
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        // Run after hydration so the server render and the browser's first render agree on an absent
        // badge; only then open the stream and reveal it.
        Effect::new(move |_| {
            match crate::data::subscribe_topology(move |snapshot| live.set(snapshot), move |state| status.set(state)) {
                // The stream holds JS closures, so it cannot cross into the `Send + Sync` cleanup
                // callback. Park it in a thread-local the callback reaches on navigation, like uploads.
                Some(stream) => {
                    drop(TOPOLOGY_STREAM.with(|slot| slot.borrow_mut().replace(stream)));
                    streaming.set(true);
                    on_cleanup(close_topology_stream);
                }
                None => {
                    streaming.set(true);
                    status.set(StreamStatus::Offline);
                }
            }
        });
    }
    #[cfg(not(all(not(feature = "ssr"), feature = "hydrate")))]
    {
        let _ = (live, status, streaming);
    }
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
thread_local! {
    static TOPOLOGY_STREAM: std::cell::RefCell<Option<crate::data::TopologyStream>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
fn close_topology_stream() {
    drop(TOPOLOGY_STREAM.with(|slot| slot.borrow_mut().take()));
}

#[cfg(all(test, feature = "ssr"))]
#[path = "../../tests/unit/pages/topology/tests.rs"]
mod tests;
