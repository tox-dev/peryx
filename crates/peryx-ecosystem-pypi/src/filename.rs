//! Python distribution filename parsing for upload identity checks.

use crate::{Version, is_valid_name, normalize_name, parse_version};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistributionKind {
    Wheel,
    SdistTarGz,
    SdistZip,
}

impl DistributionKind {
    #[must_use]
    pub const fn upload_filetype(self) -> &'static str {
        match self {
            Self::Wheel => "bdist_wheel",
            Self::SdistTarGz | Self::SdistZip => "sdist",
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

/// Parse a wheel or a PEP 527 sdist (`.tar.gz` or `.zip`) filename into its upload identity.
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
        return parse_sdist_filename(stem, DistributionKind::SdistTarGz);
    }
    if let Some(stem) = strip_ascii_suffix_ignore_case(filename, ".zip") {
        return parse_sdist_filename(stem, DistributionKind::SdistZip);
    }
    Err(DistributionFilenameError::UnsupportedExtension)
}

fn strip_ascii_suffix_ignore_case<'a>(value: &'a str, suffix: &str) -> Option<&'a str> {
    let split = value.len().checked_sub(suffix.len())?;
    value.as_bytes()[split..]
        .eq_ignore_ascii_case(suffix.as_bytes())
        .then_some(&value[..split])
}

/// The raw version segment of a distribution filename, matching where [`parse_distribution_filename`]
/// finds it, or `None` when no version segment is present.
///
/// An sdist name may itself contain `-`, so its version is the segment after the *last* `-`: splitting
/// `python-dateutil-2.8.2.tar.gz` on the first `-` misreads the version as `dateutil`. A wheel escapes
/// its project name (no `-` inside it), so its version is the component after the first `-`; legacy
/// shapes (`.egg`, and anything else) escape their names too, so they follow the same first-`-` rule.
#[must_use]
pub fn distribution_version_segment(filename: &str) -> Option<&str> {
    if let Some(stem) =
        strip_ascii_suffix_ignore_case(filename, ".tar.gz").or_else(|| strip_ascii_suffix_ignore_case(filename, ".zip"))
    {
        return stem.rsplit_once('-').map(|(_name, version)| version);
    }
    let stem = strip_ascii_suffix_ignore_case(filename, ".whl").unwrap_or(filename);
    let (_name, rest) = stem.split_once('-')?;
    Some(rest.split('-').next().unwrap_or(rest))
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

fn parse_sdist_filename(stem: &str, kind: DistributionKind) -> Result<DistributionFilename, DistributionFilenameError> {
    let Some((name, version)) = stem.rsplit_once('-') else {
        return Err(DistributionFilenameError::InvalidSdistShape);
    };
    parsed(name, version, [], kind)
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
