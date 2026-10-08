//! Canonical non-generation workload contracts.
//!
//! These describe gateway semantics, not any provider's HTTP payload. Like the
//! generation types they carry no `Debug` on prompt/document/media-bearing
//! structures. Rerank and System One are complete; the image and audio shapes
//! are the initial typed subset that their workload owners extend (never by
//! adding unchecked passthrough fields).
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::{error::InferenceError, types::Usage};

type Result<T> = std::result::Result<T, InferenceError>;

fn valid_model(model: &str) -> bool {
    !model.trim().is_empty() && model.len() <= 200
}
fn finite_unit(n: f64) -> bool {
    n.is_finite() && (0.0..=1.0).contains(&n)
}

// ---------------------------------------------------------------- Rerank ----

pub const RERANK_MAX_DOCUMENTS: usize = 1000;
pub const RERANK_MAX_DOCUMENT_BYTES: usize = 64 * 1024;
pub const RERANK_MAX_QUERY_BYTES: usize = 32 * 1024;
/// Aggregate query + document content, independent of the HTTP body cap.
pub const RERANK_MAX_CONTENT_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub struct RerankRequest {
    pub model: String,
    pub query: String,
    pub documents: Vec<String>,
    pub top_n: Option<u32>,
}
impl RerankRequest {
    pub fn validate(&self) -> Result<()> {
        let total = self
            .documents
            .iter()
            .try_fold(self.query.len(), |n, d| n.checked_add(d.len()));
        if !valid_model(&self.model)
            || self.query.trim().is_empty()
            || self.query.len() > RERANK_MAX_QUERY_BYTES
            || self.documents.is_empty()
            || self.documents.len() > RERANK_MAX_DOCUMENTS
            || self
                .documents
                .iter()
                .any(|d| d.trim().is_empty() || d.len() > RERANK_MAX_DOCUMENT_BYTES)
            || total.is_none_or(|n| n > RERANK_MAX_CONTENT_BYTES)
            || self.top_n == Some(0)
        {
            return Err(InferenceError::InvalidRequest);
        }
        Ok(())
    }
    /// Maximum number of results the provider may return.
    pub fn result_limit(&self) -> usize {
        self.top_n.map_or(self.documents.len(), |n| {
            (n as usize).min(self.documents.len())
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RerankResult {
    pub index: usize,
    pub relevance_score: f64,
}
pub struct RerankResponse {
    pub results: Vec<RerankResult>,
    pub usage: Usage,
}
impl RerankResponse {
    /// Unique in-range indices, finite scores, at most `min(top_n, documents)`.
    pub fn valid_for(&self, request: &RerankRequest) -> bool {
        let mut seen = std::collections::BTreeSet::new();
        !self.results.is_empty()
            && self.results.len() <= request.result_limit()
            && self.results.iter().all(|r| {
                r.index < request.documents.len()
                    && seen.insert(r.index)
                    && r.relevance_score.is_finite()
            })
    }
}

// ------------------------------------------------------------ System One ----

pub const SYSTEMONE_MAX_QUESTIONS: usize = 64;
pub const SYSTEMONE_MAX_KEY_BYTES: usize = 128;
pub const SYSTEMONE_MAX_CHOICES: usize = 255;
pub const SYSTEMONE_MAX_OPTION_BYTES: usize = 256;
pub const SYSTEMONE_SCORE_LEVELS: std::ops::RangeInclusive<usize> = 2..=10;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuestionKind {
    Noul,
    Choice,
    Score,
}
impl QuestionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Noul => "noul",
            Self::Choice => "choice",
            Self::Score => "score",
        }
    }
}
/// One TypeSafe System One primitive. The question key is never sent to the model.
#[derive(Clone)]
pub struct Question {
    pub kind: QuestionKind,
    pub instructions: Value,
    pub criteria: Option<Value>,
}
/// TypeSafe `POST /v1/systemone` request: `{model, state, questions}`.
#[derive(Clone)]
pub struct SystemoneRequest {
    pub model: String,
    pub state: Value,
    pub questions: BTreeMap<String, Question>,
}
fn valid_key(key: &str, max: usize) -> bool {
    !key.is_empty() && key.len() <= max && !key.chars().any(char::is_control)
}
fn text_or_json(value: &Value, allow_null: bool) -> bool {
    match value {
        Value::String(s) => !s.trim().is_empty(),
        Value::Object(_) | Value::Array(_) => true,
        Value::Null => allow_null,
        _ => false,
    }
}
impl Question {
    /// Answer keys this question admits: choice option keys or score level indices.
    fn options(&self) -> Vec<String> {
        match (self.kind, &self.criteria) {
            (QuestionKind::Choice, Some(Value::Object(o))) => o.keys().cloned().collect(),
            (QuestionKind::Score, Some(Value::Array(a))) => {
                (0..a.len()).map(|i| i.to_string()).collect()
            }
            _ => Vec::new(),
        }
    }
    fn validate(&self) -> Result<()> {
        let ok = text_or_json(&self.instructions, false)
            && match (self.kind, &self.criteria) {
                (QuestionKind::Noul, None) => true,
                (QuestionKind::Noul, Some(Value::Object(o))) => {
                    o.len() == 2
                        && ["true", "false"]
                            .iter()
                            .all(|k| o.get(*k).is_some_and(|v| text_or_json(v, false)))
                }
                (QuestionKind::Choice, Some(Value::Object(o))) => {
                    (2..=SYSTEMONE_MAX_CHOICES).contains(&o.len())
                        && o.iter().all(|(k, v)| {
                            valid_key(k, SYSTEMONE_MAX_OPTION_BYTES) && text_or_json(v, true)
                        })
                }
                (QuestionKind::Score, Some(Value::Array(a))) => {
                    SYSTEMONE_SCORE_LEVELS.contains(&a.len())
                        && a.iter().all(|v| text_or_json(v, false))
                }
                _ => false,
            };
        if ok {
            Ok(())
        } else {
            Err(InferenceError::InvalidRequest)
        }
    }
}
impl SystemoneRequest {
    pub fn validate(&self) -> Result<()> {
        let state_ok = match &self.state {
            Value::String(s) => !s.trim().is_empty(),
            Value::Object(_) => true,
            Value::Array(a) => !a.is_empty(),
            _ => false,
        };
        if !valid_model(&self.model)
            || !state_ok
            || self.questions.is_empty()
            || self.questions.len() > SYSTEMONE_MAX_QUESTIONS
            || self
                .questions
                .keys()
                .any(|k| !valid_key(k, SYSTEMONE_MAX_KEY_BYTES))
        {
            return Err(InferenceError::InvalidRequest);
        }
        for question in self.questions.values() {
            question.validate()?;
        }
        // Image state parts need a per-deployment image-input capability that
        // no profile declares yet; reject instead of forwarding or dropping.
        if let Value::Array(items) = &self.state
            && items
                .iter()
                .any(|i| i.get("type").and_then(Value::as_str) == Some("image_url"))
        {
            return Err(InferenceError::Unsupported);
        }
        Ok(())
    }
    /// Upstream JSON body (TypeSafe/OpenRouter shape) for `model`.
    pub fn wire(&self, model: &str) -> Value {
        let questions: Map<String, Value> = self
            .questions
            .iter()
            .map(|(k, q)| {
                let mut v = json!({"type": q.kind.as_str(), "instructions": q.instructions});
                if let Some(c) = &q.criteria {
                    v["criteria"] = c.clone();
                }
                (k.clone(), v)
            })
            .collect();
        json!({"model": model, "state": self.state, "questions": questions})
    }
}
// No Debug: choice/legend values echo client criteria.
#[derive(Clone, PartialEq)]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
        confidence: Option<f64>,
    },
    Score {
        score: f64,
        legend: BTreeMap<String, Value>,
        probabilities: BTreeMap<String, f64>,
        confidence: Option<f64>,
    },
}
impl Answer {
    pub fn kind(&self) -> QuestionKind {
        match self {
            Self::Noul { .. } => QuestionKind::Noul,
            Self::Choice { .. } => QuestionKind::Choice,
            Self::Score { .. } => QuestionKind::Score,
        }
    }
    /// TypeSafe answer shape with numeric fields, so the TypeSafe SDKs parse it.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Noul { noul } => json!({"type":"noul","noul":noul}),
            Self::Choice {
                choice,
                probabilities,
                confidence,
            } => {
                let mut v = json!({"type":"choice","choice":choice,"probabilities":probabilities});
                if let Some(c) = confidence {
                    v["confidence"] = json!(c);
                }
                v
            }
            Self::Score {
                score,
                legend,
                probabilities,
                confidence,
            } => {
                let mut v = json!({"type":"score","score":score,"legend":legend,"probabilities":probabilities});
                if let Some(c) = confidence {
                    v["confidence"] = json!(c);
                }
                v
            }
        }
    }
}
fn probabilities(value: &Value, options: &[String]) -> Result<BTreeMap<String, f64>> {
    let object = value.as_object().ok_or(InferenceError::InvalidUpstream)?;
    let map: BTreeMap<String, f64> = object
        .iter()
        .map(|(k, v)| {
            v.as_f64()
                .filter(|n| finite_unit(*n))
                .map(|n| (k.clone(), n))
                .ok_or(InferenceError::InvalidUpstream)
        })
        .collect::<Result<_>>()?;
    let sum: f64 = map.values().sum();
    // Providers round each probability (four decimals observed).
    let tolerance = 0.01 + options.len() as f64 * 1e-4;
    if map.len() != options.len()
        || !options.iter().all(|o| map.contains_key(o))
        || (sum - 1.0).abs() > tolerance
    {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(map)
}
fn confidence(value: &Value) -> Result<Option<f64>> {
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_f64()
        .filter(|n| finite_unit(*n))
        .map(Some)
        .ok_or(InferenceError::InvalidUpstream)
}
fn fields(value: &Value, allowed: &[&str]) -> Result<()> {
    let object = value.as_object().ok_or(InferenceError::InvalidUpstream)?;
    if object.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(())
}
/// Strictly parse provider answers: exactly one answer per question, matching
/// type, options and probability keys; values finite and within range.
pub fn parse_answers(
    value: &Value,
    request: &SystemoneRequest,
) -> Result<BTreeMap<String, Answer>> {
    let object = value.as_object().ok_or(InferenceError::InvalidUpstream)?;
    if object.len() != request.questions.len() {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut answers = BTreeMap::new();
    for (key, question) in &request.questions {
        let a = object.get(key).ok_or(InferenceError::InvalidUpstream)?;
        if a["type"].as_str() != Some(question.kind.as_str()) {
            return Err(InferenceError::InvalidUpstream);
        }
        let options = question.options();
        let answer = match question.kind {
            QuestionKind::Noul => {
                fields(a, &["type", "noul"])?;
                Answer::Noul {
                    noul: a["noul"]
                        .as_f64()
                        .filter(|n| finite_unit(*n))
                        .ok_or(InferenceError::InvalidUpstream)?,
                }
            }
            QuestionKind::Choice => {
                fields(a, &["type", "choice", "probabilities", "confidence"])?;
                let choice = a["choice"]
                    .as_str()
                    .filter(|c| options.iter().any(|o| o == c))
                    .ok_or(InferenceError::InvalidUpstream)?
                    .to_owned();
                Answer::Choice {
                    choice,
                    probabilities: probabilities(&a["probabilities"], &options)?,
                    confidence: confidence(&a["confidence"])?,
                }
            }
            QuestionKind::Score => {
                fields(
                    a,
                    &["type", "score", "legend", "probabilities", "confidence"],
                )?;
                let max = (options.len() - 1) as f64;
                let score = a["score"]
                    .as_f64()
                    .filter(|n| n.is_finite() && (0.0..=max).contains(n))
                    .ok_or(InferenceError::InvalidUpstream)?;
                let legend: BTreeMap<String, Value> = a["legend"]
                    .as_object()
                    .ok_or(InferenceError::InvalidUpstream)?
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                if legend.len() != options.len() || !options.iter().all(|o| legend.contains_key(o))
                {
                    return Err(InferenceError::InvalidUpstream);
                }
                Answer::Score {
                    score,
                    legend,
                    probabilities: probabilities(&a["probabilities"], &options)?,
                    confidence: confidence(&a["confidence"])?,
                }
            }
        };
        answers.insert(key.clone(), answer);
    }
    Ok(answers)
}
pub struct SystemoneResponse {
    pub answers: BTreeMap<String, Answer>,
    pub usage: Usage,
}
impl SystemoneResponse {
    pub fn valid_for(&self, request: &SystemoneRequest) -> bool {
        self.answers.len() == request.questions.len()
            && request.questions.iter().all(|(k, q)| {
                self.answers.get(k).is_some_and(|a| {
                    a.kind() == q.kind
                        && parse_answers(
                            &Value::Object(Map::from_iter([(k.clone(), a.to_json())])),
                            &SystemoneRequest {
                                model: request.model.clone(),
                                state: Value::Null,
                                questions: BTreeMap::from([(k.clone(), q.clone())]),
                            },
                        )
                        .is_ok()
                })
            })
            // System One must report both token counters (TypeSafe contract).
            && self.usage.input_tokens.is_some()
            && self.usage.output_tokens.is_some()
    }
}

// ---------------------------------------------------------------- Images ----
// Owned by `inference::images` (contract, validation and `Workload` impl).
pub use super::images::{
    GeneratedImage, ImageMediaType, ImageQuality, ImageRequest, ImageResponse, ImageSize, ImageTier,
};

// ----------------------------------------------------------------- Audio ----
// Owned by `inference::audio` (transcription + speech).
pub use super::audio::{
    AudioFormat, AudioStream, SpeechFormat, SpeechRequest, SpeechResponse, TranscriptionFormat,
    TranscriptionRequest, TranscriptionResponse,
};

#[cfg(test)]
mod tests {
    use super::*;
    fn rerank() -> RerankRequest {
        RerankRequest {
            model: "m".into(),
            query: "cat".into(),
            documents: vec!["kitten".into(), "airplane".into(), "dog".into()],
            top_n: Some(2),
        }
    }
    #[test]
    fn rerank_bounds_and_result_validation() {
        assert!(rerank().validate().is_ok());
        for bad in [
            RerankRequest {
                documents: vec![],
                ..rerank()
            },
            RerankRequest {
                documents: vec![" ".into()],
                ..rerank()
            },
            RerankRequest {
                top_n: Some(0),
                ..rerank()
            },
            RerankRequest {
                query: String::new(),
                ..rerank()
            },
            RerankRequest {
                documents: vec!["x".into(); RERANK_MAX_DOCUMENTS + 1],
                ..rerank()
            },
            RerankRequest {
                documents: vec!["x".repeat(RERANK_MAX_DOCUMENT_BYTES + 1)],
                ..rerank()
            },
            RerankRequest {
                documents: vec!["x".repeat(RERANK_MAX_DOCUMENT_BYTES); 17],
                ..rerank()
            },
        ] {
            assert!(bad.validate().is_err());
        }
        let ok = |results: Vec<(usize, f64)>| RerankResponse {
            results: results
                .into_iter()
                .map(|(index, relevance_score)| RerankResult {
                    index,
                    relevance_score,
                })
                .collect(),
            usage: Usage::default(),
        };
        assert!(ok(vec![(0, 0.9), (2, 0.1)]).valid_for(&rerank()));
        assert!(!ok(vec![]).valid_for(&rerank()));
        assert!(!ok(vec![(0, 0.9), (0, 0.1)]).valid_for(&rerank()));
        assert!(!ok(vec![(3, 0.9)]).valid_for(&rerank()));
        assert!(!ok(vec![(0, f64::NAN)]).valid_for(&rerank()));
        assert!(!ok(vec![(0, 0.3), (1, 0.2), (2, 0.1)]).valid_for(&rerank()));
    }
    fn systemone() -> SystemoneRequest {
        SystemoneRequest {
            model: "m".into(),
            state: json!("hello there"),
            questions: BTreeMap::from([
                (
                    "is_q".into(),
                    Question {
                        kind: QuestionKind::Noul,
                        instructions: json!("Is it a greeting?"),
                        criteria: None,
                    },
                ),
                (
                    "lang".into(),
                    Question {
                        kind: QuestionKind::Choice,
                        instructions: json!("Language"),
                        criteria: Some(json!({"en":"English","fr":null})),
                    },
                ),
                (
                    "tone".into(),
                    Question {
                        kind: QuestionKind::Score,
                        instructions: json!("Tone"),
                        criteria: Some(json!(["casual", "neutral", "formal"])),
                    },
                ),
            ]),
        }
    }
    fn answers() -> Value {
        json!({
            "is_q":{"type":"noul","noul":0.9543},
            "lang":{"type":"choice","choice":"en","probabilities":{"en":0.9638,"fr":0.0362},"confidence":0.9276},
            "tone":{"type":"score","score":0.4555,"legend":{"0":"casual","1":"neutral","2":"formal"},"probabilities":{"0":0.5546,"1":0.4353,"2":0.0101},"confidence":0.3167}
        })
    }
    #[test]
    fn systemone_request_validation() {
        assert!(systemone().validate().is_ok());
        let wire = systemone().wire("upstream");
        assert_eq!(wire["model"], "upstream");
        assert_eq!(
            wire["questions"]["is_q"],
            json!({"type":"noul","instructions":"Is it a greeting?"})
        );
        let mutate = |f: &dyn Fn(&mut SystemoneRequest)| {
            let mut r = systemone();
            f(&mut r);
            r.validate()
        };
        for f in [
            &(|r: &mut SystemoneRequest| r.state = json!(1)) as &dyn Fn(&mut SystemoneRequest),
            &|r| r.state = json!(""),
            &|r| r.state = Value::Null,
            &|r| r.questions.clear(),
            &|r| r.questions.get_mut("lang").unwrap().criteria = Some(json!({"en":"only"})),
            &|r| r.questions.get_mut("lang").unwrap().criteria = None,
            &|r| r.questions.get_mut("tone").unwrap().criteria = Some(json!(["one"])),
            &|r| r.questions.get_mut("tone").unwrap().criteria = Some(json!(vec!["x"; 11])),
            &|r| r.questions.get_mut("is_q").unwrap().criteria = Some(json!({"true":"y"})),
            &|r| r.questions.get_mut("is_q").unwrap().instructions = json!(""),
            &|r| {
                let q = r.questions.remove("is_q").unwrap();
                r.questions.insert(String::new(), q);
            },
        ] {
            assert_eq!(mutate(f), Err(InferenceError::InvalidRequest));
        }
        assert_eq!(
            mutate(
                &|r| r.state = json!(["text", {"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}])
            ),
            Err(InferenceError::Unsupported)
        );
    }
    #[test]
    fn systemone_answers_are_strict() {
        let parsed = parse_answers(&answers(), &systemone()).unwrap();
        assert_eq!(parsed.len(), 3);
        let response = SystemoneResponse {
            answers: parsed,
            usage: Usage {
                input_tokens: Some(276),
                output_tokens: Some(0),
                ..Default::default()
            },
        };
        assert!(response.valid_for(&systemone()));
        assert_eq!(response.answers["lang"].to_json(), answers()["lang"]);
        let bad = |f: &dyn Fn(&mut Value)| {
            let mut v = answers();
            f(&mut v);
            parse_answers(&v, &systemone()).is_err()
        };
        assert!(bad(&|v| v["is_q"]["noul"] = json!(1.5)));
        assert!(bad(&|v| v["is_q"]["type"] = json!("choice")));
        assert!(bad(&|v| v["lang"]["choice"] = json!("de")));
        assert!(bad(
            &|v| v["lang"]["probabilities"] = json!({"en":0.5,"fr":0.1})
        ));
        assert!(bad(
            &|v| v["lang"]["probabilities"] = json!({"en":0.9,"de":0.1})
        ));
        assert!(bad(&|v| v["tone"]["score"] = json!(2.5)));
        assert!(bad(&|v| v["tone"]["legend"] = json!({"0":"a"})));
        assert!(bad(&|v| v["tone"]["confidence"] = json!(-0.1)));
        assert!(bad(&|v| v["tone"]["extra"] = json!(1)));
        assert!(bad(&|v| {
            v.as_object_mut().unwrap().remove("tone");
        }));
        assert!(bad(&|v| v["extra"] = json!({"type":"noul","noul":0.1})));
        let mut no_usage = SystemoneResponse {
            answers: parse_answers(&answers(), &systemone()).unwrap(),
            usage: Usage::default(),
        };
        assert!(!no_usage.valid_for(&systemone()));
        no_usage.usage.input_tokens = Some(1);
        no_usage.usage.output_tokens = Some(0);
        assert!(no_usage.valid_for(&systemone()));
    }
}
