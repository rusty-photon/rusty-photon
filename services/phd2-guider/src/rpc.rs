//! JSON RPC 2.0 types for PHD2 communication

use serde::{Deserialize, Serialize};

use crate::events::Phd2Event;

/// JSON RPC 2.0 request
#[derive(Debug, Clone, Serialize)]
pub struct RpcRequest {
    pub jsonrpc: &'static str,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    pub id: u64,
}

impl RpcRequest {
    pub fn new(method: impl Into<String>, params: Option<serde_json::Value>, id: u64) -> Self {
        Self {
            jsonrpc: "2.0",
            method: method.into(),
            params,
            id,
        }
    }
}

/// JSON RPC 2.0 response
///
/// Deserialized by hand rather than derived, for one PHD2 defect: with
/// its camera disconnected, PHD2 2.6.14 answers `capture_single_frame`
/// with **two** `error` members in one object (its handler appends a
/// second error without returning after the first). A derived
/// deserializer rejects the duplicate, the reader then drops the whole
/// line, and the request waits out its command timeout. This one keeps
/// the first `error`, which carries PHD2's reason, and skips any
/// repeat. Unknown members are skipped; a missing `id` is still an
/// error, which is how the reader tells an event from a reply.
#[derive(Debug, Clone)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub result: Option<serde_json::Value>,
    pub error: Option<RpcErrorObject>,
    pub id: u64,
}

impl<'de> Deserialize<'de> for RpcResponse {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(ResponseVisitor)
    }
}

struct ResponseVisitor;

impl<'de> serde::de::Visitor<'de> for ResponseVisitor {
    type Value = RpcResponse;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON-RPC response object")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<RpcResponse, A::Error> {
        let mut jsonrpc = None;
        let mut result = None;
        let mut error = None;
        let mut id = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "jsonrpc" if jsonrpc.is_none() => jsonrpc = Some(map.next_value()?),
                "result" if result.is_none() => result = map.next_value()?,
                // The first error is PHD2's reason; a repeat is the
                // defect described on `RpcResponse`, and is skipped.
                "error" if error.is_none() => error = map.next_value()?,
                "id" if id.is_none() => id = Some(map.next_value()?),
                _ => {
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
            }
        }
        Ok(RpcResponse {
            jsonrpc: jsonrpc.ok_or_else(|| serde::de::Error::missing_field("jsonrpc"))?,
            result,
            error,
            id: id.ok_or_else(|| serde::de::Error::missing_field("id"))?,
        })
    }
}

/// JSON RPC 2.0 error object
#[derive(Debug, Clone, Deserialize)]
pub struct RpcErrorObject {
    pub code: i32,
    pub message: String,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

/// Incoming message from PHD2 - either an event or a response
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Phd2Message {
    Response(RpcResponse),
    Event(Phd2Event),
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn test_rpc_request_serialization() {
        let request = RpcRequest::new("get_app_state", None, 1);
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"method\":\"get_app_state\""));
        assert!(json.contains("\"id\":1"));
        assert!(!json.contains("params"));
    }

    #[test]
    fn test_rpc_request_with_params() {
        let request = RpcRequest::new("set_connected", Some(serde_json::json!(true)), 2);
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"params\":true"));
    }

    #[test]
    fn test_rpc_response_parsing() {
        let json = r#"{"jsonrpc":"2.0","result":"Guiding","id":1}"#;
        let response: RpcResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.id, 1);
        assert_eq!(response.result.unwrap().as_str().unwrap(), "Guiding");
        assert!(response.error.is_none());
    }

    #[test]
    fn a_reply_with_two_error_members_keeps_the_first() {
        // PHD2 2.6.14's answer to capture_single_frame with its camera
        // disconnected, as read off the wire on pier1.
        let json = r#"{"jsonrpc":"2.0","error":{"code":1,"message":"cannot capture single frame when camera is not connected"},"error":{"code":2,"message":"failed to start exposure"},"id":3}"#;
        let response: RpcResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.id, 3);
        let error = response.error.unwrap();
        assert_eq!(error.code, 1);
        assert_eq!(
            error.message,
            "cannot capture single frame when camera is not connected"
        );
    }

    #[test]
    fn an_event_line_is_not_a_reply() {
        let json = r#"{"Event":"AppState","State":"Stopped"}"#;
        let err = serde_json::from_str::<RpcResponse>(json).unwrap_err();
        assert!(err.to_string().contains("missing field"), "{err}");
    }

    #[test]
    fn a_null_result_reads_as_no_result() {
        let json = r#"{"jsonrpc":"2.0","result":null,"id":7}"#;
        let response: RpcResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.id, 7);
        assert!(response.result.is_none());
        assert!(response.error.is_none());
    }

    #[test]
    fn test_rpc_error_response_parsing() {
        let json =
            r#"{"jsonrpc":"2.0","error":{"code":-32600,"message":"Invalid request"},"id":1}"#;
        let response: RpcResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.id, 1);
        assert!(response.result.is_none());
        let error = response.error.unwrap();
        assert_eq!(error.code, -32600);
        assert_eq!(error.message, "Invalid request");
    }
}
