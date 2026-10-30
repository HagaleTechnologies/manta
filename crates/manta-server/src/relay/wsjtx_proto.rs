//! MAN-320 D7: a bounds-checked decoder for WSJT-X's UDP `NetworkMessage`
//! datagrams. Each is a Qt `QDataStream` in big-endian order: magic
//! `0xadbccbda`, schema, message type, then the sender's `Id` and the
//! type's fields. Only `Status` (type 1) and `Decode` (type 2) are read;
//! every other type comes back as [`Message::Other`].
//!
//! The bytes are untrusted: every length is checked against what is left
//! of the datagram, strings decode as lossy UTF-8, and no input panics.

use std::fmt;

/// The `NetworkMessage` magic number every datagram starts with.
pub const MAGIC: u32 = 0xadbc_cbda;
/// A `QByteArray` length of all ones: a null string, read as empty.
const NULL_LEN: u32 = 0xffff_ffff;

/// One decoded datagram.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Status(Status),
    Decode(Decode),
    /// Any other message type (heartbeat, clear, QSO logged ...), by number.
    Other(u32),
}

/// The leading fields of a `Status` (type 1); the rest are not needed.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub id: String,
    pub dial_freq_hz: u64,
    /// The readable mode name (`FT8`, `FT4`, `JT65` ...).
    pub mode: String,
}

/// A `Decode` (type 2).
#[derive(Debug, Clone, PartialEq)]
pub struct Decode {
    pub id: String,
    /// False for a decode WSJT-X replays (to a newly started client).
    pub new: bool,
    /// Milliseconds since midnight UTC (`QTime`).
    pub time_ms: u32,
    pub snr: i32,
    pub delta_time_s: f64,
    /// Audio offset above the dial frequency.
    pub delta_freq_hz: u32,
    pub mode: String,
    pub message: String,
    /// Absent from older senders; false then.
    pub low_confidence: bool,
    /// Absent from older senders; false then.
    pub off_air: bool,
}

/// Why a datagram did not decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtoError {
    BadMagic(u32),
    /// The datagram ended inside a field.
    Truncated,
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::BadMagic(m) => write!(f, "bad magic {m:#010x}"),
            ProtoError::Truncated => write!(f, "datagram ends inside a field"),
        }
    }
}

impl std::error::Error for ProtoError {}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtoError> {
        if n > self.remaining() {
            return Err(ProtoError::Truncated);
        }
        let bytes = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ProtoError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, ProtoError> {
        Ok(self.array::<1>()?[0])
    }

    fn bool(&mut self) -> Result<bool, ProtoError> {
        Ok(self.u8()? != 0)
    }

    fn u32(&mut self) -> Result<u32, ProtoError> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn i32(&mut self) -> Result<i32, ProtoError> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, ProtoError> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn f64(&mut self) -> Result<f64, ProtoError> {
        Ok(f64::from_be_bytes(self.array()?))
    }

    /// `QTime`: milliseconds since midnight.
    fn qtime(&mut self) -> Result<u32, ProtoError> {
        self.u32()
    }

    /// `utf8`: a `QByteArray`, u32 length then bytes; all ones is null.
    fn utf8(&mut self) -> Result<String, ProtoError> {
        let len = self.u32()?;
        if len == NULL_LEN {
            return Ok(String::new());
        }
        let len = usize::try_from(len).map_err(|_| ProtoError::Truncated)?;
        Ok(String::from_utf8_lossy(self.take(len)?).into_owned())
    }

    /// An optional trailing bool: false when the datagram has ended.
    fn trailing_bool(&mut self) -> Result<bool, ProtoError> {
        if self.remaining() == 0 {
            Ok(false)
        } else {
            self.bool()
        }
    }
}

/// Decodes one datagram. Never panics.
pub fn parse(datagram: &[u8]) -> Result<Message, ProtoError> {
    let mut r = Reader {
        buf: datagram,
        pos: 0,
    };
    let magic = r.u32()?;
    if magic != MAGIC {
        return Err(ProtoError::BadMagic(magic));
    }
    let _schema = r.u32()?;
    let kind = r.u32()?;
    match kind {
        1 => {
            let id = r.utf8()?;
            let dial_freq_hz = r.u64()?;
            let mode = r.utf8()?;
            Ok(Message::Status(Status {
                id,
                dial_freq_hz,
                mode,
            }))
        }
        2 => {
            let id = r.utf8()?;
            let new = r.bool()?;
            let time_ms = r.qtime()?;
            let snr = r.i32()?;
            let delta_time_s = r.f64()?;
            let delta_freq_hz = r.u32()?;
            let mode = r.utf8()?;
            let message = r.utf8()?;
            let low_confidence = r.trailing_bool()?;
            let off_air = r.trailing_bool()?;
            Ok(Message::Decode(Decode {
                id,
                new,
                time_ms,
                snr,
                delta_time_s,
                delta_freq_hz,
                mode,
                message,
                low_confidence,
                off_air,
            }))
        }
        other => {
            // Every message carries the Id; a truncated one is still bad.
            r.utf8()?;
            Ok(Message::Other(other))
        }
    }
}

#[cfg(test)]
pub(crate) mod encode {
    //! A test-only QDataStream writer.

    #[derive(Default)]
    pub struct Writer(pub Vec<u8>);

    impl Writer {
        pub fn header(schema: u32, kind: u32) -> Self {
            let mut w = Writer::default();
            w.u32(super::MAGIC).u32(schema).u32(kind);
            w
        }
        pub fn u32(&mut self, v: u32) -> &mut Self {
            self.0.extend_from_slice(&v.to_be_bytes());
            self
        }
        pub fn i32(&mut self, v: i32) -> &mut Self {
            self.0.extend_from_slice(&v.to_be_bytes());
            self
        }
        pub fn u64(&mut self, v: u64) -> &mut Self {
            self.0.extend_from_slice(&v.to_be_bytes());
            self
        }
        pub fn f64(&mut self, v: f64) -> &mut Self {
            self.0.extend_from_slice(&v.to_be_bytes());
            self
        }
        pub fn bool(&mut self, v: bool) -> &mut Self {
            self.0.push(u8::from(v));
            self
        }
        pub fn utf8(&mut self, s: &str) -> &mut Self {
            self.u32(s.len() as u32);
            self.0.extend_from_slice(s.as_bytes());
            self
        }
        pub fn null(&mut self) -> &mut Self {
            self.u32(super::NULL_LEN)
        }
    }

    /// A `Decode` with every field; `trailing` false stops after the message.
    pub fn decode(id: &str, new: bool, snr: i32, df: u32, msg: &str, trailing: bool) -> Vec<u8> {
        let mut w = Writer::header(3, 2);
        w.utf8(id)
            .bool(new)
            .u32(52_215_000)
            .i32(snr)
            .f64(0.2)
            .u32(df)
            .utf8("~")
            .utf8(msg);
        if trailing {
            w.bool(false).bool(false);
        }
        w.0
    }
}

#[cfg(test)]
mod tests {
    use super::encode::{decode, Writer};
    use super::*;

    fn full_status(id: &str, dial: u64, mode: &str) -> Vec<u8> {
        let mut w = Writer::header(3, 1);
        w.utf8(id)
            .u64(dial)
            .utf8(mode)
            .utf8("K1ABC") // DX call
            .utf8("-12") // report
            .utf8(mode) // Tx mode
            .bool(false) // Tx enabled
            .bool(false) // transmitting
            .bool(true) // decoding
            .u32(1500) // Rx df
            .u32(1500) // Tx df
            .utf8("W3XYZ") // DE call
            .utf8("FN20") // DE grid
            .utf8("FN42") // DX grid
            .bool(false) // Tx watchdog
            .null() // sub-mode
            .bool(false) // fast mode
            .u8_(0) // special operation mode
            .u32(0) // frequency tolerance
            .u32(15) // T/R period
            .null() // configuration name
            .null(); // Tx message
        w.0
    }

    impl Writer {
        fn u8_(&mut self, v: u8) -> &mut Self {
            self.0.push(v);
            self
        }
    }

    #[test]
    fn decodes_a_status_message() {
        let msg = parse(&full_status("WSJT-X", 14_074_000, "FT8")).unwrap();
        assert_eq!(
            msg,
            Message::Status(Status {
                id: "WSJT-X".into(),
                dial_freq_hz: 14_074_000,
                mode: "FT8".into(),
            })
        );
    }

    #[test]
    fn decodes_a_status_that_ends_after_the_mode_field() {
        let mut w = Writer::header(2, 1);
        w.utf8("JTDX").u64(7_074_000).utf8("FT4");
        assert_eq!(
            parse(&w.0).unwrap(),
            Message::Status(Status {
                id: "JTDX".into(),
                dial_freq_hz: 7_074_000,
                mode: "FT4".into(),
            })
        );
    }

    #[test]
    fn decodes_a_decode_message() {
        let mut w = Writer::header(3, 2);
        w.utf8("WSJT-X")
            .bool(true)
            .u32(52_215_000)
            .i32(-12)
            .f64(0.3)
            .u32(1520)
            .utf8("~")
            .utf8("CQ K1ABC FN42")
            .bool(true)
            .bool(true);
        assert_eq!(
            parse(&w.0).unwrap(),
            Message::Decode(Decode {
                id: "WSJT-X".into(),
                new: true,
                time_ms: 52_215_000,
                snr: -12,
                delta_time_s: 0.3,
                delta_freq_hz: 1520,
                mode: "~".into(),
                message: "CQ K1ABC FN42".into(),
                low_confidence: true,
                off_air: true,
            })
        );
    }

    #[test]
    fn decodes_a_decode_without_low_confidence_and_off_air() {
        let Message::Decode(d) = parse(&decode("A", true, -3, 900, "CQ K1ABC", false)).unwrap()
        else {
            panic!("not a decode");
        };
        assert!(!d.low_confidence && !d.off_air);
        assert_eq!(d.message, "CQ K1ABC");
        assert_eq!((d.snr, d.delta_freq_hz), (-3, 900));
    }

    #[test]
    fn a_null_string_reads_as_empty() {
        let mut w = Writer::header(3, 1);
        w.null().u64(14_074_000).null();
        assert_eq!(
            parse(&w.0).unwrap(),
            Message::Status(Status {
                id: String::new(),
                dial_freq_hz: 14_074_000,
                mode: String::new(),
            })
        );
    }

    #[test]
    fn other_message_types_are_skipped() {
        for kind in [0, 3, 5, 10, 99] {
            let mut w = Writer::header(3, kind);
            w.utf8("WSJT-X").u32(3).utf8("2.6.1");
            assert_eq!(parse(&w.0).unwrap(), Message::Other(kind), "type {kind}");
        }
    }

    #[test]
    fn rejects_bad_magic_and_truncation() {
        let mut bad = decode("A", true, -12, 1520, "CQ K1ABC FN42", true);
        bad[0] = 0;
        assert!(matches!(parse(&bad), Err(ProtoError::BadMagic(_))));

        // Every strict prefix that cuts a mandatory field is an error; the
        // two trailing bools are optional, so a cut between them is not.
        let full = decode("A", true, -12, 1520, "CQ K1ABC FN42", true);
        let mandatory = full.len() - 2;
        for n in 0..mandatory {
            assert_eq!(parse(&full[..n]), Err(ProtoError::Truncated), "prefix {n}");
        }

        let mut w = Writer::header(3, 2);
        w.u32(1_000);
        w.0.extend_from_slice(b"short");
        assert_eq!(parse(&w.0), Err(ProtoError::Truncated));
    }

    #[test]
    fn never_panics_on_arbitrary_bytes() {
        let mut state: u64 = 0x5eed_1234_abcd_0001;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u32
        };
        for i in 0..10_000 {
            let len = (next() % 96) as usize;
            let mut bytes: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            // Half the inputs get a valid header, so the field readers run.
            if i % 2 == 0 && bytes.len() >= 12 {
                bytes[..4].copy_from_slice(&MAGIC.to_be_bytes());
                bytes[8..12].copy_from_slice(&(next() % 4).to_be_bytes());
            }
            let _ = parse(&bytes);
        }
    }
}
