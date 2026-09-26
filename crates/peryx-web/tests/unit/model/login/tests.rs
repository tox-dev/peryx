use rstest::rstest;

use super::*;

#[rstest]
#[case::nothing_to_offer(None, &[], false)]
#[case::a_provider(None, &["work"], true)]
#[case::a_session(Some("Ada"), &[], true)]
fn test_login_state_offers_sign_in(#[case] user: Option<&str>, #[case] providers: &[&str], #[case] expected: bool) {
    let state = UiLoginState {
        user: user.map(str::to_owned),
        providers: providers.iter().map(|&provider| provider.to_owned()).collect(),
    };

    assert_eq!(state.offers_sign_in(), expected);
}
