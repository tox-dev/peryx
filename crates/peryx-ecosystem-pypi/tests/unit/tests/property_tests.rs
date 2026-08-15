use std::io::Write as _;

use url::Url;

use crate::archive::{list_members, read_member};
use crate::{parse_detail_html, parse_metadata};

#[test]
fn metadata_preserves_generated_fields() {
    for seed in 0..256 {
        let name = generated(seed, b"abcdefghijklmnopqrstuvwxyz0123456789_", 20);
        let version = format!("{}.{}", seed % 100, seed.wrapping_mul(17) % 100);
        let summary = generated(seed.wrapping_mul(31), b"abcdefghijklmnopqrstuvwxyz ", 40);
        let keywords = (0..seed % 8)
            .map(|offset| generated(seed.wrapping_add(offset), b"abcdefghijklmnopqrstuvwxyz", 8))
            .collect::<Vec<_>>();
        let description = generated(seed.wrapping_mul(43), b"abcdefghijklmnopqrstuvwxyz .,", 80);
        let parsed = parse_metadata(&format!(
            "Metadata-Version: 2.4\nName: {name}\nVersion: {version}\nSummary: {summary}\nKeywords: {}\n\n{description}",
            keywords.join(", "),
        ))
        .unwrap();
        assert_eq!(
            (
                parsed.name,
                parsed.version,
                parsed.summary,
                parsed.keywords,
                parsed.description,
            ),
            (name, version, Some(summary.trim().to_owned()), keywords, description,),
            "seed {seed}",
        );
    }
}

#[test]
fn html_preserves_generated_file_attributes() {
    for seed in 0..256 {
        let stem = generated(seed, b"abcdefghijklmnopqrstuvwxyz0123456789_", 20);
        let digest = generated(seed.wrapping_mul(47), b"abcdef0123456789", 64)
            .repeat(64)
            .chars()
            .take(64)
            .collect::<String>();
        let filename = format!("{stem}-1.0-py3-none-any.whl");
        let requires_python = format!(">=3.{}", seed % 20);
        let parsed = parse_detail_html(
            &stem,
            &format!(
                r#"<a href="../../packages/{filename}#sha256={digest}" data-requires-python="{requires_python}">{filename}</a>"#,
            ),
            &Url::parse("https://pypi.org/simple/project/").unwrap(),
        )
        .unwrap();
        let file = &parsed.files[0];
        assert_eq!(
            (
                parsed.name,
                file.filename.as_str(),
                file.url.as_str(),
                file.sha256(),
                file.requires_python.as_deref(),
            ),
            (
                stem,
                filename.as_str(),
                format!("https://pypi.org/packages/{filename}").as_str(),
                Some(digest.as_str()),
                Some(requires_python.as_str()),
            ),
            "seed {seed}",
        );
    }
}

#[test]
fn zip_archive_recovers_generated_member() {
    for seed in 0..128 {
        let path = format!("package/{}.txt", generated(seed, b"abcdefghijklmnopqrstuvwxyz", 16));
        let content = (0..seed % 64)
            .map(|offset| seed.wrapping_mul(31).wrapping_add(offset).to_le_bytes()[0])
            .collect::<Vec<_>>();
        let mut bytes = Vec::new();
        {
            let mut archive = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
            archive
                .start_file(&path, zip::write::SimpleFileOptions::default())
                .unwrap();
            archive.write_all(&content).unwrap();
            archive.finish().unwrap();
        }
        assert_eq!(list_members("package.whl", &bytes).unwrap().len(), 1, "seed {seed}");
        assert_eq!(
            read_member("package.whl", &bytes, &path).unwrap(),
            content,
            "seed {seed}",
        );
    }
}

fn generated(mut state: u32, alphabet: &[u8], max_len: usize) -> String {
    let len = usize::try_from(state).unwrap() % max_len + 1;
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            char::from(alphabet[usize::try_from(state).unwrap() % alphabet.len()])
        })
        .collect()
}
