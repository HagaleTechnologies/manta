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
}
