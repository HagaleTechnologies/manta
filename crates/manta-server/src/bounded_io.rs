//! Bounded, timed-out line reads shared by the telnet and metrics HTTP
//! servers -- both are publicly bound (ARCHITECTURE §7), so an
//! unauthenticated client sending a line with no terminating newline must
//! not be able to grow a read buffer without bound, and an idle client
//! must not hold a spawned task open forever.

use crate::iac::IacFilter;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

pub const MAX_LINE_BYTES: usize = 1024;
pub const IDLE_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on protocol-framing bytes (telnet IAC negotiation, including
/// keepalives) consumed over a connection's whole lifetime -- distinct
/// from `MAX_LINE_BYTES`, and checked against `IacFilter::framing_bytes`,
/// which `reset_line` never clears. MAN-87 remediation (round-4
/// validation, code-review finding F2): a much larger, session-lifetime
/// budget accommodates realistic keepalive traffic (PuTTY's telnet
/// keepalive is `IAC NOP`; even a keepalive every few seconds for weeks
/// stays well under this) while still bounding a pure-framing flood to a
/// fixed amount of work, since the counter is never released.
pub const MAX_FRAMING_BYTES: usize = 1024 * 1024;

/// MAN-538: up to three bytes of a UTF-8 character that a read chunk ended
/// inside, carried into the next chunk. Owned by the caller beside the line
/// `String` (inside `IacFilter` for the telnet reader), so it survives a
/// `tokio::select!` cancellation exactly as `buf` does. Empty whenever a
/// read returns, `Ok` or `Err`; only a dropped read future leaves it set,
/// and the next call on the same `buf` resumes from it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Utf8Tail {
    bytes: [u8; 3],
    len: u8,
}

impl Utf8Tail {
    /// Pending bytes, 0 to 3.
    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Drops the pending bytes (a fresh line).
    pub fn clear(&mut self) {
        self.len = 0;
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len()]
    }

    fn last(&self) -> Option<u8> {
        self.as_slice().last().copied()
    }
}

fn invalid_utf8() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "line contains invalid UTF-8",
    )
}

/// Appends `tail` + `chunk` to `buf`. When `line_ended` is false and the
/// bytes end inside a character more bytes could complete
/// (`Utf8Error::error_len() == None`), the valid prefix is appended and the
/// incomplete end is kept in `tail`. Any other invalid byte is an error and
/// clears `tail`; `buf` keeps only bytes that were already valid.
fn push_utf8(
    buf: &mut String,
    tail: &mut Utf8Tail,
    chunk: &[u8],
    line_ended: bool,
) -> std::io::Result<()> {
    let joined: std::borrow::Cow<'_, [u8]> = if tail.is_empty() {
        std::borrow::Cow::Borrowed(chunk)
    } else {
        let mut v = tail.as_slice().to_vec();
        v.extend_from_slice(chunk);
        std::borrow::Cow::Owned(v)
    };
    match std::str::from_utf8(&joined) {
        Ok(s) => {
            buf.push_str(s);
            tail.clear();
            Ok(())
        }
        Err(e) if e.error_len().is_none() && !line_ended => {
            let (valid, rest) = joined.split_at(e.valid_up_to());
            // `valid_up_to` is a character boundary, and an incomplete
            // character is at most three bytes.
            let valid = std::str::from_utf8(valid).map_err(|_| invalid_utf8())?;
            if rest.len() > tail.bytes.len() {
                tail.clear();
                return Err(invalid_utf8());
            }
            buf.push_str(valid);
            tail.bytes[..rest.len()].copy_from_slice(rest);
            tail.len = rest.len() as u8;
            Ok(())
        }
        Err(_) => {
            tail.clear();
            Err(invalid_utf8())
        }
    }
}

/// Reads one line, accumulating at most `MAX_LINE_BYTES` before treating
/// an unterminated line as a protocol violation (an `InvalidData` error)
/// rather than growing `buf` without bound. Returns the number of bytes
/// read (including whatever `buf` already held on entry), `0` meaning EOF
/// with `buf` empty on entry. A chunk containing invalid UTF-8 is also an
/// `InvalidData` error, matching `AsyncBufReadExt::read_line`'s own
/// behavior rather than silently lossy-converting it (MAN-58/PR #80
/// review, round 2: `manta-server/src/uplink.rs`'s use of this function
/// had regressed `docs/DECISIONS/2026-09-02-man23-threat-model.md`
/// finding 19's "non-UTF8 from the target errors cleanly" disposition,
/// which the raw `read_line` it replaced had already covered).
///
/// Deliberately does **not** clear `buf` itself -- this future is not
/// cancellation-safe against losing already-consumed bytes (nothing async
/// I/O can be, short of buffering independently of the caller), but it
/// *is* resumable: a caller using this inside `tokio::select!` and NOT
/// clearing `buf` between calls can safely let a losing-race read be
/// dropped mid-line and simply call this again later to continue where it
/// left off, because every byte already pulled off the reader was already
/// appended to `buf` as a side effect before any cancellation point. A
/// caller that clears `buf` before every call (a fresh line each time)
/// gets the same behavior `read_line_bounded` had when it auto-cleared.
///
/// UTF-8 is validated over the line, not per chunk (MAN-538): a chunk that
/// ends inside a character keeps its at most three bytes in `tail`, which
/// the caller owns beside `buf`, so a character split across reads, even
/// across a cancelled read, is accepted. Invalid bytes, a character cut by
/// the newline, and EOF inside a character are still `InvalidData`
/// (threat-model finding 19).
pub async fn read_line_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut String,
    tail: &mut Utf8Tail,
) -> std::io::Result<usize> {
    let mut total = buf.len() + tail.len();
    loop {
        let available = match reader.fill_buf().await {
            Ok(available) => available,
            Err(e) => {
                tail.clear();
                return Err(e);
            }
        };
        if available.is_empty() {
            if !tail.is_empty() {
                // EOF inside a character.
                tail.clear();
                return Err(invalid_utf8());
            }
            return Ok(total); // EOF
        }
        let (chunk_len, found_newline) = match available.iter().position(|&b| b == b'\n') {
            Some(nl) => (nl + 1, true),
            None => (available.len(), false),
        };
        total += chunk_len;
        if total > MAX_LINE_BYTES {
            reader.consume(chunk_len);
            tail.clear();
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "line exceeds maximum length",
            ));
        }
        push_utf8(buf, tail, &available[..chunk_len], found_newline)?;
        reader.consume(chunk_len);
        if found_newline {
            return Ok(total);
        }
    }
}

/// `read_line_bounded`, plus an idle-read deadline -- a client that never
/// sends anything, or stalls mid-line, must not hold its task open forever.
pub async fn read_line_bounded_with_timeout<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut String,
    tail: &mut Utf8Tail,
) -> std::io::Result<usize> {
    match tokio::time::timeout(IDLE_READ_TIMEOUT, read_line_bounded(reader, buf, tail)).await {
        Ok(result) => result,
        Err(_) => {
            // The dropped read may have left a pending character.
            tail.clear();
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "read timed out",
            ))
        }
    }
}

/// `read_line_bounded`, but telnet-aware: RFC 854 IAC option-negotiation
/// sequences are stripped from the byte stream before UTF-8 validation
/// ever sees them, and refusals to answer them are queued on `filter`
/// for the caller to write back (MAN-87).
///
/// Why a separate entry point rather than teaching `read_line_bounded`
/// itself about IAC: its other two callers -- `metrics_http`'s HTTP
/// request line and `uplink`'s inbound RBN stream -- are ASCII-by-
/// contract protocols where `0xFF` is genuinely malformed input that
/// must keep being rejected (`docs/DECISIONS/2026-09-02-man23-threat-
/// model.md` finding 19). Only the telnet listener talks to clients that
/// legitimately prepend binary negotiation.
///
/// The newline is searched for in the FILTERED output, not the raw
/// chunk: `0x0A` occurs inside telnet framing as an option code
/// (option 10, NAOCRD) and as subnegotiation payload, so scanning raw
/// bytes would end the line in the middle of a negotiation sequence.
///
/// A bare `\n` is not the only line terminator recognized: RFC 854's NVT
/// encodes Enter as `CR NUL` too, and that is what BSD/macOS `telnet(1)`
/// sends by default (`crlf` toggle default FALSE) while keeping the
/// connection open -- unlike the piped-stdin case, there is no EOF to
/// fall back on, so without this a line never completes and the client
/// is eventually dropped by the idle timeout (MAN-87 review round 2, C1).
///
/// The length cap counts RAW application-content bytes consumed
/// (`filter.raw_line_bytes`), not surviving text bytes, so a client
/// streaming endless negotiation with no completed line still can't grow
/// `buf` without bound -- but as of MAN-87 remediation (round-4
/// validation, code-review finding F2), protocol-framing bytes no longer
/// count against this PER-LINE budget at all; they count against
/// `filter.framing_bytes` (checked against `MAX_FRAMING_BYTES`), a
/// separate, connection-lifetime budget that `reset_line` never clears.
///
/// MAN-87 review round 2 (C2) tried releasing the (then-shared) budget on
/// pure-framing reads, to keep a client sending only occasional keepalive
/// negotiation from accumulating it across a long session -- but that made
/// the cap unreachable for a client that never lets a read return anything
/// but framing (a flood of `IAC SB`/`IAC NOP`), and chunk-size dependent
/// for everyone else (round-3 validation, code-review findings 2/3).
/// Reverted. Splitting the budget in two (rather than reviving the C2
/// release) gets both properties at once: framing bytes are tracked in
/// their own counter that is large enough not to trip for realistic
/// keepalive traffic, yet is still monotonic -- never released -- so a
/// pure-framing flood remains bounded regardless of chunking.
///
/// Cancellation-safety matches `read_line_bounded`: every byte pulled
/// off the reader is folded into `buf` and `filter` -- both caller-owned
/// -- before `consume`, with no await point in between.
/// The pending bytes of a UTF-8 character a chunk ended inside live in
/// `filter` too (MAN-538).
pub async fn read_line_bounded_telnet<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut String,
    filter: &mut IacFilter,
) -> std::io::Result<usize> {
    loop {
        let available = match reader.fill_buf().await {
            Ok(available) => available,
            Err(e) => {
                filter.utf8.clear();
                return Err(e);
            }
        };
        if available.is_empty() {
            if !filter.utf8.is_empty() {
                // EOF inside a character.
                filter.reset_line();
                return Err(invalid_utf8());
            }
            return Ok(buf.len()); // EOF
        }
        let mut consumed = 0usize;
        let mut text = Vec::with_capacity(available.len());
        let mut found_newline = false;
        for &b in available {
            consumed += 1;
            if let Some(app) = filter.push(b) {
                let prev = text
                    .last()
                    .copied()
                    .or_else(|| filter.utf8.last())
                    .or_else(|| buf.as_bytes().last().copied());
                text.push(app);
                if app == b'\n' || (app == 0 && prev == Some(b'\r')) {
                    found_newline = true;
                    break;
                }
            }
        }
        filter.raw_line_bytes += text.len();
        filter.framing_bytes += consumed - text.len();
        if filter.raw_line_bytes > MAX_LINE_BYTES {
            reader.consume(consumed);
            filter.reset_line();
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "line exceeds maximum length",
            ));
        }
        if filter.framing_bytes > MAX_FRAMING_BYTES {
            reader.consume(consumed);
            filter.reset_line();
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "excessive protocol negotiation",
            ));
        }
        if let Err(e) = push_utf8(buf, &mut filter.utf8, &text, found_newline) {
            reader.consume(consumed);
            filter.reset_line();
            return Err(e);
        }
        reader.consume(consumed);
        if found_newline {
            filter.reset_line();
            return Ok(buf.len());
        }
    }
}

/// `read_line_bounded_telnet`, plus the same idle-read deadline
/// `read_line_bounded_with_timeout` applies.
pub async fn read_line_bounded_telnet_with_timeout<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut String,
    filter: &mut IacFilter,
) -> std::io::Result<usize> {
    match tokio::time::timeout(
        IDLE_READ_TIMEOUT,
        read_line_bounded_telnet(reader, buf, filter),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            // The dropped read may have left a pending character.
            filter.utf8.clear();
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "read timed out",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn reads_a_normal_line() {
        let mut reader = BufReader::new(&b"hello\r\nworld\r\n"[..]);
        let mut buf = String::new();
        let n = read_line_bounded(&mut reader, &mut buf, &mut Utf8Tail::default())
            .await
            .unwrap();
        assert_eq!(buf, "hello\r\n");
        assert_eq!(n, 7);
    }

    #[tokio::test]
    async fn returns_zero_on_eof_with_no_data() {
        let mut reader = BufReader::new(&b""[..]);
        let mut buf = String::new();
        let n = read_line_bounded(&mut reader, &mut buf, &mut Utf8Tail::default())
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(buf, "");
    }

    #[tokio::test]
    async fn rejects_a_line_with_no_newline_past_the_cap() {
        let long = "A".repeat(MAX_LINE_BYTES + 1);
        let mut reader = BufReader::new(long.as_bytes());
        let mut buf = String::new();
        let err = read_line_bounded(&mut reader, &mut buf, &mut Utf8Tail::default())
            .await
            .expect_err("must reject an unbounded line");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    /// MAN-58/PR #80 review, round 2: invalid UTF-8 must error, not be
    /// silently lossy-converted -- `docs/DECISIONS/2026-09-02-man23-
    /// threat-model.md` finding 19 already commits to this disposition
    /// for the outbound uplink, which the raw `AsyncBufReadExt::read_line`
    /// it originally used already provided.
    #[tokio::test]
    async fn rejects_invalid_utf8() {
        let mut bytes = b"before ".to_vec();
        bytes.extend_from_slice(&[0xFF, 0xFE]); // never valid UTF-8
        bytes.extend_from_slice(b" after\n");
        let mut reader = BufReader::new(&bytes[..]);
        let mut buf = String::new();
        let err = read_line_bounded(&mut reader, &mut buf, &mut Utf8Tail::default())
            .await
            .expect_err("must reject invalid UTF-8, not lossy-convert it");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn accepts_a_line_exactly_at_the_cap() {
        let exact = "A".repeat(MAX_LINE_BYTES - 1) + "\n"; // total == MAX_LINE_BYTES
        let mut reader = BufReader::new(exact.as_bytes());
        let mut buf = String::new();
        let n = read_line_bounded(&mut reader, &mut buf, &mut Utf8Tail::default())
            .await
            .unwrap();
        assert_eq!(n, MAX_LINE_BYTES);
    }

    #[tokio::test]
    async fn resumes_a_partially_read_line_across_a_select_cancellation() {
        // Regression test for the telnet server's real bug: a command
        // split across TCP chunks, where `tokio::select!` cancels the
        // read future after it consumed the first chunk (no newline yet)
        // but before a second chunk arrives. The bytes already pulled off
        // the reader must not vanish with the cancelled future. Manually
        // polling once (rather than racing inside a real `select!`) makes
        // the cancellation point deterministic instead of scheduler-order
        // dependent.
        use std::future::Future;
        use tokio::io::AsyncWriteExt;

        let (mut write_half, read_half) = tokio::io::duplex(64);
        let mut reader = BufReader::new(read_half);
        let mut buf = String::new();
        let mut tail0 = Utf8Tail::default();

        write_half.write_all(b"sh/d").await.unwrap();
        tokio::task::yield_now().await; // let the duplex pipe deliver it

        {
            let fut = read_line_bounded(&mut reader, &mut buf, &mut tail0);
            tokio::pin!(fut);
            let waker = futures_util::task::noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            match fut.as_mut().poll(&mut cx) {
                std::task::Poll::Pending => {} // expected: no newline in "sh/d" yet
                std::task::Poll::Ready(r) => panic!("must not complete yet, got {r:?}"),
            }
            // `fut` drops here at the end of this scope -- the cancellation.
        }
        assert_eq!(
            buf, "sh/d",
            "bytes already consumed from the reader must survive cancellation"
        );

        write_half.write_all(b"x\r\n").await.unwrap();
        let n = read_line_bounded(&mut reader, &mut buf, &mut tail0)
            .await
            .unwrap();
        assert_eq!(buf, "sh/dx\r\n");
        assert_eq!(n, 7);
    }

    #[tokio::test]
    async fn timeout_variant_errors_when_no_data_ever_arrives() {
        // A reader that never becomes ready: pending forever. `duplex`
        // gives us a real AsyncRead half whose write side we simply never
        // write to and never close, so `fill_buf` stays pending.
        let (_write_half, read_half) = tokio::io::duplex(64);
        let mut reader = BufReader::new(read_half);
        let mut buf = String::new();
        let mut tail0 = Utf8Tail::default();

        tokio::time::pause();
        let fut = read_line_bounded_with_timeout(&mut reader, &mut buf, &mut tail0);
        tokio::pin!(fut);
        tokio::time::advance(IDLE_READ_TIMEOUT + Duration::from_secs(1)).await;
        let err = fut.await.expect_err("must time out");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn telnet_variant_strips_negotiation_and_queues_refusals() {
        let mut reader = BufReader::new(&b"\xff\xfb\x18\xff\xfd\x03W5AU\r\n"[..]);
        let mut buf = String::new();
        let mut filter = IacFilter::new();
        let n = read_line_bounded_telnet(&mut reader, &mut buf, &mut filter)
            .await
            .unwrap();
        assert_eq!(buf, "W5AU\r\n");
        assert_eq!(n, 6);
        assert_eq!(filter.take_replies(), vec![255, 254, 24, 255, 252, 3]);
    }

    #[tokio::test]
    async fn telnet_variant_does_not_end_the_line_on_an_option_byte_of_0x0a() {
        // Option 10 (NAOCRD) makes `IAC DO 10` contain a raw 0x0A -- a
        // reader scanning raw bytes for the newline would cut the line
        // in half here.
        let mut reader = BufReader::new(&b"\xff\xfd\x0aW5AU\n"[..]);
        let mut buf = String::new();
        let mut filter = IacFilter::new();
        read_line_bounded_telnet(&mut reader, &mut buf, &mut filter)
            .await
            .unwrap();
        assert_eq!(buf, "W5AU\n");
    }

    #[tokio::test]
    async fn telnet_variant_caps_a_flood_of_negotiation_that_never_ends_a_line() {
        // Stripped bytes still count against MAX_FRAMING_BYTES (MAN-87
        // remediation, round-4 validation, code-review finding F2: this
        // budget used to be shared with the per-line content cap, now
        // split out into its own connection-lifetime counter -- see
        // `filter.framing_bytes`), so endless negotiation cannot hold a
        // read open forever -- regardless of how the flood happens to be
        // chunked on the wire. Driven through a small-buffer
        // `tokio::io::duplex` (not a single `BufReader`-over-slice, where
        // one `fill_buf` call hands over the whole flood at once) so the
        // cap is proven across several genuinely separate reads, matching
        // what a real socket delivers (round-3 validation, code-review
        // finding 3: the prior version of this test passed only by
        // accident of that single-chunk shortcut). This also covers
        // finding 2: MAN-87 review round 2 (C2) released the (then-shared)
        // budget on pure-framing reads to keep a legitimate slow
        // keepalive-only client from tripping the cap, but that made the
        // cap unreachable for exactly this flood -- reverted; splitting
        // the budget instead of reviving that release keeps trickled
        // negotiation that never completes a line capped the same as a
        // single burst.
        use tokio::io::AsyncWriteExt;

        let (mut write_half, read_half) = tokio::io::duplex(64);
        let mut reader = BufReader::new(read_half);
        let mut buf = String::new();
        let mut filter = IacFilter::new();

        let _writer = tokio::spawn(async move {
            let mut sent = 0usize;
            while sent <= MAX_FRAMING_BYTES {
                if write_half.write_all(b"\xff\xfb\x18").await.is_err() {
                    return; // reader stopped reading once the cap tripped
                }
                sent += 3;
            }
        });

        let err = read_line_bounded_telnet(&mut reader, &mut buf, &mut filter)
            .await
            .expect_err("negotiation flood must hit the framing cap regardless of chunking");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(buf.is_empty());
    }

    #[tokio::test]
    async fn telnet_variant_keepalive_only_traffic_does_not_trip_the_line_cap() {
        // MAN-87 remediation (round-4 validation, code-review finding F2):
        // PuTTY's telnet keepalive is `IAC NOP` (2 bytes). Before this fix,
        // framing bytes counted against the same per-line budget as
        // application content, reset only on line completion -- so a
        // keepalive-only client (never completing a line) accumulated that
        // budget across its whole session and was eventually disconnected
        // with "line exceeds maximum length" after ~1024 raw bytes (~512
        // keepalives), having sent nothing resembling a long line. 600
        // pairs (1200 bytes) exceeds that old cap; with framing now
        // tracked in its own separate, much larger budget, the connection
        // survives and a real line sent afterwards still reads correctly.
        use tokio::io::AsyncWriteExt;

        let (mut write_half, read_half) = tokio::io::duplex(64);
        let mut reader = BufReader::new(read_half);
        let mut buf = String::new();
        let mut filter = IacFilter::new();

        let _writer = tokio::spawn(async move {
            for _ in 0..600 {
                if write_half.write_all(b"\xff\xf1").await.is_err() {
                    return;
                }
            }
            let _ = write_half.write_all(b"W5AU\n").await;
        });

        let n = read_line_bounded_telnet(&mut reader, &mut buf, &mut filter)
            .await
            .expect("keepalive-only traffic must not trip the line cap");
        assert_eq!(buf, "W5AU\n");
        assert_eq!(n, 5);
        assert_eq!(
            filter.framing_bytes, 1200,
            "framing bytes are still tracked, just in the separate, larger budget"
        );
    }

    #[tokio::test]
    async fn telnet_variant_still_rejects_genuinely_invalid_utf8() {
        // 0xC3 0x28 is malformed UTF-8 and is NOT telnet framing -- the
        // MAN-23 rejection must survive the IAC change.
        let mut reader = BufReader::new(&b"ab\xc3\x28cd\n"[..]);
        let mut buf = String::new();
        let mut filter = IacFilter::new();
        let err = read_line_bounded_telnet(&mut reader, &mut buf, &mut filter)
            .await
            .expect_err("must still reject malformed UTF-8");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn telnet_variant_accepts_a_cr_nul_terminated_line_with_the_connection_kept_open() {
        // MAN-87 review round 2 (C1): CR NUL must end a line even when the
        // client keeps the connection open afterwards (interactive BSD/
        // macOS telnet(1), `crlf` toggle default FALSE) -- not only when
        // it is followed by EOF, which is all the piped-stdin case proves.
        use std::future::Future;
        use tokio::io::AsyncWriteExt;

        let (mut write_half, read_half) = tokio::io::duplex(64);
        let mut reader = BufReader::new(read_half);
        let mut buf = String::new();
        let mut filter = IacFilter::new();

        write_half.write_all(b"W5AU\r\0").await.unwrap();
        tokio::task::yield_now().await;

        let n = {
            let fut = read_line_bounded_telnet(&mut reader, &mut buf, &mut filter);
            tokio::pin!(fut);
            let waker = futures_util::task::noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            match fut.as_mut().poll(&mut cx) {
                std::task::Poll::Ready(r) => r.expect("must accept the CR NUL terminated line"),
                std::task::Poll::Pending => {
                    panic!("must not stall waiting for EOF that never comes")
                }
            }
        };
        assert_eq!(buf, "W5AU\r\0");
        assert_eq!(n, 6);
    }

    #[tokio::test]
    async fn telnet_variant_resumes_a_negotiation_split_across_a_cancellation() {
        use std::future::Future;
        use tokio::io::AsyncWriteExt;

        let (mut write_half, read_half) = tokio::io::duplex(64);
        let mut reader = BufReader::new(read_half);
        let mut buf = String::new();
        let mut filter = IacFilter::new();

        write_half.write_all(b"\xff\xfb").await.unwrap(); // IAC WILL, option pending
        tokio::task::yield_now().await;
        {
            let fut = read_line_bounded_telnet(&mut reader, &mut buf, &mut filter);
            tokio::pin!(fut);
            let waker = futures_util::task::noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            match fut.as_mut().poll(&mut cx) {
                std::task::Poll::Pending => {}
                std::task::Poll::Ready(r) => panic!("must not complete yet, got {r:?}"),
            }
        }
        write_half.write_all(b"\x18W5AU\n").await.unwrap();
        read_line_bounded_telnet(&mut reader, &mut buf, &mut filter)
            .await
            .unwrap();
        assert_eq!(
            buf, "W5AU\n",
            "the split IAC sequence must not leak into the line"
        );
        assert_eq!(filter.take_replies(), vec![255, 254, 24]);
    }

    /// MAN-86/PR #128 review: a `CR NUL`-terminated login must not swallow
    /// the next command (`SKIMMER/SETT`) sent on the same connection.
    #[tokio::test]
    async fn telnet_variant_cr_nul_does_not_swallow_the_next_command() {
        let mut reader = BufReader::new(&b"N0CALL\r\0SETT\r\0"[..]);
        let mut buf = String::new();
        let mut filter = IacFilter::new();
        let n = read_line_bounded_telnet(&mut reader, &mut buf, &mut filter)
            .await
            .unwrap();
        assert_eq!(buf, "N0CALL\r\0");
        assert_eq!(n, 8);
        buf.clear();
        read_line_bounded_telnet(&mut reader, &mut buf, &mut filter)
            .await
            .unwrap();
        assert_eq!(buf, "SETT\r\0");
    }

    #[tokio::test]
    async fn telnet_variant_still_terminates_on_a_plain_newline_first() {
        // An `LF` earlier in the chunk wins over a later `CR NUL`.
        let mut reader = BufReader::new(&b"BYE\r\nSETT\r\0"[..]);
        let mut buf = String::new();
        let mut filter = IacFilter::new();
        read_line_bounded_telnet(&mut reader, &mut buf, &mut filter)
            .await
            .unwrap();
        assert_eq!(buf, "BYE\r\n");
    }

    #[tokio::test]
    async fn plain_variant_does_not_treat_cr_nul_as_a_terminator() {
        // The HTTP/uplink callers keep the strict `LF`-only behavior.
        use std::future::Future;
        use tokio::io::AsyncWriteExt;

        let (mut write_half, read_half) = tokio::io::duplex(64);
        let mut reader = BufReader::new(read_half);
        let mut buf = String::new();
        let mut tail0 = Utf8Tail::default();
        write_half.write_all(b"GET / HTTP/1.1\r\0").await.unwrap();
        tokio::task::yield_now().await;

        let fut = read_line_bounded(&mut reader, &mut buf, &mut tail0);
        tokio::pin!(fut);
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(
            matches!(fut.as_mut().poll(&mut cx), std::task::Poll::Pending),
            "CR NUL must not terminate a non-Telnet line"
        );
    }

    /// The two terminator bytes can arrive in separate TCP segments.
    #[tokio::test]
    async fn telnet_variant_recognizes_cr_nul_split_across_chunks() {
        use std::future::Future;
        use tokio::io::AsyncWriteExt;

        let (mut write_half, read_half) = tokio::io::duplex(64);
        let mut reader = BufReader::new(read_half);
        let mut buf = String::new();
        let mut filter = IacFilter::new();

        write_half.write_all(b"N0CALL\r").await.unwrap();
        tokio::task::yield_now().await;
        {
            let fut = read_line_bounded_telnet(&mut reader, &mut buf, &mut filter);
            tokio::pin!(fut);
            let waker = futures_util::task::noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            match fut.as_mut().poll(&mut cx) {
                std::task::Poll::Pending => {} // no terminator yet
                std::task::Poll::Ready(r) => panic!("must not complete yet, got {r:?}"),
            }
        }
        assert_eq!(buf, "N0CALL\r");

        write_half.write_all(&[0]).await.unwrap();
        let n = read_line_bounded_telnet(&mut reader, &mut buf, &mut filter)
            .await
            .unwrap();
        assert_eq!(buf, "N0CALL\r\0");
        assert_eq!(n, 8);
    }

    // ---- MAN-538: a character split across reads ----

    /// Lines whose last one ends in `€\n`, laid out so `€`'s first two
    /// bytes (`E2 82`) are the last two bytes of the first `cap`-byte fill.
    fn straddling_lines(cap: usize) -> Vec<u8> {
        let room = cap - 2;
        let full = room / 50;
        let pad = room - full * 50;
        let mut bytes = Vec::new();
        for _ in 0..full {
            bytes.extend_from_slice(&[b'x'; 49]);
            bytes.push(b'\n');
        }
        bytes.extend(std::iter::repeat_n(b'y', pad));
        bytes.extend_from_slice("€\n".as_bytes());
        assert_eq!(bytes[cap - 2..cap], [0xE2, 0x82]);
        bytes
    }

    #[tokio::test]
    async fn a_character_split_across_chunks_is_accepted() {
        for cap in [4, 16, 8192, 16384] {
            let bytes = straddling_lines(cap);
            // Lines longer than the cap would be rejected for length, so
            // the long-capacity layouts are 50-byte lines plus a short one.
            let mut reader = BufReader::with_capacity(cap, &bytes[..]);
            let mut tail = Utf8Tail::default();
            let mut last = String::new();
            loop {
                let mut buf = String::new();
                let n = read_line_bounded(&mut reader, &mut buf, &mut tail)
                    .await
                    .unwrap_or_else(|e| panic!("plain, cap {cap}: {e}"));
                if n == 0 {
                    break;
                }
                assert!(tail.is_empty());
                last = buf;
            }
            assert!(last.ends_with("€\n"), "plain, cap {cap}: {last:?}");

            let mut reader = BufReader::with_capacity(cap, &bytes[..]);
            let mut filter = IacFilter::new();
            let mut last = String::new();
            loop {
                let mut buf = String::new();
                let n = read_line_bounded_telnet(&mut reader, &mut buf, &mut filter)
                    .await
                    .unwrap_or_else(|e| panic!("telnet, cap {cap}: {e}"));
                if n == 0 {
                    break;
                }
                assert!(filter.utf8.is_empty());
                last = buf;
            }
            assert!(last.ends_with("€\n"), "telnet, cap {cap}: {last:?}");
        }
    }

    #[tokio::test]
    async fn a_split_character_survives_a_cancelled_read() {
        use std::future::Future;
        use tokio::io::AsyncWriteExt;

        let (mut write_half, read_half) = tokio::io::duplex(64);
        let mut reader = BufReader::new(read_half);
        let mut buf = String::new();
        let mut tail = Utf8Tail::default();
        write_half.write_all(b"caf\xE2\x82").await.unwrap();
        tokio::task::yield_now().await;
        {
            let fut = read_line_bounded(&mut reader, &mut buf, &mut tail);
            tokio::pin!(fut);
            let waker = futures_util::task::noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            match fut.as_mut().poll(&mut cx) {
                std::task::Poll::Pending => {}
                std::task::Poll::Ready(r) => panic!("must not complete yet, got {r:?}"),
            }
        }
        assert_eq!(buf, "caf");
        assert_eq!(tail.len(), 2);

        write_half.write_all(b"\xAC\n").await.unwrap();
        let n = read_line_bounded(&mut reader, &mut buf, &mut tail)
            .await
            .unwrap();
        assert_eq!((buf.as_str(), n), ("caf€\n", 7));
        assert!(tail.is_empty());
    }

    #[tokio::test]
    async fn a_split_character_survives_a_cancelled_telnet_read() {
        use std::future::Future;
        use tokio::io::AsyncWriteExt;

        let (mut write_half, read_half) = tokio::io::duplex(64);
        let mut reader = BufReader::new(read_half);
        let mut buf = String::new();
        let mut filter = IacFilter::new();
        write_half.write_all(b"caf\xE2\x82").await.unwrap();
        tokio::task::yield_now().await;
        {
            let fut = read_line_bounded_telnet(&mut reader, &mut buf, &mut filter);
            tokio::pin!(fut);
            let waker = futures_util::task::noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            match fut.as_mut().poll(&mut cx) {
                std::task::Poll::Pending => {}
                std::task::Poll::Ready(r) => panic!("must not complete yet, got {r:?}"),
            }
        }
        assert_eq!(buf, "caf");
        assert_eq!(filter.utf8.len(), 2);

        write_half.write_all(b"\xAC\r\n").await.unwrap();
        let n = read_line_bounded_telnet(&mut reader, &mut buf, &mut filter)
            .await
            .unwrap();
        assert_eq!((buf.as_str(), n), ("caf€\r\n", 8));
        assert!(filter.utf8.is_empty());
    }

    #[tokio::test]
    async fn invalid_utf8_is_still_rejected_at_any_boundary() {
        let cases: [(&[u8], bool); 5] = [
            (b"caf\xE2\x28\n", true),
            (b"\x80\n", true),
            (b"caf\xE2\x82\n", true), // a character cut by the newline
            (b"caf\xE2", true),       // EOF inside a character
            (b"\xFF\n", false),       // telnet strips 0xFF as IAC
        ];
        for (bytes, telnet_too) in cases {
            let mut reader = BufReader::with_capacity(4, bytes);
            let mut buf = String::new();
            let mut tail = Utf8Tail::default();
            let err = read_line_bounded(&mut reader, &mut buf, &mut tail)
                .await
                .expect_err("plain must reject");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{bytes:?}");
            assert_eq!(err.to_string(), "line contains invalid UTF-8", "{bytes:?}");
            assert!(tail.is_empty(), "{bytes:?}");

            if telnet_too {
                let mut reader = BufReader::with_capacity(4, bytes);
                let mut buf = String::new();
                let mut filter = IacFilter::new();
                let err = read_line_bounded_telnet(&mut reader, &mut buf, &mut filter)
                    .await
                    .expect_err("telnet must reject");
                assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{bytes:?}");
                assert_eq!(err.to_string(), "line contains invalid UTF-8", "{bytes:?}");
                assert!(filter.utf8.is_empty(), "{bytes:?}");
            }
        }
    }

    #[tokio::test]
    async fn the_line_cap_counts_carried_bytes() {
        // 1020 + 3 + 1 = 1024 bytes; with cap 1021 the first fill ends
        // right after `€`'s lead byte.
        for (a_count, ok) in [(1020, true), (1021, false)] {
            let line = "a".repeat(a_count) + "€\n";
            for telnet in [false, true] {
                let mut reader = BufReader::with_capacity(1021, line.as_bytes());
                let mut buf = String::new();
                let result = if telnet {
                    read_line_bounded_telnet(&mut reader, &mut buf, &mut IacFilter::new()).await
                } else {
                    read_line_bounded(&mut reader, &mut buf, &mut Utf8Tail::default()).await
                };
                match (ok, result) {
                    (true, Ok(n)) => assert_eq!((n, buf.as_str()), (1024, line.as_str())),
                    (false, Err(e)) => {
                        assert_eq!(e.to_string(), "line exceeds maximum length")
                    }
                    (_, r) => panic!("a = {a_count}, telnet = {telnet}: {r:?}"),
                }
            }
        }
        // Rejected for length while a character is pending (the first fill
        // ends right after `€`'s lead byte): the tail is still emptied.
        let line = "a".repeat(1021) + "€\n";
        let mut reader = BufReader::with_capacity(1022, line.as_bytes());
        let mut tail = Utf8Tail::default();
        let err = read_line_bounded(&mut reader, &mut String::new(), &mut tail)
            .await
            .expect_err("1025 bytes");
        assert_eq!(err.to_string(), "line exceeds maximum length");
        assert!(tail.is_empty());
    }

    #[tokio::test]
    async fn cr_then_a_pending_lead_byte_then_nul_is_not_a_line_end() {
        use tokio::io::AsyncWriteExt;

        let (mut write_half, read_half) = tokio::io::duplex(64);
        let mut reader = BufReader::new(read_half);
        let mut buf = String::new();
        let mut filter = IacFilter::new();
        write_half.write_all(b"ab\r\xE2").await.unwrap();
        tokio::task::yield_now().await;
        {
            use std::future::Future;
            let fut = read_line_bounded_telnet(&mut reader, &mut buf, &mut filter);
            tokio::pin!(fut);
            let waker = futures_util::task::noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            assert!(fut.as_mut().poll(&mut cx).is_pending());
        }
        assert_eq!((buf.as_str(), filter.utf8.len()), ("ab\r", 1));
        write_half.write_all(&[0]).await.unwrap();
        let err = read_line_bounded_telnet(&mut reader, &mut buf, &mut filter)
            .await
            .expect_err("E2 00 is not UTF-8, and the CR is not right before the NUL");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(filter.utf8.is_empty());
    }
}
