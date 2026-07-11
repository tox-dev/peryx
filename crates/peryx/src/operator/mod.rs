//! Operator workflows for offline state movement and local artifact import.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read as _, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, bail};
use peryx_storage::blob::Digest;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

mod backup;
mod import;
mod restore;
mod snapshot;
mod verify;

pub use backup::backup_create;
pub use import::import_dir;
pub use restore::restore;
pub use verify::backup_verify;

const BACKUP_FORMAT: u32 = 1;
const BUFFER_BYTES: usize = 1024 * 1024;
const BLOB_INDEX_HEADER: &str = "sha256\tsize_bytes\tpath";

#[derive(Debug, Serialize, Deserialize)]
struct BackupManifest {
    format: u32,
    created_at_unix: i64,
    config: ManifestFile,
    metadata: ManifestFile,
    blob_index: ManifestBlobIndex,
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

fn write_manifest(path: &Path, manifest: &BackupManifest) -> anyhow::Result<()> {
    let manifest_path = path.join("manifest.json");
    let mut file = File::create(&manifest_path).context(format!("create {}", manifest_path.display()))?;
    serde_json::to_writer_pretty(&mut file, manifest)?;
    writeln!(file)?;
    file.sync_all()?;
    Ok(())
}

fn read_manifest(path: &Path) -> anyhow::Result<BackupManifest> {
    let manifest_path = path.join("manifest.json");
    let manifest: BackupManifest =
        serde_json::from_reader(File::open(&manifest_path).context(format!("open {}", manifest_path.display()))?)
            .context(format!("parse {}", manifest_path.display()))?;
    if manifest.format != BACKUP_FORMAT {
        bail!("unsupported backup format {}", manifest.format);
    }
    Ok(manifest)
}

fn copy_hashed(source: &Path, dest: &Path, manifest_path: &str) -> anyhow::Result<ManifestFile> {
    let parent = dest.parent().expect("hashed files are written below a directory");
    std::fs::create_dir_all(parent).context(format!("create {}", parent.display()))?;
    let mut input = BufReader::with_capacity(BUFFER_BYTES, File::open(source)?);
    let mut output = BufWriter::with_capacity(BUFFER_BYTES, File::create(dest)?);
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
    output.into_inner()?.sync_all()?;
    Ok(ManifestFile {
        path: manifest_path.to_owned(),
        sha256: hex(&hasher.finalize()),
        size_bytes,
    })
}

fn write_hashed(path: &Path, bytes: &[u8], manifest_path: &str) -> anyhow::Result<ManifestFile> {
    let parent = path.parent().expect("hashed files are written below a directory");
    std::fs::create_dir_all(parent).context(format!("create {}", parent.display()))?;
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(ManifestFile {
        path: manifest_path.to_owned(),
        sha256: hex(&Sha256::digest(bytes)),
        size_bytes: bytes.len() as u64,
    })
}

fn hash_existing_file(path: &Path) -> anyhow::Result<HashedFile> {
    let mut input = BufReader::with_capacity(BUFFER_BYTES, File::open(path)?);
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
