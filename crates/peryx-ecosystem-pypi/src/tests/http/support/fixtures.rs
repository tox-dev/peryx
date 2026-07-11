//! The wheels, sdists and archives the tests upload and inspect.

use super::*;

pub fn fixture_wheel() -> Vec<u8> {
    fixture_wheel_for("1.0")
}
pub fn fixture_sdist() -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let content = b"Metadata-Version: 2.2\nName: peryxpkg\nVersion: 1.0\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "peryxpkg-1.0/PKG-INFO", content.as_slice())
            .unwrap();
        let pyproject = b"[build-system]\nrequires = []\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(pyproject.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "peryxpkg-1.0/pyproject.toml", pyproject.as_slice())
            .unwrap();
        tar.finish().unwrap();
    }
    buf
}
pub fn fixture_sdist_without_pkg_info() -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let content = b"x = 1\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "peryxpkg-1.0/module.py", content.as_slice())
            .unwrap();
        let pyproject = b"[build-system]\nrequires = []\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(pyproject.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "peryxpkg-1.0/pyproject.toml", pyproject.as_slice())
            .unwrap();
        tar.finish().unwrap();
    }
    buf
}
pub fn fixture_zip_sdist() -> Vec<u8> {
    fixture_zip_sdist_with_metadata(b"Metadata-Version: 2.2\nName: peryxpkg\nVersion: 1.0\n")
}
pub fn fixture_zip_sdist_with_metadata(metadata: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.add_directory("peryxpkg-1.0", options).unwrap();
        zip.start_file("peryxpkg-1.0/PKG-INFO", options).unwrap();
        zip.write_all(metadata).unwrap();
        zip.start_file("peryxpkg-1.0/pyproject.toml", options).unwrap();
        zip.write_all(b"[build-system]\nrequires = []\n").unwrap();
        zip.finish().unwrap();
    }
    buf
}
pub fn fixture_wheel_for(version: &str) -> Vec<u8> {
    fixture_wheel_with_body(version, b"VALUE = 1\n")
}
pub fn fixture_wheel_with_body(version: &str, body: &[u8]) -> Vec<u8> {
    fixture_wheel_with_body_and_metadata(
        version,
        body,
        Some(format!("Metadata-Version: 2.1\nName: peryxpkg\nVersion: {version}\nRequires-Python: >=3.8\n").as_bytes()),
    )
}
pub fn fixture_wheel_without_metadata() -> Vec<u8> {
    fixture_wheel_with_body_and_metadata("1.0", b"VALUE = 1\n", None)
}
pub fn fixture_wheel_with_metadata(metadata: &[u8]) -> Vec<u8> {
    fixture_wheel_with_body_and_metadata("1.0", b"VALUE = 1\n", Some(metadata))
}
pub fn empty_zip() -> Vec<u8> {
    let mut bytes = Vec::new();
    zip::ZipWriter::new(std::io::Cursor::new(&mut bytes)).finish().unwrap();
    bytes
}
pub fn fixture_wheel_with_metadata_compression(metadata: &[u8], compression: zip::CompressionMethod) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default().compression_method(compression);
        let dist_info = "peryxpkg-1.0.dist-info";
        let wheel = b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n";
        let entries = [
            ("peryxpkg/__init__.py".to_owned(), b"VALUE = 1\n".to_vec()),
            (format!("{dist_info}/METADATA"), metadata.to_vec()),
            (format!("{dist_info}/WHEEL"), wheel.to_vec()),
        ];
        for (path, bytes) in &entries {
            zip.start_file(path, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        let record_path = format!("{dist_info}/RECORD");
        zip.start_file(&record_path, options).unwrap();
        zip.write_all(record(&entries, &record_path).as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    buf
}
pub fn wheel_with_invalid_deflated_metadata(metadata: &[u8]) -> Vec<u8> {
    let mut wheel = fixture_wheel_with_metadata(metadata);
    let data_start = metadata_local_data_start(&wheel);
    wheel[data_start] = 0x07;
    wheel
}
pub fn wheel_with_metadata_compression_method(metadata: &[u8], compression_method: u16) -> Vec<u8> {
    let mut wheel = fixture_wheel_with_metadata(metadata);
    let position = metadata_central_directory_position(&wheel);
    wheel[position + 10..position + 12].copy_from_slice(&compression_method.to_le_bytes());
    wheel
}
pub fn wheel_with_metadata_uncompressed_size(metadata: &[u8], uncompressed_size: u32) -> Vec<u8> {
    let mut wheel = fixture_wheel_with_metadata(metadata);
    let position = metadata_central_directory_position(&wheel);
    wheel[position + 24..position + 28].copy_from_slice(&uncompressed_size.to_le_bytes());
    wheel
}
pub fn overwrite_metadata_local_signature(wheel: &mut [u8], signature: [u8; 4]) {
    let position = metadata_local_header_position(wheel);
    wheel[position..position + 4].copy_from_slice(&signature);
}
pub fn overwrite_metadata_central_signature(wheel: &mut [u8], signature: [u8; 4]) {
    let position = metadata_central_directory_position(wheel);
    wheel[position..position + 4].copy_from_slice(&signature);
}
pub fn metadata_local_data_start(wheel: &[u8]) -> usize {
    let position = metadata_local_header_position(wheel);
    let name_len = usize::from(u16::from_le_bytes(
        wheel[position + 26..position + 28].try_into().unwrap(),
    ));
    let extra_len = usize::from(u16::from_le_bytes(
        wheel[position + 28..position + 30].try_into().unwrap(),
    ));
    position + 30 + name_len + extra_len
}
pub fn metadata_local_header_position(wheel: &[u8]) -> usize {
    let metadata = b"peryxpkg-1.0.dist-info/METADATA";
    for position in 0..wheel.len().saturating_sub(30) {
        if !wheel[position..].starts_with(b"PK\x03\x04") {
            continue;
        }
        let name_len = usize::from(u16::from_le_bytes(
            wheel[position + 26..position + 28].try_into().unwrap(),
        ));
        let name_start = position + 30;
        let name_end = name_start + name_len;
        if wheel.get(name_start..name_end) == Some(metadata.as_slice()) {
            return position;
        }
    }
    panic!("metadata local header not found");
}
pub fn metadata_central_directory_position(wheel: &[u8]) -> usize {
    let metadata = b"peryxpkg-1.0.dist-info/METADATA";
    for position in 0..wheel.len().saturating_sub(46) {
        if !wheel[position..].starts_with(b"PK\x01\x02") {
            continue;
        }
        let name_len = usize::from(u16::from_le_bytes(
            wheel[position + 28..position + 30].try_into().unwrap(),
        ));
        let name_start = position + 46;
        let name_end = name_start + name_len;
        if wheel.get(name_start..name_end) == Some(metadata.as_slice()) {
            return position;
        }
    }
    panic!("metadata central directory entry not found");
}
pub fn fixture_wheel_with_body_and_metadata(version: &str, body: &[u8], metadata: Option<&[u8]>) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        let dist_info = format!("peryxpkg-{version}.dist-info");
        let wheel = b"Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n";
        let mut entries = vec![("peryxpkg/__init__.py".to_owned(), body.to_vec())];
        if let Some(metadata) = metadata {
            entries.push((format!("{dist_info}/METADATA"), metadata.to_vec()));
        }
        entries.push((format!("{dist_info}/WHEEL"), wheel.to_vec()));
        for (path, bytes) in &entries {
            zip.start_file(path, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        let record_path = format!("{dist_info}/RECORD");
        zip.start_file(&record_path, options).unwrap();
        zip.write_all(record(&entries, &record_path).as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    buf
}
pub fn record(entries: &[(String, Vec<u8>)], record_path: &str) -> String {
    let mut record = String::new();
    for (path, bytes) in entries {
        let digest = URL_SAFE_NO_PAD.encode(Sha256::digest(bytes));
        writeln!(record, "{path},sha256={digest},{}", bytes.len()).unwrap();
    }
    writeln!(record, "{record_path},,").unwrap();
    record
}
pub async fn upload_wheel(state: &Arc<AppState>, filename: &str, bytes: &[u8]) -> Digest {
    upload_wheel_to(state, "/hosted/", filename, "1.0", bytes).await
}
pub async fn upload_wheel_to(state: &Arc<AppState>, uri: &str, filename: &str, version: &str, bytes: &[u8]) -> Digest {
    let fields = vec![
        (":action", "file_upload"),
        ("name", "peryxpkg"),
        ("version", version),
        ("filetype", "bdist_wheel"),
    ];
    let (ct, body) = multipart_body(&fields, Some((filename, bytes)));
    assert_eq!(
        post_upload(state, uri, Some(&upload_auth()), &ct, body).await,
        StatusCode::OK
    );
    Digest::of(bytes)
}
pub fn blob_count(state: &AppState) -> u64 {
    let mut count = 0;
    state
        .blobs
        .scan(|_entry| {
            count += 1;
            Ok::<(), std::io::Error>(())
        })
        .unwrap();
    count
}
pub fn upload_record(
    filename: &str,
    version: &str,
    url: String,
    hashes: BTreeMap<String, String>,
    size: Option<u64>,
) -> Uploaded {
    Uploaded {
        version: version.to_owned(),
        file: File {
            filename: filename.to_owned(),
            url,
            hashes,
            requires_python: None,
            size,
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        },
    }
}
pub fn put_local_file(state: &AppState, filename: &str, bytes: &[u8], version: &str) -> Digest {
    let digest = Digest::of(bytes);
    state.blobs.write_verified(bytes, &digest).unwrap();
    let uploaded = upload_record(
        filename,
        version,
        local_file_url("hosted", digest.as_str(), filename),
        BTreeMap::from([("sha256".to_owned(), digest.as_str().to_owned())]),
        Some(bytes.len() as u64),
    );
    state
        .meta
        .put_upload("hosted", "peryxpkg", filename, &to_json(&uploaded).into_bytes())
        .unwrap();
    state.meta.put_project("hosted", "peryxpkg", "peryxpkg").unwrap();
    digest
}
