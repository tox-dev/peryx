//! `entry_points.txt`: an INI-shaped file whose group and name grammar a wheel must respect.

use super::{ArchiveError, invalid_wheel};

pub(super) fn validate_entry_points(bytes: &[u8]) -> Result<(), ArchiveError> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid_wheel("entry_points.txt is not valid UTF-8"))?;
    let mut section = None;
    for (line_no, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if section.is_none() {
                return Err(invalid_wheel(format!(
                    "entry_points.txt continuation on line {} has no section",
                    line_no + 1
                )));
            }
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let name = trimmed[1..trimmed.len() - 1].trim();
            if name.is_empty() {
                return Err(invalid_wheel(format!(
                    "entry_points.txt has an empty section on line {}",
                    line_no + 1
                )));
            }
            section = Some(name.to_owned());
            continue;
        }
        let Some((name, _value)) = trimmed.split_once('=') else {
            return Err(invalid_wheel(format!(
                "entry_points.txt line {} is not a key=value entry",
                line_no + 1
            )));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(invalid_wheel(format!(
                "entry_points.txt line {} has an empty entry point name",
                line_no + 1
            )));
        }
        let Some(section) = section.as_deref() else {
            return Err(invalid_wheel(format!(
                "entry_points.txt entry on line {} has no section",
                line_no + 1
            )));
        };
        if matches!(section, "console_scripts" | "gui_scripts") && !is_valid_entry_point_name(name) {
            return Err(invalid_wheel(format!(
                "entry_points.txt has invalid entry point name {name:?} in section {section:?}"
            )));
        }
    }
    Ok(())
}

fn is_valid_entry_point_name(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('/')
        && !value.contains('\\')
        && value
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '.' | '-'))
}
