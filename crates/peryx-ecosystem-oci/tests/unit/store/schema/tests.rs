use rstest::rstest;

use super::*;

const IMAGE_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const INDEX_TYPE: &str = "application/vnd.oci.image.index.v1+json";
const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn image(fields: &str) -> String {
    format!(r#"{{"schemaVersion":2,"mediaType":"{IMAGE_TYPE}",{fields}}}"#)
}

fn index(fields: &str) -> String {
    format!(r#"{{"schemaVersion":2,"mediaType":"{INDEX_TYPE}",{fields}}}"#)
}

fn layer(fields: &str) -> String {
    format!(r#"{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{DIGEST}",{fields}}}"#)
}

fn config() -> String {
    format!(r#""config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{DIGEST}","size":7}}"#)
}

#[rstest]
#[case::oci_image(IMAGE_TYPE, Some(ManifestSchema::Image))]
#[case::oci_index(INDEX_TYPE, Some(ManifestSchema::Index))]
#[case::docker_image("application/vnd.docker.distribution.manifest.v2+json", Some(ManifestSchema::Image))]
#[case::docker_list(
    "application/vnd.docker.distribution.manifest.list.v2+json",
    Some(ManifestSchema::Index)
)]
#[case::schema_one("application/vnd.docker.distribution.manifest.v1+json", None)]
#[case::plain_json("application/json", None)]
fn test_schema_of_media_type(#[case] media_type: &str, #[case] expected: Option<ManifestSchema>) {
    assert_eq!(ManifestSchema::of(media_type), expected);
}

#[rstest]
#[case::minimal(image(&format!(r#"{}, "layers":[]"#, config())))]
#[case::layers(image(&format!(r#"{},"layers":[{}]"#, config(), layer(r#""size":0"#))))]
#[case::foreign_layer(
    image(&format!(r#"{},"layers":[{}]"#, config(), layer(r#""size":9,"urls":["https://example.invalid/l"]"#)))
)]
#[case::subject(
    image(&format!(r#"{},"layers":[],"subject":{{"mediaType":"{IMAGE_TYPE}","digest":"{DIGEST}","size":3}}"#, config()))
)]
#[case::extension_fields(
    image(&format!(r#"{},"layers":[],"artifactType":"application/vnd.example","annotations":{{"a":"b"}}"#, config()))
)]
fn test_valid_image_manifests_are_accepted(#[case] body: String) {
    ManifestSchema::Image.validate(IMAGE_TYPE, body.as_bytes()).unwrap();
}

#[rstest]
#[case::empty(index(r#""manifests":[]"#))]
#[case::child(
    index(&format!(r#""manifests":[{{"mediaType":"{IMAGE_TYPE}","digest":"{DIGEST}","size":11,"platform":{{"os":"linux","architecture":"amd64"}}}}]"#))
)]
fn test_valid_indexes_are_accepted(#[case] body: String) {
    ManifestSchema::Index.validate(INDEX_TYPE, body.as_bytes()).unwrap();
}

#[rstest]
#[case::truncated("{", "manifest body is not JSON: EOF while parsing an object at line 1 column 1")]
#[case::array("[]", "manifest body is not a JSON object")]
#[case::number("2", "manifest body is not a JSON object")]
#[case::empty_object("{}", "manifest schemaVersion must be 2")]
#[case::schema_version_one(r#"{"schemaVersion":1}"#, "manifest schemaVersion must be 2")]
#[case::schema_version_string(r#"{"schemaVersion":"2"}"#, "manifest schemaVersion must be 2")]
fn test_root_shape_is_rejected(#[case] body: &str, #[case] message: &str) {
    let fault = ManifestSchema::Image.validate(IMAGE_TYPE, body.as_bytes()).unwrap_err();

    assert_eq!(fault.to_string(), message);
}

#[rstest]
#[case::absent(r#"{"schemaVersion":2}"#)]
#[case::other_type(r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json"}"#)]
#[case::not_a_string(r#"{"schemaVersion":2,"mediaType":2}"#)]
fn test_declared_media_type_must_match_the_body(#[case] body: &str) {
    let fault = ManifestSchema::Image.validate(IMAGE_TYPE, body.as_bytes()).unwrap_err();

    assert_eq!(fault.to_string(), format!("manifest mediaType must be {IMAGE_TYPE}"));
}

#[rstest]
#[case::no_config(ManifestSchema::Image, IMAGE_TYPE, image(r#""layers":[]"#), "config")]
#[case::no_layers(ManifestSchema::Image, IMAGE_TYPE, image(&config()), "layers")]
#[case::no_manifests(ManifestSchema::Index, INDEX_TYPE, index(r#""x":1"#), "manifests")]
fn test_required_document_fields(
    #[case] schema: ManifestSchema,
    #[case] declared: &str,
    #[case] body: String,
    #[case] field: &str,
) {
    let fault = schema.validate(declared, body.as_bytes()).unwrap_err();

    assert_eq!(
        fault.to_string(),
        format!("manifest is missing the required {field} field")
    );
}

#[rstest]
#[case::layers(
    ManifestSchema::Image,
    IMAGE_TYPE,
    image(&format!(r#"{},"layers":{{}}"#, config())),
    "layers"
)]
#[case::manifests(ManifestSchema::Index, INDEX_TYPE, index(r#""manifests":"none""#), "manifests")]
fn test_descriptor_lists_must_be_arrays(
    #[case] schema: ManifestSchema,
    #[case] declared: &str,
    #[case] body: String,
    #[case] field: &str,
) {
    let fault = schema.validate(declared, body.as_bytes()).unwrap_err();

    assert_eq!(
        fault.to_string(),
        format!("manifest {field} must be an array of descriptors")
    );
}

#[rstest]
#[case::not_an_object(r#""config":"sha256:x""#, "config", "a JSON object")]
#[case::no_media_type(r#""config":{"digest":"sha256:x","size":1}"#, "config", "a mediaType string")]
#[case::empty_media_type(
    r#""config":{"mediaType":"","digest":"sha256:x","size":1}"#,
    "config",
    "a mediaType string"
)]
#[case::no_digest(r#""config":{"mediaType":"application/json","size":1}"#, "config", "a digest string")]
#[case::digest_not_a_string(
    r#""config":{"mediaType":"application/json","digest":7,"size":1}"#,
    "config",
    "a digest string"
)]
#[case::empty_digest(
    r#""config":{"mediaType":"application/json","digest":"","size":1}"#,
    "config",
    "a digest string"
)]
#[case::no_size(
    r#""config":{"mediaType":"application/json","digest":"sha256:x"}"#,
    "config",
    "a non-negative integer size"
)]
#[case::negative_size(
    r#""config":{"mediaType":"application/json","digest":"sha256:x","size":-1}"#,
    "config",
    "a non-negative integer size"
)]
#[case::fractional_size(
    r#""config":{"mediaType":"application/json","digest":"sha256:x","size":1.5}"#,
    "config",
    "a non-negative integer size"
)]
fn test_config_descriptor_faults(#[case] fields: &str, #[case] location: &str, #[case] requirement: &str) {
    let body = image(&format!(r#"{fields},"layers":[]"#));

    let fault = ManifestSchema::Image.validate(IMAGE_TYPE, body.as_bytes()).unwrap_err();

    assert_eq!(
        fault.to_string(),
        format!("the {location} descriptor requires {requirement}")
    );
}

#[test]
fn test_a_faulty_layer_names_its_position() {
    let body = image(&format!(
        r#"{},"layers":[{},{}]"#,
        config(),
        layer(r#""size":1"#),
        layer(r#""size":-4"#)
    ));

    let fault = ManifestSchema::Image.validate(IMAGE_TYPE, body.as_bytes()).unwrap_err();

    assert_eq!(
        fault.to_string(),
        "the layers[1] descriptor requires a non-negative integer size"
    );
}

#[test]
fn test_a_faulty_index_child_names_its_position() {
    let body = index(r#""manifests":[{"mediaType":"application/json"}]"#);

    let fault = ManifestSchema::Index.validate(INDEX_TYPE, body.as_bytes()).unwrap_err();

    assert_eq!(
        fault.to_string(),
        "the manifests[0] descriptor requires a digest string"
    );
}

#[test]
fn test_a_present_subject_is_validated() {
    let body = image(&format!(r#"{},"layers":[],"subject":{{}}"#, config()));

    let fault = ManifestSchema::Image.validate(IMAGE_TYPE, body.as_bytes()).unwrap_err();

    assert_eq!(fault.to_string(), "the subject descriptor requires a mediaType string");
}
