use axum::http::StatusCode;
use peryx_ha::DcAck;

pub(super) struct AckResponse {
    pub status: StatusCode,
    pub body: Vec<u8>,
    pub finalize: bool,
}

pub(super) fn ack_response(ack: DcAck, operation: &str) -> AckResponse {
    match ack {
        DcAck::Durable { .. } => AckResponse {
            status: StatusCode::OK,
            body: b"upload accepted".to_vec(),
            finalize: true,
        },
        DcAck::Unknown => AckResponse {
            status: StatusCode::ACCEPTED,
            body: format!("upload accepted; durability pending, retry-safe operation {operation}").into_bytes(),
            finalize: false,
        },
        DcAck::Pending => AckResponse {
            status: StatusCode::ACCEPTED,
            body: b"upload accepted; durability pending".to_vec(),
            finalize: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use peryx_storage::blob::BlobDurability;

    use super::*;

    #[test]
    fn test_durable_response_is_final() {
        let response = ack_response(
            DcAck::Durable {
                scope: BlobDurability::Filesystem,
            },
            "op-1",
        );
        assert_eq!(
            (response.status, response.body, response.finalize),
            (StatusCode::OK, b"upload accepted".to_vec(), true)
        );
    }

    #[test]
    fn test_unknown_response_carries_the_operation() {
        let response = ack_response(DcAck::Unknown, "op-1");
        assert_eq!(response.status, StatusCode::ACCEPTED);
        assert!(String::from_utf8_lossy(&response.body).contains("op-1"));
        assert!(!response.finalize);
    }

    #[test]
    fn test_pending_response_is_not_final() {
        let response = ack_response(DcAck::Pending, "op-1");
        assert_eq!(
            (response.status, response.body, response.finalize),
            (
                StatusCode::ACCEPTED,
                b"upload accepted; durability pending".to_vec(),
                false
            )
        );
    }
}
