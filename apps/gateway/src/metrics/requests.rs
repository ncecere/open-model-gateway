//! HTTP request and upstream attempt observation.
use super::*;

/// One of a fixed set, so a client cannot mint label values.
fn method_label(method: &axum::http::Method) -> &'static str {
    use axum::http::Method;
    match *method {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::PATCH => "PATCH",
        Method::DELETE => "DELETE",
        Method::OPTIONS => "OPTIONS",
        _ => "other",
    }
}

impl Metrics {
    pub fn observe_http(
        &self,
        method: &axum::http::Method,
        route: &str,
        status: u16,
        elapsed: Duration,
    ) {
        let method = method_label(method).to_owned();
        self.http_requests
            .get_or_create(&[
                ("method", method.clone()),
                ("route", route.to_owned()),
                ("status", status.to_string()),
            ])
            .inc();
        self.http_duration
            .get_or_create(&[("method", method), ("route", route.to_owned())])
            .observe(elapsed.as_secs_f64());
    }

    pub fn labels(&self, provider: &str, model: &str) -> AttemptLabels {
        AttemptLabels {
            provider: bounded(&self.providers, provider),
            model: bounded(&self.models, model),
        }
    }

    pub fn observe_attempt(&self, a: AttemptObservation<'_>) {
        let (provider, model) = (a.labels.provider.clone(), a.labels.model.clone());
        self.attempts
            .get_or_create(&[
                ("provider", provider.clone()),
                ("model", model.clone()),
                ("outcome", a.outcome.to_owned()),
            ])
            .inc();
        if let Some(error) = a.error {
            self.attempt_errors
                .get_or_create(&[
                    ("provider", provider.clone()),
                    ("code", error.code().to_owned()),
                ])
                .inc();
        }
        let pm = [("provider", provider.clone()), ("model", model.clone())];
        if let Some(ms) = a.generation_ms {
            self.upstream_latency
                .get_or_create(&pm)
                .observe(ms as f64 / 1000.0);
        }
        if let Some(ms) = a.time_to_first_token_ms {
            self.time_to_first_token
                .get_or_create(&pm)
                .observe(ms as f64 / 1000.0);
        }
        for (direction, n) in [("input", a.input_tokens), ("output", a.output_tokens)] {
            if let Some(n) = n.filter(|n| *n > 0) {
                self.tokens
                    .get_or_create(&[
                        ("provider", provider.clone()),
                        ("model", model.clone()),
                        ("direction", direction.to_owned()),
                    ])
                    .inc_by(n as u64);
            }
        }
    }
}
