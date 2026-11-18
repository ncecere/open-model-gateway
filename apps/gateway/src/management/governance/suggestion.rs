//! Draft pricing-v3 lines from OpenRouter's public catalog (no key is sent).
//!
//! Server-side HTTPS fetch of a fixed origin with no redirects, ambient proxies
//! or retries, a 5-second deadline, a 4 MiB body cap and a 10-minute cache. Units
//! are never inferred from a bare catalog key: the workload decides the meter,
//! and model-specific or ambiguous units are returned with `needs_review:true`.
//! The result is only a draft; publishing remains the immutable price POST.
use super::*;
use crate::{
    billing::v3::{Meter, PriceLines, display_line},
    inference::types::{ApiProtocol, WorkloadKind},
};
use futures_util::StreamExt;
use serde_json::value::RawValue;
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

mod limits;
mod mapping;
mod selection;
pub(crate) use selection::map_with_endpoints;
#[cfg(test)]
mod mapping_property_tests;
#[cfg(test)]
mod regression_tests;
pub(crate) use mapping::map;

const BODY_CAP: usize = 4 * 1024 * 1024;
const CACHE_TTL: Duration = Duration::from_secs(600);
const CACHE_ENTRIES: usize = 64;

type Cache = HashMap<String, (Instant, Arc<Vec<u8>>)>;
pub(crate) struct OpenRouterCatalog {
    client: crate::egress::HttpClient,
    base: String,
    timeout: Duration,
    cache: tokio::sync::Mutex<Cache>,
}
impl OpenRouterCatalog {
    fn build(base: String, timeout: Duration) -> anyhow::Result<Self> {
        Ok(Self {
            // Every resolved address must pass the egress policy (#33).
            client: crate::egress::client_builder(crate::egress::resolver(
                crate::egress::Purpose::Catalog,
            ))
            .connect_timeout(timeout)
            .pool_idle_timeout(crate::egress::UpstreamClient::Openrouter.pool_idle_timeout())
            .build()
            .map_err(|_| anyhow::anyhow!("Unable to initialize catalog transport"))?,
            base,
            timeout,
            cache: tokio::sync::Mutex::new(HashMap::new()),
        })
    }
    pub(crate) fn production() -> anyhow::Result<Self> {
        Self::build(
            resources::OPENROUTER_BASE.to_owned(),
            Duration::from_secs(5),
        )
    }
    /// Test-only origin injection; restricted to a loopback mock server.
    #[cfg(test)]
    pub(crate) fn for_test(base: String, timeout: Duration) -> Self {
        let url = reqwest::Url::parse(&base).unwrap();
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        Self::build(base, timeout).unwrap()
    }
    async fn get(&self, path: &str) -> Result<Arc<Vec<u8>>, ()> {
        let url = format!("{}{path}", self.base);
        if let Some((at, body)) = self.cache.lock().await.get(&url)
            && at.elapsed() < CACHE_TTL
        {
            return Ok(body.clone());
        }
        let body = tokio::time::timeout(self.timeout, async {
            let response = self
                .client
                .get(&url)
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await
                .map_err(crate::egress::UpstreamClient::Openrouter.count_then(()))?;
            let json = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("application/json"));
            // Redirects are not followed: any non-200 (including 3xx) fails.
            if response.status() != reqwest::StatusCode::OK
                || !json
                || response
                    .content_length()
                    .is_some_and(|n| n > BODY_CAP as u64)
            {
                return Err(());
            }
            let mut out = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| ())?;
                if out.len() + chunk.len() > BODY_CAP {
                    return Err(());
                }
                out.extend_from_slice(&chunk);
            }
            Ok(out)
        })
        .await
        .map_err(|_| ())??;
        // Decode only bounded, complete JSON; a malformed 200 must not poison
        // subsequent previews for the cache TTL.
        limits::catalog_json(&body)?;
        let body = Arc::new(body);
        let mut cache = self.cache.lock().await;
        cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
        if cache.len() >= CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert(url, (Instant::now(), body.clone()));
        Ok(body)
    }
}
fn default_catalog() -> Result<Arc<OpenRouterCatalog>, ApiError> {
    static CATALOG: OnceLock<Option<Arc<OpenRouterCatalog>>> = OnceLock::new();
    CATALOG
        .get_or_init(|| OpenRouterCatalog::production().ok().map(Arc::new))
        .clone()
        .ok_or_else(unavailable)
}
fn unavailable() -> ApiError {
    ApiError(StatusCode::BAD_GATEWAY, "OpenRouter catalog unavailable")
}
/// OpenRouter slugs only; no empty, dot or query/path-injection segments.
fn slug_valid(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/-_.:".contains(&b))
        && s.split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}
pub(super) async fn price_suggestion(
    State(s): State<Store>,
    Extension(u): Extension<ManagementPrincipal>,
    catalog: Option<Extension<Arc<OpenRouterCatalog>>>,
    Path(id): Path<Uuid>,
) -> ApiResult {
    let mut tx = resources::catalog_tx(&s, &u, false).await?;
    resources::platform_write(&mut tx, &u).await?;
    let (upstream, provider, protocols): (String, String, Vec<String>) = sqlx::query_as("SELECT d.upstream_model,p.provider,m.supported_protocols FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id JOIN models m ON m.id=d.model_id WHERE d.id=$1").bind(id).fetch_optional(&mut *tx).await?.ok_or_else(missing)?;
    // Recent provider-reported costs on this route identify which OpenRouter
    // endpoint actually served it. Only counts are read; nothing is returned
    // beyond how many attempts matched.
    let evidence: Vec<(i64, i64, i64)> = sqlx::query_as("SELECT input_tokens,output_tokens,provider_cost_microusd FROM inference_executions WHERE deployment_id=$1 AND state='succeeded' AND input_tokens IS NOT NULL AND output_tokens IS NOT NULL AND provider_cost_microusd IS NOT NULL ORDER BY started_at DESC,id DESC LIMIT 20").bind(id).fetch_all(&mut *tx).await?;
    // No database transaction or lock is held across the network fetch.
    tx.commit().await?;
    if provider != "openrouter" {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Price suggestions require an OpenRouter connection",
        ));
    }
    let workload = protocols
        .first()
        .and_then(|p| ApiProtocol::parse(p))
        .map(ApiProtocol::workload)
        .ok_or_else(invalid)?;
    if !slug_valid(&upstream) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Upstream model ID is not an OpenRouter catalog slug",
        ));
    }
    let catalog = match catalog {
        Some(Extension(c)) => c,
        None => default_catalog()?,
    };
    let body = catalog
        .get("/models?output_modalities=all")
        .await
        .map_err(|_| unavailable())?;
    let models: Value = serde_json::from_slice(&body).map_err(|_| unavailable())?;
    let model = models["data"]
        .as_array()
        .ok_or_else(unavailable)?
        .iter()
        .find(|m| m["id"] == upstream.as_str() || m["canonical_slug"] == upstream.as_str())
        .cloned()
        .ok_or(ApiError(
            StatusCode::NOT_FOUND,
            "Model not found in the OpenRouter public catalog",
        ))?;
    let slug = model["id"]
        .as_str()
        .filter(|s| slug_valid(s))
        .ok_or_else(unavailable)?
        .to_owned();
    let images = if workload == WorkloadKind::Images {
        Some(
            catalog
                .get(&format!("/images/models/{slug}/endpoints"))
                .await
                .map_err(|_| unavailable())?,
        )
    } else {
        None
    };
    // Token-priced workloads: price from one concrete endpoint (the one the
    // evidence matches, else the cheapest current one). If the endpoint list
    // is unavailable, fall back to the catalog's top-provider price.
    let endpoints = if workload == WorkloadKind::Images {
        None
    } else {
        catalog
            .get(&format!("/models/{slug}/endpoints"))
            .await
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    };
    let evidence: Vec<Evidence> = evidence
        .into_iter()
        .filter_map(|(i, o, c)| {
            Some(Evidence {
                input: u64::try_from(i).ok()?,
                output: u64::try_from(o).ok()?,
                cost: c,
            })
        })
        .collect();
    let mut out = map_with_endpoints(
        &model,
        workload,
        images.as_deref().map(Vec::as_slice),
        endpoints.as_ref(),
        &evidence,
    );
    out["deployment_id"] = json!(id);
    out["upstream_model"] = json!(upstream);
    Ok(Json(out))
}

/// Exact `usd_per_unit × units` in micro-USD, rounded up. Returns `(amount, exact)`;
/// `None` for negative/variable (`-1`), malformed or overflowing values.
pub(crate) fn microusd(usd_per_unit: &str, units: u64) -> Option<(i64, bool)> {
    let s = usd_per_unit.trim();
    if s.starts_with('-') || s.is_empty() || s.len() > 64 {
        return None;
    }
    let (mantissa, exponent) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], s[i + 1..].parse::<i32>().ok()?),
        None => (s, 0),
    };
    let (whole, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty() && frac.is_empty()
        || !whole
            .bytes()
            .chain(frac.bytes())
            .all(|b| b.is_ascii_digit())
        || !(-40..=40).contains(&exponent)
    {
        return None;
    }
    let digits = format!("{whole}{frac}");
    let digits = digits.trim_start_matches('0');
    let value: u128 = if digits.is_empty() {
        0
    } else {
        digits.parse().ok()?
    };
    // value × 10^(exponent − frac_len) USD per unit; × units × 10^6 for µUSD.
    let scale = exponent - frac.len() as i32 + 6;
    let n = value.checked_mul(u128::from(units))?;
    let (amount, exact) = if scale >= 0 {
        (n.checked_mul(10u128.checked_pow(scale as u32)?)?, true)
    } else {
        let d = 10u128.checked_pow((-scale) as u32)?;
        (n.div_ceil(d), n % d == 0)
    };
    Some((i64::try_from(amount).ok()?, exact))
}

struct Draft {
    lines: Vec<Value>,
    warnings: Warnings,
    exceeded: bool,
}
impl Draft {
    fn na(&mut self, meter: Meter, review: bool, note: &str) {
        if self.has(meter) {
            return;
        }
        let mut v = json!({"meter":meter.as_str(),"not_applicable":true,"needs_review":review,"source":"workload"});
        if !note.is_empty() {
            v["note"] = json!(note);
        }
        self.push_line(v);
    }
    fn push_line(&mut self, v: Value) {
        if self.lines.len() >= crate::billing::v3::MAX_LINES {
            self.exceeded = true;
        } else {
            self.lines.push(v);
        }
    }
    fn has(&self, meter: Meter) -> bool {
        self.lines.iter().any(|l| l["meter"] == meter.as_str())
    }
    /// A priced line from a USD-per-unit string. `units_per_unit` converts the
    /// catalog unit into the meter's base unit (e.g. 1 second = 1000 ms).
    #[allow(clippy::too_many_arguments)]
    fn priced(
        &mut self,
        meter: Meter,
        sku: &str,
        usd: Option<&str>,
        source: &str,
        mut review: bool,
        min_prompt_tokens: Option<u64>,
        variant: Option<&str>,
        note: &str,
    ) {
        if self.exceeded {
            return;
        }
        let batches: Vec<u64> = match meter {
            Meter::InputAudioSecondsMs | Meter::OutputAudioSecondsMs => {
                vec![60_000, 1_000, 3_600_000]
            }
            m => m.batches().to_vec(),
        };
        // Catalog audio rates are per second; meters count milliseconds.
        let per_unit = |batch: u64| match meter {
            Meter::InputAudioSecondsMs | Meter::OutputAudioSecondsMs => batch / 1_000,
            _ => batch,
        };
        let mut notes = vec![];
        if !note.is_empty() {
            notes.push(note.to_owned());
        }
        let chosen = usd.and_then(|usd| {
            batches
                .iter()
                .find_map(|b| {
                    microusd(usd, per_unit(*b))
                        .filter(|x| x.1)
                        .map(|x| (*b, x.0))
                })
                .or_else(|| {
                    let b = *batches.iter().max()?;
                    microusd(usd, per_unit(b))
                        .map(|x| (b, x.0))
                        .inspect(|_| notes.push("rounded up to whole micro-USD per batch".into()))
                })
        });
        let (batch, amount) = match chosen {
            Some((b, a)) => (b, Some(a)),
            None => {
                review = true;
                notes.push(if usd.is_some() {
                    "variable or unparseable catalog price; enter manually".into()
                } else {
                    "no catalog price; enter manually".into()
                });
                (batches[0], None)
            }
        };
        if notes.iter().any(|n| n.starts_with("rounded")) {
            review = true;
        }
        let mut v = json!({"meter":meter.as_str(),"microusd_per_batch":amount.map(|a|a.to_string()),"batch":batch,"unit_label":meter.unit_label(batch),"sku_label":sku,"needs_review":review,"source":source});
        if let Some(m) = min_prompt_tokens {
            v["min_prompt_tokens"] = json!(m);
        }
        if let Some(variant) = variant {
            v["variant"] = json!(variant);
        }
        if !notes.is_empty() {
            v["note"] = json!(notes.join("; "));
        }
        // Later overrides win for the same meter/variant/threshold.
        self.lines.retain(|l| {
            !(l["meter"] == v["meter"]
                && l["variant"] == v["variant"]
                && l["min_prompt_tokens"] == v["min_prompt_tokens"])
        });
        self.push_line(v);
    }
}
#[derive(Default)]
struct Warnings {
    items: Vec<String>,
}
impl Warnings {
    fn push(&mut self, w: String) {
        if self.items.contains(&w) {
            return;
        }
        if self.items.len() < limits::MAX_WARNINGS - 1 {
            self.items.push(w);
        } else if self.items.len() == limits::MAX_WARNINGS - 1 {
            self.items.push(
                "Additional catalog warnings were omitted; review all rates before publishing."
                    .into(),
            );
        }
    }
}
const TOKEN_KEYS: [(&str, Meter, &str); 6] = [
    ("prompt", Meter::InputTokens, "Input"),
    ("completion", Meter::OutputTokens, "Output"),
    ("input_cache_read", Meter::CacheReadTokens, "Cache read"),
    ("input_cache_write", Meter::CacheWriteTokens, "Cache write"),
    (
        "input_cache_write",
        Meter::CacheWrite5mTokens,
        "Cache write (5m)",
    ),
    (
        "input_cache_write_1h",
        Meter::CacheWrite1hTokens,
        "Cache write (1h)",
    ),
];
/// A nonzero discount (number or decimal string).
fn discounted(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Number(n) => n.as_f64().is_none_or(|f| f != 0.0),
        Value::String(s) => microusd(s, 1).is_none_or(|x| x.0 != 0 || !x.1),
        _ => true,
    }
}
/// A catalog pricing key as words, for warnings.
fn human_key(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(32)
        .collect::<String>()
        .replace('_', " ")
}
fn price_str(pricing: &Value, key: &str) -> Option<String> {
    match &pricing[key] {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        // Non-string values are not trusted as exact decimals.
        _ => Some("invalid".into()),
    }
}
/// Conservative imported token ceilings. They fit the default workspace
/// limits; the admin raises them deliberately when larger prompts are needed.
pub(crate) const DEFAULT_INPUT_CEILING: u64 = 8_192;
pub(crate) const DEFAULT_OUTPUT_CEILING: u64 = 1_024;

/// One recent attempt on this route with a provider-reported cost.
pub(crate) struct Evidence {
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) cost: i64,
}
/// Prompt-size tiers (`min_prompt_tokens`, strictly greater). Time-window
/// overrides are not representable and are reported instead of guessed.
fn overrides(d: &mut Draft, pricing: &Value) {
    let Some(list) = pricing["overrides"].as_array() else {
        return;
    };
    for o in list {
        if o.as_object()
            .is_some_and(|m| m.keys().any(|k| k.starts_with("utc_")))
        {
            d.warnings
                .push("A time-of-day price override cannot be represented and was ignored.".into());
            continue;
        }
        let Some(min) = o["min_prompt_tokens"]
            .as_u64()
            .filter(|n| (1..=i32::MAX as u64).contains(n))
        else {
            d.warnings.push(
                "A catalog price override without a prompt-size threshold was ignored.".into(),
            );
            continue;
        };
        for (key, meter, sku) in TOKEN_KEYS {
            if let Some(p) = price_str(o, key) {
                if !d
                    .lines
                    .iter()
                    .any(|l| l["meter"] == meter.as_str() && l["not_applicable"] != true)
                {
                    continue;
                }
                d.priced(
                    meter,
                    sku,
                    Some(&p),
                    &format!("pricing.overrides.{key}"),
                    false,
                    Some(min),
                    None,
                    "",
                );
            }
        }
        for key in o.as_object().into_iter().flat_map(|m| m.keys()) {
            if key != "min_prompt_tokens" && !TOKEN_KEYS.iter().any(|(k, _, _)| *k == key.as_str())
            {
                d.warnings.push(format!(
                    "A prompt-size override for {} pricing has no meter and was ignored.",
                    human_key(key)
                ));
            }
        }
    }
}
#[derive(Deserialize)]
struct ImageEndpoints {
    endpoints: Vec<ImageEndpoint>,
}
#[derive(Deserialize)]
struct ImageEndpoint {
    #[serde(default)]
    pricing: Vec<ImagePrice>,
    #[serde(default)]
    discount: Option<Box<RawValue>>,
}
#[derive(Deserialize)]
struct ImagePrice {
    billable: String,
    unit: String,
    cost_usd: Box<RawValue>,
    #[serde(default)]
    variant: Option<String>,
}
fn image_lines(d: &mut Draft, body: Option<&[u8]>) {
    #[derive(Deserialize)]
    struct Wrapped {
        data: ImageEndpoints,
    }
    let parsed = body.and_then(|b| {
        serde_json::from_slice::<ImageEndpoints>(b)
            .ok()
            .or_else(|| serde_json::from_slice::<Wrapped>(b).ok().map(|w| w.data))
    });
    let Some(parsed) = parsed else {
        d.priced(
            Meter::OutputImages,
            "Image output",
            None,
            "images endpoints",
            true,
            None,
            None,
            "image endpoint prices unavailable",
        );
        return;
    };
    if parsed.endpoints.len() > 1 {
        d.warnings.push(
            "Several image endpoints are listed; the highest price per size was used.".into(),
        );
    }
    if parsed.endpoints.iter().any(|e| {
        e.discount
            .as_ref()
            .is_some_and(|d| discounted(&serde_json::from_str(d.get()).unwrap_or(Value::Null)))
    }) {
        d.warnings
            .push("An endpoint discount was not applied; list prices are conservative.".into());
    }
    let mut best: Vec<(Option<String>, i64, String)> = vec![];
    for p in parsed.endpoints.iter().flat_map(|e| &e.pricing) {
        if p.billable != "output_image" || p.unit != "image" {
            d.warnings.push(format!(
                "An image price per {} cannot be represented and was ignored.",
                p.unit
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .take(32)
                    .collect::<String>()
            ));
            continue;
        }
        let text = p.cost_usd.get().trim_matches('"').to_owned();
        let Some((amount, _)) = microusd(&text, 1) else {
            continue;
        };
        let variant = p
            .variant
            .clone()
            .filter(|v| crate::billing::MeterVariant::new(v).is_some());
        match best.iter_mut().find(|b| b.0 == variant) {
            Some(b) if b.1 >= amount => {}
            Some(b) => {
                b.1 = amount;
                b.2 = text;
            }
            None => {
                if best.len() >= crate::billing::v3::MAX_LINES {
                    d.exceeded = true;
                    return;
                }
                best.push((variant, amount, text));
            }
        }
    }
    if best.is_empty() {
        d.priced(
            Meter::OutputImages,
            "Image output",
            None,
            "images endpoints",
            true,
            None,
            None,
            "no per-image price",
        );
    }
    for (variant, _, text) in best {
        d.priced(
            Meter::OutputImages,
            "Image output",
            Some(&text),
            "images endpoints pricing",
            true,
            None,
            variant.as_deref(),
            "per-image endpoint price; confirm variant names match requests",
        );
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) async fn mock(app: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (url, task)
    }
    #[tokio::test]
    async fn fetch_is_capped_unredirected_bounded_and_cached() {
        use axum::{body::Body, http::Response, routing::get};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = axum::Router::new()
            .route(
                "/ok",
                get(move |headers: axum::http::HeaderMap| {
                    let h = h.clone();
                    async move {
                        assert!(headers.get("authorization").is_none());
                        h.fetch_add(1, Ordering::SeqCst);
                        Response::builder()
                            .header("content-type", "application/json")
                            .body(Body::from("{\"data\":[]}"))
                            .unwrap()
                    }
                }),
            )
            .route(
                "/redirect",
                get(|| async {
                    Response::builder()
                        .status(302)
                        .header("location", "/ok")
                        .header("content-type", "application/json")
                        .body(Body::empty())
                        .unwrap()
                }),
            )
            .route(
                "/html",
                get(|| async {
                    Response::builder()
                        .header("content-type", "text/html")
                        .body(Body::from("<html>"))
                        .unwrap()
                }),
            )
            .route(
                "/big",
                get(|| async {
                    Response::builder()
                        .header("content-type", "application/json")
                        .body(Body::from(vec![b' '; BODY_CAP + 1]))
                        .unwrap()
                }),
            )
            .route(
                "/stream",
                get(|| async {
                    let chunks = futures_util::stream::iter((0..5).map(|_| {
                        Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(vec![
                            b' ';
                            1024 * 1024
                        ]))
                    }));
                    Response::builder()
                        .header("content-type", "application/json")
                        .body(Body::from_stream(chunks))
                        .unwrap()
                }),
            )
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    Response::builder()
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap()
                }),
            );
        let (url, task) = mock(app).await;
        let catalog = OpenRouterCatalog::for_test(url, Duration::from_millis(300));
        assert!(catalog.get("/ok").await.is_ok());
        assert!(catalog.get("/ok").await.is_ok());
        assert_eq!(hits.load(Ordering::SeqCst), 1, "second read is cached");
        for path in ["/redirect", "/html", "/big", "/stream", "/slow", "/missing"] {
            assert!(catalog.get(path).await.is_err(), "{path}");
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1, "redirect was not followed");
        task.abort();
    }
    #[test]
    fn exact_decimal_micro_usd() {
        assert_eq!(microusd("0.000000032", 1_000_000), Some((32_000, true)));
        assert_eq!(microusd("0.000015", 1_000_000), Some((15_000_000, true)));
        assert_eq!(microusd("0.041", 1), Some((41_000, true)));
        assert_eq!(microusd("1e-7", 1_000_000), Some((100_000, true)));
        assert_eq!(microusd("0.00000333", 60), Some((200, false)));
        assert_eq!(microusd("0.00000333", 3600), Some((11_988, true)));
        assert_eq!(
            microusd("0.00000491017964071857", 1_000_000),
            Some((4_910_180, false))
        );
        assert_eq!(microusd("0", 1), Some((0, true)));
        for bad in [
            "-1",
            "",
            "abc",
            "1.2.3",
            "1e999",
            ".",
            "9".repeat(40).as_str(),
        ] {
            assert_eq!(microusd(bad, 1_000_000), None, "{bad}");
        }
    }
    #[test]
    fn slug_rejects_traversal() {
        assert!(slug_valid("black-forest-labs/flux-3-image"));
        assert!(slug_valid("nvidia/nemotron-3-embed-1b:free"));
        for bad in ["", "../x", "a/../b", "a//b", "a?b", "a b", "a/%2e"] {
            assert!(!slug_valid(bad), "{bad}");
        }
    }
    proptest::proptest! {
        /// Whatever a connection's upstream model says, an accepted slug only
        /// ever extends the path of the fixed OpenRouter origin (#196).
        #[test]
        fn an_accepted_slug_never_leaves_the_fixed_origin_and_path(
            slug in "[A-Za-z0-9/._:%?#@\\\\ -]{0,60}"
        ) {
            if slug_valid(&slug) {
                for path in [
                    format!("/models/{slug}/endpoints"),
                    format!("/images/models/{slug}/endpoints"),
                ] {
                    let url =
                        reqwest::Url::parse(&format!("{}{path}", resources::OPENROUTER_BASE)).unwrap();
                    proptest::prop_assert_eq!(url.scheme(), "https");
                    proptest::prop_assert_eq!(url.host_str(), Some("openrouter.ai"));
                    proptest::prop_assert_eq!(url.port(), None);
                    proptest::prop_assert!(url.query().is_none() && url.fragment().is_none());
                    proptest::prop_assert!(
                        url.path().starts_with("/api/v1/") && url.path().ends_with("/endpoints"),
                        "{}",
                        url
                    );
                    proptest::prop_assert!(
                        url.path_segments().unwrap().all(|s| s != ".." && s != "."),
                        "{}",
                        url
                    );
                }
            }
        }
    }
    fn line<'a>(v: &'a Value, meter: &str, min: Option<u64>) -> &'a Value {
        v["lines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["meter"] == meter && l["min_prompt_tokens"].as_u64() == min)
            .unwrap()
    }
    #[test]
    fn generation_tiers_cache_and_ignored_keys() {
        let model = json!({"id":"anthropic/claude-haiku-5.5","context_length":1000000,"top_provider":{"max_completion_tokens":128000},"pricing":{"prompt":"0.0000001","completion":"0.0000005","web_search":"0.01","input_cache_read":"0.00000001","input_cache_write":"0.000000125","input_cache_write_1h":"0.0000002","overrides":[{"min_prompt_tokens":100000,"prompt":"0.0000005","completion":"0.0000025","input_cache_read":"0.00000005","input_cache_write":"0.000000625","input_cache_write_1h":"0.000001"},{"utc_start":"0000","utc_end":"0800","prompt":"0"}]}});
        let v = map(&model, WorkloadKind::Generation, None);
        assert_eq!(
            line(&v, "input_tokens", None)["microusd_per_batch"],
            "100000"
        );
        assert_eq!(
            line(&v, "input_tokens", Some(100000))["microusd_per_batch"],
            "500000"
        );
        assert_eq!(
            line(&v, "cache_write_1h_tokens", Some(100000))["microusd_per_batch"],
            "1000000"
        );
        assert_eq!(
            line(&v, "cache_write_5m_tokens", None)["microusd_per_batch"],
            "125000"
        );
        assert_eq!(line(&v, "output_images", None)["not_applicable"], true);
        assert_eq!(line(&v, "requests", None)["microusd_per_batch"], "0");
        assert_eq!(v["needs_review"], false);
        // Conservative ceilings, never the full context window.
        assert_eq!(v["draft"]["input_token_limit"], 8192);
        assert_eq!(v["draft"]["output_token_limit"], 1024);
        assert_eq!(v["ceilings"]["context_length"], 1000000);
        assert!(v["warnings"].to_string().contains("time-of-day"));
        assert!(v["warnings"].to_string().contains("Web search pricing"));
        assert!(v["warnings"].to_string().contains("conservatively"));
        // Warnings use human labels, never raw field names.
        for raw in ["pricing.", "max_units", "input_token_limit", "web_search"] {
            assert!(!v["warnings"].to_string().contains(raw), "{raw}");
        }
        assert_eq!(v["display_lines"][0], "$0.10/M input tokens");
        // The draft is a valid v3 POST body.
        assert!(serde_json::from_value::<PriceLines>(v["draft"]["price_lines"].clone()).is_ok());
    }
    #[test]
    fn ambiguous_units_need_review() {
        let stt = json!({"id":"openai/whisper-large-v3-turbo","context_length":0,"pricing":{"prompt":"0.00000333","completion":"0"}});
        let v = map(&stt, WorkloadKind::AudioTranscriptions, None);
        let audio = line(&v, "input_audio_seconds_ms", None);
        assert_eq!(
            (
                audio["microusd_per_batch"].as_str(),
                audio["batch"].as_u64(),
                audio["needs_review"].as_bool()
            ),
            (Some("11988"), Some(3_600_000), Some(true))
        );
        assert_eq!(v["needs_review"], true);
        // No input token meter: zero input ceiling; output stays small.
        assert_eq!(v["draft"]["input_token_limit"], 0);
        assert_eq!(v["draft"]["output_token_limit"], 1024);
        let tts = json!({"id":"microsoft/mai-voice-2-flash","pricing":{"prompt":"0.000015","completion":"0"}});
        let v = map(&tts, WorkloadKind::AudioSpeech, None);
        assert_eq!(
            line(&v, "input_characters", None)["microusd_per_batch"],
            "15000000"
        );
        assert_eq!(line(&v, "input_characters", None)["needs_review"], false);
        assert_eq!(v["display_lines"][0], "$15/M characters");
        assert_eq!(v["draft"]["input_token_limit"], 0);
        assert_eq!(v["draft"]["output_token_limit"], 0);
        assert!(
            v["warnings"]
                .to_string()
                .contains("maximum input characters per request")
        );
        let rerank = json!({"id":"cohere/rerank-4-fast","pricing":{"prompt":"0","completion":"0"}});
        let v = map(&rerank, WorkloadKind::Rerank, None);
        assert!(line(&v, "search_units", None)["microusd_per_batch"].is_null());
        assert_eq!(v["needs_review"], true);
        assert!(
            !v["draft"]["price_lines"]
                .to_string()
                .contains("search_units\",\"micro")
        );
        let variable = json!({"id":"openrouter/auto","pricing":{"prompt":"-1","completion":"-1"}});
        let v = map(&variable, WorkloadKind::Generation, None);
        assert!(line(&v, "input_tokens", None)["microusd_per_batch"].is_null());
    }
    #[test]
    fn image_variants_from_endpoints_exact() {
        let model = json!({"id":"black-forest-labs/flux-3-image","context_length":46864,"pricing":{"prompt":"0","completion":"0","image_token":"0.00000491017964071857","image_output":"0.00000491017964071857"}});
        let body = br#"{"id":"black-forest-labs/flux-3-image","endpoints":[{"provider_slug":"bfl","pricing":[{"billable":"output_image","unit":"image","cost_usd":0.041,"variant":"768"},{"billable":"output_image","unit":"image","cost_usd":0.607,"variant":"4k"},{"billable":"output_image","unit":"megapixel","cost_usd":0.01}]}]}"#;
        let v = map(&model, WorkloadKind::Images, Some(body));
        let images: Vec<_> = v["lines"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|l| l["meter"] == "output_images")
            .collect();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0]["microusd_per_batch"], "41000");
        assert_eq!(images[0]["variant"], "768");
        assert_eq!(images[1]["microusd_per_batch"], "607000");
        assert!(images.iter().all(|l| l["needs_review"] == true));
        assert!(v["warnings"].to_string().contains("megapixel"));
        assert!(
            v["warnings"]
                .to_string()
                .contains("maximum images per request")
        );
        assert_eq!(
            v["display_lines"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|l| l.as_str().unwrap().ends_with("(768)"))
                .count(),
            1
        );
    }
    fn endpoints(list: Value) -> Value {
        json!({"data":{"id":"cloudflare/clef-flash","endpoints":list}})
    }
    #[test]
    fn small_context_models_keep_their_own_smaller_ceilings() {
        let model = json!({"id":"a/b","context_length":4096,"top_provider":{"max_completion_tokens":512},"pricing":{"prompt":"0.000001","completion":"0.000002"}});
        let v = map(&model, WorkloadKind::Generation, None);
        assert_eq!(v["draft"]["input_token_limit"], 4096);
        assert_eq!(v["draft"]["output_token_limit"], 512);
        assert!(!v["warnings"].to_string().contains("conservatively"));
        let unknown = json!({"id":"a/b","pricing":{"prompt":"0.000001","completion":"0.000002"}});
        let v = map(&unknown, WorkloadKind::Generation, None);
        assert_eq!(v["draft"]["input_token_limit"], 8192);
        assert_eq!(v["draft"]["output_token_limit"], 1024);
        assert!(v["warnings"].to_string().contains("no context length"));
        // Default ceilings fit a 100,000 tokens-per-minute default.
        const { assert!(DEFAULT_INPUT_CEILING + DEFAULT_OUTPUT_CEILING <= 100_000) };
    }
    #[test]
    fn a_malformed_top_provider_or_endpoint_pricing_is_not_a_panic() {
        // #634: the catalog is untrusted upstream JSON. A `top_provider` that
        // is not an object is read as absent (as `map` already does), so the
        // chosen endpoint's own `max_completion_tokens` is the ceiling.
        let model = json!({"id":"v/m","pricing":{"prompt":"1","completion":"2","request":"0"},"top_provider":"x"});
        let list = endpoints(json!([
            {"pricing":{"prompt":"1","completion":"2","request":"0"},"max_completion_tokens":100}
        ]));
        let v = map_with_endpoints(&model, WorkloadKind::Generation, None, Some(&list), &[]);
        assert_eq!(v["draft"]["output_token_limit"], 100);
        assert_eq!(v["endpoint"]["selection"], "cheapest");
        // With no endpoint ceiling the malformed value is still just absent.
        let bare = endpoints(json!([{"pricing":{"prompt":"1","completion":"2","request":"0"}}]));
        let v = map_with_endpoints(&model, WorkloadKind::Generation, None, Some(&bare), &[]);
        assert_eq!(v["draft"]["output_token_limit"], 1024);
        // An endpoint whose `pricing` is not an object is never chosen (it has
        // no readable prompt price): the catalog price is used, with a warning.
        let odd = endpoints(json!([{"pricing":"free","max_completion_tokens":100}]));
        let v = map_with_endpoints(&model, WorkloadKind::Generation, None, Some(&odd), &[]);
        assert_eq!(v["endpoint"]["selection"], "catalog");
        assert_eq!(v["draft"]["output_token_limit"], 1024);
    }
    #[test]
    fn endpoint_choice_follows_evidence_else_cheapest() {
        let model = json!({"id":"cloudflare/clef-flash","context_length":65536,"top_provider":{"max_completion_tokens":58982},"pricing":{"prompt":"0.00000009","completion":"0","request":"0"}});
        let list = endpoints(json!([
            {"provider_name":"Cloudflare","tag":"cloudflare","context_length":65536,"max_completion_tokens":58982,"status":0,"pricing":{"prompt":"0.00000009","completion":"0","request":"0"}},
            {"provider_name":"PrimeIntellect","tag":"primeintellect","context_length":16384,"status":0,"pricing":{"prompt":"0.000000021","completion":"0","request":"0"}}
        ]));
        // No evidence: the cheapest current endpoint, flagged for review.
        let v = map_with_endpoints(&model, WorkloadKind::Systemone, None, Some(&list), &[]);
        assert_eq!(
            line(&v, "input_tokens", None)["microusd_per_batch"],
            "21000"
        );
        assert_eq!(v["endpoint"]["provider"], "PrimeIntellect");
        assert_eq!(v["endpoint"]["selection"], "cheapest");
        assert_eq!(v["draft"]["input_token_limit"], 8192);
        assert_eq!(v["needs_review"], true);
        let w = v["warnings"].to_string();
        assert!(
            w.contains("PrimeIntellect endpoint ($0.021/M input tokens)"),
            "{w}"
        );
        assert!(
            w.contains("higher price ($0.09/M input tokens on Cloudflare)"),
            "{w}"
        );
        // Evidence: 145 input tokens cost 14 µUSD → the Cloudflare endpoint.
        let evidence = [Evidence {
            input: 145,
            output: 0,
            cost: 14,
        }];
        let v = map_with_endpoints(
            &model,
            WorkloadKind::Systemone,
            None,
            Some(&list),
            &evidence,
        );
        assert_eq!(
            line(&v, "input_tokens", None)["microusd_per_batch"],
            "90000"
        );
        assert_eq!(v["endpoint"]["selection"], "evidence");
        assert_eq!(v["endpoint"]["matched_attempts"], 1);
        assert!(v["warnings"].to_string().contains("1 recent request."));
        // Evidence at the cheaper rate (276 × $0.021/M ≈ 5.8 → 6 µUSD).
        let evidence = [Evidence {
            input: 276,
            output: 0,
            cost: 6,
        }];
        let v = map_with_endpoints(
            &model,
            WorkloadKind::Systemone,
            None,
            Some(&list),
            &evidence,
        );
        assert_eq!(v["endpoint"]["provider"], "PrimeIntellect");
        assert_eq!(v["endpoint"]["selection"], "evidence");
        // Unmatched evidence falls back to the cheapest endpoint.
        let evidence = [Evidence {
            input: 1000,
            output: 0,
            cost: 999,
        }];
        let v = map_with_endpoints(
            &model,
            WorkloadKind::Systemone,
            None,
            Some(&list),
            &evidence,
        );
        assert_eq!(v["endpoint"]["selection"], "cheapest");
        // Unavailable endpoint list: the catalog price, with a warning.
        let v = map_with_endpoints(&model, WorkloadKind::Systemone, None, None, &[]);
        assert_eq!(
            line(&v, "input_tokens", None)["microusd_per_batch"],
            "90000"
        );
        assert_eq!(v["endpoint"]["selection"], "catalog");
        // Down endpoints are skipped when an available one exists.
        let list = endpoints(json!([
            {"provider_name":"Down","status":-5,"pricing":{"prompt":"0.00000001","completion":"0","request":"0"}},
            {"provider_name":"Up","status":0,"pricing":{"prompt":"0.00000005","completion":"0","request":"0"}}
        ]));
        let v = map_with_endpoints(&model, WorkloadKind::Systemone, None, Some(&list), &[]);
        assert_eq!(v["endpoint"]["provider"], "Up");
    }
}
