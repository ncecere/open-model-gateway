//! The one connection-profile → client-protocol capability table.
//!
//! Every registered adapter's `supports_protocol` reads this table, so the
//! inference path (`unsupported_capability` before admission), route and
//! model-setup validation (`route_unsupported_capability`), readiness counts
//! and the Add model form (`apps/web/src/lib/model-setup.ts`
//! `protocolProfiles`, checked against this table by a test) share one
//! source of truth. Request-specific checks (sizes, formats, tool choice)
//! stay with each adapter; this table is only "can this profile carry the
//! protocol at all".
use crate::inference::types::{ApiProtocol as P, WorkloadKind};

/// Registered profiles and the client protocols their adapters serve.
pub const PROFILES: &[(&str, &[P])] = &[
    // OpenAI shut down the Sora 2 models and the Videos API on 2026-09-24
    // (no replacement), so no profile serves `videos`; the OpenAI video wire
    // code remains only for jobs created before the shutdown.
    (
        "openai",
        &[
            P::ChatCompletions,
            P::Responses,
            P::Embeddings,
            P::Images,
            P::AudioTranscriptions,
            P::AudioSpeech,
            P::Realtime,
            P::Batches,
        ],
    ),
    ("anthropic", &[P::ChatCompletions, P::Messages]),
    ("bedrock", &[P::ChatCompletions, P::Messages]),
    (
        "openrouter",
        &[
            P::ChatCompletions,
            P::Embeddings,
            P::Rerank,
            P::Systemone,
            P::Images,
            P::AudioTranscriptions,
            P::AudioSpeech,
        ],
    ),
    // Local profiles: Jina/Cohere rerank on compatible and vLLM servers,
    // SGLang's native rerank; TypeSafe System One on compatible servers (such
    // as OpenJev) and Ollama v0.35.0+. Ollama has no rerank API; vLLM and
    // SGLang serve no System One route.
    (
        "openai_compatible",
        &[P::ChatCompletions, P::Embeddings, P::Rerank, P::Systemone],
    ),
    ("vllm", &[P::ChatCompletions, P::Embeddings, P::Rerank]),
    ("sglang", &[P::ChatCompletions, P::Embeddings, P::Rerank]),
    ("ollama", &[P::ChatCompletions, P::Embeddings, P::Systemone]),
];

/// Protocols a registered profile serves; `None` for an unknown profile.
pub fn protocols(profile: &str) -> Option<&'static [P]> {
    PROFILES
        .iter()
        .find(|(id, _)| *id == profile)
        .map(|(_, protocols)| *protocols)
}

/// Whether `profile`'s adapter can carry `protocol` at all.
pub fn serves(profile: &str, protocol: P) -> bool {
    protocols(profile).is_some_and(|p| p.contains(&protocol))
}

/// Whether a route on `profile` can serve at least one of a model's declared
/// protocols (a text model's other protocols may be served by other routes).
pub fn route_serves<S: AsRef<str>>(profile: &str, model_protocols: &[S]) -> bool {
    model_protocols
        .iter()
        .filter_map(|p| P::parse(p.as_ref()))
        .any(|p| serves(profile, p))
}

/// Short workload name for messages.
pub fn workload_label(kind: WorkloadKind) -> &'static str {
    match kind {
        WorkloadKind::Generation => "Text generation",
        WorkloadKind::Embeddings => "Embeddings",
        WorkloadKind::Images => "Image generation",
        WorkloadKind::AudioTranscriptions => "Speech to text",
        WorkloadKind::AudioSpeech => "Text to speech",
        WorkloadKind::Rerank => "Rerank",
        WorkloadKind::Systemone => "System One",
        WorkloadKind::Realtime => "Realtime audio",
        WorkloadKind::Videos => "Video generation",
        WorkloadKind::Batches => "Batch",
    }
}

/// Why a route on `profile` cannot serve a model with `model_protocols`, or
/// `None` when it can. Names the workload, the protocols and the profile, and
/// what the profile does serve. Never contains credentials or endpoints.
pub fn unsupported_reason<S: AsRef<str>>(profile: &str, model_protocols: &[S]) -> Option<String> {
    if route_serves(profile, model_protocols) {
        return None;
    }
    let parsed: Vec<P> = model_protocols
        .iter()
        .filter_map(|p| P::parse(p.as_ref()))
        .collect();
    let workload = parsed
        .first()
        .map_or("this model's workload", |p| workload_label(p.workload()));
    let names = parsed.iter().map(|p| p.as_str()).collect::<Vec<_>>();
    let served = protocols(profile).map_or_else(
        || "it is not a registered connection profile".to_owned(),
        |p| {
            format!(
                "it serves {}",
                p.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(", ")
            )
        },
    );
    Some(format!(
        "The {profile} connection profile can't serve {workload} ({}); {served}",
        names.join(", ")
    ))
}

/// SQL predicate (deployment alias `d`): the route's connection profile
/// serves at least one of its model's protocols. Generated from [`PROFILES`]
/// so readiness and the API validation never disagree.
pub fn route_serves_sql() -> String {
    let values = PROFILES
        .iter()
        .flat_map(|(profile, protocols)| {
            protocols
                .iter()
                .map(move |p| format!("('{profile}','{}')", p.as_str()))
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "EXISTS(SELECT 1 FROM provider_connections cap_p JOIN models cap_m ON cap_m.id=d.model_id JOIN (VALUES {values}) cap(profile,protocol) ON cap.profile=cap_p.provider AND cap.protocol=ANY(cap_m.supported_protocols) WHERE cap_p.id=d.provider_connection_id)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn profiles_are_unique_and_protocols_distinct() {
        let mut ids = BTreeSet::new();
        for (id, protocols) in PROFILES {
            assert!(ids.insert(*id), "{id}");
            let distinct: BTreeSet<_> = protocols.iter().map(|p| p.as_str()).collect();
            assert_eq!(distinct.len(), protocols.len(), "{id}");
        }
        // Video has no supported provider (OpenAI shut down its Videos API).
        assert!(PROFILES.iter().all(|(_, p)| !p.contains(&P::Videos)));
    }

    #[test]
    fn routes_must_serve_some_model_protocol() {
        assert!(route_serves("vllm", &["chat_completions", "responses"]));
        assert!(route_serves("vllm", &["rerank"]));
        assert!(!route_serves("vllm", &["systemone"]));
        assert!(!route_serves("ollama", &["rerank"]));
        assert!(!route_serves("vllm", &["responses"]));
        assert!(!route_serves("unknown_profile", &["chat_completions"]));
        let reason = unsupported_reason("vllm", &["systemone"]).unwrap();
        assert!(
            reason.contains("vllm") && reason.contains("System One"),
            "{reason}"
        );
        assert!(
            reason.contains("systemone") && reason.contains("rerank"),
            "{reason}"
        );
        assert_eq!(unsupported_reason("ollama", &["systemone"]), None);
        let reason = unsupported_reason("ollama", &["rerank"]).unwrap();
        assert!(
            reason.starts_with("The ollama connection profile can't serve Rerank (rerank)"),
            "{reason}"
        );
    }

    /// The Add model form mirrors this table (`protocolProfiles` in
    /// `apps/web/src/lib/model-setup.ts`); a drift fails here.
    #[test]
    fn web_protocol_profiles_match_the_capability_table() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../web/src/lib/model-setup.ts");
        let source = std::fs::read_to_string(path).expect("web model-setup source");
        let start = source
            .find("export const protocolProfiles")
            .expect("protocolProfiles");
        let body = &source[start..];
        let body = &body[body.find('{').unwrap() + 1..body.find("\n};").unwrap()];
        let mut web: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for line in body.lines().map(str::trim) {
            if line.starts_with("//") || line.is_empty() {
                continue;
            }
            let (key, rest) = line.split_once(':').expect("protocol entry");
            let profiles = rest
                .split('"')
                .skip(1)
                .step_by(2)
                .map(str::to_owned)
                .collect();
            web.insert(key.trim().to_owned(), profiles);
        }
        let mut rust: BTreeMap<String, BTreeSet<String>> = P::ALL
            .iter()
            .map(|p| (p.as_str().to_owned(), BTreeSet::new()))
            .collect();
        for (profile, protocols) in PROFILES {
            for p in *protocols {
                rust.get_mut(p.as_str())
                    .unwrap()
                    .insert((*profile).to_owned());
            }
        }
        assert_eq!(web, rust);
    }
}
