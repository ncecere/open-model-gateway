//! Server-side upload duration from container structure only (no decoding).
//!
//! Returns milliseconds rounded up, or `None` when the duration cannot be
//! determined. Measurements are conservative upper bounds where the format
//! allows one:
//! - **WAV**: PCM/float/A-law/µ-law only, with a self-consistent `fmt ` chunk;
//!   every byte after the `data` header counts as audio (≥ the declared size).
//! - **MP3**: walks every MPEG audio frame header to the end (ID3v2/ID3v1/APE
//!   tags skipped); any junk between frames makes the duration unknown, so
//!   hidden frames cannot be smuggled past the count.
//! - **Ogg** (Vorbis/Opus): walks every page with CRC verification, a single
//!   logical stream, exact end-of-file, and takes the highest granule.
//! - **FLAC**: the STREAMINFO total sample count. This is a declared value: a
//!   crafted file can understate it. The provider's reported duration is then
//!   above the admission ceiling, so the response is withheld and the attempt
//!   fails with the observed usage retained (never under-recorded).
//!
//! MP4/M4A and WebM are not measured.
use super::AudioFormat;

pub fn measure(format: AudioFormat, bytes: &[u8]) -> Option<u64> {
    match format {
        AudioFormat::Wav => wav(bytes),
        AudioFormat::Mp3 => mp3(bytes),
        AudioFormat::Flac => flac(bytes),
        AudioFormat::Ogg => ogg(bytes),
        AudioFormat::Mp4 | AudioFormat::Webm => None,
    }
}

fn ms(units: u64, per_second: u64) -> Option<u64> {
    if per_second == 0 || units == 0 {
        return None;
    }
    u64::try_from((u128::from(units) * 1000).div_ceil(u128::from(per_second))).ok()
}
fn u16le(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn u32le(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn u64le(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

fn wav(b: &[u8]) -> Option<u64> {
    if b.get(0..4)? != b"RIFF" || b.get(8..12)? != b"WAVE" {
        return None;
    }
    let mut pos = 12usize;
    let mut byte_rate = None;
    while pos.checked_add(8)? <= b.len() {
        let id = &b[pos..pos + 4];
        let size = u32le(b, pos + 4)? as usize;
        let body = pos + 8;
        if id == b"fmt " {
            let f = b.get(body..body.checked_add(size)?)?;
            if size < 16 || byte_rate.is_some() {
                return None;
            }
            let mut tag = u16le(f, 0)?;
            let channels = u32::from(u16le(f, 2)?);
            let rate = u64::from(u32le(f, 4)?);
            let declared = u64::from(u32le(f, 8)?);
            let align = u32::from(u16le(f, 12)?);
            let bits = u32::from(u16le(f, 14)?);
            if tag == 0xFFFE {
                // WAVE_FORMAT_EXTENSIBLE: the subformat GUID starts with the tag.
                tag = u16le(f, 24)?;
            }
            if !matches!(tag, 1 | 3 | 6 | 7)
                || channels == 0
                || rate == 0
                || bits == 0
                || align != channels * bits.div_ceil(8)
                || declared != rate * u64::from(align)
            {
                return None;
            }
            byte_rate = Some(declared);
        } else if id == b"data" {
            // All remaining bytes bound what any decoder can read.
            return ms((b.len() - body) as u64, byte_rate?);
        }
        pos = body.checked_add(size)?.checked_add(size & 1)?;
    }
    None
}

/// Total length of leading ID3v2 tags, if any.
pub(super) fn id3_len(b: &[u8]) -> Option<usize> {
    let mut pos = 0usize;
    while b.get(pos..pos + 3) == Some(b"ID3") {
        let h = b.get(pos..pos + 10)?;
        if h[6..10].iter().any(|x| x & 0x80 != 0) {
            return None;
        }
        let size = h[6..10]
            .iter()
            .fold(0usize, |n, x| (n << 7) | usize::from(*x));
        let footer = if h[5] & 0x10 != 0 { 10 } else { 0 };
        pos = pos.checked_add(10 + size + footer)?;
    }
    Some(pos)
}

/// `(frame bytes, samples, sample rate)` for an MPEG audio frame header.
fn mpeg_frame(h: &[u8]) -> Option<(usize, u64, u64)> {
    if h.len() < 4 || h[0] != 0xFF || h[1] & 0xE0 != 0xE0 {
        return None;
    }
    let version = (h[1] >> 3) & 3; // 0: MPEG-2.5, 2: MPEG-2, 3: MPEG-1
    let layer = (h[1] >> 1) & 3; // 1: III, 2: II, 3: I
    let bitrate = usize::from(h[2] >> 4);
    let rate = usize::from((h[2] >> 2) & 3);
    let pad = usize::from((h[2] >> 1) & 1);
    if version == 1 || layer == 0 || bitrate == 0 || bitrate == 15 || rate == 3 {
        return None;
    }
    const V1_L1: [usize; 15] = [
        0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
    ];
    const V1_L2: [usize; 15] = [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
    ];
    const V1_L3: [usize; 15] = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ];
    const V2_L1: [usize; 15] = [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
    ];
    const V2_L23: [usize; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let v1 = version == 3;
    let kbps = match (v1, layer) {
        (true, 3) => V1_L1,
        (true, 2) => V1_L2,
        (true, _) => V1_L3,
        (false, 3) => V2_L1,
        (false, _) => V2_L23,
    }[bitrate];
    let base = [44_100usize, 48_000, 32_000][rate];
    let rate = match version {
        3 => base,
        2 => base / 2,
        _ => base / 4,
    };
    let bps = kbps * 1000;
    let (len, samples) = match layer {
        3 => ((12 * bps / rate + pad) * 4, 384),
        2 => (144 * bps / rate + pad, 1152),
        _ if v1 => (144 * bps / rate + pad, 1152),
        _ => (72 * bps / rate + pad, 576),
    };
    Some((len, samples, rate as u64))
}

fn mp3(b: &[u8]) -> Option<u64> {
    let mut pos = id3_len(b)?;
    let mut end = b.len();
    if end >= pos + 128 && &b[end - 128..end - 125] == b"TAG" {
        end -= 128;
    }
    if end >= pos + 32 && &b[end - 32..end - 24] == b"APETAGEX" {
        let size = u32le(b, end - 20)? as usize;
        let header = if u32le(b, end - 12)? & 0x8000_0000 != 0 {
            32
        } else {
            0
        };
        end = end.checked_sub(size)?.checked_sub(header)?;
        if end < pos {
            return None;
        }
    }
    let mut samples = 0u64;
    let mut rate = None;
    while pos < end {
        let Some((len, n, r)) = b.get(pos..end).and_then(mpeg_frame) else {
            // Only zero padding may follow the last frame.
            if b[pos..end].iter().all(|x| *x == 0) {
                break;
            }
            return None;
        };
        if rate.is_some_and(|x| x != r) {
            return None;
        }
        rate = Some(r);
        samples = samples.checked_add(n)?;
        // A truncated final frame still counts (conservative).
        pos = pos.checked_add(len)?;
    }
    ms(samples, rate?)
}

fn flac(b: &[u8]) -> Option<u64> {
    let pos = id3_len(b)?;
    if b.get(pos..pos + 4)? != b"fLaC" {
        return None;
    }
    let h = b.get(pos + 4..pos + 8)?;
    let len = (usize::from(h[1]) << 16) | (usize::from(h[2]) << 8) | usize::from(h[3]);
    if h[0] & 0x7F != 0 || len != 34 {
        return None;
    }
    let info = b.get(pos + 8..pos + 8 + 34)?;
    let packed = u64::from_be_bytes(info[10..18].try_into().ok()?);
    let rate = packed >> 44;
    let total = packed & ((1 << 36) - 1);
    ms(total, rate)
}

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut r = (i as u32) << 24;
        let mut j = 0;
        while j < 8 {
            r = if r & 0x8000_0000 != 0 {
                (r << 1) ^ 0x04C1_1DB7
            } else {
                r << 1
            };
            j += 1;
        }
        table[i] = r;
        i += 1;
    }
    table
}
static CRC: [u32; 256] = crc_table();
fn crc(mut c: u32, data: &[u8]) -> u32 {
    for &x in data {
        c = (c << 8) ^ CRC[usize::from((c >> 24) as u8 ^ x)];
    }
    c
}
/// Ogg page CRC (checksum field taken as zero).
pub(super) fn ogg_crc(page: &[u8]) -> u32 {
    crc(crc(crc(0, &page[..22]), &[0; 4]), &page[26..])
}

fn ogg(b: &[u8]) -> Option<u64> {
    let mut pos = 0usize;
    let mut serial = None;
    let mut rate = None;
    let mut preskip = 0u64;
    let mut granule: Option<u64> = None;
    while pos < b.len() {
        let h = b.get(pos..pos + 27)?;
        if &h[..4] != b"OggS" || h[4] != 0 {
            return None;
        }
        let segments = usize::from(h[26]);
        let table = b.get(pos + 27..pos + 27 + segments)?;
        let body = pos + 27 + segments;
        let end = body + table.iter().map(|x| usize::from(*x)).sum::<usize>();
        let page = b.get(pos..end)?;
        if ogg_crc(page) != u32le(h, 22)? {
            return None;
        }
        let s = u32le(h, 14)?;
        if serial.is_some_and(|x| x != s) {
            return None;
        }
        if serial.is_none() {
            let payload = &b[body..end];
            if h[5] & 0x02 == 0 {
                return None;
            }
            if payload.starts_with(b"\x01vorbis") {
                rate = Some(u64::from(u32le(payload, 12)?));
            } else if payload.starts_with(b"OpusHead") {
                // Opus granule positions always count 48 kHz samples.
                rate = Some(48_000);
                preskip = u64::from(u16le(payload, 10)?);
            } else {
                return None;
            }
        }
        serial = Some(s);
        let g = u64le(h, 6)?;
        if g != u64::MAX {
            if g > i64::MAX as u64 {
                return None;
            }
            granule = Some(granule.map_or(g, |x| x.max(g)));
        }
        pos = end;
    }
    ms(granule?.saturating_sub(preskip), rate?)
}
