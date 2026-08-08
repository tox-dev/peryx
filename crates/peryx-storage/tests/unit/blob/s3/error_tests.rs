use super::{BlobError, S3Error};
use crate::blob::BlobErrorKind;

#[test]
fn test_blob_error_from_s3_error() {
    assert_eq!(BlobError::from(S3Error::NotFound).kind(), BlobErrorKind::Io);
    assert_eq!(
        BlobError::from(S3Error::Request("reset".to_owned())).kind(),
        BlobErrorKind::Io
    );
}
