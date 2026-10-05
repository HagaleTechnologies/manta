//! Telnet IAC (RFC 854 "Interpret As Command") option-negotiation filter.
//!
//! MAN-87: real telnet clients -- Windows `telnet.exe`, PuTTY's telnet
//! mode, and most DX-cluster client software -- send option negotiation
//! the instant the TCP connection opens, before any application data.
//! IAC is `0xFF`, a byte that is never valid anywhere in UTF-8, so those
//! bytes reached `bounded_io`'s UTF-8 validation and dropped the
//! connection before the callsign was ever read.
//!
//! This is a byte-at-a-time state machine, deliberately not an async
//! reader: an IAC sequence can straddle a `fill_buf` chunk boundary, and
//! the telnet command read runs inside `tokio::select!` where the read
//! future can be cancelled mid-line -- so the parse state has to live in
//! a value the CALLER owns across calls, exactly like `cmd_line` does.

/// Interpret As Command.
const IAC: u8 = 255;
const SE: u8 = 240;
const SB: u8 = 250;
const WILL: u8 = 251;
const WONT: u8 = 252;
const DO: u8 = 253;
const DONT: u8 = 254;

/// Upper bound on queued negotiation replies per connection. A client
/// that streams `IAC WILL <opt>` without end would otherwise make us
/// buffer an unbounded reply. Past this, options are still parsed and
/// stripped, just no longer answered (MAN-23 threat model: an
/// unauthenticated client must not be able to grow a server-side buffer).
pub const MAX_NEGOTIATION_REPLY_BYTES: usize = 192; // 64 refusals

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Data,
    /// Saw IAC, waiting for the command byte.
    Iac,
    /// Saw IAC WILL/WONT/DO/DONT, waiting for the option byte.
    Negotiating(u8),
    /// Inside IAC SB ... IAC SE.
    Subneg,
    /// Inside a subnegotiation, saw IAC.
    SubnegIac,
}

#[derive(Debug, Default)]
pub struct IacFilter {
    state: State,
    replies: Vec<u8>,
    /// Raw APPLICATION-content bytes consumed for the line currently
    /// being assembled -- i.e. bytes `push` returned `Some` for.
    /// Protocol framing no longer counts here (see `framing_bytes`), so
    /// this is released the same way it always was: on line completion
    /// or a read error. Lives here rather than in the read function
    /// because it must survive a `tokio::select!` cancellation mid-line.
    pub raw_line_bytes: usize,
    /// Raw protocol-framing bytes consumed -- i.e. bytes `push` returned
    /// `None` for -- accumulated over the CONNECTION's whole lifetime,
    /// never reset by `reset_line`. Split out from `raw_line_bytes`
    /// (MAN-87 remediation, round-4 validation code-review finding F2):
    /// counting framing bytes against the per-line budget meant a
    /// keepalive-only client (PuTTY's telnet keepalive is `IAC NOP`)
    /// accumulated that budget across its entire session and was
    /// eventually disconnected with "line exceeds maximum length" having
    /// never sent anything resembling a long line. A separate,
    /// connection-lifetime budget (`bounded_io::MAX_FRAMING_BYTES`, checked
    /// by the caller) is large enough that realistic keepalive traffic
    /// never approaches it, while still bounding a pure-framing flood (an
    /// `IAC SB` stream with no closing `IAC SE`, or a fast `IAC NOP`
    /// trickle) to a fixed amount of work -- unlike review round 2's
    /// now-reverted fix, which fully released the shared budget on any
    /// framing-only read and made that exact flood unbounded.
    pub framing_bytes: usize,
}

impl IacFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one raw byte. Returns the application byte to append to the
    /// line, or `None` if the byte was part of telnet protocol framing.
    pub fn push(&mut self, b: u8) -> Option<u8> {
        match self.state {
            State::Data => {
                if b == IAC {
                    self.state = State::Iac;
                    None
                } else {
                    Some(b)
                }
            }
            State::Iac => {
                match b {
                    // IAC IAC is an escaped literal 0xFF data byte.
                    // Dropped, not emitted: 0xFF is never valid UTF-8, so
                    // emitting it would recreate the exact disconnect
                    // this module exists to prevent, and a literal 0xFF
                    // has no meaning in a callsign or a cluster command.
                    IAC => self.state = State::Data,
                    WILL | WONT | DO | DONT => self.state = State::Negotiating(b),
                    SB => self.state = State::Subneg,
                    // Every other command (NOP, AYT, BRK, IP, GA, a
                    // stray SE, or an unknown byte) is consumed with no
                    // reply -- none of them carry data for a
                    // line-oriented read-mostly protocol.
                    _ => self.state = State::Data,
                }
                None
            }
            State::Negotiating(verb) => {
                self.refuse(verb, b);
                self.state = State::Data;
                None
            }
            State::Subneg => {
                if b == IAC {
                    self.state = State::SubnegIac;
                }
                None
            }
            State::SubnegIac => {
                // IAC SE ends the subnegotiation; IAC IAC is escaped
                // payload data; anything else is a malformed sequence we
                // resynchronize from by staying inside the subnegotiation.
                self.state = if b == SE { State::Data } else { State::Subneg };
                None
            }
        }
    }

    /// Answers an option negotiation by refusing it.
    ///
    /// manta refuses EVERY option, deliberately. ARCHITECTURE §7 describes
    /// a line-oriented, read-mostly text protocol -- there is no option
    /// here worth the state it would cost, and refusing is what ends
    /// negotiation fastest. Per RFC 854's loop-avoidance rule a refusal
    /// (WONT/DONT) is never itself answered, which is why only WILL and
    /// DO produce a reply: answering a refusal is how negotiation loops
    /// start.
    fn refuse(&mut self, verb: u8, option: u8) {
        let answer = match verb {
            WILL => DONT,
            DO => WONT,
            _ => return, // WONT/DONT: already refused, stay silent.
        };
        if self.replies.len() + 3 > MAX_NEGOTIATION_REPLY_BYTES {
            return;
        }
        self.replies.extend_from_slice(&[IAC, answer, option]);
    }

    /// Takes the negotiation replies queued so far, for the caller to
    /// write back to the client.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    pub fn has_replies(&self) -> bool {
        !self.replies.is_empty()
    }

    /// Called when a line completes: the content-byte budget is per
    /// line. `framing_bytes` is deliberately untouched -- it is a
    /// connection-lifetime budget, never released (see its doc comment).
    pub fn reset_line(&mut self) {
        self.raw_line_bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(input: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut f = IacFilter::new();
        let mut out = Vec::new();
        for &b in input {
            if let Some(a) = f.push(b) {
                out.push(a);
            }
        }
        (out, f.take_replies())
    }

    #[test]
    fn plain_text_passes_through_untouched() {
        let (out, replies) = run(b"N0CALL\r\n");
        assert_eq!(out, b"N0CALL\r\n");
        assert!(replies.is_empty());
    }

    #[test]
    fn windows_telnet_negotiation_is_stripped_and_refused() {
        // IAC WILL TERMINAL-TYPE(24), IAC DO SUPPRESS-GO-AHEAD(3), callsign.
        let (out, replies) = run(b"\xff\xfb\x18\xff\xfd\x03W5AU\r\n");
        assert_eq!(out, b"W5AU\r\n");
        assert_eq!(replies, vec![IAC, DONT, 24, IAC, WONT, 3]);
    }

    #[test]
    fn a_refusal_from_the_client_is_never_answered() {
        // RFC 854 loop avoidance: answering WONT/DONT is how loops start.
        let (out, replies) = run(b"\xff\xfc\x18\xff\xfe\x03");
        assert!(out.is_empty());
        assert!(replies.is_empty());
    }

    #[test]
    fn subnegotiation_is_skipped_including_escaped_payload() {
        // IAC SB NAWS 0 80 IAC IAC 24 IAC SE, then text.
        let (out, replies) = run(b"\xff\xfa\x1f\x00\x50\xff\xff\x18\xff\xf0OK\n");
        assert_eq!(out, b"OK\n");
        assert!(replies.is_empty(), "subnegotiation gets no reply");
    }

    #[test]
    fn a_sequence_split_across_chunks_is_still_recognized() {
        let mut f = IacFilter::new();
        let mut out = Vec::new();
        for chunk in [&b"\xff"[..], &b"\xfb"[..], &b"\x18W5"[..], &b"AU\n"[..]] {
            for &b in chunk {
                if let Some(a) = f.push(b) {
                    out.push(a);
                }
            }
        }
        assert_eq!(out, b"W5AU\n");
        assert_eq!(f.take_replies(), vec![IAC, DONT, 24]);
    }

    #[test]
    fn an_escaped_literal_ff_data_byte_is_dropped_not_emitted() {
        let (out, replies) = run(b"AB\xff\xffCD\n");
        assert_eq!(
            out, b"ABCD\n",
            "0xFF is never valid UTF-8; emitting it re-breaks the login"
        );
        assert!(replies.is_empty());
    }

    #[test]
    fn other_two_byte_commands_are_consumed_without_a_reply() {
        // IAC NOP(241), IAC AYT(246), IAC GA(249).
        let (out, replies) = run(b"\xff\xf1A\xff\xf6B\xff\xf9\n");
        assert_eq!(out, b"AB\n");
        assert!(replies.is_empty());
    }

    #[test]
    fn negotiation_replies_are_bounded() {
        let mut f = IacFilter::new();
        for _ in 0..1000 {
            for &b in b"\xff\xfb\x18" {
                f.push(b);
            }
        }
        let replies = f.take_replies();
        assert!(
            replies.len() <= MAX_NEGOTIATION_REPLY_BYTES,
            "reply buffer must stay bounded, got {}",
            replies.len()
        );
        assert!(!replies.is_empty());
    }
}
