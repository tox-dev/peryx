//! Parse a PEP 503 HTML simple detail page into the model, so peryx can proxy HTML-only upstreams
//! (Artifactory, GitLab, plain static indexes) and re-serve them as JSON downstream.

use std::borrow::Cow;
use std::collections::BTreeMap;

use tl::{HTMLTag, ParserOptions};
use url::Url;

use super::simple::{
    CoreMetadata, File, Meta, ParsedDetail, ProjectList, ProjectListEntry, Provenance, SimpleError, Yanked,
};

/// # Errors
/// Returns an error when the HTML page advertises an unsupported Simple API major version.
pub fn parse_detail_html(project: &str, html: &str, base: &Url) -> Result<ParsedDetail, SimpleError> {
    let dom = tl::parse(html, ParserOptions::default())?;
    let base = link_base(&dom, base);
    let mut meta = UpstreamMeta::default();
    let mut files = Vec::new();
    for tag in dom.nodes().iter().filter_map(|node| node.as_tag()) {
        if is_tag(tag, b"a") {
            files.extend(anchor_to_file(tag, &base));
        } else if is_tag(tag, b"meta") {
            meta.read(tag);
        }
    }
    Ok(ParsedDetail {
        meta: meta.build()?,
        name: project.to_owned(),
        versions: Vec::new(),
        files,
    })
}

/// Parse the HTML root project list, resolving anchors the same way a PEP 503 client would.
///
/// # Errors
/// Returns an error when the HTML page advertises an unsupported Simple API major version.
pub fn parse_index_html(html: &str, base: &Url) -> Result<ProjectList, SimpleError> {
    let dom = tl::parse(html, ParserOptions::default())?;
    let base = link_base(&dom, base);
    let parser = dom.parser();
    let mut meta = UpstreamMeta::default();
    let mut projects = Vec::new();
    for tag in dom.nodes().iter().filter_map(|node| node.as_tag()) {
        if is_tag(tag, b"a") {
            projects.extend(anchor_to_project(tag, &base, parser));
        } else if is_tag(tag, b"meta") {
            meta.read(tag);
        }
    }
    Ok(ProjectList {
        meta: meta.build()?,
        projects,
    })
}

/// The PEP 700/708 `<meta>` values a Simple page carries, gathered as the document is walked.
///
/// A page is thousands of anchors and a handful of meta tags. Collecting the meta tags in their own
/// pass walked every anchor a second time to look at its tag name and discard it.
#[derive(Default)]
struct UpstreamMeta {
    api_version: Option<String>,
    project_status: Option<String>,
    project_status_reason: Option<String>,
}

impl UpstreamMeta {
    /// Read one `<meta>` tag, keeping the values PEP 700 and PEP 708 define.
    fn read(&mut self, tag: &HTMLTag) {
        let Some(name) = attr_string(tag, "name") else {
            return;
        };
        match name.as_str() {
            "pypi:repository-version" => self.api_version = attr_string(tag, "content"),
            "pypi:project-status" => self.project_status = attr_string(tag, "content"),
            "pypi:project-status-reason" => self.project_status_reason = attr_string(tag, "content"),
            _ => {}
        }
    }

    /// # Errors
    /// Returns an error when the page advertises an unsupported Simple API major version.
    fn build(self) -> Result<Meta, SimpleError> {
        Meta::from_upstream(
            self.api_version.as_deref(),
            self.project_status,
            self.project_status_reason,
        )
    }
}

fn link_base(dom: &tl::VDom<'_>, base: &Url) -> Url {
    dom.nodes()
        .iter()
        .filter_map(|node| node.as_tag())
        .take_while(|tag| !is_tag(tag, b"a") && !is_tag(tag, b"link"))
        .find(|tag| is_tag(tag, b"base"))
        .and_then(|tag| attr_string(tag, "href"))
        .and_then(|href| base.join(&href).ok())
        .unwrap_or_else(|| base.clone())
}

fn anchor_to_project(tag: &HTMLTag, base: &Url, parser: &tl::Parser<'_>) -> Option<ProjectListEntry> {
    let href = attr_value(tag, "href").filter(|href| !href.is_empty())?;
    let name = decode_entities(tag.inner_text(parser).trim(), Context::Text).into_owned();
    if !name.is_empty() {
        return Some(ProjectListEntry { name });
    }
    // Only an anchor with no text needs its target resolved to name the project, and PEP 503 says
    // the text is the name. Resolving every anchor's href parses a URL per project on a page that
    // lists every project there is.
    let resolved = base.join(&decode_entities(&href, Context::Attribute)).ok()?;
    Some(ProjectListEntry {
        name: project_from_url(&resolved)?,
    })
}

#[must_use]
pub fn project_from_url(url: &Url) -> Option<String> {
    url.path()
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .map(percent_decode)
}

fn anchor_to_file(tag: &HTMLTag, base: &Url) -> Option<File> {
    let href = attr_string(tag, "href").filter(|href| !href.is_empty())?;
    let mut resolved = base.join(&href).ok()?;
    let hashes = fragment_hash(resolved.fragment());
    let filename = filename_from_url(&resolved)?;
    resolved.set_fragment(None);
    let mut file = File {
        filename,
        url: resolved.to_string(),
        hashes,
        requires_python: attr_string(tag, "data-requires-python"),
        size: attr_string(tag, "data-size").and_then(|value| value.parse().ok()),
        upload_time: attr_string(tag, "data-upload-time"),
        yanked: parse_yanked(tag),
        core_metadata: parse_metadata_attr(tag, "data-core-metadata"),
        dist_info_metadata: parse_metadata_attr(tag, "data-dist-info-metadata"),
        gpg_sig: parse_gpg_sig(tag),
        provenance: attr_string(tag, "data-provenance").map_or(Provenance::Absent, Provenance::Url),
    };
    file.provenance.retain_secure_url();
    Some(file)
}

fn filename_from_url(url: &Url) -> Option<String> {
    let filename = url
        .path()
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|filename| !filename.is_empty())?;
    Some(percent_decode(filename))
}

fn fragment_hash(fragment: Option<&str>) -> BTreeMap<String, String> {
    let mut hashes = BTreeMap::new();
    let Some(fragment) = fragment.map(percent_decode) else {
        return hashes;
    };
    for part in fragment.split('&') {
        let Some((algo, value)) = part.split_once('=') else {
            continue;
        };
        if is_supported_hash(algo) && !value.is_empty() {
            hashes.insert(algo.to_owned(), value.to_owned());
        }
    }
    hashes
}

fn is_supported_hash(algo: &str) -> bool {
    matches!(algo, "sha512" | "sha384" | "sha256" | "sha224" | "sha1" | "md5")
}

fn is_tag(tag: &HTMLTag, name: &[u8]) -> bool {
    tag.name().as_bytes().eq_ignore_ascii_case(name)
}

fn attr_string(tag: &HTMLTag, name: &str) -> Option<String> {
    attr_value(tag, name).map(|value| decode_entities(&value, Context::Attribute).into_owned())
}

fn attr_value<'tag>(tag: &'tag HTMLTag, name: &'tag str) -> Option<Cow<'tag, str>> {
    tag.attributes()
        .get(name)
        .flatten()
        .map(|value| value.as_utf8_str())
        .or_else(|| {
            tag.attributes().iter().find_map(|(attr_name, value)| {
                attr_name
                    .eq_ignore_ascii_case(name)
                    .then_some(value)
                    .and_then(|value| value)
            })
        })
}

fn has_attr(tag: &HTMLTag, name: &str) -> bool {
    tag.attributes().contains(name)
        || tag
            .attributes()
            .iter()
            .any(|(attr_name, _)| attr_name.eq_ignore_ascii_case(name))
}

fn parse_yanked(tag: &HTMLTag) -> Yanked {
    if !has_attr(tag, "data-yanked") {
        return Yanked::No;
    }
    let reason = attr_string(tag, "data-yanked").unwrap_or_default();
    if reason.is_empty() {
        Yanked::Yes
    } else {
        Yanked::Reason(reason)
    }
}

fn parse_metadata_attr(tag: &HTMLTag, name: &str) -> CoreMetadata {
    if !has_attr(tag, name) {
        return CoreMetadata::Absent;
    }
    let value = attr_string(tag, name).unwrap_or_default();
    match value.as_str() {
        "false" => CoreMetadata::Absent,
        "true" | "" => CoreMetadata::Available,
        _ => match value.split_once('=') {
            Some((algo, hash)) => CoreMetadata::Hashes(BTreeMap::from([(algo.to_owned(), hash.to_owned())])),
            None => CoreMetadata::Available,
        },
    }
}

fn parse_gpg_sig(tag: &HTMLTag) -> Option<bool> {
    if !has_attr(tag, "data-gpg-sig") {
        return None;
    }
    match attr_string(tag, "data-gpg-sig").as_deref() {
        Some(value) if value.eq_ignore_ascii_case("false") => Some(false),
        Some(value) if value.eq_ignore_ascii_case("true") => Some(true),
        None | Some("") => Some(true),
        Some(_) => None,
    }
}

/// Whether a character reference sits in element text or in an attribute value.
///
/// The HTML tokenizer treats the two contexts differently: a named reference not ending in `;` is
/// left untouched inside an attribute when the next character is `=` or alphanumeric, because URLs
/// carry query strings such as `?a=1&copy=2` where `&copy` must stay literal.
#[derive(Clone, Copy)]
enum Context {
    Text,
    Attribute,
}

/// Decode the HTML character references a Simple page may carry, allocating only when one is present.
///
/// A reference begins with `&`, and almost no attribute or anchor text on a Simple page contains one,
/// so the common case borrows. Named references resolve against the full HTML5 table (`&amp;`,
/// `&period;`, `&sol;`, the two-code-point `&fjlig;`, and the semicolon-less legacy forms);
/// numeric references (`&#46;`, `&#x2e;`) decode to their scalar with the tokenizer's replacements
/// for the C1 range and out-of-range or surrogate values. Anything that is not a well-formed
/// reference is left as written, so malformed upstream markup round-trips instead of turning into a
/// stray character.
fn decode_entities(text: &str, context: Context) -> Cow<'_, str> {
    if !text.contains('&') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let reference = &rest[start..];
        if let Some((decoded, count, consumed)) = decode_reference(reference, context) {
            out.extend(&decoded[..count]);
            rest = &reference[consumed..];
        } else {
            out.push('&');
            rest = &reference[1..];
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// Decode one HTML character reference at the start of `text` (which begins with `&`), returning its
/// one or two code points, how many are valid, and the byte length the reference spans. `None` means
/// `&` does not begin a reference, so the caller emits a literal `&` and rescans from the next byte.
fn decode_reference(text: &str, context: Context) -> Option<([char; 2], usize, usize)> {
    let body = &text[1..];
    let (chars, count, consumed) = match body.as_bytes().first()? {
        b'#' => {
            let digits = &body[1..];
            let (radix, prefix) = if digits.starts_with(['x', 'X']) {
                (16, 2)
            } else {
                (10, 1)
            };
            let digits = &digits[prefix - 1..];
            let mut num: u32 = 0;
            let mut too_big = false;
            let mut span = 0;
            for character in digits.chars() {
                let Some(value) = character.to_digit(radix) else {
                    break;
                };
                num = num.wrapping_mul(radix);
                too_big |= num > 0x10_FFFF;
                num = num.wrapping_add(value);
                span += 1;
            }
            if span == 0 {
                return None;
            }
            let mut consumed = prefix + span;
            if body[consumed..].starts_with(';') {
                consumed += 1;
            }
            ([numeric_char(num, too_big), '\0'], 1, consumed)
        }
        byte if byte.is_ascii_alphanumeric() => {
            let mut best: Option<(u32, u32)> = None;
            let mut best_len = 0;
            let mut span = 0;
            for character in body.chars() {
                span += character.len_utf8();
                match web_atoms::NAMED_ENTITIES.get(&body[..span]) {
                    Some(&(first, second)) if first != 0 => {
                        best = Some((first, second));
                        best_len = span;
                    }
                    Some(_) => {}
                    None => break,
                }
            }
            let (first, second) = best?;
            let next = body[best_len..].chars().next();
            let ambiguous = matches!(context, Context::Attribute)
                && !body[..best_len].ends_with(';')
                && matches!(next, Some(character) if character == '=' || character.is_ascii_alphanumeric());
            if ambiguous {
                return None;
            }
            let chars = [
                char::from_u32(first).expect("named reference code point is a valid scalar"),
                char::from_u32(second).unwrap_or('\0'),
            ];
            (chars, if second == 0 { 1 } else { 2 }, best_len)
        }
        _ => return None,
    };
    Some((chars, count, consumed + 1))
}

fn numeric_char(num: u32, too_big: bool) -> char {
    match num {
        _ if too_big || num > 0x10_FFFF => '\u{FFFD}',
        0x00 | 0xD800..=0xDFFF => '\u{FFFD}',
        0x80..=0x9F => web_atoms::C1_REPLACEMENTS[(num - 0x80) as usize]
            .unwrap_or_else(|| char::from_u32(num).expect("C1 code point is a valid scalar")),
        _ => char::from_u32(num).expect("non-surrogate scalar value in range"),
    }
}

fn percent_decode(text: &str) -> String {
    let mut bytes = Vec::with_capacity(text.len());
    let input = text.as_bytes();
    let mut index = 0;
    while let Some(&byte) = input.get(index) {
        if byte == b'%'
            && let (Some(high), Some(low)) = (input.get(index + 1), input.get(index + 2))
            && let (Some(high), Some(low)) = (hex_value(*high), hex_value(*low))
        {
            bytes.push(high << 4 | low);
            index += 3;
        } else {
            bytes.push(byte);
            index += 1;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
