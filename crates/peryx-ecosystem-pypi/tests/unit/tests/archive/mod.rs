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

/// A stored-compression zip assembled from raw records, so a fixture can carry the duplicate names
/// and file/directory collisions that [`zip::ZipWriter`] rejects but a hand-crafted upload can hold.
/// A name ending in `/` becomes a directory entry.
pub(super) fn raw_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let is_dir = name.ends_with('/');
        let data: &[u8] = if is_dir { &[] } else { data };
        let name = name.as_bytes();
        let size = u32::try_from(data.len()).unwrap();
        let name_len = u16::try_from(name.len()).unwrap();
        let crc = crc32(data);
        let offset = u32::try_from(out.len()).unwrap();
        out.extend_from_slice(&0x0403_4b50_u32.to_le_bytes());
        out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(name);
        out.extend_from_slice(data);

        let external = if is_dir { (0o0_040_755_u32 << 16) | 0x10 } else { 0 };
        central.extend_from_slice(&0x0201_4b50_u32.to_le_bytes());
        central.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&name_len.to_le_bytes());
        central.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
        central.extend_from_slice(&external.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name);
    }
    let central_offset = u32::try_from(out.len()).unwrap();
    let central_size = u32::try_from(central.len()).unwrap();
    let count = u16::try_from(entries.len()).unwrap();
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50_u32.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&central_size.to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

pub(super) fn temp_archive(bytes: &[u8]) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(bytes).unwrap();
    file.flush().unwrap();
    file
}
