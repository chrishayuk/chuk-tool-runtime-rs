//! Length-prefixed JSON framing for the host↔guest tool-broker channel.
//!
//! Wire format: a 4-byte big-endian length prefix followed by that many bytes of
//! UTF-8 JSON. JSON (never a native deserializer on guest-controlled bytes) is
//! used deliberately — the guest is untrusted. This matches
//! `chuk-tool-processor`'s `_wire.py` so the same guest bootstrap can drive it.

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Hard cap on a single frame so a malicious guest cannot force a huge allocation.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

// Envelope / frame keys.
pub const KEY_ID: &str = "id";
pub const KEY_METHOD: &str = "method";
pub const KEY_PARAMS: &str = "params";
pub const KEY_TOKEN: &str = "token";
pub const KEY_OK: &str = "ok";
pub const KEY_ERROR: &str = "error";
pub const KEY_VALUE: &str = "value";

// call_tool parameter keys.
pub const KEY_NAME: &str = "name";
pub const KEY_NAMESPACE: &str = "namespace";
pub const KEY_ARGUMENTS: &str = "arguments";

// RPC method names.
pub const METHOD_HELLO: &str = "hello";
pub const METHOD_LIST_TOOLS: &str = "list_tools";
pub const METHOD_CALL_TOOL: &str = "call_tool";
pub const METHOD_RESULT: &str = "result";

fn invalid(msg: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.into())
}

/// Send one framed JSON message.
pub async fn send<W: AsyncWrite + Unpin>(w: &mut W, obj: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(obj)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(invalid(format!("message too large: {}", body.len())));
    }
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Read one framed JSON message. Returns an `UnexpectedEof` error when the peer
/// closes cleanly at a frame boundary.
pub async fn recv<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Value> {
    let mut header = [0u8; 4];
    r.read_exact(&mut header).await?;
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(invalid(format!("declared frame length {len} exceeds cap")));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    let value: Value = serde_json::from_slice(&body)?;
    if !value.is_object() {
        return Err(invalid("wire message must be a JSON object"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn round_trips_a_message() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        send(&mut a, &json!({"method": "hello", "id": 1})).await.unwrap();
        assert_eq!(recv(&mut b).await.unwrap(), json!({"method": "hello", "id": 1}));
    }

    #[tokio::test]
    async fn recv_rejects_an_oversized_declared_frame() {
        let (mut a, mut b) = tokio::io::duplex(16);
        a.write_all(&(MAX_FRAME_BYTES as u32 + 1).to_be_bytes()).await.unwrap();
        a.flush().await.unwrap();
        let err = recv(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn recv_rejects_a_non_object_message() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let body = b"[1,2,3]";
        a.write_all(&(body.len() as u32).to_be_bytes()).await.unwrap();
        a.write_all(body).await.unwrap();
        a.flush().await.unwrap();
        assert!(recv(&mut b).await.unwrap_err().to_string().contains("JSON object"));
    }
}
