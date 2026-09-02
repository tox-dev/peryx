//! Durable, generation-based synchronization of a remote Simple root project catalog.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io::{Read, Seek as _, Write};
use std::rc::Rc;

use futures_util::TryStreamExt as _;
use html5ever::TokenizerResult;
use html5ever::tendril::StrTendril;
use html5ever::tendril::stream::{TendrilSink, Utf8LossyDecoder};
use html5ever::tokenizer::{BufferQueue, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer};
use peryx_index::serving::Inflight;
use peryx_storage::meta::{MetaError, MetaStore};
use peryx_upstream::UpstreamError;
use serde::Deserialize;
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use time::OffsetDateTime;
use url::Url;

use crate::html::project_from_url;
use crate::simple::Meta;
use crate::store::{
    CatalogGeneration, abort_catalog_generation, begin_catalog_generation, catalog_state, publish_catalog_generation,
    put_catalog_projects, recover_catalog_generations, refresh_catalog_generation,
};
use crate::{CachedValidators, SimpleClientExt, SimpleError, SimpleHead, is_valid_name, normalize_name};

/// Root responses are currently about 44 MiB at Warehouse. The cap leaves roughly sixfold growth
/// while preventing an upstream or decompressor from filling local storage.
pub const MAX_CATALOG_BYTES: u64 = 256 * 1024 * 1024;
/// Warehouse currently lists roughly 700,000 names. This bound leaves room for almost threefold growth.
pub const MAX_CATALOG_PROJECTS: u64 = 2_000_000;
const CATALOG_BATCH: usize = 10_000;

/// The result of a root-catalog synchronization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogSyncOutcome {
    Published { projects: u64 },
    NotModified { projects: u64 },
}

/// A remote root catalog could not be fetched, parsed, or published.
#[derive(Debug, thiserror::Error)]
pub enum CatalogSyncError {
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Simple(#[from] SimpleError),
    #[error("upstream project list returned {0}")]
    Status(u16),
    #[error("upstream project list exceeds the {MAX_CATALOG_BYTES}-byte limit")]
    TooLarge,
    #[error("upstream project list exceeds the {MAX_CATALOG_PROJECTS}-entry limit")]
    TooManyProjects,
    #[error("upstream project list contains invalid project name {0:?}")]
    InvalidName(String),
    #[error("upstream HTML root contains an anchor without a project name")]
    MissingHtmlProjectName,
}

/// Fetch and atomically publish the project-name catalog for `index`.
///
/// Network transfer completes into a bounded temporary file before any staging rows are written.
/// Parsing then commits fixed-size metadata batches, and only a complete valid document swaps the
/// active-generation pointer.
///
/// # Errors
/// Returns an error without changing the active generation when transfer, parsing, or publication fails.
pub async fn sync_catalog<C: SimpleClientExt + Sync>(
    client: &C,
    inflight: &Inflight,
    meta: &MetaStore,
    index: &str,
    fallback_source: &str,
) -> Result<CatalogSyncOutcome, CatalogSyncError> {
    let (_guard, waited) = crate::sync_lock::acquire(inflight, &format!("pypi\0catalog\0{index}")).await;
    if waited && let Some(active) = catalog_state(meta, index)?.active {
        return Ok(CatalogSyncOutcome::NotModified {
            projects: active.projects,
        });
    }
    recover_catalog_generations(meta, index)?;
    let previous = catalog_state(meta, index)?.active;
    let head = client
        .head_index(CachedValidators {
            source: previous.as_ref().map(|active| active.source.as_str()),
            etag: previous.as_ref().and_then(|active| active.etag.as_deref()),
            last_modified: previous.as_ref().and_then(|active| active.last_modified.as_deref()),
        })
        .await?;
    let fetched_at_unix = OffsetDateTime::now_utc().unix_timestamp();
    if head.status == 304 {
        let previous = previous
            .filter(|active| head.source.as_deref().is_none_or(|answered| answered == active.source))
            .ok_or_else(|| {
                MetaError::DriverPrecondition("upstream returned 304 without a matching catalog".to_owned())
            })?;
        let generation = previous.generation;
        refresh_catalog_generation(meta, index, generation, head.etag, head.last_modified, fetched_at_unix)?;
        return Ok(CatalogSyncOutcome::NotModified {
            projects: previous.projects,
        });
    }
    publish_response(meta, index, fallback_source, head, fetched_at_unix).await
}

/// Fetch and parse the remote project catalog, keeping the names in memory.
///
/// `peryx mirror plan` previews a run, so it needs the upstream project names while leaving the store
/// as it found it: this transfers and parses the document [`sync_catalog`] publishes, but stages no
/// generation, writes no rows, and records no catalog metric. It holds at most 2,000,000 names, the
/// ceiling a publishing parse enforces on the store.
///
/// The request carries no validators: a `304` would answer with an active generation this caller has
/// no business consulting or refreshing.
///
/// # Errors
/// Returns an error when the transfer, the response status, or the parse fails.
pub async fn read_catalog_projects<C: SimpleClientExt + Sync>(client: &C) -> Result<Vec<String>, CatalogSyncError> {
    let head = client.head_index(CachedValidators::default()).await?;
    check_transferable(&head)?;
    let base = head.url.clone();
    let format = catalog_format(head.content_type.as_deref());
    let (mut file, _) = transfer_catalog(head).await?;
    let mut sink = MemorySink::default();
    parse_catalog(&mut file, format, &base, &mut sink)?;
    Ok(sink.projects.into_iter().collect())
}

async fn publish_response(
    meta: &MetaStore,
    index: &str,
    fallback_source: &str,
    head: SimpleHead,
    fetched_at_unix: i64,
) -> Result<CatalogSyncOutcome, CatalogSyncError> {
    check_transferable(&head)?;
    let source = head.source.clone().unwrap_or_else(|| redact_url(fallback_source));
    let base = head.url.clone();
    let final_url = redact_url(head.url.as_str());
    let format = catalog_format(head.content_type.as_deref());
    let etag = head.etag.clone();
    let last_modified = head.last_modified.clone();
    let last_serial = head.last_serial;
    let (mut file, bytes) = transfer_catalog(head).await?;

    let (generation, expected_active) = begin_catalog_generation(meta, index)?;
    let mut sink = GenerationSink::new(meta, index, generation);
    let result = parse_catalog(&mut file, format, &base, &mut sink);
    let projects = match result {
        Ok(projects) => projects,
        Err(err) => {
            abort_catalog_generation(meta, index, generation)?;
            return Err(err);
        }
    };
    let catalog = CatalogGeneration {
        generation,
        source,
        url: final_url,
        format: format.to_owned(),
        etag,
        last_modified,
        last_serial,
        fetched_at_unix,
        bytes,
        projects,
    };
    publish_catalog_generation(meta, index, expected_active, catalog)?;
    recover_catalog_generations(meta, index)?;
    Ok(CatalogSyncOutcome::Published { projects })
}

fn check_transferable(head: &SimpleHead) -> Result<(), CatalogSyncError> {
    match head.status {
        200 if head.content_length.is_some_and(|bytes| bytes > MAX_CATALOG_BYTES) => Err(CatalogSyncError::TooLarge),
        200 => Ok(()),
        status => Err(CatalogSyncError::Status(status)),
    }
}

fn catalog_format(content_type: Option<&str>) -> &'static str {
    let content_type = content_type.unwrap_or_default();
    if content_type
        .split_once(';')
        .map_or(content_type, |(media_type, _)| media_type)
        .trim()
        .eq_ignore_ascii_case("application/vnd.pypi.simple.v1+json")
    {
        "json"
    } else {
        "html"
    }
}

/// Drain the response body into a bounded temporary file, rewound and ready to parse, so a parse
/// never runs against a transfer still in flight.
async fn transfer_catalog(head: SimpleHead) -> Result<(std::fs::File, u64), CatalogSyncError> {
    let mut file = tempfile::tempfile()?;
    let bytes = write_catalog_stream(head.into_stream(), &mut file, MAX_CATALOG_BYTES).await?;
    file.flush()?;
    file.rewind()?;
    Ok((file, bytes))
}

async fn write_catalog_stream<S>(mut stream: S, writer: &mut impl Write, limit: u64) -> Result<u64, CatalogSyncError>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, UpstreamError>> + Unpin,
{
    let mut bytes = 0_u64;
    while let Some(chunk) = stream.try_next().await? {
        write_catalog_chunk(writer, &chunk, &mut bytes, limit)?;
    }
    Ok(bytes)
}

fn write_catalog_chunk(
    writer: &mut impl Write,
    chunk: &[u8],
    bytes: &mut u64,
    limit: u64,
) -> Result<(), CatalogSyncError> {
    *bytes = bytes
        .checked_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX))
        .filter(|bytes| *bytes <= limit)
        .ok_or(CatalogSyncError::TooLarge)?;
    writer.write_all(chunk)?;
    Ok(())
}

pub(crate) fn redact_url(raw: &str) -> String {
    let Ok(mut url) = Url::parse(raw) else {
        return "<invalid-url>".to_owned();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.into()
}

fn parse_catalog(
    reader: &mut impl Read,
    format: &str,
    base: &Url,
    sink: &mut dyn CatalogSink,
) -> Result<u64, CatalogSyncError> {
    parse_catalog_with_limit(reader, format, base, sink, MAX_CATALOG_PROJECTS)
}

fn parse_catalog_with_limit(
    reader: &mut impl Read,
    format: &str,
    base: &Url,
    sink: &mut dyn CatalogSink,
    max_projects: u64,
) -> Result<u64, CatalogSyncError> {
    let mut batcher = CatalogBatcher::new(sink, max_projects);
    if format == "json" {
        let mut deserializer = serde_json::Deserializer::from_reader(reader);
        RootSeed { batcher: &mut batcher }.deserialize(&mut deserializer)?;
        deserializer.end()?;
    } else {
        parse_html(reader, base, &mut batcher)?;
    }
    batcher.finish()
}

/// Where a parsed catalog's names go. The caller's intent picks the sink: publishing a generation
/// writes rows, previewing one keeps the names in memory. Each call reports how many names it had
/// not already seen, which is what a published generation counts.
trait CatalogSink {
    fn accept(&mut self, batch: &[(String, String)]) -> Result<u64, CatalogSyncError>;
}

struct GenerationSink<'a> {
    meta: &'a MetaStore,
    index: &'a str,
    generation: u64,
}

impl<'a> GenerationSink<'a> {
    const fn new(meta: &'a MetaStore, index: &'a str, generation: u64) -> Self {
        Self {
            meta,
            index,
            generation,
        }
    }
}

impl CatalogSink for GenerationSink<'_> {
    fn accept(&mut self, batch: &[(String, String)]) -> Result<u64, CatalogSyncError> {
        Ok(put_catalog_projects(self.meta, self.index, self.generation, batch)?)
    }
}

#[derive(Default)]
struct MemorySink {
    projects: BTreeSet<String>,
}

impl CatalogSink for MemorySink {
    fn accept(&mut self, batch: &[(String, String)]) -> Result<u64, CatalogSyncError> {
        let mut added = 0;
        for (normalized, _) in batch {
            added += u64::from(self.projects.insert(normalized.clone()));
        }
        Ok(added)
    }
}

struct CatalogBatcher<'a> {
    sink: &'a mut dyn CatalogSink,
    batch: Vec<(String, String)>,
    entries: u64,
    projects: u64,
    max_projects: u64,
}

impl<'a> CatalogBatcher<'a> {
    fn new(sink: &'a mut dyn CatalogSink, max_projects: u64) -> Self {
        Self {
            sink,
            batch: Vec::with_capacity(CATALOG_BATCH),
            entries: 0,
            projects: 0,
            max_projects,
        }
    }

    fn add(&mut self, display: String) -> Result<(), CatalogSyncError> {
        if !is_valid_name(&display) {
            return Err(CatalogSyncError::InvalidName(display));
        }
        self.entries += 1;
        if self.entries > self.max_projects {
            return Err(CatalogSyncError::TooManyProjects);
        }
        self.batch.push((normalize_name(&display), display));
        if self.batch.len() == CATALOG_BATCH {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), CatalogSyncError> {
        self.projects += self.sink.accept(&self.batch)?;
        self.batch.clear();
        Ok(())
    }

    fn finish(mut self) -> Result<u64, CatalogSyncError> {
        self.flush()?;
        Ok(self.projects)
    }
}

#[derive(Deserialize)]
struct JsonProject {
    name: String,
}

#[derive(Default, Deserialize)]
struct JsonMeta {
    #[serde(rename = "api-version")]
    api_version: Option<String>,
}

struct RootSeed<'a, 'store> {
    batcher: &'a mut CatalogBatcher<'store>,
}

impl<'de> DeserializeSeed<'de> for RootSeed<'_, '_> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(RootVisitor { batcher: self.batcher })
    }
}

struct RootVisitor<'a, 'store> {
    batcher: &'a mut CatalogBatcher<'store>,
}

impl<'de> Visitor<'de> for RootVisitor<'_, '_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a PEP 691 root object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut meta = None;
        let mut saw_projects = false;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "meta" => meta = Some(map.next_value::<JsonMeta>()?),
                "projects" => {
                    map.next_value_seed(ProjectsSeed { batcher: self.batcher })?;
                    saw_projects = true;
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        if !saw_projects {
            return Err(serde::de::Error::custom(
                "PEP 691 root response omits the required \"projects\" array",
            ));
        }
        Meta::from_upstream(meta.and_then(|meta| meta.api_version).as_deref(), None, None)
            .map_err(serde::de::Error::custom)?;
        Ok(())
    }
}

struct ProjectsSeed<'a, 'store> {
    batcher: &'a mut CatalogBatcher<'store>,
}

impl<'de> DeserializeSeed<'de> for ProjectsSeed<'_, '_> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(ProjectsVisitor { batcher: self.batcher })
    }
}

struct ProjectsVisitor<'a, 'store> {
    batcher: &'a mut CatalogBatcher<'store>,
}

impl<'de> Visitor<'de> for ProjectsVisitor<'_, '_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a project array")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
        while let Some(project) = sequence.next_element::<JsonProject>()? {
            self.batcher.add(project.name).map_err(serde::de::Error::custom)?;
        }
        Ok(())
    }
}

fn parse_html(reader: &mut impl Read, base: &Url, batcher: &mut CatalogBatcher<'_>) -> Result<(), CatalogSyncError> {
    let state = Rc::new(RefCell::new(HtmlState::new(base, batcher)));
    let tokenizer = Tokenizer::new(
        HtmlSink {
            state: Rc::clone(&state),
        },
        html5ever::tokenizer::TokenizerOpts::default(),
    );
    Utf8LossyDecoder::new(HtmlTokenizer { tokenizer }).read_from(reader)?;
    Rc::into_inner(state)
        .expect("HTML tokenizer released its state")
        .into_inner()
        .finish()
}

struct HtmlTokenizer<S: TokenSink> {
    tokenizer: Tokenizer<S>,
}

impl<S: TokenSink> TendrilSink<html5ever::tendril::fmt::UTF8> for HtmlTokenizer<S> {
    type Output = ();

    fn process(&mut self, tendril: StrTendril) {
        let input = BufferQueue::default();
        input.push_back(tendril);
        while !matches!(self.tokenizer.feed(&input), TokenizerResult::Done) {}
    }

    fn error(&mut self, _description: std::borrow::Cow<'static, str>) {}

    fn finish(self) {
        self.tokenizer.end();
    }
}

struct HtmlSink<'a, 'store> {
    state: Rc<RefCell<HtmlState<'a, 'store>>>,
}

impl TokenSink for HtmlSink<'_, '_> {
    type Handle = ();

    fn process_token(&self, token: Token, _line_number: u64) -> TokenSinkResult<Self::Handle> {
        self.state.borrow_mut().token(token);
        TokenSinkResult::Continue
    }
}

struct HtmlAnchor {
    text: String,
    href: Option<String>,
}

struct HtmlState<'a, 'store> {
    base: &'a Url,
    batcher: &'a mut CatalogBatcher<'store>,
    anchor: Option<HtmlAnchor>,
    api_version: Option<String>,
    error: Option<CatalogSyncError>,
}

impl<'a, 'store> HtmlState<'a, 'store> {
    const fn new(base: &'a Url, batcher: &'a mut CatalogBatcher<'store>) -> Self {
        Self {
            base,
            batcher,
            anchor: None,
            api_version: None,
            error: None,
        }
    }

    fn token(&mut self, token: Token) {
        if self.error.is_some() {
            return;
        }
        match token {
            Token::TagToken(tag) if tag.kind == TagKind::StartTag && tag.name.as_ref() == "a" => {
                self.anchor = Some(HtmlAnchor {
                    text: String::new(),
                    href: attr(&tag.attrs, "href"),
                });
            }
            Token::CharacterTokens(text) => {
                if let Some(anchor) = self.anchor.as_mut() {
                    anchor.text.push_str(&text);
                }
            }
            Token::TagToken(tag) if tag.kind == TagKind::EndTag && tag.name.as_ref() == "a" => {
                let Some(anchor) = self.anchor.take() else {
                    return;
                };
                let display = if anchor.text.trim().is_empty() {
                    anchor
                        .href
                        .and_then(|href| self.base.join(&href).ok())
                        .as_ref()
                        .and_then(project_from_url)
                        .ok_or(CatalogSyncError::MissingHtmlProjectName)
                } else {
                    Ok(anchor.text.trim().to_owned())
                };
                self.error = display.and_then(|display| self.batcher.add(display)).err();
            }
            Token::TagToken(tag)
                if tag.kind == TagKind::StartTag
                    && tag.name.as_ref() == "meta"
                    && attr(&tag.attrs, "name").as_deref() == Some("pypi:repository-version") =>
            {
                self.api_version = attr(&tag.attrs, "content");
            }
            _ => {}
        }
    }

    fn finish(self) -> Result<(), CatalogSyncError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        Meta::from_upstream(self.api_version.as_deref(), None, None)?;
        Ok(())
    }
}

fn attr(attributes: &[html5ever::Attribute], name: &str) -> Option<String> {
    attributes
        .iter()
        .find(|attribute| attribute.name.local.as_ref() == name)
        .map(|attribute| attribute.value.to_string())
}

#[cfg(test)]
#[path = "../tests/unit/catalog/tests.rs"]
mod tests;
