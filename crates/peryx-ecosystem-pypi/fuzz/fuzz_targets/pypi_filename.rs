#![no_main]

use libfuzzer_sys::fuzz_target;
use peryx_ecosystem_pypi_fuzz::pypi_filename;

fuzz_target!(|data: &[u8]| run(data));

fn run(data: &[u8]) {
    let _ = pypi_filename(data);
}
