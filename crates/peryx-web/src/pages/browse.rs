use leptos::prelude::*;
use leptos_router::hooks::use_location;
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
use peryx_core::UiActionMethod;
use peryx_core::{BrowseBadge, BrowseCell, BrowseLink, BrowsePage, BrowseProperty, BrowseRow, BrowseSection, UiAction};

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
use super::copy_to_clipboard;
use super::{ErrorMessage, human_size};
use crate::data::load_browse;
use crate::markdown::external_link_rel;

#[component]
pub fn Browse() -> impl IntoView {
    let location = use_location();
    let page = Resource::new(move || location.search.get(), load_browse);
    view! {
        <section class="page browse-page">
            <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
                {move || Suspend::new(async move {
                    match page.await {
                        Ok(Some(document)) => view! { <BrowseDocument document refresh=page /> }.into_any(),
                        Ok(None) => view! { <p class="dim">"Nothing matched this browse query."</p> }.into_any(),
                        Err(message) => view! { <ErrorMessage message /> }.into_any(),
                    }
                })}
            </Suspense>
        </section>
    }
}

#[component]
fn BrowseDocument(document: BrowsePage, refresh: Resource<Result<Option<BrowsePage>, String>>) -> impl IntoView {
    let (user, set_user) = signal(String::new());
    let (password, set_password) = signal(String::new());
    #[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
    let (outcome, set_outcome) = signal(String::new());
    #[cfg(not(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate")))]
    let (_, set_outcome) = signal(String::new());
    #[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
    let outcome_view = move || (!outcome.get().is_empty()).then(|| view! { <p class="outcome">{outcome.get()}</p> });
    #[cfg(not(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate")))]
    let outcome_view = ();
    view! {
        <BrowseBreadcrumbs links=document.breadcrumbs />
        <header class="browse-head">
            <h1>{document.title}</h1>
            {document.subtitle.map(|subtitle| view! { <p class="browse-subtitle">{subtitle}</p> })}
            {document.summary.map(|summary| view! { <p class="summary">{summary}</p> })}
            <BrowseBadges badges=document.badges />
        </header>
        {document.command.map(|command| {
            let copied = command.clone();
            view! {
                <div class="install">
                    <code>{command}</code>
                    <CopyButton command=copied />
                </div>
            }
        })}
        {document.sections.into_iter().map(|section| view! { <BrowseSectionView section /> }).collect_view()}
        <BrowseActions actions=document.actions user set_user password set_password set_outcome refresh />
        {outcome_view}
    }
}

#[component]
#[cfg(not(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate")))]
fn CopyButton(command: String) -> impl IntoView {
    let _ = command;
    view! { <button class="copy">"copy"</button> }
}

#[component]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn CopyButton(command: String) -> impl IntoView {
    view! { <button class="copy" on:click=move |_| copy_to_clipboard(&command)>"copy"</button> }
}

#[component]
fn BrowseBreadcrumbs(links: Vec<BrowseLink>) -> impl IntoView {
    (!links.is_empty()).then(|| {
        view! {
            <nav class="breadcrumb">
                {links.into_iter().enumerate().map(|(position, link)| view! {
                    {(position > 0).then_some(" / ")}
                <a href=link.href.clone() rel=external_link_rel(&link.href)>{link.label}</a>
                }).collect_view()}
            </nav>
        }
    })
}

#[component]
fn BrowseBadges(badges: Vec<BrowseBadge>) -> impl IntoView {
    view! {
        <div class="browse-badges">
            {badges.into_iter().map(|badge| {
                let class = format!("badge {}", badge.class);
                view! { <span class=class title=badge.hint>{badge.label}</span> }
            }).collect_view()}
        </div>
    }
}

#[component]
fn BrowseSectionView(section: BrowseSection) -> impl IntoView {
    match section {
        BrowseSection::Markup { heading, html, notice } => view! {
            <section class="browse-section">
                <h2>{heading}</h2>
                {notice.map(|message| view! { <p class="dim">{message}</p> })}
                <div class="description" inner_html=html></div>
            </section>
        }
        .into_any(),
        BrowseSection::Properties { heading, entries } => view! {
            <section class="browse-section">
                <h2>{heading}</h2>
                <dl class="browse-properties">
                    {entries.into_iter().map(|entry| view! { <BrowsePropertyView entry /> }).collect_view()}
                </dl>
            </section>
        }
        .into_any(),
        BrowseSection::Links {
            heading,
            entries,
            empty,
        } => view! {
            <section class="browse-section">
                <h2>{heading}</h2>
                <BrowseLinks links=entries empty />
            </section>
        }
        .into_any(),
        BrowseSection::Table {
            heading,
            columns,
            rows,
            empty,
        } => view! {
            <section class="browse-section">
                <h2>{heading}</h2>
                <BrowseTable columns rows empty />
            </section>
        }
        .into_any(),
        BrowseSection::Content {
            heading,
            text,
            size,
            offset,
            next,
        } => view! {
            <section class="browse-section">
                <h2>{heading}</h2>
                {size.map(|size| view! { <p class="dim">{format!("{} at byte {offset}", human_size(size))}</p> })}
                <pre class="browse-content"><code>{text}</code></pre>
                {next.map(|link| view! { <a class="page-link" href=link.href>{link.label}</a> })}
            </section>
        }
        .into_any(),
    }
}

#[component]
fn BrowsePropertyView(entry: BrowseProperty) -> impl IntoView {
    view! {
        <dt>{entry.label}</dt>
        <dd>{match entry.href {
            Some(href) => view! { <a href=href.clone() rel=external_link_rel(&href)>{entry.value}</a> }.into_any(),
            None => view! { <span>{entry.value}</span> }.into_any(),
        }}</dd>
    }
}

#[component]
fn BrowseLinks(links: Vec<BrowseLink>, empty: String) -> impl IntoView {
    if links.is_empty() {
        return view! { <p class="dim">{empty}</p> }.into_any();
    }
    view! {
        <ul class="links-list">
            {links.into_iter().map(|link| view! {
                <li><a href=link.href.clone() rel=external_link_rel(&link.href)>{link.label}</a></li>
            }).collect_view()}
        </ul>
    }
    .into_any()
}

#[component]
fn BrowseTable(columns: Vec<String>, rows: Vec<BrowseRow>, empty: String) -> impl IntoView {
    if rows.is_empty() {
        return view! { <p class="dim">{empty}</p> }.into_any();
    }
    view! {
        <div class="table-scroll">
            <table class="browse-table">
                <thead><tr>{columns.into_iter().map(|column| view! { <th>{column}</th> }).collect_view()}</tr></thead>
                <tbody>{rows.into_iter().map(|row| view! { <BrowseRowView row /> }).collect_view()}</tbody>
            </table>
        </div>
    }
    .into_any()
}

#[component]
fn BrowseRowView(row: BrowseRow) -> impl IntoView {
    view! {
        <tr>
            {row.cells.into_iter().map(|cell| view! { <BrowseCellView cell /> }).collect_view()}
            {(!row.badges.is_empty()).then(|| view! { <td><BrowseBadges badges=row.badges /></td> })}
            {(!row.actions.is_empty()).then(|| view! { <td>{row.actions.into_iter().map(|action| action.label).collect_view()}</td> })}
        </tr>
    }
}

#[component]
fn BrowseCellView(cell: BrowseCell) -> impl IntoView {
    let content = if cell.code {
        view! { <code>{cell.text}</code> }.into_any()
    } else {
        view! { <span>{cell.text}</span> }.into_any()
    };
    view! { <td>{match cell.href {
        Some(href) => view! { <a href=href.clone() rel=external_link_rel(&href)>{content}</a> }.into_any(),
        None => content,
    }}</td> }
}

#[component]
#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn BrowseActions(
    actions: Vec<UiAction>,
    user: ReadSignal<String>,
    set_user: WriteSignal<String>,
    password: ReadSignal<String>,
    set_password: WriteSignal<String>,
    set_outcome: WriteSignal<String>,
    refresh: Resource<Result<Option<BrowsePage>, String>>,
) -> impl IntoView {
    if actions.is_empty() {
        return ().into_any();
    }
    view! {
        <details class="admin">
            <summary>"Manage"</summary>
            <input
                autocomplete="username"
                placeholder="Username"
                on:input:target=move |event| set_user.set(event.target().value())
            />
            <input
                class="token"
                type="password"
                placeholder="Password"
                on:input:target=move |event| set_password.set(event.target().value())
            />
            {actions.into_iter().map(|action| {
                let class = if action.destructive { "danger" } else { "" };
                view! {
                    <button class=class on:click=move |_| {
                        run_action(
                            action.method,
                            action.endpoint.clone(),
                            user.get_untracked(),
                            password.get_untracked(),
                            set_outcome,
                            refresh,
                        );
                    }>{action.label}</button>
                }
            }).collect_view()}
        </details>
    }
    .into_any()
}

#[component]
#[cfg(not(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate")))]
fn BrowseActions(
    actions: Vec<UiAction>,
    user: ReadSignal<String>,
    set_user: WriteSignal<String>,
    password: ReadSignal<String>,
    set_password: WriteSignal<String>,
    set_outcome: WriteSignal<String>,
    refresh: Resource<Result<Option<BrowsePage>, String>>,
) -> impl IntoView {
    let _ = (user, set_user, password, set_password, set_outcome, refresh);
    if actions.is_empty() {
        return ().into_any();
    }
    view! {
        <details class="admin">
            <summary>"Manage"</summary>
            <input autocomplete="username" placeholder="Username" />
            <input type="password" placeholder="Password" class="token" />
            {actions.into_iter().map(|action| {
                let class = if action.destructive { "danger" } else { "" };
                view! { <button class=class>{action.label}</button> }
            }).collect_view()}
        </details>
    }
    .into_any()
}

#[cfg(all(target_arch = "wasm32", not(feature = "ssr"), feature = "hydrate"))]
fn run_action(
    method: UiActionMethod,
    endpoint: String,
    user: String,
    password: String,
    outcome: WriteSignal<String>,
    refresh: Resource<Result<Option<BrowsePage>, String>>,
) {
    outcome.set(String::new());
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    leptos::task::spawn_local(async move {
        outcome.set(crate::data::admin_request(method, &endpoint, &user, &password).await);
        refresh.refetch();
    });
    #[cfg(any(feature = "ssr", not(feature = "hydrate")))]
    drop((method, endpoint, user, password, refresh));
}

#[cfg(all(test, feature = "ssr"))]
#[path = "../../tests/unit/pages/browse/tests.rs"]
mod tests;
