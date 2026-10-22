//! Unbound loopback ports for this binary's unit tests (MAN-410).
//!
//! Binding `127.0.0.1:0` and dropping the listener lets the kernel hand
//! the freed number to a sibling test before the caller probes it. Use
//! the private band 20256-20511 instead, below the default ephemeral
//! ranges on Linux (32768) and macOS/Windows (49152), and disjoint from
//! the 20000-20255 integration-test band in `tests/doctor_checks.rs`.
//! Holding a listener cannot express an unbound port: connections would
//! succeed, and doctor's bind probe would report it as in use.
//!
//! All unit tests in this binary that need an unbound port must share
//! this allocator. A second cursor over the same band in this process
//! would issue the same numbers. Like the integration-test allocator,
//! this assumes cargo test's single process per binary, not nextest.

use std::net::TcpListener;
use std::sync::Mutex;

const BAND_START: u16 = 20_256;
const BAND_LEN: u16 = 256;

/// A monotonic cursor issues each port at most once per process.
struct Band {
    next: u16,
    range_checked: bool,
}

static BAND: Mutex<Band> = Mutex::new(Band {
    next: 0,
    range_checked: false,
});

impl Band {
    /// Fail loudly, once, if this host's ephemeral port range reaches into
    /// the band: a widened range would hand `bind(…, 0)` ports inside the
    /// band and silently reintroduce the race the band exists to remove.
    /// Linux states its range in `/proc/sys/net/ipv4/ip_local_port_range`;
    /// macOS and Windows default to 49152-65535, far above the band, and
    /// are not checked.
    fn check_ephemeral_range_once(&mut self) {
        if self.range_checked {
            return;
        }
        self.range_checked = true;
        #[cfg(target_os = "linux")]
        {
            let text = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
                .expect("read /proc/sys/net/ipv4/ip_local_port_range");
            let ends: [u32; 2] = text
                .split_whitespace()
                .map(|t| t.parse().expect("ip_local_port_range endpoint"))
                .collect::<Vec<_>>()
                .try_into()
                .expect("ip_local_port_range holds two numbers");
            let [lo, hi] = ends;
            let band_lo = u32::from(BAND_START);
            let band_hi = band_lo + u32::from(BAND_LEN) - 1;
            assert!(
                hi < band_lo || lo > band_hi,
                "this host's ephemeral port range {lo}-{hi} overlaps the test port band \
                 {band_lo}-{band_hi}: move the band out from under it"
            );
        }
    }
}

/// The next unissued band port that a TCP bind on 127.0.0.1 accepts.
/// Skip candidates a foreign program holds or Windows excludes; panic
/// when the band is exhausted.
pub(crate) fn free_port() -> u16 {
    let mut band = BAND.lock().unwrap();
    band.check_ephemeral_range_once();
    loop {
        assert!(
            band.next < BAND_LEN,
            "test port band {}-{} exhausted",
            BAND_START,
            BAND_START + BAND_LEN - 1
        );
        let port = BAND_START + band.next;
        band.next += 1;
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

#[test]
fn concurrent_handouts_are_distinct_and_inside_the_band() {
    let handles: Vec<_> = (0..8)
        .map(|_| std::thread::spawn(|| [free_port(), free_port(), free_port()]))
        .collect();
    let mut ports: Vec<u16> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    let band = BAND_START..BAND_START + BAND_LEN;
    assert!(ports.iter().all(|p| band.contains(p)), "{ports:?}");
    let total = ports.len();
    ports.sort_unstable();
    ports.dedup();
    assert_eq!(ports.len(), total, "a port was handed out twice");
}
