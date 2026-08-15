//! PEP 639 lets an index accept only case-normalized expressions built from non-deprecated SPDX
//! identifiers, which is what a strict SPDX parse admits, so peryx defers to the `spdx` crate's
//! license list rather than shipping a copy of the list.

use spdx::error::Reason;

/// Validate an SPDX license expression, returning the reason it was rejected.
pub fn validate_expression(value: &str) -> Result<(), &'static str> {
    spdx::Expression::parse(value).map_err(|err| match err.reason {
        Reason::UnknownLicense | Reason::UnknownException | Reason::UnknownTerm => {
            "is not a known SPDX license identifier in its reference case"
        }
        Reason::DeprecatedLicenseId => "uses a deprecated SPDX license identifier",
        _ => "is not a valid SPDX license expression",
    })?;
    Ok(())
}
