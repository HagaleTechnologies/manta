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
/// **Known limitation** (PR #80 review, round 3, P3): UTF-8 is validated
/// per `fill_buf` chunk, not over the fully-assembled line -- a valid
/// multi-byte character whose bytes are split exactly across two chunks
/// (a rare TCP-segmentation artifact) is rejected as `InvalidData` even
/// though the complete line would have been valid. A fully correct fix
/// requires buffering unvalidated raw bytes somewhere that survives a
/// `tokio::select!` cancellation -- the only thing that survives is `buf`
/// itself (a local variable inside this function is lost the same way
/// `pending` bytes would be), and `buf: &mut String` cannot hold
/// not-yet-valid bytes by Rust's own invariant, so closing this
/// completely means threading a raw `&mut Vec<u8>` through every caller
/// (`telnet.rs`, `metrics_http.rs`, `uplink.rs`) instead. A cheaper-looking
/// alternative -- leave the incomplete tail unconsumed and retry
/// `fill_buf` for more -- is unsound as written: `tokio::io::BufReader`'s
/// `poll_fill_buf` returns already-buffered-but-unconsumed bytes
/// synchronously without polling the underlying reader, so a loop that
/// consumes nothing new would spin without ever yielding or actually
/// waiting for more data. Accepted as-is: every protocol using this
/// function (telnet commands, the outbound RBN uplink's `DX de` wire
/// format, HTTP request lines) is ASCII by spec, so the failure mode is a
/// spurious reconnect/disconnect on essentially never-occurring non-ASCII
/// input at an unlucky buffer boundary, not data loss or corruption.
pub async fn read_line_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut String,
) -> std::io::Result<usize> {
    let mut total = buf.len();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(total); // EOF
        }
        let (chunk_len, found_newline) = match available.iter().position(|&b| b == b'\n') {
            Some(nl) => (nl + 1, true),
            None => (available.len(), false),
        };
        total += chunk_len;
        if total > MAX_LINE_BYTES {
            reader.consume(chunk_len);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "line exceeds maximum length",
            ));
        }
        buf.push_str(std::str::from_utf8(&available[..chunk_len]).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "line contains invalid UTF-8",
            )
        })?);
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
) -> std::io::Result<usize> {
    tokio::time::timeout(IDLE_READ_TIMEOUT, read_line_bounded(reader, buf))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "read timed out"))?
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
/// The length cap counts RAW bytes consumed (`filter.raw_line_bytes`),
/// not surviving text bytes, so a client streaming endless negotiation
/// still hits `MAX_LINE_BYTES` instead of reading forever, and that
/// budget is released ONLY when a line completes or a read errors (never
/// on a read that consumed nothing but protocol framing). MAN-87 review
/// round 2 (C2) tried releasing it on pure-framing reads too, to keep a
/// client sending only occasional keepalive negotiation from accumulating
/// `raw_line_bytes` across a long session -- but that made the cap
/// unreachable for a client that never lets a read return anything but
/// framing (a flood of `IAC SB`/`IAC NOP`), and chunk-size dependent for
/// everyone else (round-3 validation, code-review findings 2/3). Reverted:
/// the few dozen bytes real negotiation costs against the 1024-byte budget
/// (this decision's own original tradeoff) is cheaper than reopening the
/// cap.
///
/// Cancellation-safety matches `read_line_bounded`: every byte pulled
/// off the reader is folded into `buf` and `filter` -- both caller-owned
/// -- before `consume`, with no await point in between.
pub async fn read_line_bounded_telnet<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut String,
    filter: &mut IacFilter,
) -> std::io::Result<usize> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
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
                    .or_else(|| buf.as_bytes().last().copied());
                text.push(app);
                if app == b'\n' || (app == 0 && prev == Some(b'\r')) {
                    found_newline = true;
                    break;
                }
            }
        }
        filter.raw_line_bytes += consumed;
        if filter.raw_line_bytes > MAX_LINE_BYTES {
            reader.consume(consumed);
            filter.reset_line();
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "line exceeds maximum length",
            ));
        }
        match std::str::from_utf8(&text) {
            Ok(s) => buf.push_str(s),
            Err(_) => {
                reader.consume(consumed);
                filter.reset_line();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "line contains invalid UTF-8",
                ));
            }
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
    tokio::time::timeout(
        IDLE_READ_TIMEOUT,
        read_line_bounded_telnet(reader, buf, filter),
    )
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "read timed out"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn reads_a_normal_line() {
        let mut reader = BufReader::new(&b"hello\r\nworld\r\n"[..]);
        let mut buf = String::new();
        let n = read_line_bounded(&mut reader, &mut buf).await.unwrap();
        assert_eq!(buf, "hello\r\n");
        assert_eq!(n, 7);
    }

    #[tokio::test]
    async fn returns_zero_on_eof_with_no_data() {
        let mut reader = BufReader::new(&b""[..]);
        let mut buf = String::new();
        let n = read_line_bounded(&mut reader, &mut buf).await.unwrap();
        assert_eq!(n, 0);
        assert_eq!(buf, "");
    }

    #[tokio::test]
    async fn rejects_a_line_with_no_newline_past_the_cap() {
        let long = "A".repeat(MAX_LINE_BYTES + 1);
        let mut reader = BufReader::new(long.as_bytes());
        let mut buf = String::new();
        let err = read_line_bounded(&mut reader, &mut buf)
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
        let err = read_line_bounded(&mut reader, &mut buf)
            .await
            .expect_err("must reject invalid UTF-8, not lossy-convert it");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn accepts_a_line_exactly_at_the_cap() {
        let exact = "A".repeat(MAX_LINE_BYTES - 1) + "\n"; // total == MAX_LINE_BYTES
        let mut reader = BufReader::new(exact.as_bytes());
        let mut buf = String::new();
        let n = read_line_bounded(&mut reader, &mut buf).await.unwrap();
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

        write_half.write_all(b"sh/d").await.unwrap();
        tokio::task::yield_now().await; // let the duplex pipe deliver it

        {
            let fut = read_line_bounded(&mut reader, &mut buf);
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
        let n = read_line_bounded(&mut reader, &mut buf).await.unwrap();
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

        tokio::time::pause();
        let fut = read_line_bounded_with_timeout(&mut reader, &mut buf);
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
        // Stripped bytes still count against MAX_LINE_BYTES, so endless
        // negotiation cannot hold a read open forever -- regardless of how
        // the flood happens to be chunked on the wire. Driven through a
        // small-buffer `tokio::io::duplex` (not a single `BufReader`-over-
        // slice, where one `fill_buf` call hands over the whole flood at
        // once) so the cap is proven across several genuinely separate
        // reads, matching what a real socket delivers (round-3 validation,
        // code-review finding 3: the prior version of this test passed
        // only by accident of that single-chunk shortcut). This also
        // covers finding 2: MAN-87 review round 2 (C2) added a budget
        // release for pure-framing reads to keep a legitimate slow
        // keepalive-only client from tripping the cap, but that made the
        // cap unreachable for exactly this flood -- reverted, so trickled
        // negotiation that never completes a line is capped the same as a
        // single burst.
        use tokio::io::AsyncWriteExt;

        let (mut write_half, read_half) = tokio::io::duplex(64);
        let mut reader = BufReader::new(read_half);
        let mut buf = String::new();
        let mut filter = IacFilter::new();

        let _writer = tokio::spawn(async move {
            let mut sent = 0usize;
            while sent <= MAX_LINE_BYTES {
                if write_half.write_all(b"\xff\xfb\x18").await.is_err() {
                    return; // reader stopped reading once the cap tripped
                }
                sent += 3;
            }
        });

        let err = read_line_bounded_telnet(&mut reader, &mut buf, &mut filter)
            .await
            .expect_err("negotiation flood must hit the line cap regardless of chunking");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(buf.is_empty());
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
}
