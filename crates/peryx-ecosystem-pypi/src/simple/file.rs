use std::collections::BTreeMap;
use std::fmt;

use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use url::{Host, Url};

/// Whether a file is yanked (PEP 592): not yanked, yanked, or yanked with a reason.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Yanked {
    #[default]
    No,
    Yes,
    Reason(String),
}

impl Serialize for Yanked {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::No => serializer.serialize_bool(false),
            Self::Yes => serializer.serialize_bool(true),
            Self::Reason(reason) => serializer.serialize_str(reason),
        }
    }
}

impl<'de> Deserialize<'de> for Yanked {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct YankedVisitor;
        impl Visitor<'_> for YankedVisitor {
            type Value = Yanked;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a boolean or a reason string")
            }
            fn visit_bool<E>(self, value: bool) -> Result<Yanked, E> {
                Ok(if value { Yanked::Yes } else { Yanked::No })
            }
            fn visit_str<E>(self, value: &str) -> Result<Yanked, E> {
                Ok(Yanked::Reason(value.to_owned()))
            }
        }
        deserializer.deserialize_any(YankedVisitor)
    }
}

/// Availability of the PEP 658/714 core-metadata sibling for a file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CoreMetadata {
    #[default]
    Absent,
    Available,
    Hashes(BTreeMap<String, String>),
}

impl CoreMetadata {
    #[must_use]
    pub const fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }
}

impl Serialize for CoreMetadata {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Absent => serializer.serialize_bool(false),
            Self::Available => serializer.serialize_bool(true),
            Self::Hashes(hashes) => hashes.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for CoreMetadata {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct CoreMetadataVisitor;
        impl<'de> Visitor<'de> for CoreMetadataVisitor {
            type Value = CoreMetadata;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a boolean or a hashes object")
            }
            fn visit_bool<E>(self, value: bool) -> Result<CoreMetadata, E> {
                Ok(if value {
                    CoreMetadata::Available
                } else {
                    CoreMetadata::Absent
                })
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<CoreMetadata, A::Error> {
                let mut hashes = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, String>()? {
                    hashes.insert(key, value);
                }
                Ok(CoreMetadata::Hashes(hashes))
            }
        }
        deserializer.deserialize_any(CoreMetadataVisitor)
    }
}

/// A file provenance URL from PEP 740, or an explicit JSON `null`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Provenance {
    #[default]
    Absent,
    None,
    Url(String),
}

impl Provenance {
    /// The advertised URL when it is an absolute secure origin without embedded credentials.
    #[must_use]
    pub fn secure_url(&self) -> Option<&str> {
        let Self::Url(value) = self else {
            return None;
        };
        let url = Url::parse(value).ok()?;
        let secure = url.scheme() == "https"
            || (url.scheme() == "http"
                && url.host().is_some_and(|host| match host {
                    Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
                    Host::Ipv4(address) => address.is_loopback(),
                    Host::Ipv6(address) => address.is_loopback(),
                }));
        (secure && url.host().is_some() && url.username().is_empty() && url.password().is_none()).then_some(value)
    }

    /// Drop a provenance marker that cannot name the secure absolute URL required by the Simple API.
    pub fn retain_secure_url(&mut self) {
        if matches!(self, Self::Url(_)) && self.secure_url().is_none() {
            *self = Self::Absent;
        }
    }
}

impl<'de> Deserialize<'de> for Provenance {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Option::<String>::deserialize(deserializer)?.map_or(Self::None, Self::Url))
    }
}

/// One downloadable file in a project's detail page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub filename: String,
    pub url: String,
    pub hashes: BTreeMap<String, String>,
    pub requires_python: Option<String>,
    pub size: Option<u64>,
    pub upload_time: Option<String>,
    pub yanked: Yanked,
    pub core_metadata: CoreMetadata,
    pub dist_info_metadata: CoreMetadata,
    pub gpg_sig: Option<bool>,
    pub provenance: Provenance,
}

impl File {
    /// The content address a served file resolves through: its `sha256` hash, the digest peryx keys
    /// every cached artifact by. A file the Simple response left without one cannot be content-addressed.
    #[must_use]
    pub fn sha256(&self) -> Option<&str> {
        self.hashes.get("sha256").map(String::as_str)
    }

    #[must_use]
    pub const fn metadata(&self) -> &CoreMetadata {
        if self.core_metadata.is_absent() {
            &self.dist_info_metadata
        } else {
            &self.core_metadata
        }
    }

    /// Clear both metadata spellings after peryx cannot verify the sibling digest.
    pub fn clear_metadata(&mut self) {
        self.core_metadata = CoreMetadata::Absent;
        self.dist_info_metadata = CoreMetadata::Absent;
    }

    pub fn set_metadata(&mut self, metadata: CoreMetadata) {
        self.core_metadata = metadata.clone();
        self.dist_info_metadata = metadata;
    }
}

#[derive(Deserialize)]
struct IncomingFile {
    filename: String,
    url: String,
    #[serde(default)]
    hashes: BTreeMap<String, String>,
    #[serde(rename = "requires-python", default)]
    requires_python: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(rename = "upload-time", default)]
    upload_time: Option<String>,
    #[serde(default)]
    yanked: Yanked,
    #[serde(rename = "core-metadata", default)]
    core_metadata: CoreMetadata,
    #[serde(rename = "dist-info-metadata", default)]
    dist_info_metadata: CoreMetadata,
    #[serde(rename = "gpg-sig", default)]
    gpg_sig: Option<bool>,
    #[serde(default)]
    provenance: Provenance,
}

impl From<IncomingFile> for File {
    fn from(file: IncomingFile) -> Self {
        Self {
            filename: file.filename,
            url: file.url,
            hashes: file.hashes,
            requires_python: file.requires_python,
            size: file.size,
            upload_time: file.upload_time,
            yanked: file.yanked,
            core_metadata: file.core_metadata,
            dist_info_metadata: file.dist_info_metadata,
            gpg_sig: file.gpg_sig,
            provenance: file.provenance,
        }
    }
}

impl<'de> Deserialize<'de> for File {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        IncomingFile::deserialize(deserializer).map(Self::from)
    }
}

impl Serialize for File {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("filename", &self.filename)?;
        map.serialize_entry("url", &self.url)?;
        map.serialize_entry("hashes", &self.hashes)?;
        if let Some(requires_python) = &self.requires_python {
            map.serialize_entry("requires-python", requires_python)?;
        }
        if let Some(size) = self.size {
            map.serialize_entry("size", &size)?;
        }
        if let Some(upload_time) = &self.upload_time {
            map.serialize_entry("upload-time", upload_time)?;
        }
        map.serialize_entry("yanked", &self.yanked)?;
        let metadata = self.metadata();
        map.serialize_entry("core-metadata", metadata)?;
        if !metadata.is_absent() {
            map.serialize_entry("dist-info-metadata", metadata)?;
        }
        if let Some(gpg_sig) = self.gpg_sig {
            map.serialize_entry("gpg-sig", &gpg_sig)?;
        }
        match &self.provenance {
            Provenance::Absent => {}
            Provenance::None => map.serialize_entry("provenance", &Option::<String>::None)?,
            Provenance::Url(url) => map.serialize_entry("provenance", url)?,
        }
        map.end()
    }
}
