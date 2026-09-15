#!/usr/bin/env python3
"""Throwaway: does threshold placement explain mark inflation on real
signals? Compare run-length histograms (in true-dit units, using RBN WPM)
for thresholds at (a) geometric mean of rails (manta SPEC §3.2), (b) half
amplitude of the mark rail (-6 dB), (c) -3 dB. Rails = 90th/10th pct of
amplitude over the window, static (oracle rails, no adaptation)."""
import sys, glob, json, os
import numpy as np

D = sys.argv[1]
summary = {s['call']: s for s in json.load(open(f"{D}/summary.json"))}

def runs(on):
    out = []; cur, n = on[0], 0
    for v in on:
        if v == cur: n += 1
        else: out.append((cur, n)); cur, n = v, 1
    out.append((cur, n)); return out

def measure(a, thr, dit_ms, debounce_hops=5):
    on = a > thr
    rl = runs(on)
    # manta-style debounce: merge runs shorter than debounce into neighbors
    merged = []
    for v, n in rl:
        if merged and n < debounce_hops and len(merged) >= 1:
            # absorb into previous run
            pv, pn = merged[-1]; merged[-1] = (pv, pn + n)
            continue
        if merged and merged[-1][0] == v:
            pv, pn = merged[-1]; merged[-1] = (pv, pn + n)
        else:
            merged.append((v, n))
    hop_ms = 1000 / 375
    m = np.array([n for v, n in merged if v]) * hop_ms / dit_ms
    s = np.array([n for v, n in merged if not v]) * hop_ms / dit_ms
    return m, s

edges = [0, 0.5, 0.75, 1.25, 1.75, 2.5, 3.5, 5, 8, 1e9]
labels = ["<.5", ".5-.75", ".75-1.25", "1.25-1.75", "1.75-2.5", "2.5-3.5", "3.5-5", "5-8", ">8"]
print("bins:", labels)
agg = {}
for f in sorted(glob.glob(f"{D}/*.own.f32")):
    call = os.path.basename(f).split('_')[0]
    st = summary[call]
    p = np.fromfile(f, np.float32); a = np.sqrt(np.maximum(p, 0))
    dit_ms = 1200 / st['wpm']
    hi = np.percentile(a, 90); lo = max(np.percentile(a, 10), 1e-9)
    thrs = {"geomean": np.sqrt(hi * lo), "-6dB": hi * 0.5, "-3dB": hi * 0.707, "-10dB": hi * 0.316}
    print(f"\n{call} wpm={st['wpm']} rails {20*np.log10(hi/lo):.0f} dB")
    for name, thr in thrs.items():
        m, s = measure(a, thr, dit_ms)
        hm = np.histogram(m, edges)[0]; hs = np.histogram(s, edges)[0]
        # median of the dit cluster (marks < 2 dits) and gap cluster (<1.5)
        dm = np.median(m[m < 2.0]) if (m < 2.0).any() else np.nan
        ds = np.median(s[s < 1.5]) if (s < 1.5).any() else np.nan
        dah = np.median(m[(m >= 2.0) & (m < 5)]) if ((m >= 2.0) & (m < 5)).any() else np.nan
        print(f"  {name:8s} thr={20*np.log10(thr/hi):6.1f}dB re mark | dit-med={dm:.2f} dah-med={dah:.2f} egap-med={ds:.2f} | marks {hm.tolist()} | spaces {hs.tolist()}")
        agg.setdefault(name, []).append((dm, dah, ds))
print("\n=== medians across signals (dit, dah, element-gap in true dits) ===")
for name, v in agg.items():
    v = np.array(v)
    print(f"{name:8s} dit {np.nanmedian(v[:,0]):.2f}  dah {np.nanmedian(v[:,1]):.2f}  egap {np.nanmedian(v[:,2]):.2f}")
