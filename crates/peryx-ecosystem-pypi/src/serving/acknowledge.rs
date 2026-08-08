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
#[path = "../../tests/unit/serving/acknowledge/tests.rs"]
mod tests;
