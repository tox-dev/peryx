use std::fmt::Write as _;
use std::io::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use flate2::Compression;
use flate2::write::GzEncoder;
use peryx_ecosystem_pypi::store::PypiStore as _;
use peryx_storage::meta::MetaStore;
use sha2::{Digest as _, Sha256};

use crate::config::{Config, IndexConfig, IndexKind};
use crate::operator;

use super::backup_fixture;

#[test]
fn test_import_dir_validates_and_reports_files() {
    let root = tempfile::tempdir().unwrap();
    let import = root.path().join("import");
    std::fs::create_dir(&import).unwrap();
    std::fs::write(
        import.join("Flask-1.0-py3-none-any.whl"),
        wheel("Flask", "1.0", ">=3.8"),
    )
    .unwrap();
    std::fs::write(import.join("Demo-2.0.tar.gz"), sdist("Demo", "2.0")).unwrap();
    std::fs::write(import.join("Broken-1.0-py3-none-any.whl"), b"not a wheel").unwrap();
    std::fs::write(import.join("notes.txt"), b"skip").unwrap();
    let config = Config {
        data_dir: root.path().join("data"),
        ..Config::default()
    };

    let mut out = Vec::new();
    operator::import_dir(&config, "root/pypi", &import, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("imported\tDemo-2.0.tar.gz\tdemo\t2.0\tstored"));
    assert!(text.contains("imported\tFlask-1.0-py3-none-any.whl\tflask\t1.0\tstored"));
    assert!(text.contains("rejected\tBroken-1.0-py3-none-any.whl\tbroken\t1.0\tinvalid content"));
    assert!(text.contains("skipped\tnotes.txt\t\t\tunsupported file type"));
    assert!(text.contains("summary\t\t\t\timported=2 skipped=1 rejected=1"));

    let meta = MetaStore::open_existing(config.data_dir.join("peryx.redb")).unwrap();
    assert_eq!(meta.list_upload_entries("hosted", "demo").unwrap().len(), 1);
    assert_eq!(meta.list_upload_entries("hosted", "flask").unwrap().len(), 1);
}

#[test]
fn test_import_dir_reports_duplicate_nested_and_invalid_files() {
    let root = tempfile::tempdir().unwrap();
    let import = root.path().join("import");
    let nested = import.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        nested.join("Flask-1.0-py3-none-any.whl"),
        wheel("Flask", "1.0", ">=3.8"),
    )
    .unwrap();
    std::fs::write(
        import.join("Flask-1.0-py3-none-any.whl"),
        wheel("Flask", "1.0", ">=3.8"),
    )
    .unwrap();
    std::fs::write(import.join("bad.whl"), b"not a valid wheel").unwrap();
    std::fs::write(import.join("Legacy-1.0-py3-none-any.egg"), b"egg").unwrap();
    let config = Config {
        data_dir: root.path().join("data"),
        ..Config::default()
    };

    let mut out = Vec::new();
    operator::import_dir(&config, "root/pypi", &import, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap().replace('\\', "/");
    assert!(text.contains("imported\tFlask-1.0-py3-none-any.whl\tflask\t1.0\tstored"));
    assert!(text.contains("skipped\tnested/Flask-1.0-py3-none-any.whl\tflask\t1.0\talready present"));
    assert!(
        text.contains("rejected\tbad.whl\t\t\tinvalid distribution filename"),
        "{text}"
    );
    assert!(text.contains("invalid distribution filename"), "{text}");
    assert!(text.contains("skipped\tLegacy-1.0-py3-none-any.egg\t\t\tunsupported file type"));
}

#[test]
fn test_import_dir_accepts_local_repository_route() {
    let root = tempfile::tempdir().unwrap();
    let import = root.path().join("import");
    std::fs::create_dir(&import).unwrap();
    std::fs::write(import.join("Demo-2.0.tar.gz"), sdist("Demo", "2.0")).unwrap();
    let config = Config {
        data_dir: root.path().join("data"),
        ..Config::default()
    };

    let mut out = Vec::new();
    operator::import_dir(&config, "hosted", &import, &mut out).unwrap();

    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("imported\tDemo-2.0.tar.gz\tdemo\t2.0\tstored")
    );
}

#[test]
fn test_import_dir_rejects_existing_filename_with_different_content() {
    let (_source, config, _content_digest, _metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let import = root.path().join("import");
    std::fs::create_dir(&import).unwrap();
    std::fs::write(
        import.join("Flask-1.0-py3-none-any.whl"),
        wheel("Flask", "1.0", ">=3.8"),
    )
    .unwrap();

    let mut out = Vec::new();
    operator::import_dir(&config, "root/pypi", &import, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("rejected\tFlask-1.0-py3-none-any.whl\tflask\t1.0"));
    assert!(text.contains("file already exists with different content"));
}

#[test]
fn test_import_dir_reports_metadata_validation_reasons() {
    let root = tempfile::tempdir().unwrap();
    let import = root.path().join("import");
    std::fs::create_dir(&import).unwrap();
    std::fs::write(
        import.join("InvalidPython-1.0-py3-none-any.whl"),
        wheel("InvalidPython", "1.0", "not a specifier"),
    )
    .unwrap();
    std::fs::write(
        import.join("NameMismatch-1.0-py3-none-any.whl"),
        wheel_with_identity("NameMismatch", "1.0", "Other", "1.0", ">=3.8"),
    )
    .unwrap();
    std::fs::write(
        import.join("Utf8-1.0-py3-none-any.whl"),
        wheel_with_metadata("Utf8", "1.0", b"\xff"),
    )
    .unwrap();
    std::fs::write(
        import.join("VersionMismatch-1.0-py3-none-any.whl"),
        wheel_with_identity("VersionMismatch", "1.0", "VersionMismatch", "2.0", ">=3.8"),
    )
    .unwrap();
    let config = Config {
        data_dir: root.path().join("data"),
        ..Config::default()
    };

    let mut out = Vec::new();
    operator::import_dir(&config, "root/pypi", &import, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("InvalidPython-1.0-py3-none-any.whl\tinvalidpython\t1.0\tinvalid Requires-Python"));
    assert!(text.contains("NameMismatch-1.0-py3-none-any.whl\tnamemismatch\t1.0\tmetadata name"));
    assert!(text.contains("Utf8-1.0-py3-none-any.whl\tutf8\t1.0\tmetadata is not UTF-8"));
    assert!(text.contains("VersionMismatch-1.0-py3-none-any.whl\tversionmismatch\t1.0\tmetadata version"));
}

#[test]
fn test_import_dir_rejects_unusable_repositories_and_paths() {
    let root = tempfile::tempdir().unwrap();
    let import = root.path().join("import");
    std::fs::create_dir(&import).unwrap();
    let cached_config = Config {
        data_dir: root.path().join("cached-data"),
        indexes: vec![IndexConfig {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            policy: peryx_policy::PolicyConfig::default(),
            ecosystem_policy: toml::Table::new(),
            ecosystem_settings: toml::Table::new(),
            webhooks: Vec::new(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                upstream: "https://pypi.org/simple/".to_owned(),
                username: None,
                password: None,
                token: None,
                upstream_concurrency: peryx_driver::rate_limit::DEFAULT_UPSTREAM_CONCURRENCY,
                offline: false,
                prefetch: Box::default(),
            },
        }],
        ..Config::default()
    };
    let virtual_config = Config {
        data_dir: root.path().join("virtual-data"),
        indexes: vec![IndexConfig {
            name: "aggregate".to_owned(),
            route: "aggregate".to_owned(),
            policy: peryx_policy::PolicyConfig::default(),
            ecosystem_policy: toml::Table::new(),
            ecosystem_settings: toml::Table::new(),
            webhooks: Vec::new(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Virtual {
                layers: Vec::new(),
                upload: None,
            },
        }],
        ..Config::default()
    };

    assert!(
        operator::import_dir(
            &Config::default(),
            "root/pypi",
            root.path().join("missing").as_path(),
            &mut Vec::new()
        )
        .is_err()
    );
    assert!(
        operator::import_dir(&cached_config, "pypi", &import, &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("read-only")
    );
    assert!(
        operator::import_dir(&virtual_config, "aggregate", &import, &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("no hosted upload target")
    );
    assert!(
        operator::import_dir(&virtual_config, "missing", &import, &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("unknown index")
    );

    // import-dir imports wheels and sdists, so it refuses a hosted index of another ecosystem.
    let oci_config = Config {
        data_dir: root.path().join("oci-data"),
        indexes: vec![IndexConfig {
            name: "images".to_owned(),
            route: "images".to_owned(),
            policy: peryx_policy::PolicyConfig::default(),
            ecosystem_policy: toml::Table::new(),
            ecosystem_settings: toml::Table::new(),
            webhooks: Vec::new(),
            ecosystem: peryx_core::Ecosystem::Oci,
            kind: IndexKind::Hosted {
                upload_token: None,
                volatile: true,
            },
        }],
        ..Config::default()
    };
    assert!(
        operator::import_dir(&oci_config, "images", &import, &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("oci ecosystem")
    );
}

fn wheel(name: &str, version: &str, requires_python: &str) -> Vec<u8> {
    wheel_with_identity(name, version, name, version, requires_python)
}

fn wheel_with_identity(
    filename_name: &str,
    filename_version: &str,
    metadata_name: &str,
    metadata_version: &str,
    requires_python: &str,
) -> Vec<u8> {
    let metadata = format!(
        "Metadata-Version: 2.1\nName: {metadata_name}\nVersion: {metadata_version}\nRequires-Python: {requires_python}\n"
    );
    wheel_with_metadata(filename_name, filename_version, metadata.as_bytes())
}

fn wheel_with_metadata(name: &str, version: &str, metadata: &[u8]) -> Vec<u8> {
    let wheel = b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n";
    let init = b"VALUE = 1\n";
    let dist_info = format!("{}-{version}.dist-info", name.to_ascii_lowercase());
    let record_path = format!("{dist_info}/RECORD");
    let entries = [
        (format!("{name}/__init__.py"), init.as_slice()),
        (format!("{dist_info}/METADATA"), metadata),
        (format!("{dist_info}/WHEEL"), wheel.as_slice()),
    ];
    let record = record(&entries, &record_path);
    let mut bytes = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
        let options = zip::write::SimpleFileOptions::default();
        for (path, content) in &entries {
            zip.start_file(path, options).unwrap();
            zip.write_all(content).unwrap();
        }
        zip.start_file(&record_path, options).unwrap();
        zip.write_all(record.as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    bytes
}

fn record(entries: &[(String, &[u8])], record_path: &str) -> String {
    let mut record = String::new();
    for (path, bytes) in entries {
        writeln!(
            record,
            "{path},sha256={},{}",
            URL_SAFE_NO_PAD.encode(Sha256::digest(bytes)),
            bytes.len()
        )
        .unwrap();
    }
    writeln!(record, "{record_path},,").unwrap();
    record
}

fn sdist(name: &str, version: &str) -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut archive = tar::Builder::new(encoder);
    let root = format!("{name}-{version}");
    append_tar_file(
        &mut archive,
        &format!("{root}/PKG-INFO"),
        format!("Metadata-Version: 2.2\nName: {name}\nVersion: {version}\n").as_bytes(),
    );
    append_tar_file(
        &mut archive,
        &format!("{root}/pyproject.toml"),
        b"[build-system]\nrequires = []\nbuild-backend = \"demo\"\n",
    );
    archive.into_inner().unwrap().finish().unwrap()
}

fn append_tar_file(archive: &mut tar::Builder<GzEncoder<Vec<u8>>>, path: &str, bytes: &[u8]) {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    archive.append_data(&mut header, path, bytes).unwrap();
}

#[cfg(unix)]
#[test]
fn test_import_dir_skips_a_symlink_entry() {
    let root = tempfile::tempdir().unwrap();
    let import = root.path().join("import");
    std::fs::create_dir_all(&import).unwrap();
    std::fs::write(
        import.join("Flask-1.0-py3-none-any.whl"),
        wheel("Flask", "1.0", ">=3.8"),
    )
    .unwrap();
    // A symlink is neither a regular file nor a directory, so the directory walk skips it.
    std::os::unix::fs::symlink("/nonexistent", import.join("dangling.whl")).unwrap();
    let config = Config {
        data_dir: root.path().join("data"),
        ..Config::default()
    };

    let mut out = Vec::new();
    operator::import_dir(&config, "root/pypi", &import, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("imported=1"), "{text}");
    assert!(!text.contains("dangling"), "{text}");
}
