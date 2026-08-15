use leptos::prelude::*;

use crate::data::load_topology;
use crate::model::{
    HealthLabel, LocalNode, RoleFilter, StreamStatus, TopologyNode, TopologySnapshot, format_instant, liveness_health,
    mode_label, role_label, stream_status_label,
};

#[component]
#[must_use]
pub fn AvailabilityTopology() -> impl IntoView {
    let topology = Resource::new(|| (), |()| load_topology());
    view! {
        <section class="page topology-page">
            <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
                {move || Suspend::new(async move {
                    loaded_topology(topology.await)
                })}
            </Suspense>
        </section>
    }
}

fn loaded_topology(result: Result<TopologySnapshot, String>) -> AnyView {
    match result {
        Ok(snapshot) => view! { <TopologyBody snapshot /> }.into_any(),
        Err(error) => view! { <p class="error" role="alert">{error}</p> }.into_any(),
    }
}

#[component]
fn TopologyBody(snapshot: TopologySnapshot) -> impl IntoView {
    let (filter, set_filter) = signal(RoleFilter::All);
    let live = RwSignal::new(snapshot);
    let status = RwSignal::new(StreamStatus::Connecting);
    // Reveal connection status after hydration so the server and first browser render match.
    let streaming = RwSignal::new(false);
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    watch_topology(live, status, streaming);
    topology_view(live, filter, set_filter, status, streaming)
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
fn watch_topology(live: RwSignal<TopologySnapshot>, status: RwSignal<StreamStatus>, streaming: RwSignal<bool>) {
    // Hydration must match the server before the browser reveals the stream state.
    Effect::new(move |_| {
        if let Some(stream) =
            crate::data::subscribe_topology(move |snapshot| live.set(snapshot), move |state| status.set(state))
        {
            // JS closures cannot cross into the Send + Sync cleanup callback.
            drop(TOPOLOGY_STREAM.with(|slot| slot.borrow_mut().replace(stream)));
            streaming.set(true);
            on_cleanup(close_topology_stream);
        } else {
            streaming.set(true);
            status.set(StreamStatus::Offline);
        }
    });
}

fn topology_view(
    live: RwSignal<TopologySnapshot>,
    filter: ReadSignal<RoleFilter>,
    set_filter: WriteSignal<RoleFilter>,
    status: RwSignal<StreamStatus>,
    streaming: RwSignal<bool>,
) -> impl IntoView {
    let summary = move || TopologySummary::derive(&live.get());
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
            <div class="stat">
                <strong>{move || summary().group.unwrap_or_else(|| "-".to_owned())}</strong>
                <span>"group"</span>
            </div>
            <div class="stat"><strong>{move || summary().node_count}</strong><span>"roster nodes"</span></div>
            <div class="stat">
                <strong>{move || format_instant(live.get().captured_at)}</strong>
                <span>"snapshot taken (UTC)"</span>
            </div>
        </div>
        {move || {
            let summary = summary();
            summary.capped.then(|| view! {
                <p class="usage-retention" role="note">
                    {format!(
                        "Showing {} of {} nodes. The roster is capped per snapshot; the count above is the full size.",
                        summary.shown,
                        summary.node_count,
                    )}
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
        {topology_roster(live, filter, set_filter)}
    }
}

fn topology_roster(
    live: RwSignal<TopologySnapshot>,
    filter: ReadSignal<RoleFilter>,
    set_filter: WriteSignal<RoleFilter>,
) -> impl IntoView {
    // Preserve the selected filter while streamed snapshots replace a non-empty roster.
    let has_roster = Memo::new(move |_| live.get().node_count > 0);
    let rows = move || {
        let snapshot = live.get();
        let show_address = snapshot.nodes.iter().any(|node| node.address.is_some());
        let current = filter.get();
        snapshot
            .nodes
            .iter()
            .filter(|node| current.matches(node.role))
            .map(|node| node_row(node, show_address))
            .collect_view()
    };
    let visible = move || {
        let current = filter.get();
        live.get()
            .nodes
            .iter()
            .filter(|node| current.matches(node.role))
            .count()
    };
    view! {
        {move || {
            if has_roster.get() {
                view! {
                    <TopologyRoleFilter set_filter />
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
                                    {move || {
                                        live.get()
                                            .nodes
                                            .iter()
                                            .any(|node| node.address.is_some())
                                            .then(|| view! { <th scope="col">"Address"</th> })
                                    }}
                                </tr>
                            </thead>
                            <tbody>{rows}</tbody>
                        </table>
                    </div>
                }.into_any()
            } else {
                view! {
                    <p class="dim" role="status">
                        "This node runs standalone. No availability roster is configured, so there are no peers to \
                         show."
                    </p>
                }.into_any()
            }
        }}
    }
}

#[component]
#[cfg(not(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate")))]
fn TopologyRoleFilter(set_filter: WriteSignal<RoleFilter>) -> impl IntoView {
    let _ = set_filter;
    view! {
        <form class="policy-filters topology-filters">
            <label for="topology-role">"Role"</label>
            <select id="topology-role">
                <option value="all">"All roles"</option>
                <option value="writer">"Writer"</option>
                <option value="replica">"Replica"</option>
            </select>
        </form>
    }
}

#[component]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn TopologyRoleFilter(set_filter: WriteSignal<RoleFilter>) -> impl IntoView {
    view! {
        <form class="policy-filters topology-filters">
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
    }
}

struct TopologySummary {
    mode: &'static str,
    group: Option<String>,
    node_count: usize,
    shown: usize,
    capped: bool,
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
        }
    }
}

#[component]
fn LocalNodePanel(local: LocalNode, captured: String) -> impl IntoView {
    view! {
        <div class="stat-row topology-local">
            <div class="stat"><strong>{role_label(local.role)}</strong><span>"role"</span></div>
            <div class="stat">
                <strong><HealthBadge health=liveness_health(local.liveness) /></strong>
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
            <td>
                <span class=format!("badge role-{}", role_label(node.role).to_lowercase())>
                    {role_label(node.role)}
                </span>
            </td>
            <td><HealthBadge health=liveness_health(node.liveness) /></td>
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
