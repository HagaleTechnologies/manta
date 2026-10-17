# field-node fixtures (MAN-96)

`scripts/field-node.py`'s tests and `crates/manta-server/tests/field_node_contract.rs`
read these files. `metrics-live.txt` and `healthz-ok.txt` are bodies served by a real
`manta run`, not hand-written text.

| File | What it is |
|---|---|
| `metrics-live.txt` | `GET /metrics` from a file-source daemon, saved on the first scrape that carried a `manta_spots_by_band_total{…}` series line (the family appears as HELP/TYPE only until the first spot). It includes `manta_source_outages_total{source="file"} 0` and `manta_source_down_seconds_total{source="file"} 0.000`. |
| `healthz-ok.txt` | `GET /healthz` from the same daemon, saved straight after the scrape above. |
| `healthz-unhealthy.txt` | `healthz-ok.txt` edited by hand: the first line changed to `unhealthy` and `source file: healthy` changed to `source soapy: unhealthy`. This is the shape `HealthReport::render_text` (`crates/manta-server/src/health.rs`) writes for an unhealthy node. |

Captured 2026-10-07 from `main` at `ee0d183` plus the MAN-96 outage counters, with a debug build.

## Regenerating

Run from the repo root. Leave the free-port lookup in place: a fixed port can collide
with another daemon on the same machine.

```sh
cargo build -p manta-cli --bin manta
S=$(mktemp -d)
target/debug/manta gen v1 --out "$S/v1"

set -- $(python3 -c '
import socket
ss = [socket.socket() for _ in range(3)]
for s in ss: s.bind(("127.0.0.1", 0))
print(" ".join(str(s.getsockname()[1]) for s in ss))
for s in ss: s.close()')

cat > "$S/node.toml" <<EOF
[server]
station_callsign = "W5AU-1"
line_format = "skimmer"
bind_addr = "127.0.0.1"
telnet_port = $1
json_port = $2
metrics_port = $3

[input]
type = "file"
path = "v1/v1.wav"
iq = true
EOF
MP=$3

target/debug/manta config check --config "$S/node.toml"   # expect "valid (from --config)"

target/debug/manta run --config "$S/node.toml" > "$S/run.log" 2>&1 &
PID=$!
# The file source exits at end of file after a few seconds, so poll quickly.
python3 - "$MP" "$S" <<'EOF'
import sys, time, urllib.error, urllib.request
mp, S = sys.argv[1], sys.argv[2]
deadline = time.time() + 120
while time.time() < deadline:
    try:
        body = urllib.request.urlopen(f"http://127.0.0.1:{mp}/metrics", timeout=2).read()
    except Exception:
        time.sleep(0.1)
        continue
    if b"manta_spots_by_band_total{" in body:
        try:
            healthz = urllib.request.urlopen(f"http://127.0.0.1:{mp}/healthz", timeout=2).read()
        except urllib.error.HTTPError as e:
            healthz = e.read()
        open(S + "/metrics-live.txt", "wb").write(body)
        open(S + "/healthz-ok.txt", "wb").write(healthz)
        print("captured")
        break
    time.sleep(0.1)
else:
    sys.exit("timed out waiting for /metrics")
EOF
kill "$PID"; wait "$PID"

D=scripts/tests/fixtures/field-node
cp "$S/metrics-live.txt" "$S/healthz-ok.txt" "$D/"
sed -e '1s/^ok$/unhealthy/' -e 's/^source file: healthy$/source soapy: unhealthy/' \
    "$S/healthz-ok.txt" > "$D/healthz-unhealthy.txt"
```

Values that differ from one capture to the next: `manta_start_time_seconds`,
`manta_uptime_seconds`, the `git_sha` label of `manta_build_info`, and the decode-latency
histogram counts.
