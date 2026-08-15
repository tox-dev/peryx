use peryx_ecosystem_pypi::parse_distribution_filename;

#[must_use]
pub fn pypi_filename(data: &[u8]) -> bool {
    let Ok(filename) = std::str::from_utf8(data) else {
        return false;
    };
    let _ = parse_distribution_filename(filename);
    true
}
