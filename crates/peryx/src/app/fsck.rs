use std::io::Write;

use peryx_driver::DriverSet;

use super::CacheStores;

pub(super) fn fsck_cache(drivers: &DriverSet, stores: &CacheStores, out: &mut dyn Write) -> anyhow::Result<()> {
    peryx_driver::cache_inspection::write_cache_fsck(drivers, &stores.meta, &stores.blobs, out)
        .map_err(anyhow::Error::new)
}

#[cfg(test)]
#[path = "../../tests/unit/tests/app/fsck_tests.rs"]
mod tests;
