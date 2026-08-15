use crate::name::{OciRoute, Reference, classify, parse_reference};
use crate::web::manifest_content_from_bytes;

#[test]
fn manifest_route_preserves_generated_tag() {
    for seed in 0..512 {
        let repository = format!(
            "{}/{}",
            generated(seed, b"abcdefghijklmnopqrstuvwxyz0123456789", 12),
            generated(seed.wrapping_mul(13), b"abcdefghijklmnopqrstuvwxyz0123456789", 12),
        );
        let tag = format!(
            "{}{}",
            generated(
                seed,
                b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_",
                1
            ),
            generated(
                seed.wrapping_mul(29),
                b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-",
                127,
            ),
        );
        assert_eq!(
            classify(&format!("/v2/{repository}/manifests/{tag}")),
            Some(OciRoute::Manifest {
                name: repository,
                reference: Reference::Tag(tag),
            }),
            "seed {seed}",
        );
    }
}

#[test]
fn reference_preserves_generated_digest() {
    for seed in 0..512 {
        let algorithm = format!("a{}", generated(seed, b"abcdefghijklmnopqrstuvwxyz0123456789+._-", 11));
        let encoded = generated(seed.wrapping_mul(37), b"abcdefghijklmnopqrstuvwxyz0123456789=_-", 64);
        let digest = format!("{algorithm}:{encoded}");
        assert_eq!(parse_reference(&digest), Some(Reference::Digest(digest)), "seed {seed}");
    }
}

#[test]
fn image_manifest_preserves_generated_descriptors() {
    for seed in 0_u32..128 {
        let config_digest = digest(seed);
        let config_size = u64::from(seed.wrapping_mul(101));
        let layers = (0..seed % 8)
            .map(|offset| {
                (
                    digest(seed.wrapping_add(offset)),
                    u64::from(seed.wrapping_mul(43).wrapping_add(offset)),
                    offset % 2 == 0,
                )
            })
            .collect::<Vec<_>>();
        let value = serde_json::json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "digest": config_digest,
                "size": config_size,
                "mediaType": "application/vnd.oci.image.config.v1+json",
            },
            "layers": layers.iter().map(|(digest, size, tar)| serde_json::json!({
                "digest": digest,
                "size": size,
                "mediaType": if *tar { "application/vnd.oci.image.layer.v1.tar" } else { "application/octet-stream" },
            })).collect::<Vec<_>>(),
        });
        let parsed = manifest_content_from_bytes(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            (
                parsed.media_type,
                parsed.is_index,
                parsed
                    .config
                    .map(|config| (config.digest, config.size, config.media_type)),
                parsed
                    .entries
                    .into_iter()
                    .map(|entry| { (entry.digest, entry.size, entry.media_type, entry.browsable) })
                    .collect::<Vec<_>>(),
                parsed.total_size,
            ),
            (
                "application/vnd.oci.image.manifest.v1+json".to_owned(),
                false,
                Some((
                    config_digest,
                    config_size,
                    "application/vnd.oci.image.config.v1+json".to_owned(),
                )),
                layers
                    .iter()
                    .map(|(digest, size, tar)| {
                        (
                            digest.clone(),
                            *size,
                            if *tar {
                                "application/vnd.oci.image.layer.v1.tar".to_owned()
                            } else {
                                "application/octet-stream".to_owned()
                            },
                            *tar,
                        )
                    })
                    .collect::<Vec<_>>(),
                config_size + layers.iter().map(|(_, size, _)| size).sum::<u64>(),
            ),
            "seed {seed}",
        );
    }
}

#[test]
fn image_index_preserves_generated_platforms() {
    for seed in 0_u32..128 {
        let children = (0..seed % 8)
            .map(|offset| {
                (
                    digest(seed.wrapping_add(offset)),
                    u64::from(seed.wrapping_mul(71).wrapping_add(offset)),
                    generated(seed.wrapping_add(offset), b"abcdefghijklmnopqrstuvwxyz", 8),
                    generated(seed.wrapping_add(offset), b"abcdefghijklmnopqrstuvwxyz0123456789_", 8),
                )
            })
            .collect::<Vec<_>>();
        let value = serde_json::json!({
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": children.iter().map(|(digest, size, os, architecture)| serde_json::json!({
                "digest": digest,
                "size": size,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "platform": {"os": os, "architecture": architecture},
            })).collect::<Vec<_>>(),
        });
        let parsed = manifest_content_from_bytes(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            (
                parsed.media_type,
                parsed.is_index,
                parsed.config,
                parsed
                    .entries
                    .into_iter()
                    .map(|entry| { (entry.digest, entry.size, entry.media_type, entry.platform) })
                    .collect::<Vec<_>>(),
                parsed.total_size,
            ),
            (
                "application/vnd.oci.image.index.v1+json".to_owned(),
                true,
                None,
                children
                    .iter()
                    .map(|(digest, size, os, architecture)| {
                        (
                            digest.clone(),
                            *size,
                            "application/vnd.oci.image.manifest.v1+json".to_owned(),
                            Some(format!("{os}/{architecture}")),
                        )
                    })
                    .collect::<Vec<_>>(),
                children.iter().map(|(_, size, _, _)| size).sum::<u64>(),
            ),
            "seed {seed}",
        );
    }
}

fn digest(seed: u32) -> String {
    format!(
        "sha256:{}",
        generated(seed.wrapping_mul(47), b"abcdef0123456789", 64)
            .repeat(64)
            .chars()
            .take(64)
            .collect::<String>()
    )
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
