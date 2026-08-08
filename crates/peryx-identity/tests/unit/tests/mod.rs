mod acl_tests;
mod basic_tests;
mod external_tests;
mod ldap_tests;
mod password_tests;
mod revocation_tests;
mod roles_tests;
mod scoped_token_tests;
mod token_tests;
mod trusted_publisher_tests;
mod user_tests;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

pub fn basic(credentials: &[u8]) -> String {
    format!("Basic {}", STANDARD.encode(credentials))
}
