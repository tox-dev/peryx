//! The chunk-at-a-time lexer that rewrites one upstream PEP 691 page as it streams.

use std::collections::BTreeSet;

use peryx_core::path::{is_local_artifact_url, local_artifact_url};
use peryx_policy::PolicyAction;
use serde::Serialize;

use super::validator::JsonValidator;
use super::{PageContext, PageSummary, Registration, TransformError, is_json_whitespace};
use crate::policy::{PypiPolicy, RemoteMetadataMode, VersionAdmission, apply_version_policy, served_version};
use crate::simple::{ProjectStatusObject, absolutize, parse_project_status};
use crate::{CoreMetadata, File, SimpleError, parse_meta};

/// The most raw bytes peryx will read from one upstream Simple page before refusing it. The biggest
/// real project pages (botocore and friends) sit in the low single-digit MiB; a page an order of
/// magnitude past that is pathological, and parsing it unbounded is the memory-exhaustion vector this
/// guards against.
pub const MAX_PAGE_BYTES: usize = 64 * 1024 * 1024;
/// The most file entries peryx will transform from one upstream Simple page. Bytes alone do not bound
/// per-element work: a page of many tiny file objects stays small yet still forces a parse, a policy
/// check, and a registration each, so the element count is capped on its own.
const MAX_PAGE_FILES: usize = 500_000;
const MAX_KEY_BYTES: usize = b"project-status".len();

/// The transformer's lexer state, kept across chunk boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Copying bytes through, watching top-level keys.
    Passthrough,
    /// Capturing the top-level `meta` object so peryx can advertise its supported version.
    Meta,
    ProjectStatus,
    /// Between `files[` and its matching `]`: elements are captured and rewritten one by one.
    Files,
    /// Between `versions[` and its matching `]`: the whole (small) array is buffered and merged.
    Versions,
}

/// Escape-decoding sub-state for the object key being captured. PEP 691 member names carry meaning
/// (`files`, `meta`, `name`, `versions`), and RFC 8259 lets any character be spelled with an escape,
/// so the key is decoded to its actual value before dispatch: `"files"` matches `files`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyDecode {
    /// Reading literal key bytes.
    Literal,
    /// Saw a backslash; the next byte selects the escape.
    Escape,
    /// Inside a `\uXXXX` sequence, holding the digits seen and the value so far.
    Unicode { seen: u8, value: u16 },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StringCapture {
    None,
    Key,
    Name,
}

struct StringState {
    active: bool,
    escaped: bool,
    capture: StringCapture,
    expect_name: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StatusState {
    Unseen,
    Seen,
    Emitted,
}

struct HeaderState {
    meta_seen: bool,
    status: StatusState,
    files_precede_headers: bool,
}

struct DocumentState {
    headers: HeaderState,
    closed: bool,
    trailing: bool,
    emitted_in_array: bool,
    pep700: Pep700State,
}

/// PEP 700's promise as the page streams: whether `meta` declared a version that owes `versions` and
/// a per-file `size`, and whether the array has arrived. PEP 691 fixes no member order, so either
/// half may be learned first.
struct Pep700State {
    promised: bool,
    versions_seen: bool,
}

/// A chunk-at-a-time rewriter for one upstream page.
pub struct PageTransformer {
    context: PageContext,
    mode: Mode,
    /// Nesting depth relative to the document root.
    depth: u32,
    string: StringState,
    /// Unknown keys pass through, so storage stops at the longest key that changes output.
    key: [u8; MAX_KEY_BYTES],
    key_len: usize,
    key_decode: KeyDecode,
    /// The page's top-level `name`, captured in flight so persistence needs no re-parse.
    name: Vec<u8>,
    project_status: Option<String>,
    project_status_reason: Option<String>,
    document: DocumentState,
    /// Element bytes being captured (a `files` object or the whole `versions` array).
    capture: Vec<u8>,
    /// The releases of the files served so far, which join the declared set so no served file is
    /// left belonging to no listed release.
    served_versions: BTreeSet<String>,
    /// Depth at which the active array closes.
    array_depth: u32,
    registrations: Vec<Registration>,
    /// Raw upstream bytes fed so far, checked against [`MAX_PAGE_BYTES`].
    consumed: usize,
    /// Upstream file elements captured so far, checked against [`MAX_PAGE_FILES`].
    files_seen: usize,
    /// The full-grammar guard: the structural lexer copies unrecognized bytes through untouched, so
    /// this independently enforces RFC 8259 and the PEP 691 object root over every raw byte.
    validator: JsonValidator,
}

impl PageTransformer {
    #[must_use]
    pub const fn new(context: PageContext) -> Self {
        Self {
            context,
            mode: Mode::Passthrough,
            depth: 0,
            string: StringState {
                active: false,
                escaped: false,
                capture: StringCapture::None,
                expect_name: false,
            },
            key: [0; MAX_KEY_BYTES],
            key_len: 0,
            key_decode: KeyDecode::Literal,
            name: Vec::new(),
            document: DocumentState {
                headers: HeaderState {
                    meta_seen: false,
                    status: StatusState::Unseen,
                    files_precede_headers: false,
                },
                closed: false,
                trailing: false,
                emitted_in_array: false,
                pep700: Pep700State {
                    promised: false,
                    versions_seen: false,
                },
            },
            capture: Vec::new(),
            served_versions: BTreeSet::new(),
            array_depth: 0,
            registrations: Vec::new(),
            consumed: 0,
            files_seen: 0,
            project_status: None,
            project_status_reason: None,
            validator: JsonValidator::new(),
        }
    }

    /// Whether preflight validated both headers or reached `files` before it could.
    #[must_use]
    pub const fn header_preflight_done(&self) -> bool {
        self.document.headers.files_precede_headers || self.headers_known()
    }

    /// Whether streaming reached `files` before validating `meta` and resolving project status.
    #[must_use]
    pub const fn files_precede_headers(&self) -> bool {
        self.document.headers.files_precede_headers
    }

    /// Whether preflight validated the API version and resolved project status.
    #[must_use]
    pub const fn headers_known(&self) -> bool {
        self.document.headers.meta_seen
            && matches!(self.document.headers.status, StatusState::Seen | StatusState::Emitted)
    }

    /// Seed a whole-page pass after parsing established the explicit or absent status.
    pub fn seed_project_status(&mut self, status: Option<String>, reason: Option<String>) {
        self.project_status = status;
        self.project_status_reason = reason;
        self.document.headers.meta_seen = true;
        self.document.headers.status = StatusState::Seen;
    }

    /// # Errors
    /// Returns [`TransformError::Parse`] when a captured element is not valid JSON, or
    /// [`TransformError::TooLarge`] once the page exceeds `MAX_PAGE_BYTES`.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, TransformError> {
        let mut out = Vec::with_capacity(chunk.len() + 64);
        self.push_into(chunk, &mut out)?;
        Ok(out)
    }

    /// # Errors
    /// Returns [`TransformError::Parse`] when a captured element is not valid JSON, or
    /// [`TransformError::TooLarge`] once the page exceeds `MAX_PAGE_BYTES`.
    pub fn push_into(&mut self, chunk: &[u8], out: &mut Vec<u8>) -> Result<(), TransformError> {
        self.consumed = self.consumed.saturating_add(chunk.len());
        if self.consumed > MAX_PAGE_BYTES {
            return Err(TransformError::TooLarge);
        }
        out.reserve(chunk.len());
        for &byte in chunk {
            self.validator.feed(byte);
            self.step(byte, out)?;
        }
        Ok(())
    }

    /// # Errors
    /// Returns [`TransformError::Truncated`] when the document ended inside a token,
    /// [`TransformError::Trailing`] when bytes followed the document root,
    /// [`TransformError::Malformed`] when the bytes were not a single well-formed JSON object, or
    /// [`TransformError::Simple`] when a page declaring Simple API 1.1 or newer carried no
    /// `versions` array.
    pub fn finish(self) -> Result<PageSummary, TransformError> {
        if self.depth != 0 || self.string.active || self.mode != Mode::Passthrough {
            return Err(TransformError::Truncated);
        }
        if self.document.trailing {
            return Err(TransformError::Trailing);
        }
        self.validator.result()?;
        if self.document.pep700.promised && !self.document.pep700.versions_seen {
            return Err(SimpleError::MissingVersions.into());
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
            Mode::ProjectStatus => self.step_project_status(byte, out),
            Mode::Files => self.step_files(byte, out),
            Mode::Versions => self.step_versions(byte, out),
        }
    }

    fn step_passthrough(&mut self, byte: u8, out: &mut Vec<u8>) {
        if self.string.active {
            self.step_passthrough_string(byte, out);
            return;
        }
        // Anything but whitespace once the root has closed is trailing garbage, whatever its kind.
        if self.document.closed && !is_json_whitespace(byte) {
            self.document.trailing = true;
        }
        match byte {
            b'"' => {
                self.string.active = true;
                // A string opening at depth 1 is an object key, or the value of the key just seen.
                if self.depth == 1 {
                    if self.string.expect_name {
                        self.string.capture = StringCapture::Name;
                        self.name.clear();
                    } else {
                        self.key_len = 0;
                        self.key_decode = KeyDecode::Literal;
                        self.string.capture = StringCapture::Key;
                    }
                }
                self.string.expect_name = false;
                out.push(byte);
            }
            b'{' | b'[' => {
                // A non-string `name` value (object, array, ...) still closes the name slot.
                self.string.expect_name = false;
                self.depth += 1;
                // `"files": [` or `"versions": [` at the top level switches modes; the bracket is
                // emitted (files) or captured (versions merges into one emission).
                if byte == b'{' && self.depth == 2 {
                    if self.key() == b"meta" {
                        self.mode = Mode::Meta;
                        self.array_depth = self.depth;
                        self.capture.clear();
                        self.capture.push(byte);
                        return;
                    }
                    if self.key() == b"project-status" {
                        self.mode = Mode::ProjectStatus;
                        self.array_depth = self.depth;
                        self.capture.clear();
                        self.capture.push(byte);
                        return;
                    }
                }
                if byte == b'[' && self.depth == 2 {
                    if self.key() == b"files" {
                        out.push(byte);
                        self.mode = Mode::Files;
                        self.array_depth = self.depth;
                        self.document.emitted_in_array = false;
                        self.emit_local_files(out);
                        return;
                    }
                    if self.key() == b"versions" {
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
                if byte == b'}' && self.depth == 1 {
                    self.emit_seeded_project_status(out);
                }
                self.depth = self.depth.saturating_sub(1);
                if self.depth == 0 {
                    self.document.closed = true;
                }
                out.push(byte);
            }
            b':' if self.depth == 1 => {
                self.string.expect_name = self.key() == b"name";
                if !self.headers_known() && self.key() == b"files" {
                    self.document.headers.files_precede_headers = true;
                }
                out.push(byte);
            }
            _ => {
                // A non-string, non-container `name` value (null, number, ...) closes the name slot.
                if !is_json_whitespace(byte) {
                    self.string.expect_name = false;
                }
                out.push(byte);
            }
        }
    }

    fn step_passthrough_string(&mut self, byte: u8, out: &mut Vec<u8>) {
        out.push(byte);
        if self.string.capture == StringCapture::Key && (self.string.escaped || byte != b'"') {
            self.decode_key_byte(byte);
        }
        if self.string.capture == StringCapture::Name {
            self.name.push(byte);
        }
        if self.string.escaped {
            self.string.escaped = false;
        } else if byte == b'\\' {
            self.string.escaped = true;
        } else if byte == b'"' {
            self.string.active = false;
            if self.string.capture == StringCapture::Name {
                self.name.pop();
            }
            self.string.capture = StringCapture::None;
        }
    }

    /// Feed one raw key byte through the JSON string-escape grammar, appending decoded bytes to the
    /// key buffer so dispatch matches the member name's value, not its spelling.
    fn decode_key_byte(&mut self, byte: u8) {
        match self.key_decode {
            KeyDecode::Literal => {
                if byte == b'\\' {
                    self.key_decode = KeyDecode::Escape;
                } else {
                    self.push_key_byte(byte);
                }
            }
            KeyDecode::Escape => {
                if byte == b'u' {
                    self.key_decode = KeyDecode::Unicode { seen: 0, value: 0 };
                } else {
                    // Only `\uXXXX` can spell an ASCII letter, so only it can build a dispatched
                    // member name. Every other escape resolves to a byte (`"`, `\`, `/`, control) no
                    // control key contains, so a sentinel that matches nothing stands in and the key
                    // falls through to passthrough.
                    self.key_decode = KeyDecode::Literal;
                    self.push_key_byte(0xFF);
                }
            }
            KeyDecode::Unicode { seen, value } => {
                let digit = match byte {
                    b'0'..=b'9' => byte - b'0',
                    b'a'..=b'f' => byte - b'a' + 10,
                    b'A'..=b'F' => byte - b'A' + 10,
                    _ => {
                        self.key_decode = KeyDecode::Literal;
                        self.push_key_byte(0xFF);
                        return;
                    }
                };
                let value = value << 4 | u16::from(digit);
                if seen + 1 == 4 {
                    self.key_decode = KeyDecode::Literal;
                    self.push_key_codepoint(value);
                } else {
                    self.key_decode = KeyDecode::Unicode { seen: seen + 1, value };
                }
            }
        }
    }

    /// Append a decoded `\uXXXX` codepoint to the key buffer as UTF-8. A lone surrogate is no valid
    /// scalar and no member name peryx dispatches on, so a sentinel byte stands in: it cannot match.
    fn push_key_codepoint(&mut self, codepoint: u16) {
        match char::from_u32(u32::from(codepoint)) {
            Some(decoded) => {
                let mut buffer = [0; 4];
                for &byte in decoded.encode_utf8(&mut buffer).as_bytes() {
                    self.push_key_byte(byte);
                }
            }
            None => self.push_key_byte(0xFF),
        }
    }

    const fn push_key_byte(&mut self, byte: u8) {
        if self.key_len < MAX_KEY_BYTES {
            self.key[self.key_len] = byte;
        }
        self.key_len += 1;
    }

    fn key(&self) -> &[u8] {
        self.key.get(..self.key_len).unwrap_or_default()
    }

    fn step_meta(&mut self, byte: u8, out: &mut Vec<u8>) -> Result<(), TransformError> {
        if self.string.active {
            self.capture.push(byte);
            if self.string.escaped {
                self.string.escaped = false;
            } else if byte == b'\\' {
                self.string.escaped = true;
            } else if byte == b'"' {
                self.string.active = false;
            }
            return Ok(());
        }
        match byte {
            b'"' => {
                self.string.active = true;
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

    fn step_project_status(&mut self, byte: u8, out: &mut Vec<u8>) -> Result<(), TransformError> {
        if self.string.active {
            self.capture.push(byte);
            if self.string.escaped {
                self.string.escaped = false;
            } else if byte == b'\\' {
                self.string.escaped = true;
            } else if byte == b'"' {
                self.string.active = false;
            }
            return Ok(());
        }
        match byte {
            b'"' => {
                self.string.active = true;
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
                    self.emit_project_status(out)?;
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
        if self.string.active {
            self.capture.push(byte);
            if self.string.escaped {
                self.string.escaped = false;
            } else if byte == b'\\' {
                self.string.escaped = true;
            } else if byte == b'"' {
                self.string.active = false;
            }
            return Ok(());
        }
        match byte {
            b'"' => {
                self.string.active = true;
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
            b',' if self.depth == self.array_depth => {}
            _ if self.capture.is_empty() && is_json_whitespace(byte) => {}
            _ => self.capture.push(byte),
        }
        Ok(())
    }

    fn step_versions(&mut self, byte: u8, out: &mut Vec<u8>) -> Result<(), TransformError> {
        if self.string.active {
            self.capture.push(byte);
            if self.string.escaped {
                self.string.escaped = false;
            } else if byte == b'\\' {
                self.string.escaped = true;
            } else if byte == b'"' {
                self.string.active = false;
            }
            return Ok(());
        }
        match byte {
            b'"' => {
                self.string.active = true;
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

    /// Remember the release a served file belongs to, so `versions` lists it even when upstream did
    /// not declare it.
    ///
    /// Only until that array is written. PEP 691 fixes no member order, and the order registries
    /// actually send puts `versions` first, so past that point the answer is already out and reading
    /// a release off every remaining filename would parse four hundred names for nothing.
    fn record_served(&mut self, filename: &str) {
        if self.document.pep700.versions_seen {
            return;
        }
        if let Some(version) = served_version(filename) {
            self.served_versions.insert(version);
        }
    }

    fn emit_local_files(&mut self, out: &mut Vec<u8>) {
        if self.project_is_quarantined() {
            return;
        }
        let mut served: BTreeSet<String> = BTreeSet::new();
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
            if self.document.emitted_in_array {
                out.push(b',');
            }
            if let Some(yanked) = self.context.yanked.get(&file.filename) {
                let mut file = file.clone();
                file.yanked = yanked.clone();
                write_json(out, &file);
            } else {
                write_json(out, file);
            }
            self.document.emitted_in_array = true;
            if !self.document.pep700.versions_seen {
                served.extend(served_version(&file.filename));
            }
        }
        self.served_versions.append(&mut served);
    }

    /// Rewrite one captured upstream file object and emit it, unless it is shadowed or hidden.
    fn emit_file(&mut self, out: &mut Vec<u8>) -> Result<(), TransformError> {
        self.files_seen += 1;
        if self.files_seen > MAX_PAGE_FILES {
            return Err(TransformError::TooLarge);
        }
        let mut file: File = serde_json::from_slice(&self.capture)?;
        file.provenance.retain_secure_url();
        // Checked before the skip and policy filters: the page's conformance is a property of what
        // the upstream sent, not of the subset peryx would re-serve.
        if self.document.pep700.promised && file.size.is_none() {
            return Err(SimpleError::MissingFileSize(file.filename).into());
        }
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
        let sha256 = file.hashes.get("sha256").cloned();
        if sha256
            .as_deref()
            .is_some_and(|sha256| is_local_artifact_url(&self.context.route, sha256, &file.filename, &file.url))
        {
            // A legacy cached record already carries peryx-route URLs; serve it as-is, but still drop
            // the gpg-sig since peryx never serves the detached `.asc` at that route.
            file.gpg_sig = None;
            if self.document.emitted_in_array {
                out.push(b',');
            }
            write_json(out, &file);
            self.document.emitted_in_array = true;
            self.record_served(&file.filename);
            return Ok(());
        }
        if let Some(base) = &self.context.base {
            absolutize(base, &mut file.url);
        }
        if let Some(sha256) = sha256 {
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
                provenance: file.provenance.secure_url().map(str::to_owned),
            });
            if file.metadata().is_absent()
                && let Some(metadata) = self.context.known_metadata.get(&sha256)
            {
                file.set_metadata(CoreMetadata::Hashes(std::collections::BTreeMap::from([(
                    "sha256".to_owned(),
                    metadata.clone(),
                )])));
            }
            file.url = local_artifact_url(&self.context.route, &sha256, &file.filename);
            if self.context.policy.remote_metadata_mode() != RemoteMetadataMode::Direct
                && file.provenance.secure_url().is_some()
            {
                file.provenance = crate::Provenance::Url(local_artifact_url(
                    &self.context.route,
                    &sha256,
                    &format!("{}.provenance", file.filename),
                ));
            }
            // The URL now points at peryx's route, which never serves the detached `.asc` sibling,
            // so drop any inherited gpg-sig rather than advertise a signature peryx cannot serve.
            file.gpg_sig = None;
        } else {
            file.clear_metadata();
        }
        if self.document.emitted_in_array {
            out.push(b',');
        }
        write_json(out, &file);
        self.document.emitted_in_array = true;
        self.record_served(&file.filename);
        Ok(())
    }

    fn emit_meta(&mut self, out: &mut Vec<u8>) -> Result<(), TransformError> {
        let meta = parse_meta(&self.capture)?;
        write_json(out, &meta);
        self.document.headers.meta_seen = true;
        self.document.pep700.promised = meta.promises_pep700();
        Ok(())
    }

    fn emit_project_status(&mut self, out: &mut Vec<u8>) -> Result<(), TransformError> {
        (self.project_status, self.project_status_reason) = parse_project_status(&self.capture)?;
        write_json(
            out,
            &ProjectStatusObject {
                status: self.project_status.as_deref(),
                reason: self.project_status_reason.as_deref(),
            },
        );
        self.document.headers.status = StatusState::Emitted;
        Ok(())
    }

    fn emit_seeded_project_status(&mut self, out: &mut Vec<u8>) {
        if self.document.headers.status == StatusState::Emitted
            || (self.project_status.is_none() && self.project_status_reason.is_none())
        {
            return;
        }
        out.extend_from_slice(b",\"project-status\":");
        write_json(
            out,
            &ProjectStatusObject {
                status: self.project_status.as_deref(),
                reason: self.project_status_reason.as_deref(),
            },
        );
        self.document.headers.status = StatusState::Emitted;
    }

    fn emit_versions(&mut self, out: &mut Vec<u8>) -> Result<(), TransformError> {
        let upstream: Vec<String> = serde_json::from_slice(&self.capture)?;
        let mut listed: BTreeSet<&str> = BTreeSet::new();
        // PEP 700 defines `versions` as a set, so an upstream repeat is a broken page rather than
        // something to silently collapse into the merge with peryx's own versions. The same set then
        // becomes the answer, so a page's releases are walked once.
        if let Some(duplicate) = upstream.iter().find(|version| !listed.insert(version)) {
            return Err(SimpleError::DuplicateVersion(duplicate.clone()).into());
        }
        apply_version_policy(
            &mut listed,
            &VersionAdmission::of(&self.context.policy),
            self.context.local_versions.iter().map(String::as_str),
            self.served_versions.iter().map(String::as_str),
        );
        write_json(out, &listed);
        drop(listed);
        self.document.pep700.versions_seen = true;
        Ok(())
    }

    fn project_is_quarantined(&self) -> bool {
        self.project_status.as_deref() == Some("quarantined")
    }
}

fn write_json(out: &mut Vec<u8>, value: &impl Serialize) {
    serde_json::to_writer(out, value).expect("simple-API model always serializes to JSON");
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
