//! Input-only workloads share routing, cancellation and durable admission
//! (see `workload.rs`), but never masquerade as chat messages.
use super::*;
use types::{EmbeddingRequest, EmbeddingResponse};

impl Engine {
    pub async fn execute_embeddings(
        &self,
        principal: Principal,
        request: EmbeddingRequest,
        request_id: Uuid,
    ) -> Result<EmbeddingResponse, InferenceError> {
        self.execute_workload(principal, request, request_id).await
    }
}
pub(super) fn valid_request(request: &EmbeddingRequest) -> bool {
    !request.model.is_empty()
        && request.input.len() <= 128
        && !request.input.is_empty()
        && request.input.iter().all(|s| !s.trim().is_empty())
        && request.input.iter().map(String::len).sum::<usize>() <= 1024 * 1024
        && request.dimensions.is_none_or(|n| (1..=16384).contains(&n))
}
pub(super) fn valid_response(request: &EmbeddingRequest, response: &EmbeddingResponse) -> bool {
    let Some(first) = response.embeddings.first() else {
        return false;
    };
    response.embeddings.len() == request.input.len()
        && !first.is_empty()
        && first.len() <= 16384
        && request.dimensions.is_none_or(|n| first.len() == n as usize)
        && response
            .embeddings
            .iter()
            .all(|v| v.len() == first.len() && v.iter().all(|n| n.is_finite()))
        && response.usage.output_tokens == Some(0)
        && valid_usage(response.usage)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_embedding_shapes_and_semantic_output_zero() {
        let request = EmbeddingRequest {
            model: "local/embed".into(),
            input: vec!["text".into()],
            dimensions: Some(2),
        };
        assert!(valid_request(&request));
        let mut response = EmbeddingResponse {
            embeddings: vec![vec![0.1, 0.2]],
            usage: Usage {
                input_tokens: Some(2),
                output_tokens: Some(0),
                billing: None,
                ..Default::default()
            },
        };
        assert!(valid_response(&request, &response));
        response.embeddings[0][0] = f32::NAN;
        assert!(!valid_response(&request, &response));
        response.embeddings[0][0] = 0.1;
        response.usage.output_tokens = None;
        assert!(!valid_response(&request, &response));
        assert!(!valid_request(&EmbeddingRequest {
            input: vec![],
            ..request.clone()
        }));
        assert!(!valid_request(&EmbeddingRequest {
            dimensions: Some(0),
            ..request.clone()
        }));
        assert!(!valid_request(&EmbeddingRequest {
            input: vec![" ".into()],
            ..request
        }));
    }
}
