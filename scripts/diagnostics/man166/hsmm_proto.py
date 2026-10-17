#!/usr/bin/env python3
"""Throwaway research prototype (NOT manta code, clean-room): a hop-level
hidden semi-Markov Viterbi Morse decoder over amplitude-normalized
evidence, with dit length (speed) selected by total path likelihood over a
hypothesis grid. Purpose: measure on real B2 envelopes whether a joint
probabilistic segmenter+decoder recovers callsigns where manta's
threshold->runs->2-means->beam chain does not. Offline windows for level
tracking -- a design probe, not an implementation.

Usage: hsmm_proto.py <env dir> [max files] [nproc]
"""
import sys, glob, os, json, math
import numpy as np
from multiprocessing import Pool

MORSE = {
 'A':'.-','B':'-...','C':'-.-.','D':'-..','E':'.','F':'..-.','G':'--.','H':'....',
 'I':'..','J':'.---','K':'-.-','L':'.-..','M':'--','N':'-.','O':'---','P':'.--.',
 'Q':'--.-','R':'.-.','S':'...','T':'-','U':'..-','V':'...-','W':'.--','X':'-..-',
 'Y':'-.--','Z':'--..','0':'-----','1':'.----','2':'..---','3':'...--','4':'....-',
 '5':'.....','6':'-....','7':'--...','8':'---..','9':'----.','/':'-..-.','?':'..--..',
 '=':'-...-','+':'.-.-.',
}

def build_tree():
    nodes = {'': 0}; glyph = {0: None}
    for ch, code in MORSE.items():
        for i in range(1, len(code) + 1):
            pre = code[:i]
            if pre not in nodes:
                nodes[pre] = len(nodes); glyph[nodes[pre]] = None
        glyph[nodes[code]] = ch
    n = len(nodes)
    cd = np.full(n, -1); ca = np.full(n, -1)
    for pre, idx in nodes.items():
        if pre + '.' in nodes: cd[idx] = nodes[pre + '.']
        if pre + '-' in nodes: ca[idx] = nodes[pre + '-']
    return n, cd, ca, [glyph[i] for i in range(n)]
NN, CHILD_DIT, CHILD_DAH, GLYPH = build_tree()
ROOT = 0
GLYPH_OK = np.array([g is not None for g in GLYPH])
VD = CHILD_DIT >= 0; VA = CHILD_DAH >= 0

def normalize(a, fs=375):
    from scipy.ndimage import percentile_filter, maximum_filter1d
    hi = percentile_filter(a, 92, size=int(0.5 * fs))
    hi = maximum_filter1d(hi, size=int(1.5 * fs))
    lo = percentile_filter(a, 10, size=int(2.0 * fs))
    u = (a - lo) / np.maximum(hi - lo, 1e-9)
    return np.clip(u, -0.5, 1.5)

SIGMA = 0.30
def emissions(u):
    llr = (-((u - 1.0) ** 2) + (u ** 2)) / (2 * SIGMA ** 2)   # mark:space
    return np.clip(llr, -10, 10)

LOG_DUR_SIGMA = 0.22
P_DIT, P_DAH = 0.12, 0.08   # includes a per-mark insertion penalty
P_EGAP, P_CGAP, P_WGAP = 0.62, 0.28, 0.10
NEG = -1e18

def durations(nom, k=9):
    ds = np.unique(np.round(nom * np.geomspace(0.6, 1.5, k)).astype(int))
    ds = ds[ds >= 1]
    lp = -((np.log(ds / nom)) ** 2) / (2 * LOG_DUR_SIGMA ** 2)
    return ds, lp

def viterbi(llr, unit):
    T = len(llr)
    cs = np.concatenate([[0.0], np.cumsum(llr)])
    S0 = np.full((T + 1, NN), NEG); S1 = np.full((T + 1, NN), NEG)
    B0 = np.zeros((T + 1, NN, 3), np.int32); B1 = np.zeros((T + 1, NN, 3), np.int32)
    S0[0, ROOT] = 0.0
    dd, ldd = durations(unit); da, lda = durations(3 * unit)
    de, lde = durations(unit); dc, ldc = durations(3 * unit); dw, ldw = durations(7 * unit, 13)
    lw_long = math.log(P_WGAP) - 4.0
    lpd, lpa = math.log(P_DIT), math.log(P_DAH)
    lpe, lpc, lpw = math.log(P_EGAP), math.log(P_CGAP), math.log(P_WGAP)
    long_ds = np.arange(int(10 * unit), int(80 * unit), max(1, int(unit)))
    for t in range(1, T + 1):
        # ---- marks ending at t
        best1 = np.full(NN, NEG); bp1 = np.zeros((NN, 3), np.int32)
        for ds, lps, child, valid, lpt, kind in ((dd, ldd, CHILD_DIT, VD, lpd, 0), (da, lda, CHILD_DAH, VA, lpa, 1)):
            m = ds <= t
            if not m.any(): continue
            ds_, lps_ = ds[m], lps[m]
            starts = t - ds_
            ev = cs[t] - cs[starts]                                # (D,)
            cand = S0[starts] + (ev + lps_ + lpt)[:, None]         # (D, NN)
            bi = np.argmax(cand, axis=0); bv = cand[bi, np.arange(NN)]
            src = np.nonzero(valid & (bv > NEG / 2))[0]
            tgt = child[src]
            better = bv[src] > best1[tgt]
            best1[tgt[better]] = bv[src][better]
            bp1[tgt[better]] = np.stack([starts[bi[src][better]], src[better], np.full(better.sum(), kind)], 1)
        S1[t] = best1; B1[t] = bp1
        # ---- spaces ending at t
        best0 = np.full(NN, NEG); bp0 = np.zeros((NN, 3), np.int32)
        m = de <= t
        if m.any():
            ds_, lps_ = de[m], lde[m]; starts = t - ds_
            ev = -(cs[t] - cs[starts])
            cand = S1[starts] + (ev + lps_ + lpe)[:, None]
            bi = np.argmax(cand, axis=0); bv = cand[bi, np.arange(NN)]
            ok = bv > NEG / 2
            best0[ok] = bv[ok]
            bp0[ok] = np.stack([starts[bi[ok]], np.nonzero(ok)[0], np.full(ok.sum(), 2)], 1)
        rb, rbp = NEG, None
        for ds, lps, lpt, kind in ((dc, ldc, lpc, 3), (dw, ldw, lpw, 4)):
            m = ds <= t
            if not m.any(): continue
            ds_, lps_ = ds[m], lps[m]; starts = t - ds_
            ev = -(cs[t] - cs[starts])
            cand = np.where(GLYPH_OK[None, :], S1[starts] + (ev + lps_ + lpt)[:, None], NEG)
            i = np.argmax(cand); di, n = divmod(int(i), NN)
            if cand[di, n] > rb: rb, rbp = cand[di, n], (starts[di], n, kind)
        m = long_ds <= t
        if m.any():
            starts = t - long_ds[m]
            ev = -(cs[t] - cs[starts])
            cand = np.where(GLYPH_OK[None, :], S1[starts] + (ev + lw_long)[:, None], NEG)
            i = np.argmax(cand); di, n = divmod(int(i), NN)
            if cand[di, n] > rb: rb, rbp = cand[di, n], (starts[di], n, 4)
        if rb > best0[ROOT]:
            best0[ROOT] = rb; bp0[ROOT] = rbp
        S0[t] = best0; B0[t] = bp0
    ends = [(S0[T, ROOT], 0, ROOT)]
    n1 = int(np.argmax(S1[T])); ends.append((S1[T, n1] - 3.0, 1, n1))
    n0 = int(np.argmax(S0[T])); ends.append((S0[T, n0] - 3.0, 0, n0))
    score, ph, node = max(ends)
    text = []; t = T; tail = GLYPH[node] if ph == 1 and GLYPH[node] else ''
    while t > 0:
        if ph == 1:
            s, n, kind = B1[t, node]; ph, node, t = 0, n, s
        else:
            s, n, kind = B0[t, node]
            if kind == 3: text.append(GLYPH[n] or '?')
            elif kind == 4: text.append((GLYPH[n] or '?') + ' ')
            ph, node, t = 1, n, s
    text.reverse()
    return score, (''.join(text) + tail).strip()

UNITS = [int(round(x)) for x in np.geomspace(9, 32, 9)]   # ~50 .. 14 WPM

def estimate_unit(u):
    """Classical speed estimate: mark run lengths at the half-amplitude
    crossing (u > 0.5) are cleanly bimodal on real signals (thr experiment);
    the dit cluster's median is the unit. Returns hops."""
    on = u > 0.5
    rl = []; cur, n = on[0], 0
    for v in on:
        if v == cur: n += 1
        else: rl.append((cur, n)); cur, n = v, 1
    rl.append((cur, n))
    m = np.array([n for v, n in rl if v and n >= 3])
    if len(m) < 10: return None
    # split at the largest log-gap between sorted values below 60 hops
    m = np.sort(m[m < 60])
    if len(m) < 10: return None
    lg = np.diff(np.log(m)); i = int(np.argmax(lg[len(m)//5: 4*len(m)//5])) + len(m)//5
    dit = np.median(m[:i+1]); dah = np.median(m[i+1:]) if i + 1 < len(m) else 3 * dit
    if dah / dit < 1.8:   # unimodal: assume dits
        dit = np.median(m)
    return int(round(dit))

def decode_file(f):
    p = np.fromfile(f, np.float32); a = np.sqrt(np.maximum(p, 0))
    u = normalize(a); llr = emissions(u)
    units = UNITS   # likelihood-selected speed grid (the run-length estimator attempt was worse)
    best = (-1e19, '', None)
    for un in units:
        sc, text = viterbi(llr, un)
        # Bayesian prior on speed from the RBN contest speed distribution (per 40 s window): >42 WPM is rare (<1%).
        wpm = 450 / un
        sc += -12.0 if wpm > 46 else (-5.0 if wpm > 41 else (-3.0 if wpm < 16 else 0.0))
        if sc > best[0]: best = (sc, text, un)
    return f, best

if __name__ == '__main__':
    D = sys.argv[1]; mx = int(sys.argv[2]) if len(sys.argv) > 2 else 9999
    nproc = int(sys.argv[3]) if len(sys.argv) > 3 else 1
    files = sorted(glob.glob(f"{D}/*.own.f32"))[:mx]
    summ = {s['call'].replace('/', '_'): s for s in json.load(open(f"{D}/summary.json"))}
    hits = ctx = 0
    import re
    with Pool(nproc) as pool:
        for f, (sc, text, un) in pool.imap(decode_file, files):
            call = os.path.basename(f).rsplit('_', 1)[0]
            words = text.split(); rcall = call.replace('_', '/')
            hit = rcall in words
            c = bool(re.search(r'\b(CQ|DE|TEST)\s+(TEST\s+)?' + re.escape(rcall) + r'\b', text))
            hits += hit; ctx += c
            wpm = summ.get(call, {}).get('wpm', 0)
            print(f"{rcall:9s} rbn_wpm={wpm:2d} unit={un:2d}hops(~{450/un:.0f}wpm) hit={int(hit)} ctx={int(c)} | {text[:140]}", flush=True)
    print(f"\ncallsign-as-word hits: {hits}/{len(files)}; with CQ/TEST/DE framing: {ctx}/{len(files)}")
