use std::io::Write as _;

mod integration_tests;
mod sdist_tests;
mod wheel_tests;

pub(super) fn valid_sdist(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut tarball = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for (path, bytes) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *bytes).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
    }
    tarball
}

pub(super) fn valid_zip_sdist(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (path, bytes) in entries {
            if let Some(dir) = path.strip_suffix('/') {
                zip.add_directory(dir, options).unwrap();
            } else {
                zip.start_file(*path, options).unwrap();
                zip.write_all(bytes).unwrap();
            }
        }
        zip.finish().unwrap();
    }
    buf
}

pub(super) fn temp_archive(bytes: &[u8]) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(bytes).unwrap();
    file.flush().unwrap();
    file
}
