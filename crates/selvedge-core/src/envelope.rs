//! The `/client/v4` response envelope.

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub code: u32,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct Envelope {
    pub success: bool,
    pub errors: Vec<ApiError>,
    pub messages: Vec<Value>,
    pub result: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_info: Option<Value>,
}

impl Envelope {
    pub fn ok(result: Value) -> Self {
        Self { success: true, errors: vec![], messages: vec![], result, result_info: None }
    }

    pub fn ok_paged(result: Value, result_info: Value) -> Self {
        Self { result_info: Some(result_info), ..Self::ok(result) }
    }

    pub fn error(code: u32, message: impl Into<String>) -> Self {
        Self {
            success: false,
            errors: vec![ApiError { code, message: message.into() }],
            messages: vec![],
            result: Value::Null,
            result_info: None,
        }
    }
}
