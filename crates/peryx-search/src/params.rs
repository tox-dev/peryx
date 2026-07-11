//! `/+search` query parameters and the source-type filter.

use serde::{Deserialize, Serialize};

use crate::error::SearchError;

const DEFAULT_PAGE_SIZE: usize = 25;
const PAGE_SIZES: [usize; 3] = [25, 50, 100];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchParams {
    pub query: String,
    pub route: Option<String>,
    pub source: SourceFilter,
    pub page: usize,
    pub page_size: usize,
}

impl Default for SearchParams {
    fn default() -> Self {
        Self {
            query: String::new(),
            route: None,
            source: SourceFilter::All,
            page: 1,
            page_size: DEFAULT_PAGE_SIZE,
        }
    }
}

impl SearchParams {
    /// Parse `/+search` query parameters.
    ///
    /// # Errors
    /// Returns an error for an unknown `type` filter.
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
        Ok(params)
    }

    #[must_use]
    pub const fn offset(&self) -> usize {
        self.page.saturating_sub(1).saturating_mul(self.page_size)
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

    pub(super) const fn package_source(self) -> Option<PackageSource> {
        match self {
            Self::All => None,
            Self::Uploaded => Some(PackageSource::Uploaded),
            Self::Cached => Some(PackageSource::Cached),
            Self::Override => Some(PackageSource::Override),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PackageSource {
    Uploaded,
    Cached,
    Override,
}

impl PackageSource {
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
