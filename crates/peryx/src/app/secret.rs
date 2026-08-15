use std::io::Read;
use std::path::Path;

use anyhow::{Context as _, bail};

const MAX_SECRET_BYTES: usize = 1_048_576;

pub(super) fn read_secret(path: Option<&Path>, input: &mut dyn Read, name: &str) -> anyhow::Result<String> {
    if let Some(path) = path {
        return read_bounded(
            &mut std::fs::File::open(path).with_context(|| format!("open {name} file {}", path.display()))?,
            name,
        )
        .context(format!("read {name} file {}", path.display()));
    }
    read_bounded(input, name).with_context(|| format!("read {name} from standard input"))
}

fn read_bounded(input: &mut dyn Read, name: &str) -> anyhow::Result<String> {
    let mut bytes = Vec::new();
    input.take((MAX_SECRET_BYTES + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_SECRET_BYTES {
        bail!("{name} input exceeds the {MAX_SECRET_BYTES}-byte limit");
    }
    if bytes.ends_with(b"\r\n") {
        bytes.truncate(bytes.len() - 2);
    } else if bytes.ends_with(b"\n") {
        bytes.pop();
    }
    String::from_utf8(bytes).with_context(|| format!("{name} input must be UTF-8"))
}

#[cfg(test)]
#[path = "../../tests/unit/tests/app/secret_tests.rs"]
mod tests;
