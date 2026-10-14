//! radiod's wire format: status TLV packets, RTP framing, IQ payloads and
//! RTP sequence tracking (MAN-140).
//!
//! Every constant here is a protocol fact recorded from ka9q-radio's own
//! documentation and headers at `main` `f44b3a3ee8dd` (2026-10-12):
//! `docs/ka9q-api.md` (TLV layout), `src/status.h` (`enum status_type`),
//! `src/rtp.h` (`enum encoding`, default ports). No ka9q-radio code is
//! copied or translated (GPL-3.0; see
//! docs/DECISIONS/2026-10-13-man140-ka9q-radio-input.md, D1). The encoders
//! exist for the tests' fake radiod; production code only decodes.

use anyhow::{bail, Result};
use num_complex::Complex32;
use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// radiod's default RTP data port (`DEFAULT_RTP_PORT`, `src/rtp.h`).
pub const DATA_PORT: u16 = 5004;
/// radiod's default status port (`DEFAULT_STAT_PORT`, `src/rtp.h`): a
/// channel's status goes to its own data group on this port.
pub const STATUS_PORT: u16 = 5006;

/// Status packet type byte (`enum pkt_type`, `src/status.h`).
const PKT_STATUS: u8 = 0;

/// Tag values from `enum status_type` (`src/status.h`); the same value at
/// every commit touching that enum since 2025-09-02.
pub mod tag {
    pub const EOL: u8 = 0;
    pub const OUTPUT_DATA_DEST_SOCKET: u8 = 17;
    pub const OUTPUT_SSRC: u8 = 18;
    pub const OUTPUT_SAMPRATE: u8 = 20;
    pub const RADIO_FREQUENCY: u8 = 33;
    pub const LOW_EDGE: u8 = 39;
    pub const HIGH_EDGE: u8 = 40;
    pub const OUTPUT_CHANNELS: u8 = 49;
    pub const INDEPENDENT_SIDEBAND: u8 = 50;
    pub const ENVELOPE: u8 = 56;
    pub const AGC_ENABLE: u8 = 62;
    pub const RTP_PT: u8 = 105;
    pub const OUTPUT_ENCODING: u8 = 107;
}

/// The sample encodings manta reads (values from `enum encoding`,
/// `src/rtp.h`, stable since 2026-01-10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    S16Le,
    S16Be,
    F16Le,
    F16Be,
    F32Le,
    F32Be,
}

impl Encoding {
    pub fn from_wire(v: u64) -> Option<Self> {
        match v {
            1 => Some(Self::S16Le),
            2 => Some(Self::S16Be),
            4 => Some(Self::F32Le),
            6 => Some(Self::F16Le),
            8 => Some(Self::F32Be),
            9 => Some(Self::F16Be),
            _ => None,
        }
    }

    pub fn wire_value(self) -> u64 {
        match self {
            Self::S16Le => 1,
            Self::S16Be => 2,
            Self::F32Le => 4,
            Self::F16Le => 6,
            Self::F32Be => 8,
            Self::F16Be => 9,
        }
    }

    pub fn bytes_per_component(self) -> usize {
        match self {
            Self::S16Le | Self::S16Be | Self::F16Le | Self::F16Be => 2,
            Self::F32Le | Self::F32Be => 4,
        }
    }
}

/// radiod's name for an encoding value, for error messages.
pub fn encoding_name(v: u64) -> String {
    match v {
        0 => "none".into(),
        1 => "s16le".into(),
        2 => "s16be".into(),
        3 => "opus".into(),
        4 => "f32le".into(),
        5 => "ax25".into(),
        6 => "f16le".into(),
        7 => "opus-voip".into(),
        8 => "f32be".into(),
        9 => "f16be".into(),
        10 => "mulaw".into(),
        11 => "alaw".into(),
        n => format!("encoding {n}"),
    }
}

/// The fields of one channel's status packet that manta reads.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChannelStatus {
    pub ssrc: Option<u32>,
    pub sample_rate_hz: Option<u64>,
    pub radio_frequency_hz: Option<f64>,
    pub low_edge_hz: Option<f64>,
    pub high_edge_hz: Option<f64>,
    pub channels: Option<u64>,
    pub isb: Option<bool>,
    pub envelope: Option<bool>,
    pub agc: Option<bool>,
    pub rtp_pt: Option<u8>,
    pub encoding: Option<u64>,
    pub data_dest: Option<SocketAddr>,
}

fn decode_int(v: &[u8]) -> Result<u64> {
    if v.len() > 8 {
        bail!("integer of {} bytes", v.len());
    }
    Ok(v.iter().fold(0u64, |a, &x| (a << 8) | u64::from(x)))
}

fn decode_float(v: &[u8]) -> Result<f64> {
    match v.len() {
        0 => Ok(0.0),
        4 => Ok(f64::from(f32::from_be_bytes([v[0], v[1], v[2], v[3]]))),
        8 => {
            let mut b = [0u8; 8];
            b.copy_from_slice(v);
            Ok(f64::from_be_bytes(b))
        }
        n => bail!("float of {n} bytes"),
    }
}

fn decode_socket(v: &[u8]) -> Result<Option<SocketAddr>> {
    match v.len() {
        0 => Ok(None),
        6 => {
            let ip = Ipv4Addr::new(v[0], v[1], v[2], v[3]);
            Ok(Some(SocketAddr::new(
                IpAddr::V4(ip),
                u16::from_be_bytes([v[4], v[5]]),
            )))
        }
        18 => {
            let mut b = [0u8; 16];
            b.copy_from_slice(&v[..16]);
            Ok(Some(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::from(b)),
                u16::from_be_bytes([v[16], v[17]]),
            )))
        }
        n => bail!("socket of {n} bytes"),
    }
}

/// Parse a radiod status packet. `Ok(None)` for a command (non-status)
/// packet; `Err` for a malformed one.
pub fn parse_status_packet(p: &[u8]) -> Result<Option<ChannelStatus>> {
    let Some(&kind) = p.first() else {
        bail!("empty status packet");
    };
    if kind != PKT_STATUS {
        return Ok(None);
    }
    let mut s = ChannelStatus::default();
    let mut i = 1;
    while i < p.len() {
        let t = p[i];
        i += 1;
        if t == tag::EOL {
            break;
        }
        let Some(&l) = p.get(i) else {
            bail!("tag {t} has no length");
        };
        i += 1;
        let mut len = usize::from(l);
        if l & 0x80 != 0 {
            let n = usize::from(l & 0x7f);
            if n == 0 || n > 4 {
                bail!("tag {t}: extended length of {n} bytes");
            }
            let Some(bytes) = p.get(i..i + n) else {
                bail!("tag {t}: truncated extended length");
            };
            len = bytes.iter().fold(0usize, |a, &x| (a << 8) | usize::from(x));
            i += n;
        }
        let Some(v) = i.checked_add(len).and_then(|end| p.get(i..end)) else {
            bail!("tag {t}: value of {len} bytes runs past the packet end");
        };
        i += len;
        match t {
            tag::OUTPUT_SSRC => {
                let x = decode_int(v)?;
                s.ssrc = Some(u32::try_from(x).map_err(|_| anyhow::anyhow!("ssrc {x}"))?);
            }
            tag::OUTPUT_SAMPRATE => s.sample_rate_hz = Some(decode_int(v)?),
            tag::RADIO_FREQUENCY => s.radio_frequency_hz = Some(decode_float(v)?),
            tag::LOW_EDGE => s.low_edge_hz = Some(decode_float(v)?),
            tag::HIGH_EDGE => s.high_edge_hz = Some(decode_float(v)?),
            tag::OUTPUT_CHANNELS => s.channels = Some(decode_int(v)?),
            tag::INDEPENDENT_SIDEBAND => s.isb = Some(decode_int(v)? != 0),
            tag::ENVELOPE => s.envelope = Some(decode_int(v)? != 0),
            tag::AGC_ENABLE => s.agc = Some(decode_int(v)? != 0),
            tag::RTP_PT => s.rtp_pt = Some((decode_int(v)? & 0x7f) as u8),
            tag::OUTPUT_ENCODING => s.encoding = Some(decode_int(v)?),
            tag::OUTPUT_DATA_DEST_SOCKET => s.data_dest = decode_socket(v)?,
            _ => {}
        }
    }
    Ok(Some(s))
}

fn push_tlv(out: &mut Vec<u8>, t: u8, v: &[u8]) {
    out.push(t);
    let len = v.len();
    if len < 128 {
        out.push(len as u8);
    } else {
        let bytes = (len as u32).to_be_bytes();
        let skip = bytes.iter().take_while(|&&b| b == 0).count();
        out.push(0x80 | (4 - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(v);
}

fn int_bytes(x: u64) -> Vec<u8> {
    let b = x.to_be_bytes();
    let skip = b.iter().take_while(|&&v| v == 0).count();
    b[skip..].to_vec()
}

/// Encode a status packet (for tests' fake radiod).
pub fn encode_status_packet(s: &ChannelStatus) -> Vec<u8> {
    let mut out = vec![PKT_STATUS];
    if let Some(d) = s.data_dest {
        let mut v = Vec::new();
        match d.ip() {
            IpAddr::V4(a) => v.extend_from_slice(&a.octets()),
            IpAddr::V6(a) => v.extend_from_slice(&a.octets()),
        }
        v.extend_from_slice(&d.port().to_be_bytes());
        push_tlv(&mut out, tag::OUTPUT_DATA_DEST_SOCKET, &v);
    }
    if let Some(x) = s.ssrc {
        push_tlv(&mut out, tag::OUTPUT_SSRC, &int_bytes(u64::from(x)));
    }
    if let Some(x) = s.sample_rate_hz {
        push_tlv(&mut out, tag::OUTPUT_SAMPRATE, &int_bytes(x));
    }
    if let Some(x) = s.radio_frequency_hz {
        push_tlv(&mut out, tag::RADIO_FREQUENCY, &x.to_be_bytes());
    }
    if let Some(x) = s.low_edge_hz {
        push_tlv(&mut out, tag::LOW_EDGE, &(x as f32).to_be_bytes());
    }
    if let Some(x) = s.high_edge_hz {
        push_tlv(&mut out, tag::HIGH_EDGE, &(x as f32).to_be_bytes());
    }
    if let Some(x) = s.channels {
        push_tlv(&mut out, tag::OUTPUT_CHANNELS, &int_bytes(x));
    }
    for (t, b) in [
        (tag::INDEPENDENT_SIDEBAND, s.isb),
        (tag::ENVELOPE, s.envelope),
        (tag::AGC_ENABLE, s.agc),
    ] {
        if let Some(b) = b {
            push_tlv(&mut out, t, &int_bytes(u64::from(b)));
        }
    }
    if let Some(x) = s.rtp_pt {
        push_tlv(&mut out, tag::RTP_PT, &int_bytes(u64::from(x)));
    }
    if let Some(x) = s.encoding {
        push_tlv(&mut out, tag::OUTPUT_ENCODING, &int_bytes(x));
    }
    out.push(tag::EOL);
    out
}

/// The RTP fixed header fields manta reads (RFC 3550 §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    pub payload_type: u8,
    pub seq: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub marker: bool,
}

/// Parse an RTP datagram: the header, and the payload with CSRCs, header
/// extension and padding removed.
pub fn parse_rtp(p: &[u8]) -> Result<(RtpHeader, &[u8])> {
    if p.len() < 12 {
        bail!(
            "RTP datagram of {} bytes is shorter than its header",
            p.len()
        );
    }
    let version = p[0] >> 6;
    if version != 2 {
        bail!("RTP version {version}");
    }
    let padding = p[0] & 0x20 != 0;
    let extension = p[0] & 0x10 != 0;
    let cc = usize::from(p[0] & 0x0f);
    let h = RtpHeader {
        marker: p[1] & 0x80 != 0,
        payload_type: p[1] & 0x7f,
        seq: u16::from_be_bytes([p[2], p[3]]),
        timestamp: u32::from_be_bytes([p[4], p[5], p[6], p[7]]),
        ssrc: u32::from_be_bytes([p[8], p[9], p[10], p[11]]),
    };
    let mut start = 12 + 4 * cc;
    if extension {
        let Some(ext) = p.get(start..start + 4) else {
            bail!("RTP header extension runs past the datagram");
        };
        start += 4 + 4 * usize::from(u16::from_be_bytes([ext[2], ext[3]]));
    }
    let mut end = p.len();
    if start > end {
        bail!("RTP header runs past the datagram");
    }
    if padding {
        let pad = usize::from(p[end - 1]);
        if pad == 0 || pad > end - start {
            bail!("RTP padding of {pad} bytes is longer than the payload");
        }
        end -= pad;
    }
    Ok((h, &p[start..end]))
}

/// Build an RTP datagram (version 2, no CSRCs, extension or padding).
pub fn encode_rtp(h: &RtpHeader, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + payload.len());
    out.push(0x80);
    out.push((u8::from(h.marker) << 7) | (h.payload_type & 0x7f));
    out.extend_from_slice(&h.seq.to_be_bytes());
    out.extend_from_slice(&h.timestamp.to_be_bytes());
    out.extend_from_slice(&h.ssrc.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// IEEE-754 binary16 to f32, bit by bit (`f16` is not stable in std).
pub(crate) fn f16_to_f32(h: u16) -> f32 {
    let sign = u32::from(h >> 15) << 31;
    let exp = u32::from((h >> 10) & 0x1f);
    let frac = u32::from(h & 0x3ff);
    let bits = match (exp, frac) {
        (0, 0) => sign,
        (0, f) => {
            // Subnormal: value = f * 2^-24.
            let v = f as f32 * f32::from_bits(0x3380_0000); // 2^-24
            return if sign != 0 { -v } else { v };
        }
        (0x1f, 0) => sign | 0x7f80_0000,
        (0x1f, f) => sign | 0x7fc0_0000 | (f << 13),
        (e, f) => sign | ((e + 112) << 23) | (f << 13),
    };
    f32::from_bits(bits)
}

/// f32 to binary16 (round toward zero; for the tests' encoder only).
pub(crate) fn f32_to_f16(x: f32) -> u16 {
    let b = x.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    if x.is_nan() {
        return sign | 0x7e00;
    }
    let a = x.abs();
    if a >= 65520.0 {
        return sign | 0x7c00;
    }
    if a < f32::from_bits(0x3880_0000) {
        // Below 2^-14: subnormal half.
        return sign | (a / f32::from_bits(0x3380_0000)).round() as u16;
    }
    let exp = ((b >> 23) & 0xff) as i32 - 127 + 15;
    let frac = ((b >> 13) & 0x3ff) as u16;
    sign | ((exp as u16) << 10) | frac
}

/// Decode an IQ payload (interleaved I, Q) into `out`, returning the number
/// of samples appended. A payload that is not a whole number of IQ pairs,
/// or holds any non-finite sample, is rejected whole and appends nothing.
pub fn decode_iq(enc: Encoding, payload: &[u8], out: &mut VecDeque<Complex32>) -> Result<usize> {
    let w = enc.bytes_per_component();
    let pair = 2 * w;
    if !payload.len().is_multiple_of(pair) {
        bail!(
            "payload of {} bytes is not a whole number of {}-byte IQ pairs",
            payload.len(),
            pair
        );
    }
    let comp = |c: &[u8]| -> f32 {
        match enc {
            Encoding::S16Le => f32::from(i16::from_le_bytes([c[0], c[1]])) / 32768.0,
            Encoding::S16Be => f32::from(i16::from_be_bytes([c[0], c[1]])) / 32768.0,
            Encoding::F16Le => f16_to_f32(u16::from_le_bytes([c[0], c[1]])),
            Encoding::F16Be => f16_to_f32(u16::from_be_bytes([c[0], c[1]])),
            Encoding::F32Le => f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
            Encoding::F32Be => f32::from_be_bytes([c[0], c[1], c[2], c[3]]),
        }
    };
    let n = payload.len() / pair;
    let start = out.len();
    for p in payload.chunks_exact(pair) {
        let s = Complex32::new(comp(&p[..w]), comp(&p[w..]));
        if !s.re.is_finite() || !s.im.is_finite() {
            out.truncate(start);
            bail!("payload holds a non-finite sample");
        }
        out.push_back(s);
    }
    Ok(n)
}

/// Encode IQ samples (for tests' fake radiod).
pub fn encode_iq(enc: Encoding, samples: &[Complex32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2 * enc.bytes_per_component());
    let s16 = |x: f32| (x * 32768.0).round().clamp(-32768.0, 32767.0) as i16;
    for s in samples {
        for x in [s.re, s.im] {
            match enc {
                Encoding::S16Le => out.extend_from_slice(&s16(x).to_le_bytes()),
                Encoding::S16Be => out.extend_from_slice(&s16(x).to_be_bytes()),
                Encoding::F16Le => out.extend_from_slice(&f32_to_f16(x).to_le_bytes()),
                Encoding::F16Be => out.extend_from_slice(&f32_to_f16(x).to_be_bytes()),
                Encoding::F32Le => out.extend_from_slice(&x.to_le_bytes()),
                Encoding::F32Be => out.extend_from_slice(&x.to_be_bytes()),
            }
        }
    }
    out
}

/// What one RTP sequence number means against the last accepted one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SeqObservation {
    /// The first packet: nothing to compare against.
    Baseline,
    InOrder,
    /// `missing` packets were lost before this one (one gap event).
    Gap {
        missing: u16,
    },
    /// A jump too large to be loss (a radiod restart): re-baselined.
    Resync,
    /// A duplicate or late packet: discard it.
    Stale,
}

/// Signed-16-bit RTP sequence tracking (D11).
#[derive(Debug, Clone)]
pub(crate) struct SeqTracker {
    last: Option<u16>,
    max_jump: u16,
}

/// How far back a packet may be and still count as late, not a restart.
const MAX_LATE: i32 = 100;

impl SeqTracker {
    pub(crate) fn new(max_jump: u16) -> Self {
        Self {
            last: None,
            max_jump: max_jump.clamp(2, 32767),
        }
    }

    pub(crate) fn observe(&mut self, seq: u16) -> SeqObservation {
        let Some(last) = self.last else {
            self.last = Some(seq);
            return SeqObservation::Baseline;
        };
        let d = i32::from(seq.wrapping_sub(last) as i16);
        if d == 1 {
            self.last = Some(seq);
            SeqObservation::InOrder
        } else if d >= 2 && d <= i32::from(self.max_jump) {
            self.last = Some(seq);
            SeqObservation::Gap {
                missing: (d - 1) as u16,
            }
        } else if (-MAX_LATE..=0).contains(&d) {
            SeqObservation::Stale
        } else {
            self.last = Some(seq);
            SeqObservation::Resync
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tlv_value(p: &[u8]) -> (u8, usize, usize) {
        // (tag, value start, value len) of the first TLV after the type byte.
        let t = p[1];
        let l = p[2];
        if l & 0x80 == 0 {
            (t, 3, usize::from(l))
        } else {
            let n = usize::from(l & 0x7f);
            let len = p[3..3 + n]
                .iter()
                .fold(0usize, |a, &x| (a << 8) | usize::from(x));
            (t, 3 + n, len)
        }
    }

    #[test]
    fn tlv_length_uses_the_extended_form_above_127() {
        // The doc's own example: 82 01 02 is 258 bytes.
        let mut p = vec![0u8, 200, 0x82, 0x01, 0x02];
        p.extend(std::iter::repeat_n(7u8, 258));
        p.push(tag::EOL);
        assert_eq!(
            parse_status_packet(&p).unwrap(),
            Some(ChannelStatus::default())
        );
        let mut out = vec![0u8];
        push_tlv(&mut out, 200, &[b'x'; 300]);
        let (t, start, len) = tlv_value(&out);
        assert_eq!((t, len), (200, 300));
        assert_eq!(&out[start..start + len], &[b'x'; 300][..]);
        assert_eq!(&out[1..5], &[200, 0x82, 0x01, 0x2c]);
    }

    #[test]
    fn integers_are_msb_first_with_leading_zeros_stripped() {
        assert_eq!(int_bytes(14_100_000), vec![0xd7, 0x26, 0x20]);
        assert_eq!(decode_int(&[0xd7, 0x26, 0x20]).unwrap(), 14_100_000);
        assert_eq!(decode_int(&[]).unwrap(), 0);
        assert!(decode_int(&[1; 9]).is_err());
    }

    #[test]
    fn floats_decode_by_length() {
        assert_eq!(
            decode_float(&14_100_000.0f64.to_be_bytes()).unwrap(),
            14_100_000.0
        );
        assert_eq!(
            decode_float(&7_000_000.0f32.to_be_bytes()).unwrap(),
            7_000_000.0
        );
        assert_eq!(decode_float(&[]).unwrap(), 0.0);
        assert!(decode_float(&[0; 5]).is_err());
        let mut p = vec![0u8];
        push_tlv(&mut p, tag::RADIO_FREQUENCY, &7_000_000.0f32.to_be_bytes());
        p.push(tag::EOL);
        let s = parse_status_packet(&p).unwrap().unwrap();
        assert_eq!(s.radio_frequency_hz, Some(7_000_000.0));
    }

    #[test]
    fn socket_decodes_ipv4_and_null() {
        let v = [239, 101, 6, 1, 0x13, 0x8c];
        assert_eq!(
            decode_socket(&v).unwrap(),
            Some("239.101.6.1:5004".parse().unwrap())
        );
        assert_eq!(decode_socket(&[]).unwrap(), None);
        let mut v6 = [0u8; 18];
        v6[0] = 0xff;
        v6[1] = 0x02;
        v6[15] = 1;
        v6[16] = 0x13;
        v6[17] = 0x8c;
        let a = decode_socket(&v6).unwrap().unwrap();
        assert!(!a.is_ipv4());
        assert_eq!(a.port(), 5004);
    }

    fn full_status() -> ChannelStatus {
        ChannelStatus {
            ssrc: Some(14_100_000),
            sample_rate_hz: Some(192_000),
            radio_frequency_hz: Some(14_100_000.0),
            low_edge_hz: Some(-90_000.0),
            high_edge_hz: Some(90_000.0),
            channels: Some(2),
            isb: Some(false),
            envelope: Some(false),
            agc: Some(true),
            rtp_pt: Some(111),
            encoding: Some(4),
            data_dest: Some("239.101.6.1:5004".parse().unwrap()),
        }
    }

    #[test]
    fn status_packet_round_trips_every_field_manta_reads() {
        let s = full_status();
        assert_eq!(
            parse_status_packet(&encode_status_packet(&s)).unwrap(),
            Some(s)
        );
    }

    #[test]
    fn unknown_tags_are_skipped() {
        let mut p = vec![0u8];
        push_tlv(&mut p, 99, b"hello");
        push_tlv(&mut p, tag::OUTPUT_SSRC, &int_bytes(42));
        p.push(tag::EOL);
        assert_eq!(parse_status_packet(&p).unwrap().unwrap().ssrc, Some(42));
    }

    #[test]
    fn parsing_stops_at_eol() {
        let mut p = vec![0u8];
        push_tlv(&mut p, tag::OUTPUT_SSRC, &int_bytes(42));
        p.push(tag::EOL);
        // Garbage after EOL is never read.
        p.extend_from_slice(&[tag::OUTPUT_SAMPRATE, 0xff]);
        let s = parse_status_packet(&p).unwrap().unwrap();
        assert_eq!(s.ssrc, Some(42));
        assert_eq!(s.sample_rate_hz, None);
    }

    #[test]
    fn truncated_or_overlong_tlv_is_an_error() {
        assert!(parse_status_packet(&[0, tag::OUTPUT_SSRC, 4, 1, 2]).is_err());
        assert!(parse_status_packet(&[0, tag::OUTPUT_SSRC, 0x85, 0, 0, 0, 0, 1, 9]).is_err());
        assert!(parse_status_packet(&[0, tag::OUTPUT_SSRC]).is_err());
        assert!(parse_status_packet(&[]).is_err());
    }

    #[test]
    fn command_packets_are_not_status() {
        assert_eq!(
            parse_status_packet(&[1, tag::OUTPUT_SSRC, 1, 5, 0]).unwrap(),
            None
        );
    }

    fn hdr() -> RtpHeader {
        RtpHeader {
            payload_type: 111,
            seq: 0x1234,
            timestamp: 0xdead_beef,
            ssrc: 14_100_000,
            marker: true,
        }
    }

    #[test]
    fn rtp_header_parses_version_pt_seq_timestamp_ssrc() {
        let p = encode_rtp(&hdr(), &[1, 2, 3, 4]);
        let (h, payload) = parse_rtp(&p).unwrap();
        assert_eq!(h, hdr());
        assert_eq!(payload, &[1, 2, 3, 4]);
    }

    #[test]
    fn rtp_skips_csrcs_extension_and_padding() {
        let mut p = encode_rtp(&hdr(), &[]);
        p[0] = 0x80 | 0x20 | 0x10 | 2; // padding, extension, 2 CSRCs
        p.extend_from_slice(&[0; 8]); // CSRCs
        p.extend_from_slice(&[0xbe, 0xde, 0, 1, 9, 9, 9, 9]); // ext, 1 word
        p.extend_from_slice(&[1, 2, 3, 4]); // payload
        p.extend_from_slice(&[0, 0, 3]); // 3 bytes of padding
        let (h, payload) = parse_rtp(&p).unwrap();
        assert_eq!(h.ssrc, 14_100_000);
        assert_eq!(payload, &[1, 2, 3, 4]);
    }

    #[test]
    fn rtp_rejects_version_other_than_2_and_short_packets() {
        let mut p = encode_rtp(&hdr(), &[1, 2, 3, 4]);
        p[0] = 0x40;
        assert!(parse_rtp(&p).is_err());
        assert!(parse_rtp(&[0x80; 11]).is_err());
        let mut p = encode_rtp(&hdr(), &[]);
        p[0] = 0x80 | 3; // 3 CSRCs that are not there
        assert!(parse_rtp(&p).is_err());
    }

    #[test]
    fn rtp_rejects_padding_longer_than_payload() {
        let mut p = encode_rtp(&hdr(), &[1, 2, 3, 9]);
        p[0] |= 0x20;
        assert!(parse_rtp(&p).is_err());
        let mut p = encode_rtp(&hdr(), &[1, 2, 3, 0]);
        p[0] |= 0x20;
        assert!(parse_rtp(&p).is_err());
    }

    const ALL: [Encoding; 6] = [
        Encoding::S16Le,
        Encoding::S16Be,
        Encoding::F16Le,
        Encoding::F16Be,
        Encoding::F32Le,
        Encoding::F32Be,
    ];

    #[test]
    fn iq_payload_decodes_i_then_q_for_every_encoding() {
        let s = vec![
            Complex32::new(0.5, -0.25),
            Complex32::new(-0.125, 0.75),
            Complex32::new(0.3, 0.001),
        ];
        for enc in ALL {
            let mut out = VecDeque::new();
            let n = decode_iq(enc, &encode_iq(enc, &s), &mut out).unwrap();
            assert_eq!(n, 3);
            for (a, b) in s.iter().zip(out.iter()) {
                let tol = match enc {
                    Encoding::F32Le | Encoding::F32Be => 0.0,
                    Encoding::S16Le | Encoding::S16Be => 1.0 / 32768.0,
                    _ => a.re.abs().max(a.im.abs()) * 2f32.powi(-10),
                };
                assert!((a.re - b.re).abs() <= tol, "{enc:?} I {a} vs {b}");
                assert!((a.im - b.im).abs() <= tol, "{enc:?} Q {a} vs {b}");
            }
        }
    }

    #[test]
    fn s16_scales_by_32768_like_wav() {
        let mut out = VecDeque::new();
        decode_iq(Encoding::S16Be, &[0x40, 0x00, 0x80, 0x00], &mut out).unwrap();
        assert_eq!(out[0], Complex32::new(0.5, -1.0));
    }

    #[test]
    fn f16_conversion_handles_subnormal_zero_inf_and_nan() {
        assert_eq!(f16_to_f32(0x0000), 0.0);
        assert!(f16_to_f32(0x8000).is_sign_negative());
        assert_eq!(f16_to_f32(0x3c00), 1.0);
        assert_eq!(f16_to_f32(0xc000), -2.0);
        assert_eq!(f16_to_f32(0x0001), 2f32.powi(-24));
        assert_eq!(f16_to_f32(0x03ff), 1023.0 * 2f32.powi(-24));
        assert_eq!(f16_to_f32(0x7c00), f32::INFINITY);
        assert_eq!(f16_to_f32(0xfc00), f32::NEG_INFINITY);
        assert!(f16_to_f32(0x7e00).is_nan());
        assert_eq!(f16_to_f32(0x7bff), 65504.0);
        let mut out = VecDeque::new();
        assert!(decode_iq(Encoding::F16Be, &[0x7c, 0, 0x3c, 0], &mut out).is_err());
        assert!(decode_iq(Encoding::F16Be, &[0x7e, 0, 0x3c, 0], &mut out).is_err());
    }

    #[test]
    fn payload_with_a_non_finite_sample_is_rejected_whole() {
        let s = [Complex32::new(0.5, 0.5), Complex32::new(f32::NAN, 0.0)];
        let mut out = VecDeque::from(vec![Complex32::new(1.0, 1.0)]);
        assert!(decode_iq(Encoding::F32Le, &encode_iq(Encoding::F32Le, &s), &mut out).is_err());
        assert_eq!(out.len(), 1, "nothing of the bad packet is kept");
    }

    #[test]
    fn payload_not_a_whole_number_of_iq_pairs_is_rejected() {
        let mut out = VecDeque::new();
        assert!(decode_iq(Encoding::F32Le, &[0; 12], &mut out).is_err());
        assert!(decode_iq(Encoding::S16Le, &[0; 6], &mut out).is_err());
        assert!(out.is_empty());
    }

    #[test]
    fn encoding_names_cover_every_wire_value() {
        let names: Vec<String> = (1..=11).map(encoding_name).collect();
        assert_eq!(
            names,
            [
                "s16le",
                "s16be",
                "opus",
                "f32le",
                "ax25",
                "f16le",
                "opus-voip",
                "f32be",
                "f16be",
                "mulaw",
                "alaw"
            ]
        );
        assert_eq!(encoding_name(12), "encoding 12");
        let readable: Vec<u64> = (0..=12)
            .filter(|&v| Encoding::from_wire(v).is_some())
            .collect();
        assert_eq!(readable, [1, 2, 4, 6, 8, 9]);
        for enc in ALL {
            assert_eq!(Encoding::from_wire(enc.wire_value()), Some(enc));
        }
    }

    #[test]
    fn seq_tracker_counts_a_gap_and_its_dropped_packets() {
        let mut t = SeqTracker::new(1000);
        assert_eq!(t.observe(1), SeqObservation::Baseline);
        assert_eq!(t.observe(2), SeqObservation::InOrder);
        assert_eq!(t.observe(5), SeqObservation::Gap { missing: 2 });
    }

    #[test]
    fn seq_tracker_wraps_at_65536() {
        let mut t = SeqTracker::new(1000);
        t.observe(65535);
        assert_eq!(t.observe(0), SeqObservation::InOrder);
    }

    #[test]
    fn seq_tracker_discards_duplicates_and_late_packets_without_counting() {
        let mut t = SeqTracker::new(1000);
        t.observe(10);
        t.observe(11);
        assert_eq!(t.observe(11), SeqObservation::Stale);
        assert_eq!(t.observe(5), SeqObservation::Stale);
        assert_eq!(t.observe(12), SeqObservation::InOrder);
    }

    #[test]
    fn seq_tracker_resyncs_on_a_jump_beyond_max_jump_and_far_backward() {
        let mut t = SeqTracker::new(1000);
        t.observe(10);
        assert_eq!(t.observe(5000), SeqObservation::Resync);
        assert_eq!(t.observe(5001), SeqObservation::InOrder);
        assert_eq!(t.observe(4000), SeqObservation::Resync);
        assert_eq!(t.observe(4001), SeqObservation::InOrder);
    }
}
