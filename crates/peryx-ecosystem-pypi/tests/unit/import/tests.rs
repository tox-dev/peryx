use super::upload_error_reason;
use crate::upload::UploadError;

#[test]
fn test_upload_error_reason_formats_metadata_field_and_fallback() {
    assert_eq!(
        upload_error_reason(&UploadError::MetadataFieldMismatch {
            field: "Project-URL",
            metadata: "Homepage, https://example.test".to_owned(),
            form: "Source, https://example.test/src".to_owned(),
        }),
        "metadata field Project-URL is \"Homepage, https://example.test\", expected \"Source, https://example.test/src\""
    );
    assert_eq!(upload_error_reason(&UploadError::NotFileUpload), "NotFileUpload");
}
