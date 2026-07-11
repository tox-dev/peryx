mod acl_tests;
mod basic_tests;
mod token_tests;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

pub fn basic(credentials: &[u8]) -> String {
    format!("Basic {}", STANDARD.encode(credentials))
}
