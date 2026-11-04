//! MAN-142 (D16): a fake PSKReporter collector's reader, written from
//! PSKReporter's developer page (<https://pskreporter.info/pskdev.html>,
//! read 2026-11-02) and RFC 5101. It shares no code with
//! `manta_server::pskreporter`, so a test that decodes manta's datagrams
//! with it checks the published format, not manta's own encoder.
//!
//! It is generic: it stores every template and options template a datagram
//! carries, by ID, and decodes data sets through them. Records decoded
//! through an options template (set 3) are receivers; records decoded
//! through a plain template (set 2) are senders.

use std::collections::{BTreeMap, HashMap};

/// PSKReporter's IANA enterprise number.
pub const PSK_ENTERPRISE: u32 = 30351;

/// PSKReporter's enterprise element IDs, as the page lists them.
pub mod element {
    pub const SENDER_CALLSIGN: u16 = 1;
    pub const RECEIVER_CALLSIGN: u16 = 2;
    pub const SENDER_LOCATOR: u16 = 3;
    pub const RECEIVER_LOCATOR: u16 = 4;
    pub const FREQUENCY: u16 = 5;
    pub const SNR: u16 = 6;
    pub const DECODING_SOFTWARE: u16 = 8;
    pub const ANTENNA_INFORMATION: u16 = 9;
    pub const MODE: u16 = 10;
    pub const INFORMATION_SOURCE: u16 = 11;
    /// IANA's `flowStartSeconds` (no enterprise number).
    pub const FLOW_START_SECONDS: u16 = 150;
}

/// One information element: its ID (without the enterprise bit) and its
/// enterprise number, `None` for an IANA element.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Element {
    pub id: u16,
    pub enterprise: Option<u32>,
}

impl Element {
    /// A PSKReporter enterprise element.
    pub fn psk(id: u16) -> Self {
        Self {
            id,
            enterprise: Some(PSK_ENTERPRISE),
        }
    }
    /// An IANA element.
    pub fn iana(id: u16) -> Self {
        Self {
            id,
            enterprise: None,
        }
    }
}

/// A decoded field: a variable-length field is a string, a fixed-length
/// one a big-endian unsigned integer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Str(String),
    Uint(u64),
}

impl Value {
    pub fn str(&self) -> &str {
        match self {
            Value::Str(s) => s,
            Value::Uint(v) => panic!("expected a string, got {v}"),
        }
    }
    pub fn uint(&self) -> u64 {
        match self {
            Value::Uint(v) => *v,
            Value::Str(s) => panic!("expected an integer, got {s:?}"),
        }
    }
}

/// One data record, by element.
pub type Record = BTreeMap<Element, Value>;

/// The IPFIX message header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub version: u16,
    pub length: u16,
    pub export_time: u32,
    pub sequence: u32,
    pub observation_domain: u32,
}

/// One datagram, read.
#[derive(Debug, Clone, PartialEq)]
pub struct Datagram {
    pub header: Header,
    /// IDs of the templates (set 2) this datagram carried.
    pub templates: Vec<u16>,
    /// IDs of the options templates (set 3) this datagram carried.
    pub options_templates: Vec<u16>,
    /// Records decoded through an options template.
    pub receivers: Vec<Record>,
    /// Records decoded through a plain template.
    pub senders: Vec<Record>,
}

#[derive(Debug, Clone, Copy)]
struct FieldSpec {
    element: Element,
    /// `0xFFFF` = variable length.
    length: u16,
}

#[derive(Debug, Clone)]
struct Template {
    options: bool,
    fields: Vec<FieldSpec>,
}

/// A collector session: templates persist from one datagram to the next,
/// as a real collector keeps them.
#[derive(Debug, Default)]
pub struct Collector {
    templates: HashMap<u16, Template>,
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.buf.len() - self.pos < n {
            return Err(format!(
                "needed {n} bytes at offset {}, {} left",
                self.pos,
                self.buf.len() - self.pos
            ));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, String> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32, String> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn left(&self) -> usize {
        self.buf.len() - self.pos
    }
    /// The rest is fewer than 4 bytes, all zero: set padding.
    fn only_padding(&self) -> bool {
        self.left() < 4 && self.buf[self.pos..].iter().all(|b| *b == 0)
    }
}

impl Collector {
    /// Reads one datagram, or says why it is malformed.
    pub fn read(&mut self, datagram: &[u8]) -> Result<Datagram, String> {
        let mut c = Cursor {
            buf: datagram,
            pos: 0,
        };
        let header = Header {
            version: c.u16()?,
            length: c.u16()?,
            export_time: c.u32()?,
            sequence: c.u32()?,
            observation_domain: c.u32()?,
        };
        if header.version != 10 {
            return Err(format!("version {} is not 10", header.version));
        }
        if usize::from(header.length) != datagram.len() {
            return Err(format!(
                "header length {} but the datagram is {} bytes",
                header.length,
                datagram.len()
            ));
        }
        let mut out = Datagram {
            header,
            templates: Vec::new(),
            options_templates: Vec::new(),
            receivers: Vec::new(),
            senders: Vec::new(),
        };
        while c.left() > 0 {
            let set_id = c.u16()?;
            let set_len = usize::from(c.u16()?);
            if set_len < 4 || set_len % 4 != 0 {
                return Err(format!(
                    "set {set_id:#06x} has length {set_len}, not a multiple of 4"
                ));
            }
            let body = c.take(set_len - 4)?;
            let mut s = Cursor { buf: body, pos: 0 };
            match set_id {
                2 | 3 => {
                    let options = set_id == 3;
                    while s.left() >= 4 && !s.only_padding() {
                        let id = s.u16()?;
                        let count = s.u16()?;
                        if options {
                            let scope = s.u16()?;
                            if scope == 0 || scope > count {
                                return Err(format!("options template {id:#06x}: scope {scope}"));
                            }
                        }
                        let mut fields = Vec::new();
                        for _ in 0..count {
                            let raw = s.u16()?;
                            let length = s.u16()?;
                            let element = if raw & 0x8000 != 0 {
                                Element {
                                    id: raw & 0x7fff,
                                    enterprise: Some(s.u32()?),
                                }
                            } else {
                                Element::iana(raw)
                            };
                            fields.push(FieldSpec { element, length });
                        }
                        if options {
                            out.options_templates.push(id);
                        } else {
                            out.templates.push(id);
                        }
                        self.templates.insert(id, Template { options, fields });
                    }
                    if !s.only_padding() && s.left() != 0 {
                        return Err(format!("set {set_id}: {} stray bytes", s.left()));
                    }
                }
                id if id >= 256 => {
                    let template = self
                        .templates
                        .get(&id)
                        .ok_or_else(|| format!("data set {id:#06x} has no template"))?
                        .clone();
                    while s.left() > 0 && !s.only_padding() {
                        let mut record = Record::new();
                        for f in &template.fields {
                            let value = if f.length == 0xffff {
                                let mut n = usize::from(s.u8()?);
                                if n == 255 {
                                    n = usize::from(s.u16()?);
                                }
                                let bytes = s.take(n)?;
                                Value::Str(
                                    String::from_utf8(bytes.to_vec())
                                        .map_err(|e| format!("a string is not UTF-8: {e}"))?,
                                )
                            } else {
                                let bytes = s.take(usize::from(f.length))?;
                                if bytes.len() > 8 {
                                    return Err(format!("a {}-byte integer", bytes.len()));
                                }
                                Value::Uint(
                                    bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)),
                                )
                            };
                            record.insert(f.element, value);
                        }
                        if template.options {
                            out.receivers.push(record);
                        } else {
                            out.senders.push(record);
                        }
                    }
                }
                other => return Err(format!("reserved set ID {other}")),
            }
        }
        Ok(out)
    }
}
