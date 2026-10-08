//! Reject wire omissions before Smithy replaces them with fabricated zeroes.
use super::transport::WIRE_LIMIT;
use serde_json::Value;
fn invalid() -> std::io::Error {
    std::io::Error::other("Invalid provider usage envelope")
}
fn usage(value: &Value) -> std::io::Result<()> {
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
pub(super) fn complete(bytes: &[u8]) -> std::io::Result<()> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    usage(&value["usage"])
}
#[derive(Default)]
pub(super) struct StreamGuard {
    buffer: Vec<u8>,
}
impl StreamGuard {
    pub(super) fn push(&mut self, bytes: &[u8]) -> std::io::Result<Vec<Vec<u8>>> {
        if bytes.len() > WIRE_LIMIT - self.buffer.len() {
            return Err(invalid());
        }
        self.buffer.extend_from_slice(bytes);
        let mut consumed = 0;
        let mut frames = Vec::new();
        while self.buffer.len() - consumed >= 12 {
            let n = u32::from_be_bytes(
                self.buffer[consumed..consumed + 4]
                    .try_into()
                    .map_err(|_| invalid())?,
            ) as usize;
            if !(16..=WIRE_LIMIT).contains(&n) {
                return Err(invalid());
            }
            if self.buffer.len() - consumed < n {
                break;
            }
            let bytes = &self.buffer[consumed..consumed + n];
            let message =
                aws_smithy_eventstream::frame::read_message_from(bytes).map_err(|_| invalid())?;
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
        Ok(frames)
    }
    pub(super) fn end(&self) -> std::io::Result<()> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(invalid())
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
