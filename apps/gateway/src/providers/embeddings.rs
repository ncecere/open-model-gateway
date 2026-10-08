//! Bounded string-only embedding wire subset shared by certified profiles.
use crate::inference::{
    error::InferenceError,
    types::{EmbeddingRequest, EmbeddingResponse},
};
use serde_json::{Value, json};
type Result<T> = std::result::Result<T, InferenceError>;
pub(crate) const MAX_BATCH: usize = 128;
pub(crate) const MAX_INPUT_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_DIMENSIONS: u32 = 16384;
pub(crate) fn validate(request: &EmbeddingRequest) -> Result<()> {
    if request.model.trim().is_empty()
        || request.model.len() > 200
        || request.input.is_empty()
        || request.input.len() > MAX_BATCH
        || request.input.iter().any(|s| s.is_empty())
        || request
            .dimensions
            .is_some_and(|n| n == 0 || n > MAX_DIMENSIONS)
    {
        return Err(InferenceError::InvalidRequest);
    }
    let size = request
        .input
        .iter()
        .try_fold(0usize, |n, s| n.checked_add(s.len()))
        .ok_or(InferenceError::InvalidRequest)?;
    if size > MAX_INPUT_BYTES {
        return Err(InferenceError::InvalidRequest);
    }
    Ok(())
}
pub(crate) fn encode(model: &str, request: &EmbeddingRequest) -> Value {
    let mut value = json!({"model":model,"input":request.input,"encoding_format":"float"});
    if let Some(n) = request.dimensions {
        value["dimensions"] = n.into();
    }
    value
}
pub(crate) fn encode_ollama(model: &str, request: &EmbeddingRequest) -> Result<Value> {
    // No compatible embedding endpoint: it cannot express truncate:false.
    // Dimensions are deliberately not certified by the Ollama adapter.
    validate(request)?;
    if request.dimensions.is_some() {
        return Err(InferenceError::Unsupported);
    }
    Ok(json!({"model":model,"input":request.input,"truncate":false}))
}
fn vector(
    value: &Value,
    request: &EmbeddingRequest,
    dimension: &mut Option<usize>,
) -> Result<Vec<f32>> {
    let values = value.as_array().ok_or(InferenceError::InvalidUpstream)?;
    if values.is_empty()
        || values.len() > MAX_DIMENSIONS as usize
        || dimension.is_some_and(|n| n != values.len())
        || request
            .dimensions
            .is_some_and(|n| n as usize != values.len())
    {
        return Err(InferenceError::InvalidUpstream);
    }
    *dimension = Some(values.len());
    values
        .iter()
        .map(|v| {
            let number = v.as_f64().ok_or(InferenceError::InvalidUpstream)? as f32;
            if number.is_finite() {
                Ok(number)
            } else {
                Err(InferenceError::InvalidUpstream)
            }
        })
        .collect()
}
pub(crate) fn decode_ollama(
    value: &Value,
    request: &EmbeddingRequest,
) -> Result<EmbeddingResponse> {
    if !value.is_object() || !value["error"].is_null() {
        return Err(InferenceError::InvalidUpstream);
    }
    let data = value["embeddings"]
        .as_array()
        .ok_or(InferenceError::InvalidUpstream)?;
    if data.len() != request.input.len() {
        return Err(InferenceError::InvalidUpstream);
    }
    // Native vectors are ordered like the input batch (not indexed objects).
    let mut dimension = None;
    let embeddings = data
        .iter()
        .map(|v| vector(v, request, &mut dimension))
        .collect::<Result<_>>()?;
    Ok(EmbeddingResponse {
        embeddings,
        usage: super::metering::ollama_embedding(value)?,
    })
}
pub(crate) fn decode(value: &Value, request: &EmbeddingRequest) -> Result<EmbeddingResponse> {
    if !value.is_object() || !value["error"].is_null() || value["object"] != "list" {
        return Err(InferenceError::InvalidUpstream);
    }
    let data = value["data"]
        .as_array()
        .ok_or(InferenceError::InvalidUpstream)?;
    if data.len() != request.input.len() {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut vectors = vec![None; data.len()];
    let mut dimension = None;
    for item in data {
        let index = item["index"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n < data.len())
            .ok_or(InferenceError::InvalidUpstream)?;
        if item["object"] != "embedding" || vectors[index].is_some() {
            return Err(InferenceError::InvalidUpstream);
        }
        vectors[index] = Some(vector(&item["embedding"], request, &mut dimension)?);
    }
    Ok(EmbeddingResponse {
        embeddings: vectors
            .into_iter()
            .map(|v| v.ok_or(InferenceError::InvalidUpstream))
            .collect::<Result<_>>()?,
        usage: super::metering::embedding(&value["usage"])?,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> EmbeddingRequest {
        EmbeddingRequest {
            model: "public".into(),
            input: vec!["one".into(), "two".into()],
            dimensions: Some(2),
        }
    }
    #[test]
    fn indices_dimensions_and_usage_checked() {
        let good = json!({"object":"list","data":[{"object":"embedding","index":1,"embedding":[1,2]},{"object":"embedding","index":0,"embedding":[3,4]}],"usage":{"prompt_tokens":2,"total_tokens":2}});
        let response = decode(&good, &request()).unwrap();
        assert_eq!(response.embeddings[0], vec![3., 4.]);
        assert_eq!(response.usage.output_tokens, Some(0));
        let mut bad = good.clone();
        bad["data"][0]["index"] = 0.into();
        assert!(decode(&bad, &request()).is_err());
        let mut bad = good.clone();
        bad["data"][0]["embedding"] = json!([1]);
        assert!(decode(&bad, &request()).is_err());
        let mut bad = good.clone();
        bad["usage"]["total_tokens"] = 3.into();
        assert!(decode(&bad, &request()).is_err());
        let mut bad = good;
        bad["data"][0]["embedding"] = json!([1e100, 2]);
        assert!(decode(&bad, &request()).is_err());
    }
    #[test]
    fn native_vectors_and_presence_are_not_guessed() {
        let good = json!({"embeddings":[[1,2],[3,4]],"prompt_eval_count":7});
        let response = decode_ollama(&good, &request()).unwrap();
        assert_eq!(response.embeddings, vec![vec![1., 2.], vec![3., 4.]]);
        assert_eq!(response.usage.input_tokens, Some(7));
        assert_eq!(response.usage.output_tokens, Some(0));
        assert_eq!(
            response.usage.billing.unwrap().uncached_input_tokens,
            Some(7)
        );
        for absent in [
            json!({"embeddings":[[1,2],[3,4]]}),
            json!({"embeddings":[[1,2],[3,4]],"prompt_eval_count":null}),
        ] {
            let u = decode_ollama(&absent, &request()).unwrap().usage;
            assert_eq!(u.input_tokens, None);
            assert_eq!(u.billing.unwrap().total_input_tokens, None);
            assert_eq!(u.billing.unwrap().uncached_input_tokens, None);
            assert_eq!(u.output_tokens, Some(0));
        }
        let mut zero = good.clone();
        zero["prompt_eval_count"] = 0.into();
        assert_eq!(
            decode_ollama(&zero, &request()).unwrap().usage.input_tokens,
            Some(0)
        );
        for bad in [
            json!([]),
            json!([[1, 2]]),
            json!([[1, 2], [3]]),
            json!([[1, 2], []]),
            json!([[1, 2], [3, 1e100]]),
            json!([[1, 2], [3, "4"]]),
        ] {
            let mut value = good.clone();
            value["embeddings"] = bad;
            assert!(decode_ollama(&value, &request()).is_err());
        }
        for bad in [json!(-1), json!(1.5), json!("7"), json!(u64::MAX)] {
            let mut value = good.clone();
            value["prompt_eval_count"] = bad;
            assert!(decode_ollama(&value, &request()).is_err());
        }
        let mut r = request();
        r.dimensions = Some(3);
        assert!(decode_ollama(&good, &r).is_err());
        assert!(
            decode_ollama(
                &json!({"error":"rejected","embeddings":[[1,2],[3,4]]}),
                &request()
            )
            .is_err()
        );
        assert!(decode_ollama(&json!({"object":"list","data":[]}), &request()).is_err());
        assert!(encode_ollama("private", &request()).is_err());
        r.dimensions = None;
        let value = encode_ollama("private", &r).unwrap();
        assert_eq!(value["truncate"], false);
        assert_eq!(value["input"], json!(["one", "two"]));
    }
    #[test]
    fn bounds_and_missing_counts() {
        let mut r = request();
        r.input = vec![String::new()];
        assert!(validate(&r).is_err());
        r.input = vec!["x".repeat(MAX_INPUT_BYTES + 1)];
        assert!(validate(&r).is_err());
        let u = super::super::metering::embedding(&json!({"total_tokens":3})).unwrap();
        assert_eq!(u.input_tokens, None);
        assert_eq!(u.billing.unwrap().uncached_input_tokens, None);
    }
}
