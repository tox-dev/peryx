use std::io::Write;

use peryx_driver::DriverSet;
use peryx_driver::Index;

use super::CacheStores;

pub(super) fn fsck_cache(
    drivers: &DriverSet,
    stores: &CacheStores,
    indexes: &[Index],
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    peryx_driver::cache_inspection::write_cache_fsck(drivers, &stores.meta, &stores.blobs, indexes, out)
        .map_err(anyhow::Error::new)
}

pub(super) fn repair_cache(
    drivers: &DriverSet,
    stores: &CacheStores,
    indexes: &[Index],
    apply: bool,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    peryx_driver::cache_inspection::write_cache_repair(drivers, &stores.meta, indexes, apply, out)
        .map_err(anyhow::Error::new)
}

#[cfg(test)]
#[path = "../../tests/unit/tests/app/fsck_tests.rs"]
mod tests;
