//! OCI mirror planning and synchronization.

use std::sync::Arc;

use anyhow::bail;
use peryx_http::{AppState, Index};

use super::report::write_count;
use super::{HEADER, Output};
use crate::cli::PrefetchOptions;
use crate::config::{Config, IndexKind as ConfigIndexKind};

/// The image references an OCI mirror pulls: the cached index's configured `[index.prefetch].packages`
/// list, plus any `--image` references on the command line. Configuration seeds a routine sync the way
/// a `PyPI` index's `[index.prefetch]` package list does; `--image` adds one-off references.
pub(super) fn oci_images(config: &Config, options: &PrefetchOptions) -> Vec<String> {
    let mut images: Vec<String> = config
        .indexes
        .iter()
        .find(|index| index.name == options.index || index.route == options.index)
        .and_then(|index| match &index.kind {
            ConfigIndexKind::Cached { prefetch, .. } => Some(prefetch.packages.clone()),
            _ => None,
        })
        .unwrap_or_default();
    images.extend(options.images.iter().cloned());
    images
}

/// The OCI index a name resolves to. [`mirror_driver`](super::dispatch) selected this driver because
/// one exists, so a miss is an invariant violation, not a user error.
pub(super) fn oci_lookup<'a>(state: &'a Arc<AppState>, name: &str) -> &'a Index {
    state
        .indexes
        .iter()
        .find(|index| index.name == name || index.route == name)
        .expect("mirror driver selected an OCI index that exists")
}

/// Write a mirror report row in the shared prefetch TSV shape (repo maps to `project`, the tag or
/// digest to `filename`).
fn write_mirror_row(out: &mut Output, row: &peryx_ecosystem_oci::MirrorRow) -> std::io::Result<()> {
    writeln!(
        out,
        "{}\t{}\t{}\t{}\t{}\t\t{}\t{}\t{}",
        row.kind, row.repo, row.reference, row.reference, row.digest, row.bytes, row.status, row.reason
    )
}

/// Mirror an OCI index's `--image` references into the store, or verify they are already present.
pub(super) async fn oci_mirror(
    state: &Arc<AppState>,
    index: &Index,
    images: &[String],
    mode: peryx_ecosystem_oci::MirrorMode,
    out: &mut Output,
) -> anyhow::Result<()> {
    if images.is_empty() {
        bail!("mirroring an OCI index needs at least one image (--image or [index.prefetch] packages)");
    }
    let rows = peryx_ecosystem_oci::mirror(state, index, images, mode).await?;
    out.write_all(HEADER.as_bytes())?;
    let mut errors = 0_u64;
    for row in &rows {
        if row.status == "error" {
            errors += 1;
        }
        write_mirror_row(out, row)?;
    }
    if errors > 0 {
        bail!("mirror found {errors} error(s)");
    }
    Ok(())
}

/// List the `--image` references an OCI index would mirror, without touching the network.
pub(super) fn oci_plan(index: &Index, images: &[String], out: &mut Output) -> anyhow::Result<()> {
    if images.is_empty() {
        bail!("mirroring an OCI index needs at least one image (--image or [index.prefetch] packages)");
    }
    out.write_all(HEADER.as_bytes())?;
    for image in images {
        writeln!(out, "manifest\t{}\t{image}\t{image}\t\t\t0\tselected\t", index.name)?;
    }
    write_count(out, &index.name, "images", images.len() as u64)?;
    Ok(())
}
