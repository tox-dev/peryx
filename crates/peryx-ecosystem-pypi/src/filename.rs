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
    if let Some(stem) = filename.strip_suffix(".whl") {
        return parse_wheel_filename(stem);
    }
    if let Some(stem) = filename.strip_suffix(".tar.gz") {
        return parse_sdist_filename(stem, DistributionKind::SdistTarGz);
    }
    if let Some(stem) = filename.strip_suffix(".zip") {
        return parse_sdist_filename(stem, DistributionKind::SdistZip);
    }
    Err(DistributionFilenameError::UnsupportedExtension)
}

fn strip_ascii_suffix_ignore_case<'a>(value: &'a str, suffix: &str) -> Option<&'a str> {
    let split = value.len().checked_sub(suffix.len())?;
    value.as_bytes()[split..]
        .eq_ignore_ascii_case(suffix.as_bytes())
        .then(|| &value[..split])
}

/// Sdist archive suffixes whose project name may itself keep a `-`, so the version is the segment
/// after the *last* `-`. Current uploads are `.tar.gz`/`.zip` (all [`parse_distribution_filename`]
/// accepts), but a mirror still serves pre-PEP 625 releases compressed as bzip2, xz, or the legacy
/// `.tar.Z`/`.tgz` spellings, so version extraction has to recognize them too.
const SDIST_ARCHIVE_SUFFIXES: [&str; 6] = [".tar.gz", ".tgz", ".tar.bz2", ".tar.xz", ".tar.z", ".zip"];

/// The raw version segment of a distribution filename, or `None` when the filename carries no
/// recognizable version.
///
/// Unlike [`parse_distribution_filename`], which is the strict upload identity check, this reads
/// versions off filenames a mirror already accepted upstream, so it spans the wider set of shapes
/// still served there. An sdist name may itself contain `-`, so for an archive suffix the version is
/// the segment after the *last* `-`: splitting `python-dateutil-2.8.2.tar.gz` on the first `-`
/// misreads it as `dateutil`. A wheel or egg escapes its project name (no `-` inside it), so its
/// version is the component after the first `-`. Any other filename yields `None` rather than a
/// segment with the extension still attached, so `foo-1.0.exe` never reports a version of `1.0.exe`.
#[must_use]
pub fn distribution_version_segment(filename: &str) -> Option<&str> {
    for suffix in SDIST_ARCHIVE_SUFFIXES {
        if let Some(stem) = strip_ascii_suffix_ignore_case(filename, suffix) {
            return stem
                .rsplit_once('-')
                .map(|(_name, version)| version)
                .filter(|version| !version.is_empty());
        }
    }
    for suffix in [".whl", ".egg"] {
        if let Some(stem) = strip_ascii_suffix_ignore_case(filename, suffix) {
            let (_name, rest) = stem.split_once('-')?;
            return rest.split('-').next().filter(|version| !version.is_empty());
        }
    }
    None
}

/// The project-name segment of a distribution filename, or `None` when the filename carries no
/// recognizable name/version boundary.
///
/// The name-side counterpart of [`distribution_version_segment`], split at the same boundary. An
/// sdist name may itself contain `-`, so for an archive suffix the name is everything before the
/// *last* `-`: splitting `python-dateutil-2.8.2.tar.gz` on the first `-` misreads its project as
/// `python`. A wheel or egg escapes its project name, so its name is the component before the first
/// `-`. Any other filename yields `None`.
#[must_use]
pub fn distribution_name_segment(filename: &str) -> Option<&str> {
    for suffix in SDIST_ARCHIVE_SUFFIXES {
        if let Some(stem) = strip_ascii_suffix_ignore_case(filename, suffix) {
            return stem
                .rsplit_once('-')
                .map(|(name, _version)| name)
                .filter(|name| !name.is_empty());
        }
    }
    for suffix in [".whl", ".egg"] {
        if let Some(stem) = strip_ascii_suffix_ignore_case(filename, suffix) {
            return stem
                .split_once('-')
                .map(|(name, _rest)| name)
                .filter(|name| !name.is_empty());
        }
    }
    None
}

/// Warehouse's `pyversion` for a distribution filename: a wheel's Python-tag component, `source`
/// for everything else.
///
/// Warehouse sets this field once, when it records the file, and then renders it in two places a
/// client reads: the legacy JSON `python_version`, and the changelog action `add {pyversion} file
/// {filename}`. Deriving both from one function is what keeps the two agreeing. A wheel name is
/// `name-version[-build]-python-abi-platform`, so the tag is third from the end; any other shape,
/// and every non-wheel, reports `source`, which is what Warehouse records for an sdist.
#[must_use]
pub fn distribution_python_tag(filename: &str) -> &str {
    let Some(stem) = strip_ascii_suffix_ignore_case(filename, ".whl") else {
        return "source";
    };
    match stem.split('-').count() {
        5 | 6 => stem.rsplit('-').nth(2).unwrap_or("source"),
        _ => "source",
    }
}

fn parse_wheel_filename(stem: &str) -> Result<DistributionFilename, DistributionFilenameError> {
    let parts: Vec<&str> = stem.split('-').collect();
    let [name, version, python, abi, platform] = parts.as_slice() else {
        let [name, version, build, python, abi, platform] = parts.as_slice() else {
            return Err(DistributionFilenameError::InvalidWheelShape);
        };
        validate_build_tag(build)?;
        return parsed(name, version, &[*python, *abi, *platform], DistributionKind::Wheel);
    };
    parsed(name, version, &[*python, *abi, *platform], DistributionKind::Wheel)
}

// A legacy (pre-PEP 625) sdist name was not escaped, so the last `-` is only a heuristic for the
// name/version boundary: `pkg-1.0-1.tar.gz` splits to name `pkg-1.0`, version `1` here, yet its
// PKG-INFO may well declare `pkg` version `1.0-1`. The filename alone cannot resolve that ambiguity,
// so `import-dir` reconciles the split against the archive's authoritative PKG-INFO identity.
fn parse_sdist_filename(stem: &str, kind: DistributionKind) -> Result<DistributionFilename, DistributionFilenameError> {
    let Some((name, version)) = stem.rsplit_once('-') else {
        return Err(DistributionFilenameError::InvalidSdistShape);
    };
    parsed(name, version, &[], kind)
}

fn parsed(
    name: &str,
    version: &str,
    tags: &[&str],
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
