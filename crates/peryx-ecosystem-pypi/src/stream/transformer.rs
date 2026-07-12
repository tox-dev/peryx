//! The chunk-at-a-time lexer that rewrites one upstream PEP 691 page as it streams.

use std::collections::BTreeSet;

use peryx_core::path::{is_local_file_url, local_file_url};
use peryx_policy::PolicyAction;

use super::{PageContext, PageSummary, Registration, TransformError};
use crate::policy::PypiPolicy;
use crate::simple::absolutize;
use crate::{CoreMetadata, File, parse_meta, to_json};

/// The transformer's lexer state, kept across chunk boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Copying bytes through, watching top-level keys.
    Passthrough,
    /// Capturing the top-level `meta` object so peryx can advertise its supported version.
    Meta,
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
    /// The top-level `meta` object has been checked.
    meta_seen: bool,
    project_status: Option<String>,
    project_status_reason: Option<String>,
    /// The `files` array opened before `meta` was seen, so a streaming pass cannot know whether the
    /// project is quarantined and its files must be withheld.
    files_before_meta: bool,
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
            meta_seen: false,
            files_before_meta: false,
            closed: false,
            trailing: false,
            capture: Vec::new(),
            array_depth: 0,
            emitted_in_array: false,
            registrations: Vec::new(),
            project_status: None,
            project_status_reason: None,
        }
    }

    /// Whether a bounded preflight has enough information to leave the streaming loop: either `meta`
    /// was seen (status known, safe to stream) or `files` opened first (status not yet known, so the
    /// caller buffers the whole page before emitting any file).
    #[must_use]
    pub const fn meta_preflight_done(&self) -> bool {
        self.meta_seen || self.files_before_meta
    }

    /// Whether streaming reached the `files` array before `meta`, which leaves the project status
    /// unknown; the caller then buffers the whole page and transforms it with the status seeded, so
    /// a quarantined project withholds its files regardless of key order.
    #[must_use]
    pub const fn files_precede_meta(&self) -> bool {
        self.files_before_meta
    }

    /// Seed the project status before a whole-page pass so a quarantined page withholds its files
    /// even when `files` precedes `meta` in the document.
    pub fn seed_project_status(&mut self, status: Option<String>) {
        self.project_status = status;
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
            project_status: self.project_status,
            project_status_reason: self.project_status_reason,
        })
    }

    fn step(&mut self, byte: u8, out: &mut Vec<u8>) -> Result<(), TransformError> {
        match self.mode {
            Mode::Passthrough => {
                self.step_passthrough(byte, out);
                Ok(())
            }
            Mode::Meta => self.step_meta(byte, out),
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
        // Anything but whitespace once the root has closed is trailing garbage, whatever its kind.
        if self.closed && !byte.is_ascii_whitespace() {
            self.trailing = true;
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
                // A non-string `name` value (object, array, ...) still closes the name slot.
                self.expect_name_value = false;
                self.depth += 1;
                // `"files": [` or `"versions": [` at the top level switches modes; the bracket is
                // emitted (files) or captured (versions merges into one emission).
                if byte == b'{' && self.depth == 2 && self.key == b"meta" {
                    self.mode = Mode::Meta;
                    self.array_depth = self.depth;
                    self.capture.clear();
                    self.capture.push(byte);
                    return;
                }
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
                if !self.meta_seen && self.key == b"files" {
                    self.files_before_meta = true;
                }
                out.push(byte);
            }
            _ => {
                // A non-string, non-container `name` value (null, number, ...) closes the name slot.
                if !byte.is_ascii_whitespace() {
                    self.expect_name_value = false;
                }
                out.push(byte);
            }
        }
    }

    fn step_meta(&mut self, byte: u8, out: &mut Vec<u8>) -> Result<(), TransformError> {
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
                if self.depth == self.array_depth - 1 {
                    self.emit_meta(out)?;
                    self.capture.clear();
                    self.mode = Mode::Passthrough;
                }
            }
            b']' => {
                self.depth = self.depth.saturating_sub(1);
                self.capture.push(byte);
            }
            _ => self.capture.push(byte),
        }
        Ok(())
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
        if self.project_is_quarantined() {
            return;
        }
        for file in &self.context.local_files {
            // Overrides recorded against a filename that was later uploaded locally apply to the
            // local file too, matching the buffered path; the local file otherwise seeds `skip` only
            // to shadow its upstream duplicate, so `hidden`/`yanked` must be consulted here directly.
            if self.context.hidden.contains(&file.filename) {
                continue;
            }
            if self
                .context
                .policy
                .check_file(PolicyAction::Serve, &self.context.project, file)
                .is_err()
            {
                continue;
            }
            let json = self.context.yanked.get(&file.filename).map_or_else(
                || to_json(file),
                |yanked| {
                    let mut file = file.clone();
                    file.yanked = yanked.clone();
                    to_json(&file)
                },
            );
            if self.emitted_in_array {
                out.push(b',');
            }
            out.extend_from_slice(json.as_bytes());
            self.emitted_in_array = true;
        }
    }

    /// Rewrite one captured upstream file object and emit it, unless it is shadowed or hidden.
    fn emit_file(&mut self, out: &mut Vec<u8>) -> Result<(), TransformError> {
        let mut file: File = serde_json::from_slice(&self.capture)?;
        if self.project_is_quarantined() {
            return Ok(());
        }
        if self.context.skip.contains(&file.filename) {
            return Ok(());
        }
        if self
            .context
            .policy
            .check_file(PolicyAction::Serve, &self.context.project, &file)
            .is_err()
        {
            return Ok(());
        }
        if let Some(yanked) = self.context.yanked.get(&file.filename) {
            file.yanked = yanked.clone();
        }
        if is_local_file_url(&self.context.route, &file.url) {
            // A legacy cached record already carries peryx-route URLs; serve it as-is, but still drop
            // the gpg-sig since peryx never serves the detached `.asc` at that route.
            file.gpg_sig = None;
            if self.emitted_in_array {
                out.push(b',');
            }
            out.extend_from_slice(to_json(&file).as_bytes());
            self.emitted_in_array = true;
            return Ok(());
        }
        if let Some(base) = &self.context.base {
            absolutize(base, &mut file.url);
        }
        if let Some(sha256) = file.hashes.get("sha256").cloned() {
            let metadata = if supports_metadata_sibling(&file.filename) {
                match file.metadata() {
                    CoreMetadata::Hashes(hashes) => hashes
                        .get("sha256")
                        .map(|digest| (metadata_sibling(&file.url), digest.clone())),
                    CoreMetadata::Absent | CoreMetadata::Available => None,
                }
            } else {
                None
            };
            if metadata.is_none() {
                file.clear_metadata();
            }
            self.registrations.push(Registration {
                filename: file.filename.clone(),
                sha256: sha256.clone(),
                url: file.url.clone(),
                size: file.size,
                metadata,
            });
            if file.metadata().is_absent()
                && let Some(metadata) = self.context.known_metadata.get(&sha256)
            {
                file.set_metadata(CoreMetadata::Hashes(std::collections::BTreeMap::from([(
                    "sha256".to_owned(),
                    metadata.clone(),
                )])));
            }
            file.url = local_file_url(&self.context.route, &sha256, &file.filename);
            // The URL now points at peryx's route, which never serves the detached `.asc` sibling,
            // so drop any inherited gpg-sig rather than advertise a signature peryx cannot serve.
            file.gpg_sig = None;
        } else {
            file.clear_metadata();
        }
        if self.emitted_in_array {
            out.push(b',');
        }
        out.extend_from_slice(to_json(&file).as_bytes());
        self.emitted_in_array = true;
        Ok(())
    }

    /// Rewrite the upstream meta object to peryx's advertised API version.
    fn emit_meta(&mut self, out: &mut Vec<u8>) -> Result<(), TransformError> {
        let meta = parse_meta(&self.capture)?;
        self.project_status.clone_from(&meta.project_status);
        self.project_status_reason.clone_from(&meta.project_status_reason);
        out.extend_from_slice(to_json(&meta).as_bytes());
        self.meta_seen = true;
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

    fn project_is_quarantined(&self) -> bool {
        self.project_status.as_deref() == Some("quarantined")
    }
}

/// The PEP 658 metadata sibling of a file URL: `.metadata` appended to the path, ahead of any query
/// or fragment. A signed upstream URL like `pkg.whl?token=abc` must yield `pkg.whl.metadata?token=abc`,
/// not `pkg.whl?token=abc.metadata`.
pub fn metadata_sibling(url: &str) -> String {
    let cut = url.find(['?', '#']).unwrap_or(url.len());
    format!("{}.metadata{}", &url[..cut], &url[cut..])
}

fn supports_metadata_sibling(filename: &str) -> bool {
    std::path::Path::new(filename)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("whl"))
        || filename
            .get(filename.len().saturating_sub(7)..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".tar.gz"))
}
