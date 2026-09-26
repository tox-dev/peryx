use leptos::prelude::*;
use leptos_router::hooks::use_query_map;

use super::{ErrorMessage, LoadState, retain, start_refresh};
use crate::data::load_login;
use crate::model::UiLoginState;

/// The browser login page: a signed-in banner with logout, or the ways to sign in - the local name and
/// password form and the OIDC providers. It reads its state through a `Suspense` so a no-JS client
/// still receives the resolved page. A rejected password sign-in lands here with an `error` query,
/// which shows one message whatever the cause.
#[component]
pub fn Login() -> impl IntoView {
    let rejected = use_query_map().read_untracked().get("error").is_some();
    let state = Resource::new(|| (), |()| load_login());
    let loaded = RwSignal::new(LoadState::default());
    start_refresh(state);
    view! {
        <section class="page">
            <Suspense fallback=|| view! { <p class="dim">"loading"</p> }>
                {move || Suspend::new(async move {
                    let loaded = retain(loaded, state.await);
                    view! {
                        {loaded.error.map(|message| view! { <ErrorMessage message /> })}
                        {loaded.value.map(|state| login_view(state, rejected))}
                    }
                })}
            </Suspense>
        </section>
    }
}

/// One message for every rejection, so the page never says whether the name exists.
const SIGN_IN_REJECTED: &str = "Sign-in failed. Check the name and password.";

/// Render the login surface from resolved state.
fn login_view(state: UiLoginState, rejected: bool) -> impl IntoView {
    view! {
        <h1>"Sign in"</h1>
        {match state.user {
            Some(name) => view! {
                <p>"Signed in as " <strong>{name}</strong>"."</p>
                <form method="post" action="/_/logout">
                    <button type="submit">"Log out"</button>
                </form>
            }
            .into_any(),
            None if !state.local && state.providers.is_empty() => {
                view! { <p class="dim">"No login providers are configured."</p> }.into_any()
            }
            None => view! {
                {rejected.then(|| view! { <ErrorMessage message=SIGN_IN_REJECTED.to_owned() /> })}
                {state.local.then(password_form)}
                {(!state.providers.is_empty()).then(|| provider_list(state.providers))}
            }
            .into_any(),
        }}
    }
}

fn password_form() -> impl IntoView {
    view! {
        <form class="login-form" method="post" action="/_/login/password">
            <label for="login-name">"Name"</label>
            <input id="login-name" class="token" name="name" autocomplete="username" required />
            <label for="login-password">"Password"</label>
            <input
                id="login-password"
                class="token"
                type="password"
                name="password"
                autocomplete="current-password"
                required
            />
            <div>
                <button type="submit">"Sign in"</button>
            </div>
        </form>
    }
}

fn provider_list(providers: Vec<String>) -> impl IntoView {
    view! {
        <p>"Choose a provider to sign in to the dashboard."</p>
        <ul class="provider-list">
            {providers
                .into_iter()
                .map(|provider| {
                    let href = format!("/_/login/{provider}");
                    view! {
                        <li>
                            <a class="button" href=href>
                                "Sign in with " {provider}
                            </a>
                        </li>
                    }
                })
                .collect_view()}
        </ul>
    }
}

#[cfg(test)]
#[path = "../../tests/unit/pages/login/tests.rs"]
mod tests;
