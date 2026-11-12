//! Price versions and cost arithmetic: the immutable `deployment_prices` rows,
//! their bounds, and the meters a pricing-v3 price must state.
use super::*;
#[derive(Clone, sqlx::FromRow)]
pub(super) struct Price {
    pub(super) id: Uuid,
    /// Null for pricing v3, whose token rates live in `price_lines`.
    pub(super) input_microusd_per_million: Option<i64>,
    pub(super) output_microusd_per_million: Option<i64>,
    pub(super) input_token_limit: i64,
    pub(super) output_token_limit: i64,
    pub(super) pricing_version: i16,
    pub(super) cache_pricing: Option<serde_json::Value>,
    pub(super) price_lines: Option<serde_json::Value>,
    pub(super) max_units: Option<serde_json::Value>,
    /// Batch price list (0021, v3 only): applies to native batches.
    pub(super) batch_price_lines: Option<serde_json::Value>,
}
pub(super) const PRICE_COLUMNS: &str = "id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version,cache_pricing,price_lines,max_units,batch_price_lines";
/// Price versions by (id, deployment): `deployment_prices` rows are
/// append-only and immutable (0001 trigger; no runtime UPDATE/DELETE), so
/// entries never need invalidation. Process-wide (ids are random UUIDs).
static PRICES: std::sync::LazyLock<
    crate::cache::ImmutableCache<(Uuid, Uuid), std::sync::Arc<Price>>,
> = std::sync::LazyLock::new(|| crate::cache::ImmutableCache::new("prices", 20_000));
/// The immutable price version `id` of `deployment` (cached; a missing row is
/// a storage error, as before).
pub(super) async fn price_version(
    tx: &mut Tx<'_>,
    id: Uuid,
    deployment: Uuid,
) -> Result<Price, InferenceError> {
    if let Some(price) = PRICES.get(&(id, deployment)) {
        return Ok(price.as_ref().clone());
    }
    let price = sqlx::query_as::<_, Price>(&format!(
        "SELECT {PRICE_COLUMNS} FROM deployment_prices WHERE id=$1 AND deployment_id=$2"
    ))
    .bind(id)
    .bind(deployment)
    .fetch_one(&mut **tx)
    .await
    .map_err(storage)?;
    PRICES.insert((id, deployment), std::sync::Arc::new(price.clone()));
    Ok(price)
}
impl Price {
    /// Whether this version publishes a batch price list (`batch::PriceTier`).
    pub(super) fn has_batch_lines(&self) -> bool {
        self.pricing_version == 3 && self.batch_price_lines.is_some()
    }
    /// The price as valued under a reservation's pinned tier (`price_tier`,
    /// 0021): the batch tier swaps in the batch price list.
    pub(super) fn tiered(mut self, tier: &str) -> Result<Price, InferenceError> {
        match tier {
            "standard" => Ok(self),
            "batch" if self.has_batch_lines() => {
                self.price_lines = self.batch_price_lines.take();
                Ok(self)
            }
            _ => Err(InferenceError::Storage),
        }
    }
    pub(super) fn rates(&self) -> Result<CachePricing, InferenceError> {
        serde_json::from_value(self.cache_pricing.clone().ok_or(InferenceError::Storage)?)
            .map_err(|_| InferenceError::Storage)
    }
    pub(super) fn token_rates(&self) -> Result<(i64, i64), InferenceError> {
        self.input_microusd_per_million
            .zip(self.output_microusd_per_million)
            .ok_or(InferenceError::Storage)
    }
    pub(super) fn lines(&self) -> Result<(PriceLines, MaxUnits), InferenceError> {
        let lines =
            serde_json::from_value(self.price_lines.clone().ok_or(InferenceError::Storage)?)
                .map_err(|_| InferenceError::Storage)?;
        let max = serde_json::from_value(self.max_units.clone().ok_or(InferenceError::Storage)?)
            .map_err(|_| InferenceError::Storage)?;
        Ok((lines, max))
    }
    /// `ceilings` are request-derived hard unit bounds; they only tighten v3
    /// `max_units` (v1/v2 have no unit meters).
    pub(super) fn bound(
        &self,
        output: u64,
        ceilings: &MeterUsage,
    ) -> Result<Option<i64>, InferenceError> {
        self.bound_scaled(1, output, ceilings, false)
    }
    /// [`Price::bound`] for `requests` requests sharing one attempt (an async
    /// batch: input ceiling `requests * input_token_limit`) and, for the
    /// video workload, the `output_video_seconds_ms` meter. Every other
    /// workload cannot produce video, so prices without a video line still
    /// bound them.
    pub(super) fn bound_scaled(
        &self,
        requests: u64,
        output: u64,
        ceilings: &MeterUsage,
        video: bool,
    ) -> Result<Option<i64>, InferenceError> {
        let input = u64::try_from(self.input_token_limit)
            .ok()
            .and_then(|n| n.checked_mul(requests))
            .ok_or(InferenceError::Configuration)?;
        match self.pricing_version {
            1 => {
                let (i, o) = self.token_rates()?;
                Ok(Some(cost(input, output, i, o)?))
            }
            2 => {
                let (i, o) = self.token_rates()?;
                billing::bound_v2(i, o, &self.rates()?, input, output)
                    .map_err(|_| InferenceError::Configuration)
            }
            3 => {
                let (lines, mut max) = self.lines()?;
                // `MeterUsage::counts` order: non-token `Meter::ALL`, then video.
                for (meter, ceiling) in billing::v3::Meter::ALL
                    .into_iter()
                    .filter(|m| !m.is_token())
                    .chain(billing::v3::Meter::VIDEO)
                    .zip(ceilings.counts())
                {
                    // A request ceiling tightens the price's `max_units`. A
                    // request that can exceed it (e.g. measured audio longer
                    // than the priced maximum) is not covered by the price:
                    // the meter becomes unbounded instead of under-held.
                    match (ceiling, max.0.get(&meter).copied()) {
                        (Some(c), Some(m)) if c > m => {
                            max.0.remove(&meter);
                        }
                        (Some(c), _) => {
                            max.0.insert(meter, c);
                        }
                        (None, _) => {}
                    }
                }
                if video {
                    billing::v3::bound_video(&lines, &max, input, output)
                } else {
                    billing::v3::bound(&lines, &max, input, output)
                }
                .map_err(|_| InferenceError::Configuration)
            }
            _ => Err(InferenceError::Configuration),
        }
    }
}
pub(super) fn cost(input: u64, output: u64, i: i64, o: i64) -> Result<i64, InferenceError> {
    checked_cost(input, output, i, o).map_err(|_| InferenceError::Storage)
}
pub(super) fn checked_cost(
    input: u64,
    output: u64,
    i: i64,
    o: i64,
) -> Result<i64, billing::BillingError> {
    billing::charge(input, i).and_then(|a| {
        billing::charge(output, o)
            .and_then(|b| a.checked_add(b).ok_or(billing::BillingError::Overflow))
    })
}
/// Native cache hints require explicitly priced reads and TTL writes. The existing
/// v2/v3 bound independently reserves the *full input ceiling* for each priced
/// cache category, including the highest requested write rate, plus output.
/// This intentionally conservative bound also covers per-component rounding;
/// settlement still uses disjoint observed allocations, never aggregate+parts.
pub(super) fn require_native_cache_price(
    price: Option<&Price>,
    controls: &[serde_json::Value],
) -> Result<(), InferenceError> {
    if controls.is_empty() {
        return Ok(());
    }
    let incompatible = || InferenceError::InvalidField("cache_control");
    let rates = match price {
        Some(p) if p.pricing_version == 2 => p.rates()?,
        Some(p) if p.pricing_version == 3 => billing::v3::cache_rates(&p.lines()?.0),
        _ => return Err(incompatible()), // No price or v1 cannot price TTL writes.
    };
    // Any cached prompt can become a cache hit on a later request, regardless
    // of its write TTL. A finite write hold alone cannot price that settlement.
    if !matches!(rates.read, CacheRate::Priced { .. }) {
        return Err(incompatible());
    }
    for control in controls {
        let rate = match control.get("ttl").and_then(serde_json::Value::as_str) {
            None | Some("5m") => rates.write_5m,
            Some("1h") => rates.write_1h,
            _ => return Err(incompatible()),
        };
        if !matches!(rate, CacheRate::Priced { .. }) {
            return Err(incompatible());
        }
    }
    Ok(())
}
/// Meters a pricing-v3 price of a `kind` route must state explicitly (priced,
/// `not_applicable` or `unknown`) at publication: every meter that kind's
/// admission bound treats as possibly used. Admission bounds every meter of
/// [`billing::v3::Meter::ALL`] except those a workload's request ceilings fix
/// at zero (images, speech to text, text to speech and video jobs fix the unit
/// meters they cannot produce; a realtime window fixes every non-token meter
/// but `requests`), plus the realtime audio-token and video meters. A meter left
/// out would make every budgeted admission unbounded (`price_unbounded`).
pub fn price_meters(kind: WorkloadKind) -> Vec<billing::v3::Meter> {
    use billing::v3::Meter;
    let tokens = Meter::ALL.into_iter().filter(|m| m.is_token());
    match kind {
        WorkloadKind::Images => tokens
            .chain([Meter::OutputImages, Meter::Requests])
            .collect(),
        WorkloadKind::AudioTranscriptions => tokens
            .chain([Meter::InputAudioSecondsMs, Meter::Requests])
            .collect(),
        WorkloadKind::AudioSpeech => tokens
            .chain([
                Meter::InputCharacters,
                Meter::OutputAudioSecondsMs,
                Meter::Requests,
            ])
            .collect(),
        WorkloadKind::Realtime => tokens
            .chain(Meter::AUDIO_TOKENS)
            .chain([Meter::Requests])
            .collect(),
        WorkloadKind::Videos => tokens
            .chain([Meter::Requests])
            .chain(Meter::VIDEO)
            .collect(),
        WorkloadKind::Generation
        | WorkloadKind::Embeddings
        | WorkloadKind::Rerank
        | WorkloadKind::Systemone
        | WorkloadKind::Batches => Meter::ALL.to_vec(),
    }
}
/// The latest (immutable, append-only) price of a deployment.
pub(super) async fn latest_price(
    tx: &mut Tx<'_>,
    deployment: Uuid,
) -> Result<Option<Price>, InferenceError> {
    sqlx::query_as::<_, Price>(&format!("SELECT {PRICE_COLUMNS} FROM deployment_prices WHERE deployment_id=$1 ORDER BY created_at DESC,id DESC LIMIT 1"))
        .bind(deployment)
        .fetch_optional(&mut **tx)
        .await
        .map_err(storage)
}
