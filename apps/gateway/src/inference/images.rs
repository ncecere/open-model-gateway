//! Image generation workload (`POST /v1/images/generations`).
//!
//! Canonical, provider-neutral contract: `{model, prompt, n, size, quality,
//! seed}` in, raw base64 images (never URLs) out. No `Debug` on prompt- or
//! image-bearing types. Every returned image is base64-validated and must carry
//! PNG, JPEG or WebP magic; the count must equal `n`.
//!
//! Meters: `output_images` is the number of images the provider returned,
//! `requests` is 1, and the other non-token meters are semantic zeros (their
//! admission ceilings are 0, so a missing price line for them never makes the
//! hold unbounded). Token meters follow each adapter's research normalization.
//! `output_image_variant` is the adapter's price-tier string.
use async_trait::async_trait;

use super::{
    error::InferenceError,
    types::{ApiProtocol, Deployment, Usage, WorkloadKind},
    workload::{OutputReservation, Workload, WorkloadAdmission},
};
use crate::{billing::MeterUsage, providers::ProviderAdapter};

type Result<T> = std::result::Result<T, InferenceError>;

/// Images per request (`n`), a hard admission ceiling.
pub const IMAGE_MAX_N: u32 = 4;
/// Prompt length in Unicode scalar values (OpenAI gpt-image maximum).
pub const IMAGE_MAX_PROMPT_CHARS: usize = 32_000;
/// Largest JSON-safe integer seed.
pub const IMAGE_MAX_SEED: u64 = (1 << 53) - 1;
/// Upper bound for an upstream `revised_prompt`.
pub const IMAGE_MAX_REVISED_PROMPT_BYTES: usize = 128 * 1024;
const MAX_PIXELS_SIDE: u32 = 16_384;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageQuality {
    Auto,
    Low,
    Medium,
    High,
}
impl ImageQuality {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "auto" => Self::Auto,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Normalized OpenRouter-style resolution tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageTier {
    P512,
    P768,
    K1,
    K1_5,
    K2,
    K4,
}
impl ImageTier {
    const ALL: [Self; 6] = [
        Self::P512,
        Self::P768,
        Self::K1,
        Self::K1_5,
        Self::K2,
        Self::K4,
    ];
    /// Wire spelling of the `resolution` parameter.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::P512 => "512",
            Self::P768 => "768",
            Self::K1 => "1K",
            Self::K1_5 => "1.5K",
            Self::K2 => "2K",
            Self::K4 => "4K",
        }
    }
    /// Price-line variant spelling (OpenRouter endpoint `pricing[].variant`).
    pub fn variant(self) -> &'static str {
        match self {
            Self::P512 => "512",
            Self::P768 => "768",
            Self::K1 => "1k",
            Self::K1_5 => "1.5k",
            Self::K2 => "2k",
            Self::K4 => "4k",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|t| t.as_str().eq_ignore_ascii_case(s))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageSize {
    Auto,
    Pixels { width: u32, height: u32 },
    Tier(ImageTier),
}
impl ImageSize {
    /// `auto`, `WxH` (canonical decimal, 1..=16384 per side) or a tier.
    pub fn parse(s: &str) -> Option<Self> {
        if s == "auto" {
            return Some(Self::Auto);
        }
        if let Some(tier) = ImageTier::parse(s) {
            return Some(Self::Tier(tier));
        }
        let (w, h) = s.split_once('x')?;
        let side = |v: &str| {
            (!v.is_empty()
                && v.len() <= 5
                && !v.starts_with('0')
                && v.bytes().all(|b| b.is_ascii_digit()))
            .then(|| v.parse::<u32>().ok())
            .flatten()
            .filter(|n| (1..=MAX_PIXELS_SIDE).contains(n))
        };
        Some(Self::Pixels {
            width: side(w)?,
            height: side(h)?,
        })
    }
    pub fn to_wire(self) -> String {
        match self {
            Self::Auto => "auto".into(),
            Self::Pixels { width, height } => format!("{width}x{height}"),
            Self::Tier(t) => t.as_str().into(),
        }
    }
}

#[derive(Clone)]
pub struct ImageRequest {
    pub model: String,
    pub prompt: String,
    pub n: u32,
    pub size: Option<ImageSize>,
    pub quality: Option<ImageQuality>,
    pub seed: Option<u64>,
    /// Upstream response cap in bytes (server configuration, never client input).
    pub max_response_bytes: usize,
}
impl ImageRequest {
    pub fn validate(&self) -> Result<()> {
        if self.model.trim().is_empty()
            || self.model.len() > 200
            || self.prompt.trim().is_empty()
            || self.prompt.chars().count() > IMAGE_MAX_PROMPT_CHARS
            || !(1..=IMAGE_MAX_N).contains(&self.n)
            || self.seed.is_some_and(|s| s > IMAGE_MAX_SEED)
            || self.max_response_bytes < 1024
        {
            return Err(InferenceError::InvalidRequest);
        }
        Ok(())
    }
    /// Request-derived meter ceilings: `n` images, one request, zero elsewhere.
    pub fn unit_ceilings(&self) -> MeterUsage {
        MeterUsage {
            output_images: Some(u64::from(self.n)),
            input_characters: Some(0),
            input_audio_seconds_ms: Some(0),
            output_audio_seconds_ms: Some(0),
            search_units: Some(0),
            requests: Some(1),
            output_video_seconds_ms: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageMediaType {
    Png,
    Jpeg,
    Webp,
}
impl ImageMediaType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [Self::Png, Self::Jpeg, Self::Webp]
            .into_iter()
            .find(|t| t.as_str().eq_ignore_ascii_case(s))
    }
    /// OpenAI `output_format` spelling.
    pub fn parse_format(s: &str) -> Option<Self> {
        Some(match s {
            "png" => Self::Png,
            "jpeg" | "jpg" => Self::Jpeg,
            "webp" => Self::Webp,
            _ => return None,
        })
    }
    /// Identify image bytes by magic number.
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
            Some(Self::Png)
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(Self::Jpeg)
        } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            Some(Self::Webp)
        } else {
            None
        }
    }
}

/// Raw base64 image bytes (never a URL the gateway would have to host).
pub struct GeneratedImage {
    pub b64_json: String,
    pub media_type: ImageMediaType,
    pub revised_prompt: Option<String>,
}
pub struct ImageResponse {
    pub created: u64,
    pub images: Vec<GeneratedImage>,
    pub usage: Usage,
}
impl ImageResponse {
    /// Exactly `n` images with matching magic, bounded revised prompts, and
    /// meters reporting `n` images from one request within the ceilings.
    pub fn valid_for(&self, request: &ImageRequest) -> bool {
        let ceilings = request.unit_ceilings();
        self.images.len() == request.n as usize
            && self.images.iter().all(|i| {
                sniff_base64_prefix(&i.b64_json) == Some(i.media_type)
                    && i.b64_json.len() % 4 == 0
                    && i.revised_prompt
                        .as_ref()
                        .is_none_or(|p| p.len() <= IMAGE_MAX_REVISED_PROMPT_BYTES)
            })
            && self.usage.meters.is_some_and(|m| {
                m.output_images == Some(u64::from(request.n))
                    && m.requests == Some(1)
                    && m.counts()
                        .into_iter()
                        .zip(ceilings.counts())
                        .all(|(n, c)| c.is_none_or(|c| n.is_none_or(|n| n <= c)))
            })
    }
}

fn base64_value(c: u8) -> Option<u32> {
    Some(match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    } as u32)
}
/// Strict, canonical standard-alphabet base64 with required padding and no
/// whitespace. Returns `None` for anything else.
pub fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let groups = bytes.len() / 4;
    let mut out = Vec::with_capacity(groups * 3);
    for (i, chunk) in bytes.as_chunks::<4>().0.iter().enumerate() {
        let pad = if i + 1 == groups {
            chunk.iter().rev().take_while(|b| **b == b'=').count()
        } else {
            0
        };
        if pad > 2 {
            return None;
        }
        let mut v = [0u32; 4];
        for j in 0..4 - pad {
            v[j] = base64_value(chunk[j])?;
        }
        if (pad == 1 && v[2] & 0b11 != 0) || (pad == 2 && v[1] & 0b1111 != 0) {
            return None;
        }
        let n = (v[0] << 18) | (v[1] << 12) | (v[2] << 6) | v[3];
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}
/// Media type from the first decoded bytes, without decoding the whole image.
pub fn sniff_base64_prefix(s: &str) -> Option<ImageMediaType> {
    let prefix = if s.len() > 16 { s.get(..16)? } else { s };
    ImageMediaType::sniff(&decode_base64(prefix)?)
}

#[async_trait]
impl Workload for ImageRequest {
    type Response = ImageResponse;
    const PROTOCOL: ApiProtocol = ApiProtocol::Images;
    fn model(&self) -> &str {
        &self.model
    }
    fn validate(&self) -> Result<()> {
        ImageRequest::validate(self)
    }
    fn admission(&self) -> WorkloadAdmission {
        WorkloadAdmission {
            kind: WorkloadKind::Images,
            // Image output tokens (gpt-image) cannot be bounded by the
            // request, so the pinned price's output ceiling is reserved.
            output: OutputReservation::PriceCeiling,
            unit_ceilings: self.unit_ceilings(),
        }
    }
    fn supported_by(&self, adapter: &dyn ProviderAdapter, target: &Deployment) -> bool {
        adapter.supports_image_request(target, self)
    }
    async fn dispatch(
        self,
        adapter: &dyn ProviderAdapter,
        target: &Deployment,
    ) -> Result<ImageResponse> {
        adapter.execute_images(target, self).await
    }
    fn usage(response: &ImageResponse) -> Usage {
        response.usage
    }
    fn valid_response(&self, response: &ImageResponse) -> bool {
        response.valid_for(self)
    }
}

#[cfg(test)]
mod engine_tests;
#[cfg(test)]
pub(crate) mod fixtures {
    /// 1×1 PNG and JPEG, base64 (tiny real images).
    pub const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
    pub const JPEG_1X1: &str = "/9j/4AAQSkZJRgABAQAASABIAAD/4QBMRXhpZgAATU0AKgAAAAgAAYdpAAQAAAABAAAAGgAAAAAAA6ABAAMAAAABAAEAAKACAAQAAAABAAAAAaADAAQAAAABAAAAAQAAAAD/7QA4UGhvdG9zaG9wIDMuMAA4QklNBAQAAAAAAAA4QklNBCUAAAAAABDUHYzZjwCyBOmACZjs+EJ+/8AAEQgAAQABAwEiAAIRAQMRAf/EAB8AAAEFAQEBAQEBAAAAAAAAAAABAgMEBQYHCAkKC//EALUQAAIBAwMCBAMFBQQEAAABfQECAwAEEQUSITFBBhNRYQcicRQygZGhCCNCscEVUtHwJDNicoIJChYXGBkaJSYnKCkqNDU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6g4SFhoeIiYqSk5SVlpeYmZqio6Slpqeoqaqys7S1tre4ubrCw8TFxsfIycrS09TV1tfY2drh4uPk5ebn6Onq8fLz9PX29/j5+v/EAB8BAAMBAQEBAQEBAQEAAAAAAAABAgMEBQYHCAkKC//EALURAAIBAgQEAwQHBQQEAAECdwABAgMRBAUhMQYSQVEHYXETIjKBCBRCkaGxwQkjM1LwFWJy0QoWJDThJfEXGBkaJicoKSo1Njc4OTpDREVGR0hJSlNUVVZXWFlaY2RlZmdoaWpzdHV2d3h5eoKDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uLj5OXm5+jp6vLz9PX29/j5+v/bAEMAAgICAgICAwICAwUDAwMFBgUFBQUGCAYGBgYGCAoICAgICAgKCgoKCgoKCgwMDAwMDA4ODg4ODw8PDw8PDw8PD//bAEMBAgICBAQEBwQEBxALCQsQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEP/dAAQAAf/aAAwDAQACEQMRAD8A+L6KKK/lM/38P//Z";
    pub const WEBP_PREFIX: &str = "UklGRhoAAABXRUJQVlA4TA0AAAAvAAAAEAcQERGIiP4HAA==";
    /// Standard padded base64 (test-only; the gateway never re-encodes images).
    pub fn encode_base64(bytes: &[u8]) -> String {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for c in bytes.chunks(3) {
            let n = (u32::from(c[0]) << 16)
                | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
                | u32::from(*c.get(2).unwrap_or(&0));
            for i in 0..4 {
                if i <= c.len() {
                    out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{fixtures::*, *};

    fn request() -> ImageRequest {
        ImageRequest {
            model: "company/image".into(),
            prompt: "a red dot".into(),
            n: 1,
            size: None,
            quality: None,
            seed: None,
            max_response_bytes: 20 * 1024 * 1024,
        }
    }
    #[test]
    fn request_bounds() {
        assert!(request().validate().is_ok());
        for bad in [
            ImageRequest { n: 0, ..request() },
            ImageRequest {
                n: IMAGE_MAX_N + 1,
                ..request()
            },
            ImageRequest {
                prompt: " ".into(),
                ..request()
            },
            ImageRequest {
                prompt: "é".repeat(IMAGE_MAX_PROMPT_CHARS + 1),
                ..request()
            },
            ImageRequest {
                model: String::new(),
                ..request()
            },
            ImageRequest {
                seed: Some(IMAGE_MAX_SEED + 1),
                ..request()
            },
        ] {
            assert_eq!(bad.validate(), Err(InferenceError::InvalidRequest));
        }
        assert!(
            ImageRequest {
                prompt: "é".repeat(IMAGE_MAX_PROMPT_CHARS),
                n: IMAGE_MAX_N,
                ..request()
            }
            .validate()
            .is_ok()
        );
        let a = request().admission();
        assert_eq!(a.kind, WorkloadKind::Images);
        assert_eq!(a.output, OutputReservation::PriceCeiling);
        assert_eq!(
            a.unit_ceilings.counts(),
            [Some(1), Some(0), Some(0), Some(0), Some(0), Some(1), None]
        );
    }
    #[test]
    fn sizes_and_tiers() {
        assert_eq!(ImageSize::parse("auto"), Some(ImageSize::Auto));
        assert_eq!(
            ImageSize::parse("1024x1536"),
            Some(ImageSize::Pixels {
                width: 1024,
                height: 1536
            })
        );
        assert_eq!(ImageSize::parse("1k"), Some(ImageSize::Tier(ImageTier::K1)));
        assert_eq!(
            ImageSize::parse("1.5K").map(ImageSize::to_wire).as_deref(),
            Some("1.5K")
        );
        assert_eq!(ImageTier::K4.variant(), "4k");
        for bad in [
            "",
            "0x10",
            "01024x1024",
            "1024X1024",
            "1024x",
            "x",
            "99999x1",
            "3K",
            "1024 x 1024",
        ] {
            assert_eq!(ImageSize::parse(bad), None, "{bad}");
        }
    }
    #[test]
    fn base64_and_magic() {
        assert_eq!(decode_base64("aGk="), Some(b"hi".to_vec()));
        assert_eq!(decode_base64("aGVsbG8="), Some(b"hello".to_vec()));
        assert_eq!(decode_base64("YWJj"), Some(b"abc".to_vec()));
        for bad in [
            "",
            "aGk",
            "aG=k",
            "aGk=\n",
            "a===",
            "aGl=",
            "aGVsbG9=",
            "a-k=",
            "data:image/png;base64,aGk=",
        ] {
            assert_eq!(decode_base64(bad), None, "{bad}");
        }
        for (b64, kind) in [
            (PNG_1X1, ImageMediaType::Png),
            (JPEG_1X1, ImageMediaType::Jpeg),
            (WEBP_PREFIX, ImageMediaType::Webp),
        ] {
            assert_eq!(
                ImageMediaType::sniff(&decode_base64(b64).unwrap()),
                Some(kind)
            );
            assert_eq!(sniff_base64_prefix(b64), Some(kind));
        }
        assert_eq!(sniff_base64_prefix("aGVsbG8gd29ybGQh"), None);
        for len in 1..12 {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 250) as u8).collect();
            assert_eq!(decode_base64(&encode_base64(&bytes)), Some(bytes));
        }
        assert_eq!(
            ImageMediaType::parse("IMAGE/PNG"),
            Some(ImageMediaType::Png)
        );
        assert_eq!(ImageMediaType::parse("image/svg+xml"), None);
    }
    #[test]
    fn response_validation() {
        let meters = |n| MeterUsage {
            output_images: Some(n),
            requests: Some(1),
            ..request().unit_ceilings()
        };
        let image = |b64: &str, t| GeneratedImage {
            b64_json: b64.into(),
            media_type: t,
            revised_prompt: None,
        };
        let ok = ImageResponse {
            created: 1,
            images: vec![image(PNG_1X1, ImageMediaType::Png)],
            usage: Usage {
                meters: Some(meters(1)),
                ..Default::default()
            },
        };
        assert!(ok.valid_for(&request()));
        let two = ImageRequest { n: 2, ..request() };
        assert!(!ok.valid_for(&two));
        let mismatched = ImageResponse {
            images: vec![image(PNG_1X1, ImageMediaType::Jpeg)],
            ..ok
        };
        assert!(!mismatched.valid_for(&request()));
        let no_meters = ImageResponse {
            images: vec![image(JPEG_1X1, ImageMediaType::Jpeg)],
            usage: Usage::default(),
            created: 1,
        };
        assert!(!no_meters.valid_for(&request()));
        let extra = ImageResponse {
            images: vec![image(JPEG_1X1, ImageMediaType::Jpeg)],
            usage: Usage {
                meters: Some(MeterUsage {
                    search_units: Some(1),
                    ..meters(1)
                }),
                ..Default::default()
            },
            created: 1,
        };
        assert!(!extra.valid_for(&request()));
    }
}
