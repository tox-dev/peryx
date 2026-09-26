use crate::model::UiLoginState;

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
use super::RequiredOption;

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
#[derive(serde::Deserialize)]
struct SessionDocument {
    user: RequiredOption<SessionUser>,
    providers: Vec<String>,
    local: bool,
}

#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
#[derive(serde::Deserialize)]
struct SessionUser {
    name: String,
}

/// The browser login state: who is signed in and how a visitor can sign in.
///
/// # Errors
///
/// Returns a typed error when the session endpoint cannot provide a valid document.
pub async fn load_login() -> Result<UiLoginState, super::LoaderError> {
    #[cfg(feature = "ssr")]
    {
        Ok(crate::ssr::login_state().await)
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async {
            let document: SessionDocument =
                super::fetch_json_required("/_/session", super::LoaderEndpoint::Session).await?;
            Ok(UiLoginState {
                user: document.user.0.map(|user| user.name),
                providers: document.providers,
                local: document.local,
            })
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        Ok(UiLoginState::default())
    }
}
