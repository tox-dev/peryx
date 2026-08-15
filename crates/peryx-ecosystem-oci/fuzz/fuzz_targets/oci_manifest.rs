#![no_main]

use libfuzzer_sys::fuzz_target;
use peryx_ecosystem_oci_fuzz::fuzz_manifest;

fuzz_target!(|data: &[u8]| run(data));

fn run(data: &[u8]) {
    let _ = fuzz_manifest(data);
}
