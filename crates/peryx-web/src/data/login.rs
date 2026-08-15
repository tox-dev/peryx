use crate::model::UiLoginState;

/// The browser login state: who is signed in and which providers to offer.
pub async fn load_login() -> UiLoginState {
    #[cfg(feature = "ssr")]
    {
        crate::ssr::login_state().await
    }
    #[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
    {
        send_wrapper::SendWrapper::new(async {
            super::fetch_json("/_/session")
                .await
                .map_or_else(UiLoginState::default, |value| UiLoginState::from_session(&value))
        })
        .await
    }
    #[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
    {
        UiLoginState::default()
    }
}
