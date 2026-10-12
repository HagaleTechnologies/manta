#!/bin/sh
# The image's entrypoint (MAN-81). With arguments it runs `manta <arguments>`
# unchanged, so `docker run <image> --help` and Compose's `command:` behave
# exactly as they did when manta itself was the entrypoint. With none, it
# runs the daemon when a config is available -- MANTA_CONFIG, else a file
# mounted at the path below -- and prints manta's help otherwise.
#
# Every branch execs: manta must be PID 1 so `docker stop`'s SIGTERM and
# `docker kill --signal=HUP` reach it directly (the Dockerfile sets no
# STOPSIGNAL). `-e`, not `-f`: a directory that `docker run -v` left for a
# missing host file must reach manta, which reports it, not fall back to help.
set -eu

config=/etc/manta/manta.toml

if [ "$#" -gt 0 ]; then
    exec manta "$@"
elif [ -n "${MANTA_CONFIG:-}" ]; then
    exec manta run
elif [ -e "$config" ]; then
    exec manta run --config "$config"
else
    exec manta --help
fi
