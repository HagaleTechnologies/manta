# MAN-81: the Docker image runs a mounted config and reports its own health

Before this change the published image's `ENTRYPOINT ["manta"]` /
`CMD ["--help"]` printed usage and exited 0 when started with no
arguments, even with a config mounted at `/etc/manta/manta.toml`. It had
no `HEALTHCHECK`, and it was built only for `linux/amd64` and
`linux/arm64`. Now a bare `docker run` with a mounted config runs the
daemon, `docker ps` shows the daemon's own health, and the image is also
built for `linux/arm/v7` (32-bit Raspberry Pi OS).

The `/healthz` endpoint the ticket depends on shipped with MAN-128
(`docs/DECISIONS/2026-10-05-man128-node-health-metrics.md`).

## Decisions

1. **Entrypoint script.** `packaging/docker/docker-entrypoint.sh`, POSIX
   `sh`, installed at `/usr/local/bin/docker-entrypoint.sh`, is the
   `ENTRYPOINT`; the final stage has no `CMD`. "Run the file if it exists"
   cannot be expressed with `ENV`/`CMD` alone: `ENV MANTA_CONFIG=...` plus
   `CMD ["run"]` makes an unmounted container fail with
   `Error: reading config file ...` instead of printing help. A separate
   file is executable by `cargo test`; an inline heredoc is not, and needs
   dockerfile 1.4 syntax.
2. **Branch order.** Arguments: `exec manta "$@"`. Else a non-empty
   `MANTA_CONFIG`: `exec manta run` (manta reads it). Else
   `/etc/manta/manta.toml` exists: `exec manta run --config
   /etc/manta/manta.toml`. Else `exec manta --help`. Arguments always win,
   so Compose's `command:`, README's `--help` smoke test and every ad hoc
   use behave as before. `MANTA_CONFIG` before the default path matches
   `run`'s own rule. Every branch `exec`s, so manta stays PID 1 and
   receives `docker stop`'s SIGTERM itself (MAN-85). `manta` is looked up
   on `PATH`, as `ENTRYPOINT ["manta"]` was.
3. **`[ -e ]`, not `[ -f ]`.** `docker run -v` with a missing host file
   mounts an empty directory. manta then fails with
   `Is a directory (os error 21)` and exits 1, instead of the container
   quietly printing help and exiting 0, which under
   `restart: unless-stopped` is a silent loop.
4. **`manta health`, not curl.** A curl probe at
   `${MANTA_SERVER_METRICS_PORT:-7302}` misses a `metrics_port` set in the
   TOML file, which since MAN-261 is the primary configuration surface.
   Reproduced on `ec8b0ca`: a daemon with `metrics_port = 17302` in its
   file answered `GET /healthz` 200 while that curl line exited 7
   (`Couldn't connect to server`) -- a healthy container marked unhealthy.
   `manta health` resolves the address with `run`'s loader and `MANTA_*`
   overlay (the `manta status` precedent), is tested in required CI, and
   adds no apt packages or CVE-scan surface to the image.
5. **Exit codes and output.** 0 when `/healthz` answers 200 with first body
   line `ok`; 1 on a 503 `unhealthy` report (printed to stdout), on any
   other reply ("did not answer like a manta health endpoint"), on a
   connect failure, timeout or config error (one `manta health: ...` line
   on stderr). Never 2, which Docker reserves and `manta status` uses for
   "couldn't ask". A config with no `[server]` table is an explicit error,
   not a probe of 127.0.0.1:7302. Peer text is escaped for the terminal and
   the body is capped at 64 KiB; Docker keeps up to 4 KiB of the output in
   `.State.Health.Log`.
6. **Timing.** `HEALTHCHECK --interval=10s --timeout=5s --start-period=60s
   --retries=3 CMD manta health --config "${MANTA_CONFIG:-/etc/manta/manta.toml}"
   --timeout-secs 3`, shell form so `/bin/sh` expands the default at check
   time (Docker does no variable substitution in `HEALTHCHECK`). 60 s
   covers a KiwiSDR's 30 s connect bound plus its handshake, and the first
   passing check ends the start period early. 10 s x 3 matches the
   Kubernetes probe in `docs/RUNBOOKS/node-health.md`. The 3 s client bound
   ends inside Docker's 5 s timeout, so the probe prints its own reason.
   No `--start-interval`: it needs Engine 25+ and older builders reject it.
   Six probes a minute use a tenth of the loopback per-IP budget (MAN-64).
7. **`linux/arm/v7`.** Added to every buildx `platforms:` line
   (`release.yml`, `release-publish.yml` x2). Both base images publish
   `linux/arm/v7`. `PKG_CONFIG_ALLOW_CROSS=1 cargo check --release --target
   armv7-unknown-linux-gnueabihf -p manta-cli --features hpsdr` passed on
   `ec8b0ca` (39.89 s). Cargo's EOVERFLOW under 32-bit QEMU
   (rust-lang/cargo#8719) comes from libgit2, which this repo already
   avoids with `git-fetch-with-cli`. The arm/v7 decode CPU budget is
   unmeasured; the image is documented as such. If the QEMU leg fails in
   release CI, the fallback is a cross-compiling builder stage, filed as a
   follow-up.
8. **`.dockerignore` excludes `manta.toml`.** Operators following the
   Compose guide create it, possibly with a receiver password, in the
   checkout; `COPY . .` would copy it into the builder layer (MAN-268's
   validation finding).
9. **CODEOWNERS.** `/packaging/docker/` is owned like `/Dockerfile`: the
   script changes the published image's behaviour as much.
10. **Shared client plumbing.** `run_status`'s config load and its
    resolve-then-fetch deadline are split into `daemon_server_config` and
    `query_daemon`, and the hand-rolled GET into `http_get`/
    `read_http_body`, which `status` and `health` both call. `manta status`
    behaviour and error strings are unchanged.
11. **Non-daemon containers report unhealthy.** `HEALTHCHECK` is static
    for the whole image. A container running `decode`, or `listen` without
    a config, has nothing to probe; the docs give
    `docker run --no-healthcheck` and Compose's `healthcheck: disable: true`
    rather than working around it.

## Not done

- No implicit config path inside the `manta` binary: `run` still reads only
  `--config` or `MANTA_CONFIG` (`config_cmd.rs`). The image supplies the
  path, as systemd and Compose do.
- No `healthcheck:` block in `docker-compose.yml`; it inherits the image's.
- No native `armv7-unknown-linux-gnueabihf` release archive, and no PR-time
  Docker build: the entrypoint and health-check behaviour reach required CI
  through `crates/manta-cli/tests/docker_image.rs` and
  `crates/manta-cli/tests/health_command.rs`, which run the script under
  `/bin/sh` and the Dockerfile's own `HEALTHCHECK` command against a real
  daemon.

## Verification

- `cargo test -p manta-cli --test health_command --test docker_image
  --test packaging_examples --test signal_shutdown --test docs_consistency`.
- Not run here (no Docker in the implementing container): `docker build`,
  `docker ps` health states, and a three-platform `release.yml`
  `workflow_dispatch` run. Run these before the first tag.
