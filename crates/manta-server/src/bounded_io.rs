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
/// read, `0` meaning EOF -- matching `AsyncBufReadExt::read_line`'s
/// contract for everything except the size cap.
pub async fn read_line_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut String,
) -> std::io::Result<usize> {
    buf.clear();
    let mut total = 0usize;
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
        buf.push_str(&String::from_utf8_lossy(&available[..chunk_len]));
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

    #[tokio::test]
    async fn accepts_a_line_exactly_at_the_cap() {
        let exact = "A".repeat(MAX_LINE_BYTES - 1) + "\n"; // total == MAX_LINE_BYTES
        let mut reader = BufReader::new(exact.as_bytes());
        let mut buf = String::new();
        let n = read_line_bounded(&mut reader, &mut buf).await.unwrap();
        assert_eq!(n, MAX_LINE_BYTES);
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
