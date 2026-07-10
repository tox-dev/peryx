//! The `WHEEL` file: its `Wheel-Version`, and that its `Tag` and `Build` agree with the filename.

use std::collections::BTreeSet;

use super::{ArchiveError, SUPPORTED_WHEEL_MAJOR_VERSION, invalid_wheel};

pub(super) fn validate_wheel_file(filename: &str, bytes: &[u8]) -> Result<(), ArchiveError> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid_wheel("WHEEL is not valid UTF-8"))?;
    let versions = header_values(text, "Wheel-Version");
    let [version] = versions.as_slice() else {
        return Err(invalid_wheel("WHEEL must contain exactly one Wheel-Version field"));
    };
    let version = parse_wheel_version(version)?;
    if version[0] > SUPPORTED_WHEEL_MAJOR_VERSION {
        return Err(invalid_wheel(format!(
            "Wheel-Version {} is newer than supported major version {SUPPORTED_WHEEL_MAJOR_VERSION}",
            version.iter().map(u64::to_string).collect::<Vec<_>>().join(".")
        )));
    }

    let purelib = header_values(text, "Root-Is-Purelib");
    let [purelib] = purelib.as_slice() else {
        return Err(invalid_wheel("WHEEL must contain exactly one Root-Is-Purelib field"));
    };
    if !matches!(purelib.to_ascii_lowercase().as_str(), "true" | "false") {
        return Err(invalid_wheel(format!("Root-Is-Purelib has invalid value {purelib:?}")));
    }

    validate_wheel_build(filename, &header_values(text, "Build"))?;

    let tags = header_values(text, "Tag");
    if tags.is_empty() {
        return Err(invalid_wheel("WHEEL must contain at least one Tag field"));
    }
    let actual = tags
        .into_iter()
        .map(validate_wheel_tag)
        .collect::<Result<BTreeSet<_>, _>>()?;
    let expected = expected_wheel_tags(filename);
    if actual != expected {
        return Err(invalid_wheel(format!(
            "WHEEL Tag fields do not match filename tags; expected {}, got {}",
            expected.into_iter().collect::<Vec<_>>().join(", "),
            actual.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    Ok(())
}

fn validate_wheel_build(filename: &str, actual: &[&str]) -> Result<(), ArchiveError> {
    match (expected_wheel_build(filename), actual) {
        (None, []) => Ok(()),
        (None, [_]) => Err(invalid_wheel(
            "WHEEL contains a Build field, but the filename has no build tag",
        )),
        (Some(expected), [actual]) if *actual == expected => Ok(()),
        (Some(expected), []) => Err(invalid_wheel(format!(
            "WHEEL is missing Build field for filename build tag {expected:?}"
        ))),
        (Some(expected), [actual]) => Err(invalid_wheel(format!(
            "WHEEL Build field {actual:?} does not match filename build tag {expected:?}"
        ))),
        (None | Some(_), _) => Err(invalid_wheel("WHEEL must contain at most one Build field")),
    }
}

fn header_values<'a>(text: &'a str, key: &str) -> Vec<&'a str> {
    text.lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case(key).then(|| value.trim())
        })
        .collect()
}

fn parse_wheel_version(value: &str) -> Result<Vec<u64>, ArchiveError> {
    let parts = value
        .split('.')
        .map(|part| {
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(invalid_wheel(format!("invalid Wheel-Version {value:?}")));
            }
            part.parse::<u64>()
                .map_err(|_| invalid_wheel(format!("invalid Wheel-Version {value:?}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if parts.len() < 2 {
        return Err(invalid_wheel(format!("invalid Wheel-Version {value:?}")));
    }
    Ok(parts)
}

fn validate_wheel_tag(value: &str) -> Result<String, ArchiveError> {
    let parts = value.split('-').collect::<Vec<_>>();
    let [python, abi, platform] = parts.as_slice() else {
        return Err(invalid_wheel(format!("invalid WHEEL Tag {value:?}")));
    };
    if [python, abi, platform]
        .into_iter()
        .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
    {
        return Err(invalid_wheel(format!("invalid WHEEL Tag {value:?}")));
    }
    Ok(value.to_owned())
}

fn expected_wheel_tags(filename: &str) -> BTreeSet<String> {
    let parts = wheel_filename_parts(filename);
    let python_tags = parts[parts.len() - 3].split('.');
    let abi_tags = parts[parts.len() - 2].split('.');
    let platform_tags = parts[parts.len() - 1].split('.');
    let mut tags = BTreeSet::new();
    for python in python_tags {
        for abi in abi_tags.clone() {
            for platform in platform_tags.clone() {
                tags.insert(format!("{python}-{abi}-{platform}"));
            }
        }
    }
    tags
}

fn expected_wheel_build(filename: &str) -> Option<&str> {
    let parts = wheel_filename_parts(filename);
    (parts.len() == 6).then_some(parts[2])
}

fn wheel_filename_parts(filename: &str) -> Vec<&str> {
    let stem = &filename[..filename.len() - 4];
    let parts = stem.split('-').collect::<Vec<_>>();
    debug_assert!(matches!(parts.len(), 5 | 6));
    parts
}
