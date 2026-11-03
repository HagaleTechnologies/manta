//! MAN-142 D6/D7: the IPFIX (RFC 5101, as simplified on
//! <https://pskreporter.info/pskdev.html>, read 2026-11-02) datagrams
//! PSKReporter reads, big-endian. The general pieces (fields, template
//! sets, padded data sets, strings) build both the page's own example and
//! manta's layout, so the page example pins the code manta sends with.
//!
//! manta's layout: the receiver options template is the page's four-field
//! descriptor (callsign, locator, decoding software, antenna; one scope
//! field) and the sender template is the seven-field layout WSJT-X sends
//! (callsign, 5-byte frequency, 1-byte SNR, mode, sender locator, 1-byte
//! information source, `flowStartSeconds`), each with manta's own
//! template ID. No `Spot` appears here: [`super::batch`] maps spots to [`Report`]s.

/// PSKReporter's IANA private enterprise number.
pub const ENTERPRISE: u32 = 30351;
/// manta's receiver options template ID (`mr`).
pub const RECEIVER_TEMPLATE_ID: u16 = 0x6D72;
/// manta's sender template ID (`ms`).
pub const SENDER_TEMPLATE_ID: u16 = 0x6D73;
/// The longest string a one-byte length code can carry, by the page's rule.
pub const MAX_STRING_BYTES: usize = crate::config::PSKREPORTER_MAX_STRING_BYTES;
/// The IPFIX version number in every header.
pub const VERSION: u16 = 10;
/// Bytes in the message header.
pub const HEADER_LEN: usize = 16;

/// A variable-length field (`0xFFFF`): a one-byte length code, then UTF-8.
pub const VARIABLE: u16 = 0xFFFF;

const TEMPLATE_SET_ID: u16 = 2;
const OPTIONS_TEMPLATE_SET_ID: u16 = 3;

/// PSKReporter's enterprise information elements (the page's table).
pub mod element {
    pub const SENDER_CALLSIGN: u16 = 1;
    pub const RECEIVER_CALLSIGN: u16 = 2;
    pub const SENDER_LOCATOR: u16 = 3;
    pub const RECEIVER_LOCATOR: u16 = 4;
    pub const FREQUENCY: u16 = 5;
    pub const SNR: u16 = 6;
    pub const IMD: u16 = 7;
    pub const DECODING_SOFTWARE: u16 = 8;
    pub const ANTENNA_INFORMATION: u16 = 9;
    pub const MODE: u16 = 10;
    pub const INFORMATION_SOURCE: u16 = 11;
    /// IANA's `flowStartSeconds` (not an enterprise element).
    pub const FLOW_START_SECONDS: u16 = 150;
}

/// One field specifier of a template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    pub element: u16,
    /// PSKReporter's enterprise element (the `0x8000` bit plus
    /// [`ENTERPRISE`]) rather than an IANA one.
    pub enterprise: bool,
    /// Bytes, or [`VARIABLE`].
    pub length: u16,
}

impl Field {
    /// A PSKReporter enterprise element.
    pub const fn psk(element: u16, length: u16) -> Self {
        Self {
            element,
            enterprise: true,
            length,
        }
    }

    /// An IANA element.
    pub const fn iana(element: u16, length: u16) -> Self {
        Self {
            element,
            enterprise: false,
            length,
        }
    }
}

/// manta's receiver options template fields (the page's four-field
/// descriptor).
pub const RECEIVER_FIELDS: [Field; 4] = [
    Field::psk(element::RECEIVER_CALLSIGN, VARIABLE),
    Field::psk(element::RECEIVER_LOCATOR, VARIABLE),
    Field::psk(element::DECODING_SOFTWARE, VARIABLE),
    Field::psk(element::ANTENNA_INFORMATION, VARIABLE),
];

/// manta's sender template fields (WSJT-X's seven-field layout).
pub const SENDER_FIELDS: [Field; 7] = [
    Field::psk(element::SENDER_CALLSIGN, VARIABLE),
    Field::psk(element::FREQUENCY, 5),
    Field::psk(element::SNR, 1),
    Field::psk(element::MODE, VARIABLE),
    Field::psk(element::SENDER_LOCATOR, VARIABLE),
    Field::psk(element::INFORMATION_SOURCE, 1),
    Field::iana(element::FLOW_START_SECONDS, 4),
];

/// The receiver information record's values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receiver {
    /// `[server].station_callsign` without its SSID.
    pub callsign: String,
    /// The receiver's Maidenhead locator.
    pub locator: String,
    /// `manta <version>`.
    pub software: String,
    /// Empty when unset.
    pub antenna: String,
}

/// One sender information record (a spot report).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub callsign: String,
    /// RF frequency, whole Hz (5 bytes on the wire).
    pub freq_hz: u64,
    pub snr_db: i8,
    /// `CW` or `RTTY`.
    pub mode: &'static str,
    /// Unix seconds the call was heard.
    pub flow_start: u32,
}

/// The message header's per-datagram values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Unix seconds.
    pub export_time: u32,
    /// Data records in datagrams built earlier in the session, reports and
    /// receiver records alike, counting datagrams a failed lookup or send
    /// dropped; template records are not counted (RFC 7011 §3.1).
    pub sequence: u32,
    /// Constant for the session.
    pub observation_domain: u32,
}

/// The `informationSource` value manta sends: 1, "automatically extracted".
pub const INFORMATION_SOURCE_AUTOMATIC: u8 = 1;

/// The prefix of `s` that fits [`MAX_STRING_BYTES`], cut on a char boundary.
pub fn truncate(s: &str) -> &str {
    if s.len() <= MAX_STRING_BYTES {
        return s;
    }
    let mut end = MAX_STRING_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// A 2500 Hz SNR in dB as IPFIX's one signed byte: rounded, clamped to
/// -128..=127 (D11).
pub fn snr_i8(snr_db: f32) -> i8 {
    if snr_db.is_nan() {
        return 0;
    }
    snr_db.round().clamp(-128.0, 127.0) as i8
}

/// Appends a one-byte length code and the (truncated) string.
pub fn put_string(out: &mut Vec<u8>, s: &str) {
    let s = truncate(s);
    out.push(s.len() as u8);
    out.extend_from_slice(s.as_bytes());
}

/// Appends the low `width` bytes of `v`, big-endian.
pub fn put_uint(out: &mut Vec<u8>, v: u64, width: usize) {
    let bytes = v.to_be_bytes();
    out.extend_from_slice(&bytes[8 - width..]);
}

/// Pads `out` with zero bytes, from `start`, to a multiple of 4.
fn pad_from(out: &mut Vec<u8>, start: usize) {
    while !(out.len() - start).is_multiple_of(4) {
        out.push(0);
    }
}

/// A set: its two-byte ID, its two-byte length (header and padding
/// included), `body`, and zero padding to a multiple of 4.
pub fn set(set_id: u16, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 7);
    out.extend_from_slice(&set_id.to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(body);
    pad_from(&mut out, 0);
    let len = out.len() as u16;
    out[2..4].copy_from_slice(&len.to_be_bytes());
    out
}

/// A template set (`scope_count` = `None`) or an options template set
/// (`Some(scope)`) holding one template.
pub fn template_set(template_id: u16, scope_count: Option<u16>, fields: &[Field]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&template_id.to_be_bytes());
    body.extend_from_slice(&(fields.len() as u16).to_be_bytes());
    if let Some(scope) = scope_count {
        body.extend_from_slice(&scope.to_be_bytes());
    }
    for f in fields {
        let id = if f.enterprise {
            f.element | 0x8000
        } else {
            f.element
        };
        body.extend_from_slice(&id.to_be_bytes());
        body.extend_from_slice(&f.length.to_be_bytes());
        if f.enterprise {
            body.extend_from_slice(&ENTERPRISE.to_be_bytes());
        }
    }
    let set_id = if scope_count.is_some() {
        OPTIONS_TEMPLATE_SET_ID
    } else {
        TEMPLATE_SET_ID
    };
    set(set_id, &body)
}

/// A whole message: the header, then `sets`, with the total length filled in.
pub fn message(header: &Header, sets: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + sets.iter().map(|s| s.len()).sum::<usize>());
    out.extend_from_slice(&VERSION.to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&header.export_time.to_be_bytes());
    out.extend_from_slice(&header.sequence.to_be_bytes());
    out.extend_from_slice(&header.observation_domain.to_be_bytes());
    for s in sets {
        out.extend_from_slice(s);
    }
    let len = out.len() as u16;
    out[2..4].copy_from_slice(&len.to_be_bytes());
    out
}

/// manta's receiver options template set (44 bytes).
pub fn receiver_template() -> Vec<u8> {
    template_set(RECEIVER_TEMPLATE_ID, Some(1), &RECEIVER_FIELDS)
}

/// manta's sender template set (60 bytes).
pub fn sender_template() -> Vec<u8> {
    template_set(SENDER_TEMPLATE_ID, None, &SENDER_FIELDS)
}

/// The receiver information record's data set.
pub fn receiver_set(r: &Receiver) -> Vec<u8> {
    let mut body = Vec::new();
    put_string(&mut body, &r.callsign);
    put_string(&mut body, &r.locator);
    put_string(&mut body, &r.software);
    put_string(&mut body, &r.antenna);
    set(RECEIVER_TEMPLATE_ID, &body)
}

fn put_report(out: &mut Vec<u8>, r: &Report) {
    put_string(out, &r.callsign);
    put_uint(out, r.freq_hz, 5);
    out.push(r.snr_db as u8);
    put_string(out, r.mode);
    // senderLocator: always empty; manta never invents a grid (MAN-138).
    put_string(out, "");
    out.push(INFORMATION_SOURCE_AUTOMATIC);
    out.extend_from_slice(&r.flow_start.to_be_bytes());
}

/// The bytes `report` adds to the sender set, for the datagram budget.
pub fn report_len(report: &Report) -> usize {
    1 + truncate(&report.callsign).len() + 5 + 1 + 1 + truncate(report.mode).len() + 1 + 1 + 4
}

/// The length of the datagram [`datagram`] would build for `receiver`,
/// `templates` and reports whose [`report_len`]s sum to `reports_len`
/// (at least one report).
pub fn datagram_len(receiver: &Receiver, templates: bool, reports_len: usize) -> usize {
    let templates_len = if templates {
        receiver_template().len() + sender_template().len()
    } else {
        0
    };
    let sender_set = (4 + reports_len).div_ceil(4) * 4;
    HEADER_LEN + templates_len + receiver_set(receiver).len() + sender_set
}

/// One datagram in manta's layout (D6, D7): header, both templates when
/// `templates`, the receiver record, then every report in one set.
pub fn datagram(
    header: &Header,
    receiver: &Receiver,
    templates: bool,
    reports: &[Report],
) -> Vec<u8> {
    let mut sets: Vec<Vec<u8>> = Vec::with_capacity(4);
    if templates {
        sets.push(receiver_template());
        sets.push(sender_template());
    }
    sets.push(receiver_set(receiver));
    if !reports.is_empty() {
        let mut body = Vec::new();
        for r in reports {
            put_report(&mut body, r);
        }
        sets.push(set(SENDER_TEMPLATE_ID, &body));
    }
    let refs: Vec<&[u8]> = sets.iter().map(Vec::as_slice).collect();
    message(header, &refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).expect("hex byte"))
            .collect()
    }

    fn manta_receiver() -> Receiver {
        Receiver {
            callsign: "W5AU".into(),
            locator: "EM10dh".into(),
            software: "manta 0.1.0".into(),
            antenna: String::new(),
        }
    }

    fn w1aw() -> Report {
        Report {
            callsign: "W1AW".into(),
            freq_hz: 14_012_340,
            snr_db: 21,
            mode: "CW",
            flow_start: 1_793_650_021,
        }
    }

    fn header() -> Header {
        Header {
            export_time: 1_793_650_330,
            sequence: 0,
            observation_domain: 0x1234_ABCD,
        }
    }

    /// The first complete example datagram on
    /// <https://pskreporter.info/pskdev.html> (read 2026-11-02): receiver
    /// N1DQ / FN42hn / "Homebrew v5.6", reports N1DQ and KB1MBX, sent at
    /// 1200960114 with sequence 1. Only this example is used: the page's
    /// descriptor-free one spells the mode bytes `50 53 4C` ("PSL").
    const PAGE_EXAMPLE: &str = "
        00 0A 00 AC 47 95 32 72 00 00 00 01 00 00 00 00
        00 03 00 24 99 92 00 03 00 01 80 02 FF FF 00 00 76 8F 80 04 FF FF 00 00 76 8F
        80 08 FF FF 00 00 76 8F 00 00
        00 02 00 2C 99 93 00 05 80 01 FF FF 00 00 76 8F 80 05 00 04 00 00 76 8F
        80 0A FF FF 00 00 76 8F 80 0B 00 01 00 00 76 8F 00 96 00 04
        99 92 00 20 04 4E 31 44 51 06 46 4E 34 32 68 6E
        0D 48 6F 6D 65 62 72 65 77 20 76 35 2E 36 00 00
        99 93 00 2C 04 4E 31 44 51 00 D6 B3 27 03 50 53 4B 01 47 95 32 54
        06 4B 42 31 4D 42 58 00 D6 B4 CB 03 50 53 4B 01 47 95 32 68
        00 00";

    #[test]
    fn rebuilds_the_developer_pages_example_datagram_byte_for_byte() {
        let receiver_tpl = template_set(
            0x9992,
            Some(1),
            &[
                Field::psk(element::RECEIVER_CALLSIGN, VARIABLE),
                Field::psk(element::RECEIVER_LOCATOR, VARIABLE),
                Field::psk(element::DECODING_SOFTWARE, VARIABLE),
            ],
        );
        let sender_tpl = template_set(
            0x9993,
            None,
            &[
                Field::psk(element::SENDER_CALLSIGN, VARIABLE),
                Field::psk(element::FREQUENCY, 4),
                Field::psk(element::MODE, VARIABLE),
                Field::psk(element::INFORMATION_SOURCE, 1),
                Field::iana(element::FLOW_START_SECONDS, 4),
            ],
        );
        let mut rx = Vec::new();
        put_string(&mut rx, "N1DQ");
        put_string(&mut rx, "FN42hn");
        put_string(&mut rx, "Homebrew v5.6");
        let receiver = set(0x9992, &rx);
        let mut tx = Vec::new();
        for (call, freq, t) in [
            ("N1DQ", 14_070_567, 1_200_960_084u32),
            ("KB1MBX", 14_070_987, 1_200_960_104),
        ] {
            put_string(&mut tx, call);
            put_uint(&mut tx, freq, 4);
            put_string(&mut tx, "PSK");
            tx.push(1);
            tx.extend_from_slice(&t.to_be_bytes());
        }
        let senders = set(0x9993, &tx);
        let built = message(
            &Header {
                export_time: 1_200_960_114,
                sequence: 1,
                observation_domain: 0,
            },
            &[&receiver_tpl, &sender_tpl, &receiver, &senders],
        );
        let page = hex(PAGE_EXAMPLE);
        assert_eq!(page.len(), 172);
        assert_eq!(built, page);
    }

    #[test]
    fn mantas_receiver_template_is_the_pages_four_field_descriptor_with_mantas_id() {
        assert_eq!(
            receiver_template(),
            hex(
                "00 03 00 2C 6D 72 00 04 00 01 80 02 FF FF 00 00 76 8F 80 04 FF FF 00 00 76 8F \
                 80 08 FF FF 00 00 76 8F 80 09 FF FF 00 00 76 8F 00 00"
            )
        );
    }

    #[test]
    fn mantas_sender_template_is_wsjtxs_seven_field_layout_with_mantas_id() {
        let want = hex(
            "00 02 00 3C 6D 73 00 07 80 01 FF FF 00 00 76 8F 80 05 00 05 00 00 76 8F \
             80 06 00 01 00 00 76 8F 80 0A FF FF 00 00 76 8F 80 03 FF FF 00 00 76 8F \
             80 0B 00 01 00 00 76 8F 00 96 00 04",
        );
        assert_eq!(want.len(), 60);
        assert_eq!(sender_template(), want);
    }

    #[test]
    fn a_first_datagram_with_one_report_matches_the_design() {
        let want = hex("00 0A 00 B0 6A E8 EE 9A 00 00 00 00 12 34 AB CD \
             00 03 00 2C 6D 72 00 04 00 01 80 02 FF FF 00 00 76 8F 80 04 FF FF 00 00 76 8F \
             80 08 FF FF 00 00 76 8F 80 09 FF FF 00 00 76 8F 00 00 \
             00 02 00 3C 6D 73 00 07 80 01 FF FF 00 00 76 8F 80 05 00 05 00 00 76 8F \
             80 06 00 01 00 00 76 8F 80 0A FF FF 00 00 76 8F 80 03 FF FF 00 00 76 8F \
             80 0B 00 01 00 00 76 8F 00 96 00 04 \
             6D 72 00 20 04 57 35 41 55 06 45 4D 31 30 64 68 0B 6D 61 6E 74 61 20 30 \
             2E 31 2E 30 00 00 00 00 \
             6D 73 00 18 04 57 31 41 57 00 00 D5 CF B4 15 02 43 57 00 01 6A E8 ED 65");
        assert_eq!(want.len(), 176);
        let built = datagram(&header(), &manta_receiver(), true, &[w1aw()]);
        assert_eq!(built, want);
        assert_eq!(
            datagram_len(&manta_receiver(), true, report_len(&w1aw())),
            176
        );
    }

    #[test]
    fn the_same_report_without_templates_is_72_bytes() {
        let built = datagram(&header(), &manta_receiver(), false, &[w1aw()]);
        assert_eq!(built.len(), 72);
        assert_eq!(&built[2..4], &[0x00, 0x48]);
        assert_eq!(
            datagram_len(&manta_receiver(), false, report_len(&w1aw())),
            72
        );
    }

    #[test]
    fn strings_longer_than_254_bytes_are_truncated_on_a_char_boundary() {
        // 'é' is two bytes: 150 of them is 300 bytes, and byte 254 is a
        // boundary; a leading 'a' moves every boundary to an odd offset.
        for antenna in ["é".repeat(150), format!("a{}", "é".repeat(150))] {
            let mut out = Vec::new();
            put_string(&mut out, &antenna);
            let n = out[0] as usize;
            assert!(n <= MAX_STRING_BYTES, "{n}");
            assert_eq!(out.len(), n + 1);
            let s = std::str::from_utf8(&out[1..]).expect("valid UTF-8");
            assert!(antenna.starts_with(s));
            assert!(n >= MAX_STRING_BYTES - 1);
        }
    }

    #[test]
    fn frequency_is_five_bytes_big_endian() {
        let mut out = Vec::new();
        put_uint(&mut out, 50_313_000, 5);
        assert_eq!(out, vec![0x00, 0x02, 0xFF, 0xB7, 0x28]);
        let mut out = Vec::new();
        put_uint(&mut out, 1_800_000, 5);
        assert_eq!(out, vec![0x00, 0x00, 0x1B, 0x77, 0x40]);
    }

    #[test]
    fn snr_is_rounded_and_clamped_to_an_i8() {
        assert_eq!(snr_i8(-200.0), -128);
        assert_eq!(snr_i8(21.4), 21);
        assert_eq!(snr_i8(21.5), 22);
        assert_eq!(snr_i8(300.0), 127);
        assert_eq!(snr_i8(f32::NAN), 0);
    }

    #[test]
    fn every_set_is_padded_to_four_bytes_and_its_length_covers_the_padding() {
        let receiver = Receiver {
            antenna: "Inverted V at 12 m".into(),
            ..manta_receiver()
        };
        for templates in [true, false] {
            for n in 1..6 {
                let reports: Vec<Report> = (0..n)
                    .map(|i| Report {
                        callsign: "K1ABC/P".chars().take(3 + i).collect(),
                        ..w1aw()
                    })
                    .collect();
                let d = datagram(&header(), &receiver, templates, &reports);
                assert_eq!(u16::from_be_bytes([d[2], d[3]]) as usize, d.len());
                let mut pos = HEADER_LEN;
                while pos < d.len() {
                    let len = u16::from_be_bytes([d[pos + 2], d[pos + 3]]) as usize;
                    assert_eq!(len % 4, 0, "set at {pos}");
                    assert!(len >= 4);
                    pos += len;
                }
                assert_eq!(pos, d.len());
                let sum: usize = reports.iter().map(report_len).sum();
                assert_eq!(datagram_len(&receiver, templates, sum), d.len());
            }
        }
    }
}
