//! Bounded, timed-out line reads shared by the telnet and metrics HTTP
//! servers -- both are publicly bound (ARCHITECTURE §7), so an
//! unauthenticated client sending a line with no terminating newline must
//! not be able to grow a read buffer without bound, and an idle client
//! must not hold a spawned task open forever.

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
    read_line_inner(reader, buf, false).await
}

/// `read_line_bounded`, but ALSO treating the Telnet NVT `CR NUL`
/// sequence as a complete line terminator (MAN-86/PR #128 review).
///
/// RFC 854 encodes an "Enter" keypress on an NVT as `CR NUL`, and real
/// clients send exactly that with no `LF` following it -- macOS `telnet`
/// is the one recorded on MAN-86's own ticket. Waiting for `\n` there
/// means the login line never completes (the client is disconnected by
/// `IDLE_READ_TIMEOUT` 30 s later, having sent a perfectly well-formed
/// callsign) and a post-login `SKIMMER/SETT` or `BYE` sits in the buffer
/// forever -- i.e. exactly the "Aggregator never gets its SETT reply"
/// failure this ticket exists to fix, for any client that terminates
/// with `CR NUL`.
///
/// The terminator bytes stay in `buf` like `\r\n` does; callers strip
/// them (`telnet::sanitize_login`, `command::parse`) rather than this
/// function editing the line it returns.
///
/// Only the Telnet listener uses this: the metrics HTTP request path
/// (RFC 9112 is `CRLF`-terminated, and a bare `NUL` there is malformed)
/// and the outbound RBN uplink (line-oriented `LF`) both keep the plain
/// `read_line_bounded` behavior.
pub async fn read_telnet_line_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut String,
) -> std::io::Result<usize> {
    read_line_inner(reader, buf, true).await
}

async fn read_line_inner<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut String,
    cr_nul_terminates: bool,
) -> std::io::Result<usize> {
    let mut total = buf.len();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(total); // EOF
        }
        // `CR NUL` split across two `fill_buf` chunks: the `CR` is
        // already in `buf` (appended by a previous iteration, or by a
        // previous cancelled-and-resumed call), so it can only be
        // recognized from there. Handled before the in-chunk scan below
        // because a two-byte terminator is the one thing that scan
        // cannot see across a chunk boundary.
        if cr_nul_terminates && buf.ends_with('\r') && available[0] == 0 {
            total += 1;
            if total > MAX_LINE_BYTES {
                reader.consume(1);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "line exceeds maximum length",
                ));
            }
            buf.push('\0');
            reader.consume(1);
            return Ok(total);
        }
        let newline_end = available.iter().position(|&b| b == b'\n').map(|i| i + 1);
        let cr_nul_end = if cr_nul_terminates {
            available
                .windows(2)
                .position(|w| w == [b'\r', 0])
                .map(|i| i + 2)
        } else {
            None
        };
        // Whichever terminator comes FIRST ends the line -- a `CR NUL`
        // later in the chunk must not swallow an earlier `LF`.
        let (chunk_len, found_newline) = match (newline_end, cr_nul_end) {
            (Some(a), Some(b)) => (a.min(b), true),
            (Some(a), None) => (a, true),
            (None, Some(b)) => (b, true),
            (None, None) => (available.len(), false),
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

/// `read_telnet_line_bounded`, plus the same idle-read deadline
/// `read_line_bounded_with_timeout` applies.
pub async fn read_telnet_line_bounded_with_timeout<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut String,
) -> std::io::Result<usize> {
    tokio::time::timeout(IDLE_READ_TIMEOUT, read_telnet_line_bounded(reader, buf))
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

    /// MAN-86/PR #128 review: RFC 854 encodes Enter as `CR NUL`, and
    /// macOS `telnet` sends it with no trailing `LF` -- the Telnet read
    /// path must complete the line on that sequence alone.
    #[tokio::test]
    async fn telnet_variant_treats_cr_nul_as_a_terminator() {
        let mut reader = BufReader::new(&b"N0CALL\r\0SETT\r\0"[..]);
        let mut buf = String::new();
        let n = read_telnet_line_bounded(&mut reader, &mut buf)
            .await
            .unwrap();
        assert_eq!(buf, "N0CALL\r\0");
        assert_eq!(n, 8);
        // The next command on the same connection is not swallowed with it.
        buf.clear();
        read_telnet_line_bounded(&mut reader, &mut buf)
            .await
            .unwrap();
        assert_eq!(buf, "SETT\r\0");
    }

    #[tokio::test]
    async fn telnet_variant_still_terminates_on_a_plain_newline_first() {
        // An `LF` earlier in the chunk wins over a later `CR NUL`.
        let mut reader = BufReader::new(&b"BYE\r\nSETT\r\0"[..]);
        let mut buf = String::new();
        read_telnet_line_bounded(&mut reader, &mut buf)
            .await
            .unwrap();
        assert_eq!(buf, "BYE\r\n");
    }

    #[tokio::test]
    async fn plain_variant_does_not_treat_cr_nul_as_a_terminator() {
        // The HTTP/uplink callers keep the strict `LF`-only behavior:
        // `CR NUL` is not a terminator there, so this line stays
        // incomplete and the read blocks past what was written.
        use std::future::Future;
        use tokio::io::AsyncWriteExt;

        let (mut write_half, read_half) = tokio::io::duplex(64);
        let mut reader = BufReader::new(read_half);
        let mut buf = String::new();
        write_half.write_all(b"GET / HTTP/1.1\r\0").await.unwrap();
        tokio::task::yield_now().await;

        let fut = read_line_bounded(&mut reader, &mut buf);
        tokio::pin!(fut);
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(
            matches!(fut.as_mut().poll(&mut cx), std::task::Poll::Pending),
            "CR NUL must not terminate a non-Telnet line"
        );
    }

    /// The two terminator bytes can arrive in separate TCP segments --
    /// the `CR` is already in `buf` when the `NUL` chunk shows up, which
    /// is the only place it can be recognized from.
    #[tokio::test]
    async fn telnet_variant_recognizes_cr_nul_split_across_chunks() {
        use std::future::Future;
        use tokio::io::AsyncWriteExt;

        let (mut write_half, read_half) = tokio::io::duplex(64);
        let mut reader = BufReader::new(read_half);
        let mut buf = String::new();

        write_half.write_all(b"N0CALL\r").await.unwrap();
        tokio::task::yield_now().await;
        {
            let fut = read_telnet_line_bounded(&mut reader, &mut buf);
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
        let n = read_telnet_line_bounded(&mut reader, &mut buf)
            .await
            .unwrap();
        assert_eq!(buf, "N0CALL\r\0");
        assert_eq!(n, 8);
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
}
