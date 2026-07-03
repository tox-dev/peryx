//! Streaming transformation of upstream PEP 691 pages.
//!
//! The transformer consumes raw upstream JSON chunk by chunk — mid-token boundaries included — and
//! emits the page velodex serves, without ever holding more than one `files[]` element: file URLs are
//! rewritten to the serving route, locally uploaded files are injected ahead of the upstream ones,
//! shadowed and hidden files are dropped, yank overrides are applied, and version lists merge. The
//! client starts receiving bytes while the upstream download is still in flight, so a cold page
//! costs wire time, not wire time plus parse-transform-serialize.

use std::collections::{BTreeSet, HashMap, HashSet};

use velodex_core::pypi::{CoreMetadata, File, to_json};

use crate::path_safety::local_file_url;

/// Per-request configuration: how to rewrite and merge one page.
#[derive(Debug, Default, Clone)]
pub struct PageContext {
    /// The route file URLs point back at, for example `root/pypi`.
    pub route: String,
    /// Locally uploaded files, emitted ahead of the upstream ones (their URLs are already local).
    pub local_files: Vec<File>,
    /// Locally known versions, merged into the upstream version list.
    pub local_versions: Vec<String>,
    /// Filenames to drop: shadowed by a local file or hidden by an override.
    pub skip: HashSet<String>,
    /// Filenames forced to the yanked state by an override.
    pub yanked: HashSet<String>,
}

/// A file's upstream source recorded while transforming, persisted later in one batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    pub sha256: String,
    pub url: String,
    /// `(sibling url, metadata sha256)` when the file advertises PEP 658 metadata.
    pub metadata: Option<(String, String)>,
}

/// Everything the transformer learned about the page, enough to persist it without a re-parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageSummary {
    pub registrations: Vec<Registration>,
    /// The page's top-level display name, when it carried one.
    pub name: Option<String>,
}

/// The transformer's lexer state, kept across chunk boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Copying bytes through, watching top-level keys.
    Passthrough,
    /// Between `files[` and its matching `]`: elements are captured and rewritten one by one.
    Files,
    /// Between `versions[` and its matching `]`: the whole (small) array is buffered and merged.
    Versions,
}

/// A chunk-at-a-time rewriter for one upstream page.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent lexer flags, not a state machine"
)]
pub struct PageTransformer {
    context: PageContext,
    mode: Mode,
    /// Nesting depth relative to the document root.
    depth: u32,
    in_string: bool,
    escaped: bool,
    /// The most recent complete top-level string, a candidate object key.
    key: Vec<u8>,
    capturing_key: bool,
    /// Set between a top-level `"name"` key's colon and its value, so the value is captured.
    expect_name_value: bool,
    capturing_name: bool,
    /// The page's top-level `name`, captured in flight so persistence needs no re-parse.
    name: Vec<u8>,
    /// The document root has closed; anything but whitespace afterwards is malformed.
    closed: bool,
    trailing: bool,
    /// Element bytes being captured (a `files` object or the whole `versions` array).
    capture: Vec<u8>,
    /// Depth at which the active array closes.
    array_depth: u32,
    emitted_in_array: bool,
    registrations: Vec<Registration>,
}

/// A malformed upstream page.
#[derive(Debug, thiserror::Error)]
pub enum TransformError {
    #[error("upstream page is not valid JSON: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("upstream page ended mid-token")]
    Truncated,
    #[error("upstream page carries data after the document root")]
    Trailing,
}

impl PageTransformer {
    #[must_use]
    pub const fn new(context: PageContext) -> Self {
        Self {
            context,
            mode: Mode::Passthrough,
            depth: 0,
            in_string: false,
            escaped: false,
            key: Vec::new(),
            capturing_key: false,
            expect_name_value: false,
            capturing_name: false,
            name: Vec::new(),
            closed: false,
            trailing: false,
            capture: Vec::new(),
            array_depth: 0,
            emitted_in_array: false,
            registrations: Vec::new(),
        }
    }

    /// Transform one chunk of upstream bytes, returning the bytes to send downstream.
    ///
    /// # Errors
    /// Returns [`TransformError::Parse`] when a captured element is not valid JSON.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, TransformError> {
        let mut out = Vec::with_capacity(chunk.len() + 64);
        for &byte in chunk {
            self.step(byte, &mut out)?;
        }
        Ok(out)
    }

    /// Finish the stream, validating that the document closed cleanly.
    ///
    /// # Errors
    /// Returns [`TransformError::Truncated`] when the document ended inside a token, or
    /// [`TransformError::Trailing`] when bytes followed the document root.
    pub fn finish(self) -> Result<PageSummary, TransformError> {
        if self.depth != 0 || self.in_string || self.mode != Mode::Passthrough {
            return Err(TransformError::Truncated);
        }
        if self.trailing {
            return Err(TransformError::Trailing);
        }
        Ok(PageSummary {
            registrations: self.registrations,
            name: String::from_utf8(self.name).ok().filter(|name| !name.is_empty()),
        })
    }

    fn step(&mut self, byte: u8, out: &mut Vec<u8>) -> Result<(), TransformError> {
        match self.mode {
            Mode::Passthrough => {
                self.step_passthrough(byte, out);
                Ok(())
            }
            Mode::Files => self.step_files(byte, out),
            Mode::Versions => self.step_versions(byte, out),
        }
    }

    fn step_passthrough(&mut self, byte: u8, out: &mut Vec<u8>) {
        if self.in_string {
            out.push(byte);
            if self.capturing_key {
                self.key.push(byte);
            }
            if self.capturing_name {
                self.name.push(byte);
            }
            if self.escaped {
                self.escaped = false;
            } else if byte == b'\\' {
                self.escaped = true;
            } else if byte == b'"' {
                self.in_string = false;
                if self.capturing_key {
                    self.key.pop();
                }
                if self.capturing_name {
                    self.name.pop();
                }
                self.capturing_key = false;
                self.capturing_name = false;
            }
            return;
        }
        match byte {
            b'"' => {
                self.in_string = true;
                // A string opening at depth 1 is an object key, or the value of the key just seen.
                if self.depth == 1 {
                    if self.expect_name_value {
                        self.capturing_name = true;
                        self.name.clear();
                    } else {
                        self.key.clear();
                        self.capturing_key = true;
                    }
                }
                self.expect_name_value = false;
                out.push(byte);
            }
            b'{' | b'[' => {
                self.depth += 1;
                // `"files": [` or `"versions": [` at the top level switches modes; the bracket is
                // emitted (files) or captured (versions merges into one emission).
                if byte == b'[' && self.depth == 2 {
                    if self.key == b"files" {
                        out.push(byte);
                        self.mode = Mode::Files;
                        self.array_depth = self.depth;
                        self.emitted_in_array = false;
                        self.emit_local_files(out);
                        return;
                    }
                    if self.key == b"versions" {
                        self.mode = Mode::Versions;
                        self.array_depth = self.depth;
                        self.capture.clear();
                        self.capture.push(byte);
                        return;
                    }
                }
                out.push(byte);
            }
            b'}' | b']' => {
                self.depth = self.depth.saturating_sub(1);
                if self.depth == 0 {
                    self.closed = true;
                }
                out.push(byte);
            }
            b':' if self.depth == 1 => {
                self.expect_name_value = self.key == b"name";
                out.push(byte);
            }
            _ => {
                if self.closed && !byte.is_ascii_whitespace() {
                    self.trailing = true;
                }
                out.push(byte);
            }
        }
    }

    fn step_files(&mut self, byte: u8, out: &mut Vec<u8>) -> Result<(), TransformError> {
        if self.in_string {
            self.capture.push(byte);
            if self.escaped {
                self.escaped = false;
            } else if byte == b'\\' {
                self.escaped = true;
            } else if byte == b'"' {
                self.in_string = false;
            }
            return Ok(());
        }
        match byte {
            b'"' => {
                self.in_string = true;
                self.capture.push(byte);
            }
            b'{' | b'[' => {
                self.depth += 1;
                self.capture.push(byte);
            }
            b'}' => {
                self.depth = self.depth.saturating_sub(1);
                self.capture.push(byte);
                if self.depth == self.array_depth {
                    self.emit_file(out)?;
                    self.capture.clear();
                }
            }
            b']' => {
                self.depth = self.depth.saturating_sub(1);
                if self.depth == self.array_depth - 1 {
                    out.push(b']');
                    self.mode = Mode::Passthrough;
                } else {
                    self.capture.push(byte);
                }
            }
            // Element separators and whitespace between elements: commas are re-managed on emit.
            b',' if self.depth == self.array_depth => {}
            _ if self.capture.is_empty() && byte.is_ascii_whitespace() => {}
            _ => self.capture.push(byte),
        }
        Ok(())
    }

    fn step_versions(&mut self, byte: u8, out: &mut Vec<u8>) -> Result<(), TransformError> {
        if self.in_string {
            self.capture.push(byte);
            if self.escaped {
                self.escaped = false;
            } else if byte == b'\\' {
                self.escaped = true;
            } else if byte == b'"' {
                self.in_string = false;
            }
            return Ok(());
        }
        match byte {
            b'"' => {
                self.in_string = true;
                self.capture.push(byte);
            }
            b'[' | b'{' => {
                self.depth += 1;
                self.capture.push(byte);
            }
            b']' | b'}' => {
                self.depth = self.depth.saturating_sub(1);
                self.capture.push(byte);
                if byte == b']' && self.depth == self.array_depth - 1 {
                    self.emit_versions(out)?;
                    self.capture.clear();
                    self.mode = Mode::Passthrough;
                }
            }
            _ => self.capture.push(byte),
        }
        Ok(())
    }

    /// Locally uploaded files open the array, ahead of anything upstream.
    fn emit_local_files(&mut self, out: &mut Vec<u8>) {
        for file in &self.context.local_files {
            if self.emitted_in_array {
                out.push(b',');
            }
            out.extend_from_slice(to_json(file).as_bytes());
            self.emitted_in_array = true;
        }
    }

    /// Rewrite one captured upstream file object and emit it, unless it is shadowed or hidden.
    fn emit_file(&mut self, out: &mut Vec<u8>) -> Result<(), TransformError> {
        let mut file: File = serde_json::from_slice(&self.capture)?;
        if self.context.skip.contains(&file.filename) {
            return Ok(());
        }
        if self.context.yanked.contains(&file.filename) {
            file.yanked = velodex_core::pypi::Yanked::Yes;
        }
        if file.url.starts_with('/') {
            // A legacy cached record already carries velodex-route URLs; serve it as-is.
            if self.emitted_in_array {
                out.push(b',');
            }
            out.extend_from_slice(to_json(&file).as_bytes());
            self.emitted_in_array = true;
            return Ok(());
        }
        if let Some(sha256) = file.hashes.get("sha256").cloned() {
            let metadata = match &file.core_metadata {
                CoreMetadata::Hashes(hashes) => hashes
                    .get("sha256")
                    .map(|digest| (format!("{}.metadata", file.url), digest.clone())),
                CoreMetadata::Absent | CoreMetadata::Available => None,
            };
            if metadata.is_none() {
                file.core_metadata = CoreMetadata::Absent;
            }
            self.registrations.push(Registration {
                sha256: sha256.clone(),
                url: file.url.clone(),
                metadata,
            });
            file.url = local_file_url(&self.context.route, &sha256, &file.filename);
        } else {
            file.core_metadata = CoreMetadata::Absent;
        }
        if self.emitted_in_array {
            out.push(b',');
        }
        out.extend_from_slice(to_json(&file).as_bytes());
        self.emitted_in_array = true;
        Ok(())
    }

    /// Merge the buffered upstream version array with the local versions and emit it sorted.
    fn emit_versions(&self, out: &mut Vec<u8>) -> Result<(), TransformError> {
        let upstream: Vec<String> = serde_json::from_slice(&self.capture)?;
        let merged: BTreeSet<String> = upstream
            .into_iter()
            .chain(self.context.local_versions.clone())
            .collect();
        let versions: Vec<String> = merged.into_iter().collect();
        out.extend_from_slice(to_json(&versions).as_bytes());
        Ok(())
    }
}

/// Build a [`PageContext`] from the overlay pieces: local files shadow upstream filenames, hidden
/// overrides drop files, yank overrides mark them.
#[must_use]
pub fn page_context<S: std::hash::BuildHasher>(
    route: &str,
    local_files: Vec<File>,
    local_versions: Vec<String>,
    overrides: &HashMap<String, String, S>,
) -> PageContext {
    let mut skip: HashSet<String> = local_files.iter().map(|file| file.filename.clone()).collect();
    let mut yanked = HashSet::new();
    for (filename, kind) in overrides {
        match kind.as_str() {
            "hidden" => {
                skip.insert(filename.clone());
            }
            "yanked" => {
                yanked.insert(filename.clone());
            }
            _ => {}
        }
    }
    PageContext {
        route: route.to_owned(),
        local_files,
        local_versions,
        skip,
        yanked,
    }
}
