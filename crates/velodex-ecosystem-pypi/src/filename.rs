//! Python distribution filename parsing for upload identity checks.

use crate::{Version, is_valid_name, normalize_name, parse_version};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistributionKind {
    Wheel,
    SdistTarGz,
}

impl DistributionKind {
    #[must_use]
    pub const fn upload_filetype(self) -> &'static str {
        match self {
            Self::Wheel => "bdist_wheel",
            Self::SdistTarGz => "sdist",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistributionFilename {
    pub kind: DistributionKind,
    pub name: String,
    pub normalized_name: String,
    pub version: Version,
    pub python_tag: Option<String>,
    pub platform_tag: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DistributionFilenameError {
    UnsupportedExtension,
    LegacyEgg,
    InvalidWheelShape,
    InvalidSdistShape,
    InvalidName(String),
    InvalidVersion(String),
    InvalidTag(String),
}

/// Parse a wheel or modern `.tar.gz` sdist filename into its upload identity.
///
/// # Errors
/// Returns [`DistributionFilenameError`] when the filename extension, component shape, project
/// name, version, or wheel tags are invalid.
pub fn parse_distribution_filename(filename: &str) -> Result<DistributionFilename, DistributionFilenameError> {
    if strip_ascii_suffix_ignore_case(filename, ".egg").is_some() {
        return Err(DistributionFilenameError::LegacyEgg);
    }
    if let Some(stem) = strip_ascii_suffix_ignore_case(filename, ".whl") {
        return parse_wheel_filename(stem);
    }
    if let Some(stem) = strip_ascii_suffix_ignore_case(filename, ".tar.gz") {
        return parse_sdist_filename(stem);
    }
    Err(DistributionFilenameError::UnsupportedExtension)
}

fn strip_ascii_suffix_ignore_case<'a>(value: &'a str, suffix: &str) -> Option<&'a str> {
    let split = value.len().checked_sub(suffix.len())?;
    value.as_bytes()[split..]
        .eq_ignore_ascii_case(suffix.as_bytes())
        .then_some(&value[..split])
}

fn parse_wheel_filename(stem: &str) -> Result<DistributionFilename, DistributionFilenameError> {
    let parts: Vec<&str> = stem.split('-').collect();
    let [name, version, python, abi, platform] = parts.as_slice() else {
        let [name, version, build, python, abi, platform] = parts.as_slice() else {
            return Err(DistributionFilenameError::InvalidWheelShape);
        };
        validate_build_tag(build)?;
        return parsed(name, version, [*python, *abi, *platform], DistributionKind::Wheel);
    };
    parsed(name, version, [*python, *abi, *platform], DistributionKind::Wheel)
}

fn parse_sdist_filename(stem: &str) -> Result<DistributionFilename, DistributionFilenameError> {
    let Some((name, version)) = stem.rsplit_once('-') else {
        return Err(DistributionFilenameError::InvalidSdistShape);
    };
    parsed(name, version, [], DistributionKind::SdistTarGz)
}

fn parsed<const N: usize>(
    name: &str,
    version: &str,
    tags: [&str; N],
    kind: DistributionKind,
) -> Result<DistributionFilename, DistributionFilenameError> {
    if !is_valid_name(name) {
        return Err(DistributionFilenameError::InvalidName(name.to_owned()));
    }
    for tag in tags {
        validate_tag(tag)?;
    }
    let Some(version) = parse_version(version) else {
        return Err(DistributionFilenameError::InvalidVersion(version.to_owned()));
    };
    Ok(DistributionFilename {
        kind,
        name: name.to_owned(),
        normalized_name: normalize_name(name),
        version,
        python_tag: tags.first().map(|tag| (*tag).to_owned()),
        platform_tag: tags.get(2).map(|tag| (*tag).to_owned()),
    })
}

fn validate_build_tag(tag: &str) -> Result<(), DistributionFilenameError> {
    let Some(first) = tag.as_bytes().first() else {
        return Err(DistributionFilenameError::InvalidWheelShape);
    };
    if !first.is_ascii_digit() || !tag.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'.') {
        return Err(DistributionFilenameError::InvalidTag(tag.to_owned()));
    }
    Ok(())
}

fn validate_tag(tag: &str) -> Result<(), DistributionFilenameError> {
    if tag.is_empty()
        || !tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.'))
    {
        return Err(DistributionFilenameError::InvalidTag(tag.to_owned()));
    }
    Ok(())
}
