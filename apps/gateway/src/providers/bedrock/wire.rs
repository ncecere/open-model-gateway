//! Reject wire omissions before Smithy replaces them with fabricated zeroes.
use super::transport::WIRE_LIMIT;
use crate::inference::error::InferenceError;
use serde_json::Value;

/// Why a response body failed, carried as the body's error so the stream
/// decoder (`stream::receive_error`) can tell a transport failure from a
/// protocol violation: the SDK wraps whatever the body yields in a dispatch
/// failure, and the class is read back by walking its sources.
///
/// The line is where the failure arises. The HTTP body and the AWS
/// event-stream framing are the transport; a frame that arrived whole and
/// decodes but whose content is wrong is the provider's protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Failure {
    /// The connection or the body failed: reset, read error, cut stream.
    Body,
    /// The transport's read or total timeout elapsed mid-body.
    Timeout,
    /// The body ended inside an event-stream frame.
    Truncated,
    /// A frame the event-stream decoder cannot decode: an impossible length,
    /// a failed CRC, malformed headers.
    Corrupt,
    /// A complete, decodable frame with wrong content, or a bound of the
    /// gateway exceeded (the wire limit): the provider's output is invalid.
    Invalid,
}
impl Failure {
    /// The class a returned stream ends with.
    pub(super) fn class(self) -> InferenceError {
        match self {
            Self::Body | Self::Truncated | Self::Corrupt => InferenceError::UpstreamUnavailable,
            Self::Timeout => InferenceError::Timeout,
            Self::Invalid => InferenceError::InvalidUpstream,
        }
    }
    /// A failed read of the provider's body.
    pub(super) fn read(error: &reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::Timeout
        } else {
            Self::Body
        }
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Constant text: nothing from the provider's response.
        f.write_str(match self {
            Self::Body => "Provider body failed",
            Self::Timeout => "Provider body timed out",
            Self::Truncated => "Provider stream ended inside a frame",
            Self::Corrupt => "Provider frame is corrupt",
            Self::Invalid => "Invalid provider response",
        })
    }
}
impl std::error::Error for Failure {}

fn invalid() -> Failure {
    Failure::Invalid
}
fn usage(value: &Value) -> Result<(), Failure> {
    if !value.is_object() {
        return Err(invalid());
    }
    for key in ["inputTokens", "outputTokens", "totalTokens"] {
        if !value[key].as_u64().is_some_and(|n| n <= i32::MAX as u64) {
            return Err(invalid());
        }
    }
    super::super::metering::bedrock(value).map_err(|_| invalid())?;
    Ok(())
}
pub(super) fn complete(bytes: &[u8]) -> Result<(), Failure> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    usage(&value["usage"])
}
#[derive(Default)]
pub(super) struct StreamGuard {
    buffer: Vec<u8>,
}
impl StreamGuard {
    /// The whole frames in `bytes` and what is left of the buffer; a failure
    /// comes with the whole frames that preceded it, which are valid and stay
    /// delivered.
    pub(super) fn push(&mut self, bytes: &[u8]) -> (Vec<Vec<u8>>, Option<Failure>) {
        let mut frames = Vec::new();
        let failure = self.scan(bytes, &mut frames).err();
        (frames, failure)
    }
    fn scan(&mut self, bytes: &[u8], frames: &mut Vec<Vec<u8>>) -> Result<(), Failure> {
        if bytes.len() > WIRE_LIMIT - self.buffer.len() {
            return Err(invalid());
        }
        self.buffer.extend_from_slice(bytes);
        let mut consumed = 0;
        while self.buffer.len() - consumed >= 12 {
            let n = u32::from_be_bytes(
                self.buffer[consumed..consumed + 4]
                    .try_into()
                    .map_err(|_| invalid())?,
            ) as usize;
            // Below the smallest frame no decoder can read it; above the wire
            // limit the gateway refuses it.
            if n < 16 {
                return Err(Failure::Corrupt);
            }
            if n > WIRE_LIMIT {
                return Err(invalid());
            }
            if self.buffer.len() - consumed < n {
                break;
            }
            let bytes = &self.buffer[consumed..consumed + n];
            let message = aws_smithy_eventstream::frame::read_message_from(bytes)
                .map_err(|_| Failure::Corrupt)?;
            if message
                .headers()
                .iter()
                .filter(|h| h.name().as_str() == ":event-type")
                .count()
                > 1
            {
                return Err(invalid());
            }
            let event_type = message
                .headers()
                .iter()
                .find(|h| h.name().as_str() == ":event-type")
                .map(|h| h.value().as_string())
                .transpose()
                .map_err(|_| invalid())?;
            if event_type.is_some_and(|t| t.as_str() == "metadata") {
                let value: Value =
                    serde_json::from_slice(message.payload()).map_err(|_| invalid())?;
                usage(&value["usage"])?;
            }
            frames.push(bytes.to_vec());
            consumed += n;
        }
        self.buffer.drain(..consumed);
        Ok(())
    }
    /// The body ended: nothing may be left of a frame.
    pub(super) fn end(&self) -> Result<(), Failure> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(Failure::Truncated)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn omissions_and_nulls_rejected_before_sdk_defaults() {
        for u in [
            json!(null),
            json!({}),
            json!({"inputTokens":0,"outputTokens":0}),
            json!({"inputTokens":null,"outputTokens":0,"totalTokens":0}),
            json!({"inputTokens":-1,"outputTokens":0,"totalTokens":0}),
            json!({"inputTokens":0,"outputTokens":0,"totalTokens":0,"cacheDetails":[{"ttl":"5m"}]} ),
        ] {
            assert!(complete(&json!({"usage":u}).to_string().into_bytes()).is_err());
        }
        assert!(
            complete(br#"{"usage":{"inputTokens":0,"outputTokens":0,"totalTokens":0}}"#).is_ok()
        );
    }
}
