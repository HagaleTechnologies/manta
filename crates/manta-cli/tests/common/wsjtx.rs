//! MAN-142 (D16): a fake WSJT-X-aware logger's reader, written from WSJT-X's
//! `Network/NetworkMessage.hpp` (schema 3, sourceforge.net/p/wsjt/wsjtx,
//! master, read 2026-11-02). It shares no code with
//! `manta_server::wsjtx_output` or manta's own `[[wsjtx_relay]]` reader.
//!
//! Every datagram is a Qt `QDataStream`, big-endian: magic `0xadbccbda`,
//! schema, message type, the sender's `Id`, then the type's fields. A
//! `utf8` is a `QByteArray`: a `quint32` length (all ones for null) and the
//! bytes. The reader is strict: a known type must be read to its last byte.

pub const MAGIC: u32 = 0xadbc_cbda;

/// `Status` (type 1), every field of the master-schema layout.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub id: String,
    pub dial_freq_hz: u64,
    pub mode: String,
    pub dx_call: String,
    pub report: String,
    pub tx_mode: String,
    pub tx_enabled: bool,
    pub transmitting: bool,
    pub decoding: bool,
    pub rx_df: u32,
    pub tx_df: u32,
    pub de_call: String,
    pub de_grid: String,
    pub dx_grid: String,
    pub tx_watchdog: bool,
    pub sub_mode: String,
    pub fast_mode: bool,
    pub special_operation_mode: u8,
    pub frequency_tolerance: u32,
    pub tr_period: u32,
    pub configuration_name: String,
    pub tx_message: String,
}

/// `Decode` (type 2).
#[derive(Debug, Clone, PartialEq)]
pub struct Decode {
    pub id: String,
    pub new: bool,
    /// `QTime`: milliseconds since midnight.
    pub time_ms: u32,
    pub snr: i32,
    pub delta_time_s: f64,
    pub delta_freq_hz: u32,
    pub mode: String,
    pub message: String,
    pub low_confidence: bool,
    pub off_air: bool,
}

/// One datagram, read.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// Type 0.
    Heartbeat {
        id: String,
        max_schema: u32,
        version: String,
        revision: String,
    },
    Status(Status),
    Decode(Decode),
    /// Type 6.
    Close {
        id: String,
    },
    /// Any other type, by number; its body is not read.
    Unknown(u32),
}

impl Message {
    /// The type number this message arrived as.
    pub fn kind(&self) -> u32 {
        match self {
            Message::Heartbeat { .. } => 0,
            Message::Status(_) => 1,
            Message::Decode(_) => 2,
            Message::Close { .. } => 6,
            Message::Unknown(k) => *k,
        }
    }
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.buf.len() - self.pos < n {
            return Err(format!(
                "datagram ends inside a field at offset {}",
                self.pos
            ));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn boolean(&mut self) -> Result<bool, String> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(format!("bool byte {other}")),
        }
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn f64(&mut self) -> Result<f64, String> {
        Ok(f64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn utf8(&mut self) -> Result<String, String> {
        let n = self.u32()?;
        if n == u32::MAX {
            return Ok(String::new());
        }
        let bytes = self.take(n as usize)?;
        String::from_utf8(bytes.to_vec()).map_err(|e| format!("not UTF-8: {e}"))
    }
}

/// Reads one datagram, or says why it is malformed.
pub fn read(datagram: &[u8]) -> Result<Message, String> {
    let mut c = Cursor {
        buf: datagram,
        pos: 0,
    };
    let magic = c.u32()?;
    if magic != MAGIC {
        return Err(format!("magic {magic:#010x}"));
    }
    let schema = c.u32()?;
    if schema != 3 {
        return Err(format!("schema {schema}, not 3"));
    }
    let kind = c.u32()?;
    let msg = match kind {
        0 => Message::Heartbeat {
            id: c.utf8()?,
            max_schema: c.u32()?,
            version: c.utf8()?,
            revision: c.utf8()?,
        },
        1 => Message::Status(Status {
            id: c.utf8()?,
            dial_freq_hz: c.u64()?,
            mode: c.utf8()?,
            dx_call: c.utf8()?,
            report: c.utf8()?,
            tx_mode: c.utf8()?,
            tx_enabled: c.boolean()?,
            transmitting: c.boolean()?,
            decoding: c.boolean()?,
            rx_df: c.u32()?,
            tx_df: c.u32()?,
            de_call: c.utf8()?,
            de_grid: c.utf8()?,
            dx_grid: c.utf8()?,
            tx_watchdog: c.boolean()?,
            sub_mode: c.utf8()?,
            fast_mode: c.boolean()?,
            special_operation_mode: c.u8()?,
            frequency_tolerance: c.u32()?,
            tr_period: c.u32()?,
            configuration_name: c.utf8()?,
            tx_message: c.utf8()?,
        }),
        2 => Message::Decode(Decode {
            id: c.utf8()?,
            new: c.boolean()?,
            time_ms: c.u32()?,
            snr: c.i32()?,
            delta_time_s: c.f64()?,
            delta_freq_hz: c.u32()?,
            mode: c.utf8()?,
            message: c.utf8()?,
            low_confidence: c.boolean()?,
            off_air: c.boolean()?,
        }),
        6 => Message::Close { id: c.utf8()? },
        other => return Ok(Message::Unknown(other)),
    };
    if c.pos != datagram.len() {
        return Err(format!(
            "type {kind}: {} bytes after the last field",
            datagram.len() - c.pos
        ));
    }
    Ok(msg)
}
