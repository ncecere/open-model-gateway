//! Media regressions: binary transport completion is not inferred from bytes.
use super::tests::generated;

#[test]
fn inference_openapi_speech_truncation_is_not_clean() {
    let doc = generated();
    let speech = &doc["paths"]["/v1/audio/speech"]["post"];
    assert_eq!(speech["x-omg-binary-stream"]["late_error"], "truncated");
    assert_eq!(speech["x-omg-binary-stream"]["positive_requires"], "clean transport EOF");
    assert!(speech["x-omg-cancellation"].is_object());
}

#[test]
fn inference_openapi_transcription_strict_parts() {
    let doc = generated();
    let parts = &doc["paths"]["/v1/audio/transcriptions"]["post"]["x-omg-multipart"];
    assert_eq!(parts["max_parts"], 16);
    assert_eq!(parts["max_header_bytes"], 8192);
    assert_eq!(parts["max_field_bytes"], 4096);
    assert_eq!(parts["unknown_parts"], "reject");
    assert_eq!(parts["file_requires_filename"], true);
    assert_eq!(parts["duplicate_fields"], "reject except stream=false");
    assert_eq!(parts["part_headers"], serde_json::json!(["Content-Disposition", "Content-Type"]));
}
