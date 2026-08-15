//! Core Metadata gives each field the legal forms of an RFC 822 `From` header: a comma-separated
//! list of addresses, each either bare or a display name with the address in angle brackets. Peryx
//! splits the list and lifts the address out of a named entry with the RFC 822 mechanics
//! `parse_metadata` already hand-rolls, then defers the address grammar to the `email_address` crate
//! the way it defers license and version grammars to focused crates, rather than re-derive an
//! addr-spec here.

use email_address::EmailAddress;

/// Validate a contact-address field, returning the reason it was rejected.
pub fn validate(value: &str) -> Result<(), &'static str> {
    let mut start = 0;
    let mut quoted = false;
    for (index, character) in value.char_indices() {
        match character {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                validate_address(&value[start..index])?;
                start = index + 1;
            }
            _ => {}
        }
    }
    validate_address(&value[start..])
}

fn validate_address(entry: &str) -> Result<(), &'static str> {
    let address = match entry.split_once('<') {
        Some((_, rest)) => rest.split_once('>').map_or(rest, |(address, _)| address).trim(),
        None => entry.trim(),
    };
    if EmailAddress::is_valid(address) {
        Ok(())
    } else {
        Err("is not a valid email address")
    }
}
