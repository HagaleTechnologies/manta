#!/usr/bin/env python3
"""Throwaway diagnostic (not manta code): WOLA-channelize windows of the B2
recording around each K5TR-spotted station, dump the per-hop channel power
(own channel + neighbors) as .npy, and print envelope statistics.

Emulates manta's SPEC §1 channelizer: N=2048 @192k, hop 512 (375 Hz),
Kaiser(7.857) windowed sinc, cutoff 46.875 Hz, 8 taps/branch.
"""
import sys, wave, csv, json, os
import numpy as np

WAV, CSV, OUT = sys.argv[1], sys.argv[2], sys.argv[3]
MAXN = int(sys.argv[4]) if len(sys.argv) > 4 else 9999
FS, FC, N, HOP, L = 192000, 7_080_000, 2048, 512, 8
DELTA = FS / N
os.makedirs(OUT, exist_ok=True)

def prototype():
    LN = L * N
    n = np.arange(LN)
    fc = DELTA / 2
    h = np.sinc(2 * fc * (n - (LN - 1) / 2) / FS) * np.kaiser(LN, 7.857)
    return (h / h.sum()).astype(np.float32)
H = prototype()

def read_slice(t0, dur):
    w = wave.open(WAV, 'rb')
    w.setpos(int(t0 * FS))
    raw = w.readframes(int(dur * FS) + L * N)
    x = np.frombuffer(raw, dtype='<i2').astype(np.float32).reshape(-1, 2)
    return (x[:, 0] + 1j * x[:, 1]) / 32768.0

def channelize(iq, chans):
    """WOLA PFB restricted to output channels `chans` (FFT bin indices).
    Returns power[hop, ch] (f32) and complex X for those channels."""
    LN = L * N
    n_hops = (len(iq) - LN) // HOP
    hrev = H[::-1]
    out = np.empty((n_hops, len(chans)), np.float32)
    outx = np.empty((n_hops, len(chans)), np.complex64)
    chunk = 512
    for s in range(0, n_hops, chunk):
        e = min(s + chunk, n_hops)
        idx = s * HOP + np.arange(LN)[None, :] + HOP * np.arange(e - s)[:, None]
        u = iq[idx] * hrev[None, :]
        v = u.reshape(e - s, L, N).sum(axis=1)
        m = np.arange(s, e)
        r = (m * HOP) % N
        # circular rotate left by r per row
        rows = np.arange(e - s)[:, None]
        cols = (np.arange(N)[None, :] + r[:, None]) % N
        v = v[rows, cols]
        X = np.fft.fft(v, axis=1)[:, chans]
        outx[s:e] = X
        out[s:e] = (np.abs(X) ** 2)
    return out, outx

def bin_for_hz(f_rf):
    k = int(round((f_rf - FC) / DELTA))
    return k % N

def stats(p, wpm):
    db = 10 * np.log10(p + 1e-20)
    on_thr = np.percentile(db, 25) + 0.5 * (np.percentile(db, 95) - np.percentile(db, 25))
    on = db > on_thr
    mark = np.median(db[on]) if on.any() else np.nan
    space = np.median(db[~on]) if (~on).any() else np.nan
    contam = float(np.mean(db[~on] > mark - 6)) if (~on).any() else np.nan
    # per-second keying depth
    win = 375; depth = []
    for i in range(0, len(db) - win, win):
        seg, so = db[i:i+win], on[i:i+win]
        if so.sum() > 10 and (~so).sum() > 10:
            depth.append(np.median(seg[so]) - np.median(seg[~so]))
    depth = np.array(depth) if depth else np.array([np.nan])
    # run lengths
    rl = []; cur, cnt = on[0], 0
    for v in on:
        if v == cur: cnt += 1
        else: rl.append((cur, cnt)); cur, cnt = v, 1
    rl.append((cur, cnt))
    dit_ms = 1200 / wpm; hop_ms = 1000 / 375
    marks = np.array([c for v, c in rl if v]) * hop_ms / dit_ms
    spaces = np.array([c for v, c in rl if not v]) * hop_ms / dit_ms
    edges = [0, 0.5, 0.75, 1.25, 1.75, 2.5, 3.5, 5, 8, 1e9]
    hm = np.histogram(marks, edges)[0].tolist()
    hs = np.histogram(spaces, edges)[0].tolist()
    return dict(mark_db=float(mark), space_db=float(space), depth_db=float(mark - space),
                duty=float(on.mean()), contam=contam,
                depth_p10=float(np.nanpercentile(depth, 10)), depth_med=float(np.nanmedian(depth)),
                secs_lt10=int(np.nansum(depth < 10)), secs=len(depth),
                marks_hist=hm, spaces_hist=hs, n_marks=int(len(marks)))

rows = [r for r in csv.DictReader(open(CSV)) if r['callsign'] == 'K5TR']
spots = {}
for r in rows:
    t = r['date'][11:19]; sec = int(t[3:5]) * 60 + int(t[6:8])
    k = (r['dx'], float(r['freq']))
    if k not in spots or sec < spots[k][0]:
        spots[k] = (sec, int(r['db']), int(r['speed']))
items = sorted(spots.items(), key=lambda kv: kv[1][0])[:MAXN]
print(f"{len(items)} K5TR spots to extract")
summary = []
for (call, khz), (sec, snr, wpm) in items:
    t0 = max(0.0, sec - 20); dur = 40
    if t0 + dur > 899: t0 = 899 - dur
    iq = read_slice(t0, dur)
    k0 = bin_for_hz(khz * 1000)
    chans = [(k0 + d) % N for d in range(-6, 7)]
    p, X = channelize(iq, chans)
    # refine: pick own channel = max mean power among k0-1..k0+1 (RBN freq is 100 Hz-rounded)
    cand = [5, 6, 7]
    own = cand[int(np.argmax([p[:, c].mean() for c in cand]))]
    st = stats(p[:, own], wpm)
    ownmed = 10 * np.log10(np.median(p[:, own]) + 1e-20)
    nb = {d: round(float(10 * np.log10(np.median(p[:, own + d]) + 1e-20) - ownmed), 1) for d in (-3, -2, -1, 1, 2, 3)}
    st.update(call=call, khz=khz, t0=t0, rbn_snr=snr, wpm=wpm, own_bin=int(chans[own]), neighbors_db=nb)
    summary.append(st)
    tag = f"{call.replace('/', '_')}_{khz:.1f}"
    np.save(f"{OUT}/{tag}.power.npy", p[:, own - 3: own + 4])
    np.save(f"{OUT}/{tag}.iq.npy", X[:, own - 1: own + 2])
    p[:, own].astype(np.float32).tofile(f"{OUT}/{tag}.own.f32")
    print(f"{call:8s} {khz:7.1f} snr={snr:2d} wpm={wpm:2d} depth={st['depth_db']:5.1f} p10={st['depth_p10']:5.1f} duty={st['duty']:.2f} contam={st['contam']:.2f} nb={nb} marks={st['marks_hist']} spaces={st['spaces_hist']}", flush=True)
json.dump(summary, open(f"{OUT}/summary.json", 'w'), indent=1)
