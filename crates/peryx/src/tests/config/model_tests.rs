use std::path::PathBuf;

use peryx_http::rate_limit::RateLimitConfig;

use crate::config::{Config, IndexKind, LogConfig};

#[test]
fn test_default_config() {
    let c = Config::default();
    assert_eq!(c.host, "127.0.0.1");
    assert_eq!(c.port, 4433);
    assert_eq!(c.data_dir, PathBuf::from("peryx-data"));
    assert!(!c.offline);
    assert_eq!(c.cache_ttl_secs, 300);
    assert_eq!(c.log, LogConfig::default());
    assert_eq!(c.rate_limit, RateLimitConfig::default());
    // One trio per ecosystem: a cache and a hosted store behind a virtual index, for PyPI and OCI.
    let routes: Vec<&str> = c.indexes.iter().map(|index| index.route.as_str()).collect();
    assert_eq!(
        routes,
        ["pypi", "hosted", "root/pypi", "dockerhub", "images", "root/oci"]
    );
    assert!(matches!(&c.indexes[0].kind, IndexKind::Cached { .. }));
    assert!(matches!(&c.indexes[1].kind, IndexKind::Hosted { .. }));
    assert!(matches!(&c.indexes[2].kind, IndexKind::Virtual { upload: Some(target), .. } if target == "hosted"));
    assert_eq!(c.indexes[3].ecosystem, peryx_core::Ecosystem::Oci);
    assert!(matches!(&c.indexes[3].kind, IndexKind::Cached { .. }));
    assert!(matches!(&c.indexes[4].kind, IndexKind::Hosted { .. }));
    assert!(matches!(&c.indexes[5].kind, IndexKind::Virtual { upload: Some(target), .. } if target == "images"));
}
