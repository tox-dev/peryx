use peryx_ecosystem_pypi_fuzz::pypi_filename;

#[test]
fn pypi_filename_matches_the_utf8_domain() {
    for (data, expected) in [(b"example-1.0.tar.gz".as_slice(), true), (&[u8::MAX], false)] {
        assert_eq!(pypi_filename(data), expected);
    }
}
