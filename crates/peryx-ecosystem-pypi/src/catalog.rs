//! Durable, generation-based synchronization of a remote Simple root project catalog.

use std::cell::RefCell;
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
use crate::{SimpleClientExt, SimpleError, SimpleHead, is_valid_name, normalize_name};

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
        .head_index(
            previous.as_ref().map(|active| active.source.as_str()),
            previous.as_ref().and_then(|active| active.etag.as_deref()),
            previous.as_ref().and_then(|active| active.last_modified.as_deref()),
        )
        .await?;
    let fetched_at_unix = OffsetDateTime::now_utc().unix_timestamp();
    if head.status == 304 {
        let previous = previous.ok_or(MetaError::DriverPrecondition(
            "upstream returned 304 without an active catalog".to_owned(),
        ))?;
        let generation = previous.generation;
        refresh_catalog_generation(meta, index, generation, head.etag, head.last_modified, fetched_at_unix)?;
        return Ok(CatalogSyncOutcome::NotModified {
            projects: previous.projects,
        });
    }
    publish_response(meta, index, fallback_source, head, fetched_at_unix).await
}

async fn publish_response(
    meta: &MetaStore,
    index: &str,
    fallback_source: &str,
    head: SimpleHead,
    fetched_at_unix: i64,
) -> Result<CatalogSyncOutcome, CatalogSyncError> {
    match head.status {
        200 if head.content_length.is_some_and(|bytes| bytes > MAX_CATALOG_BYTES) => {
            return Err(CatalogSyncError::TooLarge);
        }
        200 => {}
        status => return Err(CatalogSyncError::Status(status)),
    }
    let source = head.source.clone().unwrap_or_else(|| redact_url(fallback_source));
    let base = head.url.clone();
    let final_url = redact_url(head.url.as_str());
    let content_type = head.content_type.clone().unwrap_or_default();
    let format = if content_type
        .split_once(';')
        .map_or(content_type.as_str(), |(media_type, _)| media_type)
        .trim()
        .eq_ignore_ascii_case("application/vnd.pypi.simple.v1+json")
    {
        "json"
    } else {
        "html"
    };
    let etag = head.etag.clone();
    let last_modified = head.last_modified.clone();
    let last_serial = head.last_serial;
    let mut file = tempfile::NamedTempFile::new()?;
    let bytes = write_catalog_stream(head.into_stream(), file.as_file_mut(), MAX_CATALOG_BYTES).await?;
    file.flush()?;
    file.rewind()?;

    let (generation, expected_active) = begin_catalog_generation(meta, index)?;
    let result = parse_catalog(file.as_file_mut(), format, &base, meta, index, generation);
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
    meta: &MetaStore,
    index: &str,
    generation: u64,
) -> Result<u64, CatalogSyncError> {
    parse_catalog_with_limit(reader, format, base, meta, index, generation, MAX_CATALOG_PROJECTS)
}

fn parse_catalog_with_limit(
    reader: &mut impl Read,
    format: &str,
    base: &Url,
    meta: &MetaStore,
    index: &str,
    generation: u64,
    max_projects: u64,
) -> Result<u64, CatalogSyncError> {
    let mut batcher = CatalogBatcher::new(meta, index, generation, max_projects);
    if format == "json" {
        let mut deserializer = serde_json::Deserializer::from_reader(reader);
        RootSeed { batcher: &mut batcher }.deserialize(&mut deserializer)?;
        deserializer.end()?;
    } else {
        parse_html(reader, base, &mut batcher)?;
    }
    batcher.finish()
}

struct CatalogBatcher<'a> {
    meta: &'a MetaStore,
    index: &'a str,
    generation: u64,
    batch: Vec<(String, String)>,
    entries: u64,
    projects: u64,
    max_projects: u64,
}

impl<'a> CatalogBatcher<'a> {
    fn new(meta: &'a MetaStore, index: &'a str, generation: u64, max_projects: u64) -> Self {
        Self {
            meta,
            index,
            generation,
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
        self.projects += put_catalog_projects(self.meta, self.index, self.generation, &self.batch)?;
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
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "meta" => meta = Some(map.next_value::<JsonMeta>()?),
                "projects" => map.next_value_seed(ProjectsSeed { batcher: self.batcher })?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
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
mod tests {
    use std::cell::RefCell;
    use std::io::{Cursor, Write as _};
    use std::rc::Rc;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use peryx_index::serving::Inflight;
    use peryx_upstream::UpstreamClient;
    use url::Url;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        CatalogBatcher, CatalogSyncError, CatalogSyncOutcome, HtmlSink, HtmlState, HtmlTokenizer, MAX_CATALOG_BYTES,
        MAX_CATALOG_PROJECTS, parse_catalog_with_limit, publish_response, redact_url, sync_catalog,
        write_catalog_chunk, write_catalog_stream,
    };
    use crate::SimpleClientExt as _;
    use crate::store::{
        CatalogGeneration, abort_catalog_generation, begin_catalog_generation, catalog_state, list_projects,
        publish_catalog_generation,
    };
    use peryx_storage::meta::MetaStore;

    fn store() -> (tempfile::TempDir, MetaStore) {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        (dir, meta)
    }

    fn active(generation: u64) -> CatalogGeneration {
        CatalogGeneration {
            generation,
            source: "test".to_owned(),
            url: "https://example.invalid/simple/".to_owned(),
            format: "json".to_owned(),
            etag: Some("old".to_owned()),
            last_modified: Some("yesterday".to_owned()),
            last_serial: None,
            fetched_at_unix: 1,
            bytes: 1,
            projects: 1,
        }
    }

    fn seed_active(meta: &MetaStore, index: &str) -> u64 {
        let (generation, expected) = begin_catalog_generation(meta, index).unwrap();
        publish_catalog_generation(meta, index, expected, active(generation)).unwrap();
        generation
    }

    #[tokio::test]
    async fn test_sync_catalog_rejects_304_without_active_generation() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/simple/"))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
        let (_dir, meta) = store();

        let error = sync_catalog(&client, &Inflight::default(), &meta, "no-active-304", client.base_url())
            .await
            .unwrap_err();

        assert!(matches!(error, CatalogSyncError::Store(_)));
        assert!(catalog_state(&meta, "no-active-304").unwrap().active.is_none());
        server.verify().await;
        drop(client);
        drop(server);
    }

    #[tokio::test]
    async fn test_sync_catalog_coalesces_concurrent_fetches() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/simple/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/vnd.pypi.simple.v1+json")
                    .set_body_raw(
                        r#"{"meta":{"api-version":"1.4"},"projects":[{"name":"Flask"}]}"#,
                        "application/vnd.pypi.simple.v1+json",
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
        let (_dir, meta) = store();
        let inflight = Inflight::default();

        let (first, second) = tokio::join!(
            sync_catalog(&client, &inflight, &meta, "concurrent", client.base_url()),
            sync_catalog(&client, &inflight, &meta, "concurrent", client.base_url())
        );

        assert!(matches!(first.unwrap(), CatalogSyncOutcome::Published { projects: 1 }));
        assert!(matches!(
            second.unwrap(),
            CatalogSyncOutcome::NotModified { projects: 1 }
        ));
    }

    #[tokio::test]
    async fn test_sync_catalog_coalesces_concurrent_revalidations() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/simple/"))
            .and(header("if-none-match", "old"))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&server)
            .await;
        let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
        let (_dir, meta) = store();
        seed_active(&meta, "concurrent-revalidation");
        let inflight = Inflight::default();

        let (first, second) = tokio::join!(
            sync_catalog(&client, &inflight, &meta, "concurrent-revalidation", client.base_url()),
            sync_catalog(&client, &inflight, &meta, "concurrent-revalidation", client.base_url())
        );

        assert!(matches!(
            first.unwrap(),
            CatalogSyncOutcome::NotModified { projects: 1 }
        ));
        assert!(matches!(
            second.unwrap(),
            CatalogSyncOutcome::NotModified { projects: 1 }
        ));
    }

    #[tokio::test]
    async fn test_sync_catalog_304_sends_etag_and_merges_returned_validator() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/simple/"))
            .and(header("if-none-match", "old"))
            .respond_with(ResponseTemplate::new(304).insert_header("etag", "new"))
            .expect(1)
            .mount(&server)
            .await;
        let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
        let (_dir, meta) = store();
        let generation = seed_active(&meta, "validated");

        assert!(matches!(
            sync_catalog(&client, &Inflight::default(), &meta, "validated", client.base_url())
                .await
                .unwrap(),
            CatalogSyncOutcome::NotModified { projects: 1 }
        ));

        let catalog = catalog_state(&meta, "validated").unwrap().active.unwrap();
        assert_eq!(catalog.generation, generation);
        assert_eq!(catalog.etag.as_deref(), Some("new"));
        assert_eq!(catalog.last_modified.as_deref(), Some("yesterday"));
    }

    #[tokio::test]
    async fn test_sync_catalog_rejects_declared_oversized_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/simple/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(Vec::new(), "application/vnd.pypi.simple.v1+json"))
            .mount(&server)
            .await;
        let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
        let (_dir, meta) = store();
        let mut response = client.head_index(None, None, None).await.unwrap();
        response.content_length = Some(MAX_CATALOG_BYTES + 1);

        let error = publish_response(&meta, "oversized", client.base_url(), response, 1)
            .await
            .unwrap_err();

        assert!(matches!(error, CatalogSyncError::TooLarge));
        assert!(catalog_state(&meta, "oversized").unwrap().active.is_none());
    }

    #[tokio::test]
    async fn test_sync_catalog_aborts_invalid_staging_generation() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/simple/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"{"meta":{"api-version":"1.4"},"projects":[{"name":"bad name"}]}"#,
                "application/vnd.pypi.simple.v1+json",
            ))
            .mount(&server)
            .await;
        let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
        let (_dir, meta) = store();
        let active = seed_active(&meta, "invalid");

        let error = sync_catalog(&client, &Inflight::default(), &meta, "invalid", client.base_url())
            .await
            .unwrap_err();

        assert!(matches!(error, CatalogSyncError::Json(error) if error.to_string().contains("bad name")));
        let state = catalog_state(&meta, "invalid").unwrap();
        assert_eq!(state.active.unwrap().generation, active);
        assert!(state.staging.is_none());
    }

    #[test]
    fn test_write_catalog_stream_caps_unknown_length() {
        let mut output = Vec::new();
        let mut bytes = 0;

        write_catalog_chunk(&mut output, b"1234", &mut bytes, 7).unwrap();
        let error = write_catalog_chunk(&mut output, b"5678", &mut bytes, 7).unwrap_err();

        assert!(matches!(error, CatalogSyncError::TooLarge));
        assert_eq!(output, b"1234");
    }

    #[tokio::test]
    async fn test_sync_catalog_caps_decompressed_body() {
        let server = MockServer::start().await;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&vec![b'a'; 1024 * 1024]).unwrap();
        let compressed = encoder.finish().unwrap();
        assert!(compressed.len() < 100_000);
        Mock::given(method("GET"))
            .and(path("/simple/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/vnd.pypi.simple.v1+json")
                    .insert_header("content-encoding", "gzip")
                    .set_body_bytes(compressed),
            )
            .mount(&server)
            .await;
        let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
        let response = client.head_index(None, None, None).await.unwrap();
        let mut output = Vec::new();

        let error = write_catalog_stream(response.into_stream(), &mut output, 100_000)
            .await
            .unwrap_err();

        assert!(matches!(error, CatalogSyncError::TooLarge));
    }

    #[test]
    fn test_parse_failures_never_replace_active_generation() {
        for (document, limit) in [
            (
                r#"{"meta":{"api-version":"1.4"},"projects":[{"name":"Flask"}"#,
                MAX_CATALOG_PROJECTS,
            ),
            (
                r#"{"meta":{"api-version":"1.4"},"projects":[{"name":"one"},{"name":"two"}]}"#,
                1,
            ),
        ] {
            let (_dir, meta) = store();
            let active = seed_active(&meta, "failure");
            let (staging, _) = begin_catalog_generation(&meta, "failure").unwrap();
            let error = parse_catalog_with_limit(
                &mut Cursor::new(document),
                "json",
                &Url::parse("https://example.invalid/simple/").unwrap(),
                &meta,
                "failure",
                staging,
                limit,
            )
            .unwrap_err();
            abort_catalog_generation(&meta, "failure", staging).unwrap();

            assert!(matches!(
                error,
                CatalogSyncError::Json(_) | CatalogSyncError::TooManyProjects
            ));
            assert_eq!(
                catalog_state(&meta, "failure").unwrap().active.unwrap().generation,
                active
            );
            assert!(list_projects(&meta, "failure").unwrap().is_empty());
        }
    }

    #[test]
    fn test_json_parser_validates_shapes_and_ignores_extensions() {
        for document in [r"[]", r#"{"meta":{"api-version":"1.4"},"projects":{}}"#] {
            let (_dir, meta) = store();
            let (generation, _) = begin_catalog_generation(&meta, "shape").unwrap();

            let error = parse_catalog_with_limit(
                &mut Cursor::new(document),
                "json",
                &Url::parse("https://example.invalid/simple/").unwrap(),
                &meta,
                "shape",
                generation,
                MAX_CATALOG_PROJECTS,
            )
            .unwrap_err();

            assert!(matches!(error, CatalogSyncError::Json(_)));
        }

        let (_dir, meta) = store();
        let (generation, _) = begin_catalog_generation(&meta, "extension").unwrap();
        let document = r#"{"extension":{"ignored":true},"meta":{"api-version":"1.4"},"projects":[{"name":"Flask"}]}"#;
        assert_eq!(
            parse_catalog_with_limit(
                &mut Cursor::new(document),
                "json",
                &Url::parse("https://example.invalid/simple/").unwrap(),
                &meta,
                "extension",
                generation,
                MAX_CATALOG_PROJECTS,
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn test_catalog_batch_flushes_at_transaction_limit() {
        let (_dir, meta) = store();
        let (generation, _) = begin_catalog_generation(&meta, "batch").unwrap();
        let projects = (0..super::CATALOG_BATCH)
            .map(|index| format!(r#"{{"name":"project-{index}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let document = format!(r#"{{"meta":{{"api-version":"1.4"}},"projects":[{projects}]}}"#);

        assert_eq!(
            parse_catalog_with_limit(
                &mut Cursor::new(document),
                "json",
                &Url::parse("https://example.invalid/simple/").unwrap(),
                &meta,
                "batch",
                generation,
                MAX_CATALOG_PROJECTS,
            )
            .unwrap(),
            super::CATALOG_BATCH as u64
        );
    }

    #[test]
    fn test_streaming_html_and_json_publish_equivalent_names() {
        for (format, document) in [
            (
                "json",
                r#"{"meta":{"api-version":"1.4"},"projects":[{"name":"Flask"},{"name":"Django"}]}"#,
            ),
            (
                "html",
                r#"<!doctype html><meta name="pypi:repository-version" content="1.4"><a href="/simple/flask/">Flask</a><a href="/simple/django/">Django</a>"#,
            ),
        ] {
            let (_dir, meta) = store();
            let (generation, expected) = begin_catalog_generation(&meta, format).unwrap();
            let projects = parse_catalog_with_limit(
                &mut Cursor::new(document),
                format,
                &Url::parse("https://example.invalid/simple/").unwrap(),
                &meta,
                format,
                generation,
                MAX_CATALOG_PROJECTS,
            )
            .unwrap();
            let mut catalog = active(generation);
            catalog.projects = projects;
            publish_catalog_generation(&meta, format, expected, catalog).unwrap();
            assert_eq!(list_projects(&meta, format).unwrap(), vec!["Django", "Flask"]);
        }
    }

    #[test]
    fn test_html_parser_uses_links_and_rejects_nameless_anchors() {
        let (_dir, meta) = store();
        let base = Url::parse("https://example.invalid/simple/").unwrap();
        let (generation, _) = begin_catalog_generation(&meta, "href").unwrap();
        assert_eq!(
            parse_catalog_with_limit(
                &mut Cursor::new(r#"</a><a href="/simple/flask/"></a>"#),
                "html",
                &base,
                &meta,
                "href",
                generation,
                MAX_CATALOG_PROJECTS,
            )
            .unwrap(),
            1
        );

        let (generation, _) = begin_catalog_generation(&meta, "nameless").unwrap();
        let error = parse_catalog_with_limit(
            &mut Cursor::new(r"<a></a><a>ignored after error</a>"),
            "html",
            &base,
            &meta,
            "nameless",
            generation,
            MAX_CATALOG_PROJECTS,
        )
        .unwrap_err();
        assert!(matches!(error, CatalogSyncError::MissingHtmlProjectName));
    }

    #[test]
    fn test_html_tokenizer_accepts_decoder_errors() {
        let (_dir, meta) = store();
        let base = Url::parse("https://example.invalid/simple/").unwrap();
        let (generation, _) = begin_catalog_generation(&meta, "decoder").unwrap();
        let mut batcher = CatalogBatcher::new(&meta, "decoder", generation, MAX_CATALOG_PROJECTS);
        let state = Rc::new(RefCell::new(HtmlState::new(&base, &mut batcher)));
        let mut tokenizer = HtmlTokenizer {
            tokenizer: html5ever::tokenizer::Tokenizer::new(
                HtmlSink {
                    state: Rc::clone(&state),
                },
                html5ever::tokenizer::TokenizerOpts::default(),
            ),
        };

        html5ever::tendril::stream::TendrilSink::error(&mut tokenizer, "invalid input".into());
    }

    #[test]
    fn test_redact_url_removes_request_secrets() {
        assert_eq!(
            redact_url("https://user:password@example.invalid/simple/?token=secret#fragment"),
            "https://example.invalid/simple/"
        );
        assert_eq!(redact_url("not a URL with a secret"), "<invalid-url>");
    }
}
