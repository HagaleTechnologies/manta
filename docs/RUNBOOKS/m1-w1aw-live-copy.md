# M1 manual acceptance: live W1AW copy

Not part of CI (design doc §8). Run this yourself against real rig audio
before flipping CLAUDE.md's Status line to "M1 implemented".

## W1AW code practice schedule

W1AW's CW code practice runs on a published weekday/weekend schedule across
multiple HF bands (check <http://www.arrl.org/code-and-emergency-comms> or
ARRL's current bulletin for the live schedule -- it's shifted over the
years, don't assume a fixed time). Pick a session at a comfortable
copying speed for a first pass.

## Steps

1. Connect your rig's RX audio output to your computer's audio input
   (sound card line-in, USB audio interface, or a rig with built-in USB
   audio CODEC).
2. Tune to a W1AW code-practice frequency/time slot, confirm you can hear
   clean CW in your normal audio monitoring path first. Set the sidetone
   (CW pitch) to 600 Hz or higher: tones below about 515 Hz, or as close
   to Nyquist (24 kHz at 48 kHz), start no track through rig audio,
   because the Hilbert front end's image rejection is not guaranteed there
   ([MAN-273](../DECISIONS/2026-10-16-man273-hilbert-image-guard.md)). A
   400–500 Hz sidetone decodes nothing.
3. `cargo run --release -p manta-cli -- listen --device <your interface name>`
   (omit `--device` to use the system default input; run
   `cargo run -p manta-cli -- listen --device nonexistent` first if
   unsure what device names are visible -- the error message won't list
   them today, so cross-check via your OS's audio settings panel).
4. Watch the terminal. Decoded text arrives on stderr, one line per
   track, for example `[track 3 0.7 kHz 18 WPM] CQ CQ DE W1AW ...` (this
   runbook runs without `--dial-freq-hz`, so frequencies are audio
   offsets). A line appears at the first word gap after 32 characters, or
   when the track closes. Expected: readable text tracking the code
   practice transmission (callsigns, "CQ", punctuation-adjacent prosigns)
   --- not necessarily perfect, but clearly *recognizable*, matching
   ROADMAP's M1 bar.
5. Let it run at least several minutes to also eyeball basic stability
   (no panic, no runaway CPU/memory in Activity Monitor / htop).
6. Record the result (date, band, rough accuracy impression, any issues)
   in this file's "Runs" section below, or in a follow-up commit's message.

## Runs

(append entries here as you run this)

### 2026-10-11 — MAN-231 before: `listen --device` on main @ ec8b0ca

Recorded during MAN-231 planning. Input: ALSA's software null capture
device (`manta devices`: "Discard all samples (playback) or generate zero
samples (capture)"), in a headless container, not rig audio. It delivers
zeros and is not paced to real time. Binary: `cargo build -p manta-cli
--bin manta`.

| Procedure | Result |
|---|---|
| 5 × `timeout --preserve-status -s INT -k 10 60 manta listen --device "generate zero" --dial-freq-hz 14030000` | 5 of 5 exited **0** on their own before 60 s: after 14.23 s, 4.09 s, 9.48 s, 38.69 s and 10.27 s. Stderr held only the `manta: listening; …` banner, no error. |
| Same command, SIGINT at 2 s, 5 trials | 2 of 5 had already exited 0 on their own (0.66 s, 1.05 s); the other 3 stopped 5–7 ms after SIGINT. |

This reproduces MAN-231's early clean exit. It does not reproduce the
literal `audio source ended during startup calibration`: the null device
fills the 2 s calibration buffer before its ring runs empty. The real-rig
60 s run and the W1AW copy are still outstanding.

### 2026-10-11 — MAN-231 after: `listen --device` on branch MAN-231 @ 1302a0c

Same input (ALSA's software null capture device in a headless container,
not rig audio), same binary build, same command.

| Procedure | Result |
|---|---|
| 3 × `timeout --preserve-status -s INT -k 10 60 manta listen --device "generate zero" --dial-freq-hz 14030000` | 3 of 3 still running at 60 s; each exited 0 at 60.01 s after SIGINT. Stderr held only the banner. |
| Same command, SIGINT at 2 s, 5 trials | 5 of 5 still running at 2 s; exited 0 at 2.008 s, 2.016 s, 2.009 s, 2.010 s and 2.009 s, i.e. 8–16 ms after SIGINT. |

A device that delivers nothing now fails its read after 5 s
(`audio input delivered no samples in 5 s at 48000 Hz`) instead of ending
the run as exit 0: `soak` and `doctor` exit 1, and `run`/`listen` log the
loss and reopen the device (MAN-73). That path is covered by unit tests in
`crates/manta-input/src/audio.rs`, since this container has no device that
stalls. The real-rig 60 s run and the W1AW copy are still outstanding.
