use serde::{Deserialize, Serialize};

use crate::error::SearchError;

const DEFAULT_PAGE_SIZE: usize = 25;
const PAGE_SIZES: [usize; 3] = [25, 50, 100];

/// Maximum `offset + page_size`; Tantivy's offset collection cost grows with this sum.
const MAX_RESULT_WINDOW: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchParams {
    pub query: String,
    pub route: Option<String>,
    pub source: SourceFilter,
    pub availability: AvailabilityFilter,
    pub page: usize,
    pub page_size: usize,
}

impl Default for SearchParams {
    fn default() -> Self {
        Self {
            query: String::new(),
            route: None,
            source: SourceFilter::All,
            availability: AvailabilityFilter::All,
            page: 1,
            page_size: DEFAULT_PAGE_SIZE,
        }
    }
}

impl SearchParams {
    /// # Errors
    /// Returns an error for an unknown `type` or `availability` filter.
    pub fn from_query(query: Option<&str>) -> Result<Self, SearchError> {
        let mut params = Self::default();
        let Some(query) = query else {
            return Ok(params);
        };
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            match key.as_ref() {
                "q" => params.query = value.into_owned(),
                "route" if !value.is_empty() => params.route = Some(value.into_owned()),
                "type" if value.is_empty() || value == "all" => params.source = SourceFilter::All,
                "type" => {
                    params.source = SourceFilter::from_value(&value)
                        .ok_or_else(|| SearchError::InvalidSource(value.into_owned()))?;
                }
                "availability" if value.is_empty() || value == "all" => {
                    params.availability = AvailabilityFilter::All;
                }
                "availability" => {
                    params.availability = AvailabilityFilter::from_value(&value)
                        .ok_or_else(|| SearchError::InvalidAvailability(value.into_owned()))?;
                }
                "page" => params.page = value.parse::<usize>().unwrap_or(1).max(1),
                "page_size" => {
                    let page_size = value.parse::<usize>().unwrap_or(DEFAULT_PAGE_SIZE);
                    params.page_size = if PAGE_SIZES.contains(&page_size) {
                        page_size
                    } else {
                        DEFAULT_PAGE_SIZE
                    };
                }
                _ => {}
            }
        }
        params.offset()?;
        Ok(params)
    }

    /// # Errors
    /// Returns an error when page arithmetic overflows or the result window exceeds the fixed limit.
    pub fn offset(&self) -> Result<usize, SearchError> {
        self.page
            .checked_sub(1)
            .and_then(|page| page.checked_mul(self.page_size))
            .and_then(|offset| {
                offset
                    .checked_add(self.page_size)
                    .filter(|end| *end <= MAX_RESULT_WINDOW)
                    .map(|_| offset)
            })
            .ok_or(SearchError::ResultWindowTooLarge {
                page: self.page,
                page_size: self.page_size,
                max: MAX_RESULT_WINDOW,
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceFilter {
    All,
    Uploaded,
    Cached,
    Override,
}

impl SourceFilter {
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "all" => Some(Self::All),
            "uploaded" => Some(Self::Uploaded),
            "cached" => Some(Self::Cached),
            "override" => Some(Self::Override),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Uploaded => "uploaded",
            Self::Cached => "cached",
            Self::Override => "override",
        }
    }

    pub(super) const fn content_source(self) -> Option<ContentSource> {
        match self {
            Self::All => None,
            Self::Uploaded => Some(ContentSource::Uploaded),
            Self::Cached => Some(ContentSource::Cached),
            Self::Override => Some(ContentSource::Override),
        }
    }
}

/// Uses an indexed availability flag to avoid storage probes per result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AvailabilityFilter {
    All,
    Local,
}

impl AvailabilityFilter {
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "all" => Some(Self::All),
            "local" => Some(Self::Local),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Local => "local",
        }
    }

    pub(super) const fn local_only(self) -> bool {
        matches!(self, Self::Local)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContentSource {
    Uploaded,
    Cached,
    Override,
}

impl ContentSource {
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "uploaded" => Some(Self::Uploaded),
            "cached" => Some(Self::Cached),
            "override" => Some(Self::Override),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Uploaded => "uploaded",
            Self::Cached => "cached",
            Self::Override => "override",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Uploaded => "Uploaded",
            Self::Cached => "Cached",
            Self::Override => "Override",
        }
    }
}
