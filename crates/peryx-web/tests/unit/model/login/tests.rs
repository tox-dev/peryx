use rstest::rstest;

use super::*;

#[rstest]
#[case::nothing_to_offer(None, &[], false, false)]
#[case::a_provider(None, &["work"], false, true)]
#[case::a_session(Some("Ada"), &[], false, true)]
#[case::local_sign_in(None, &[], true, true)]
fn test_login_state_offers_sign_in(
    #[case] user: Option<&str>,
    #[case] providers: &[&str],
    #[case] local: bool,
    #[case] expected: bool,
) {
    let state = UiLoginState {
        user: user.map(str::to_owned),
        providers: providers.iter().map(|&provider| provider.to_owned()).collect(),
        local,
    };

    assert_eq!(state.offers_sign_in(), expected);
}
