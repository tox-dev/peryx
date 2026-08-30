use leptos::prelude::*;

use super::{ErrorMessage, LoadState, retain, start_refresh};
use crate::data::load_login;
use crate::model::UiLoginState;

/// The browser login page: a signed-in banner with logout, or the list of OIDC providers to sign in
/// with. It reads its state through a `Suspense` so a no-JS client still receives the resolved page.
#[component]
pub fn Login() -> impl IntoView {
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
                        {loaded.value.map(login_view)}
                    }
                })}
            </Suspense>
        </section>
    }
}

/// Render the login surface from resolved state.
fn login_view(state: UiLoginState) -> impl IntoView {
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
            None if state.providers.is_empty() => {
                view! { <p class="dim">"No login providers are configured."</p> }.into_any()
            }
            None => view! {
                <p>"Choose a provider to sign in to the dashboard."</p>
                <ul class="provider-list">
                    {state
                        .providers
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
            .into_any(),
        }}
    }
}

#[cfg(test)]
#[path = "../../tests/unit/pages/login/tests.rs"]
mod tests;
