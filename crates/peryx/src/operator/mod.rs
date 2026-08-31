use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read as _, Seek as _, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, bail};
use cap_fs_ext::{DirExt as _, FollowSymlinks, OpenOptionsFollowExt as _};
use cap_std::fs::{Dir, OpenOptions};
use peryx_storage::blob::Digest;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::config::{Config, DcMembership, DcRole};

mod backup;
mod import;
mod restore;
mod snapshot;
mod verify;
mod writer;

#[cfg(test)]
#[path = "../../tests/unit/tests/operator/parent_tests.rs"]
mod parent_tests;

#[cfg(test)]
#[path = "../../tests/unit/tests/operator/mod.rs"]
mod tests;

pub use backup::{backup_create, backup_create_with_plugins};
pub use import::{import_dir, import_dir_with_plugins};
pub use restore::{restore, restore_with_plugins};
pub use verify::{backup_verify, backup_verify_with_plugins};
pub use writer::{claim_writer, claim_writer_with_plugins, promote_writer, promote_writer_with_plugins};

const BACKUP_FORMAT: u32 = 2;
const BUFFER_BYTES: usize = 1024 * 1024;
const BLOB_INDEX_HEADER: &str = "sha256\tsize_bytes\tpath";
/// Prefix every backup staging sibling carries, so an attempt killed before it could clean up leaves a
/// name an operator recognizes and can delete. A retry reserves a fresh randomized name, so one left
/// behind never blocks it.
const STAGING_PREFIX: &str = ".peryx-backup-";

#[derive(Debug, Serialize, Deserialize)]
struct BackupManifest {
    format: u32,
    created_at_unix: i64,
    config: ManifestFile,
    metadata: ManifestFile,
    blob_index: ManifestBlobIndex,
    availability: ManifestAvailability,
}

/// The availability state a backup pins to one recovery point.
///
/// The metadata copy is a single snapshot; `metadata_frontier` names it with the store's control-plane
/// serial and `placements` sizes its artifact-availability projection, so a verifier re-derives both
/// from the copied store and rejects a metadata file swapped for one taken at a different point. `mode`
/// and `membership` carry the datacenter roster the configuration snapshot omits, making the manifest
/// the backup's sole record of the topology the recovery point belongs to. `writer_identity` is the
/// node the recovery point belongs to, so a restore refuses to adopt one node's state under another
/// node's identity. An older backup that never recorded it restores without the identity guard.
#[derive(Debug, Serialize, Deserialize)]
struct ManifestAvailability {
    mode: String,
    metadata_frontier: u64,
    placements: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    writer_identity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    membership: Option<ManifestMembership>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestMembership {
    group: String,
    members: Vec<ManifestMember>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestMember {
    node: String,
    dc: String,
    address: String,
    role: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestBlobIndex {
    #[serde(flatten)]
    file: ManifestFile,
    count: u64,
    blob_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestFile {
    path: String,
    sha256: String,
    size_bytes: u64,
}

#[derive(Debug, Clone)]
struct HashedFile {
    sha256: String,
    size_bytes: u64,
}

#[derive(Debug)]
struct BlobIndexEntry {
    size_bytes: u64,
    path: String,
}

struct BackupCheck {
    problems: u64,
    blobs: BTreeMap<String, BlobIndexEntry>,
}

struct BackupSource {
    dir: Dir,
    path: PathBuf,
}

impl BackupSource {
    fn open(path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            dir: Dir::open_ambient_dir(path, cap_std::ambient_authority())
                .context(format!("open backup directory {}", path.display()))?,
            path: path.to_owned(),
        })
    }

    fn required_file(&self, relative: &str) -> anyhow::Result<File> {
        self.file(relative)?.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("backup member {relative} is missing or is not a regular file"),
            )
            .into()
        })
    }

    fn file(&self, relative: &str) -> anyhow::Result<Option<File>> {
        let relative = backup_member_path(relative)?;
        let member = self.path.join(relative);
        let mut parent = self.dir.try_clone()?;
        for component in relative.parent().into_iter().flat_map(Path::components) {
            let metadata = match parent.symlink_metadata(component.as_os_str()) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                result => result.context(format!("inspect backup member {}", member.display()))?,
            };
            if metadata.file_type().is_symlink() {
                bail!("backup member {} contains a symbolic link", member.display());
            }
            if !metadata.file_type().is_dir() {
                return Ok(None);
            }
            let context = format!("open backup member {} without following links", member.display());
            parent = parent.open_dir_nofollow(component.as_os_str()).context(context)?;
        }
        let name = relative
            .file_name()
            .context("a validated backup member path has no file name")?;
        let metadata = match parent.symlink_metadata(name) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            result => result.context(format!("inspect backup member {}", member.display()))?,
        };
        if metadata.file_type().is_symlink() {
            bail!("backup member {} contains a symbolic link", member.display());
        }
        if !metadata.file_type().is_file() {
            return Ok(None);
        }
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = match parent.open_with(name, &options) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            result => result.context(format!(
                "open backup member {} without following links",
                member.display()
            ))?,
        };
        Ok(file.metadata()?.file_type().is_file().then(|| file.into_std()))
    }
}

fn backup_member_path(path: &str) -> anyhow::Result<&Path> {
    let path = Path::new(path);
    anyhow::ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_))),
        "invalid backup member path {}; expected a non-empty relative path with normal components",
        path.display()
    );
    Ok(path)
}

/// The availability mode and datacenter roster a manifest records from the effective configuration.
///
/// The configuration snapshot carries the mode but omits the static roster, so the manifest is the
/// backup's only durable record of it.
fn config_availability(config: &Config) -> (String, Option<ManifestMembership>) {
    (
        config.availability.mode().as_str().to_owned(),
        config.dc_membership.as_ref().map(manifest_membership),
    )
}

fn backup_config_with_plugins(
    backup: &BackupSource,
    manifest: &BackupManifest,
    plugins: &peryx_plugin_registry::PluginRegistry,
) -> anyhow::Result<Config> {
    let path = backup.path.join(&manifest.config.path);
    let mut text = String::new();
    backup
        .required_file(&manifest.config.path)?
        .read_to_string(&mut text)
        .with_context(|| format!("read backup config {}", path.display()))?;
    Config::with_plugins(plugins)
        .apply_with_plugins(crate::config::from_toml(path, &text)?, plugins)
        .context("parse backup config snapshot")
}

fn backup_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
) -> anyhow::Result<peryx_plugin_registry::PluginRegistry> {
    crate::server::activate_plugins(config, plugins)
}

fn manifest_membership(membership: &DcMembership) -> ManifestMembership {
    ManifestMembership {
        group: membership.group.clone(),
        members: membership
            .members
            .iter()
            .map(|member| ManifestMember {
                node: member.node.clone(),
                dc: member.dc.clone(),
                address: member.address.clone(),
                role: match member.role {
                    DcRole::Writer => "writer",
                    DcRole::Replica => "replica",
                }
                .to_owned(),
            })
            .collect(),
    }
}

fn write_manifest(mut file: File, manifest: &BackupManifest) -> anyhow::Result<()> {
    serde_json::to_writer_pretty(&mut file, manifest)?;
    writeln!(file)?;
    file.sync_all()?;
    Ok(())
}

/// Whether a created file may carry secrets. A backup nests every file under an owner-only root, but a
/// restore writes the configuration snapshot and metadata store straight into a data directory whose
/// own mode it does not set, so those two files carry their private mode themselves. Blobs are
/// content-addressed artifacts, not secrets, so they keep the platform default.
#[derive(Clone, Copy)]
enum Access {
    Private,
    Shared,
}

/// Create a directory tree only its owner may enter. On Unix every component is created `0700`
/// regardless of the caller umask; other platforms fall back to the default, matching prior behavior.
fn create_private_dir_all(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    let result = {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)
    };
    #[cfg(not(unix))]
    let result = std::fs::create_dir_all(path);
    result.context(format!("create backup directory {}", path.display()))
}

/// Create a file only its owner may read or write. On Unix it is created `0600` regardless of the
/// caller umask; other platforms fall back to a plain create.
fn create_private_file(path: &Path) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

fn create_file(path: &Path, access: Access) -> std::io::Result<File> {
    match access {
        Access::Private => create_private_file(path),
        Access::Shared => File::create(path),
    }
}

fn read_manifest(backup: &BackupSource) -> anyhow::Result<BackupManifest> {
    let manifest_path = backup.path.join("manifest.json");
    let manifest: BackupManifest = serde_json::from_reader(backup.required_file("manifest.json")?)
        .context(format!("parse {}", manifest_path.display()))?;
    if manifest.format != BACKUP_FORMAT {
        bail!("unsupported backup format {}", manifest.format);
    }
    ensure_manifest_path(&manifest.config.path, "config.toml", "config")?;
    ensure_manifest_path(&manifest.metadata.path, "metadata/peryx.redb", "metadata")?;
    ensure_manifest_path(&manifest.blob_index.file.path, "blobs.tsv", "blob index")?;
    Ok(manifest)
}

fn ensure_manifest_path(actual: &str, expected: &str, kind: &str) -> anyhow::Result<()> {
    let valid = backup_member_path(actual).is_ok();
    anyhow::ensure!(
        valid && actual == expected,
        "invalid {kind} path {actual:?}; expected {expected:?}"
    );
    Ok(())
}

/// Name the directory a hashed file is written below. Backup and restore targets always nest under a
/// backup root, so a parent exists in practice; a path handed in without one surfaces a structured
/// error rather than crashing the operator flow.
fn hashed_parent(path: &Path) -> anyhow::Result<&Path> {
    path.parent()
        .context(format!("hashed file {} has no parent directory", path.display()))
}

fn copy_hashed(source: File, dest: &Path, manifest_path: &str, access: Access) -> anyhow::Result<ManifestFile> {
    let parent = hashed_parent(dest)?;
    std::fs::create_dir_all(parent).context(format!("create {}", parent.display()))?;
    Ok(copy_open_file(source, create_file(dest, access)?, manifest_path)?.0)
}

fn copy_hashed_file(source: &Path, output: File, manifest_path: &str) -> anyhow::Result<(ManifestFile, File)> {
    copy_open_file(File::open(source)?, output, manifest_path)
}

fn copy_open_file(source: File, output: File, manifest_path: &str) -> anyhow::Result<(ManifestFile, File)> {
    let mut input = BufReader::with_capacity(BUFFER_BYTES, source);
    let mut output = BufWriter::with_capacity(BUFFER_BYTES, output);
    let mut hasher = Sha256::new();
    let mut size_bytes = 0;
    let mut buffer = vec![0; BUFFER_BYTES];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
        size_bytes += read as u64;
    }
    let output = output.into_inner()?;
    output.sync_all()?;
    Ok((
        ManifestFile {
            path: manifest_path.to_owned(),
            sha256: hex(&hasher.finalize()),
            size_bytes,
        },
        output,
    ))
}

fn write_hashed_file(mut file: File, bytes: &[u8], manifest_path: &str) -> anyhow::Result<ManifestFile> {
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(ManifestFile {
        path: manifest_path.to_owned(),
        sha256: hex(&Sha256::digest(bytes)),
        size_bytes: bytes.len() as u64,
    })
}

fn hash_file(mut file: File) -> anyhow::Result<HashedFile> {
    file.rewind()?;
    let mut input = BufReader::with_capacity(BUFFER_BYTES, file);
    let mut hasher = Sha256::new();
    let mut size_bytes = 0;
    let mut buffer = vec![0; BUFFER_BYTES];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size_bytes += read as u64;
    }
    Ok(HashedFile {
        sha256: hex(&hasher.finalize()),
        size_bytes,
    })
}

fn backup_blob_path(root: &Path, digest: &Digest) -> PathBuf {
    root.join(backup_blob_relpath(digest))
}

fn backup_blob_relpath(digest: &Digest) -> String {
    let hex = digest.as_str();
    format!("blobs/sha256/{}/{}/{}", &hex[0..2], &hex[2..4], hex)
}

/// Flush a directory tree from the leaves up, so every entry written below `path` is durable before
/// the tree itself is linked under a name a reader will follow.
fn sync_tree(path: &Path) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(path).context(format!("read directory {}", path.display()))? {
        let child = entry
            .context(format!("read directory entry in {}", path.display()))?
            .path();
        if child.is_dir() {
            sync_tree(&child)?;
        }
    }
    sync_dir(path)
}

/// Flush the directory a path is named in. Synchronizing a file does not make the directory entry
/// that names it durable, so a publication survives a power loss only once its parent is flushed too.
fn sync_parent(path: &Path) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .context(format!("path {} has no parent directory", path.display()))?;
    sync_dir(parent)
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> anyhow::Result<()> {
    File::open(path)
        .context(format!("open directory {} for sync", path.display()))?
        .sync_all()
        .context(format!("sync directory {}", path.display()))
}

/// Windows offers no directory-entry flush, so publication relies on the rename alone.
#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

fn is_empty_dir(path: &Path) -> anyhow::Result<bool> {
    Ok(std::fs::read_dir(path)
        .context(format!("read directory {}", path.display()))?
        .next()
        .is_none())
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn unix_now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
    .unwrap_or(i64::MAX)
}
