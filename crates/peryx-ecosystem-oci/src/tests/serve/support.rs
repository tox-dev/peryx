//! The harness the OCI serving tests build on: a virtual stack over temporary stores and a mock
//! upstream registry, plus the request and manifest helpers.

//! Proxy-pull and cached-serve paths, driven through the router with a wiremock upstream.

pub(super) use axum::http::{Method, StatusCode, header};
pub(super) use rstest::rstest;
pub(super) use wiremock::matchers::{header as match_header, method, path};
pub(super) use wiremock::{Mock, MockServer, ResponseTemplate};

pub(super) use crate::store::{self, Manifest};
pub(super) use crate::tests::{
    app_with_indexes, body_has_code, hosted, oci_digest, oci_index, proxy, proxy_with_auth, proxy_with_settings, send,
    send_with,
};

pub(super) const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
