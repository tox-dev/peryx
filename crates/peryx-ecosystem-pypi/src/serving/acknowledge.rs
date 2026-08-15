use axum::http::StatusCode;
use peryx_ha::WriteDurability;

pub(super) struct AckResponse {
    pub status: StatusCode,
    pub body: Vec<u8>,
    pub finalize: bool,
}

pub(super) fn ack_response(ack: WriteDurability, operation: &str) -> AckResponse {
    match ack {
        WriteDurability::Confirmed { .. } => AckResponse {
            status: StatusCode::OK,
            body: b"upload accepted".to_vec(),
            finalize: true,
        },
        WriteDurability::Unavailable => AckResponse {
            status: StatusCode::ACCEPTED,
            body: format!("upload accepted; durability pending, retry-safe operation {operation}").into_bytes(),
            finalize: false,
        },
        WriteDurability::Pending => AckResponse {
            status: StatusCode::ACCEPTED,
            body: b"upload accepted; durability pending".to_vec(),
            finalize: false,
        },
    }
}

#[cfg(test)]
#[path = "../../tests/unit/serving/acknowledge/tests.rs"]
mod tests;
