# manta logo assets

| File | Use |
|---|---|
| `icon.svg` | Small mark at 16–48 px on a dark rounded tile |
| `icon-16.png` | 16 px favicon or tab icon |
| `icon-32.png` | 32 px favicon or tab icon |
| `icon-48.png` | 48 px icon |
| `logo.svg` | Full mark, 64 px and above, inline SVG using `currentColor` |
| `logo-light.svg` | Full mark on light backgrounds |
| `logo-dark.svg` | Full mark on dark backgrounds |
| `logo-lockup-light.svg` | Mark plus path-drawn "manta" on light backgrounds |
| `logo-lockup-dark.svg` | Mark plus path-drawn "manta" on dark backgrounds |

The small mark uses integer coordinates in a 16-unit box. Keep the spectrum
and tail edges axis-aligned so 16, 32 and 48 px renders have solid pixels.
The ray and trace stay one grid cell inside the tile. The tail crosses the
spectrum floor, with a cell of tile between it and the nearest carriers above
the floor.

The full mark keeps its original paths, scaled and centred within the 256-unit
box with at least 16 units of padding. Apply geometry changes to all three full
marks and both lockups together. Lockups embed the padded full mark at 0.75 scale.

Light assets use body and letters `#1f2937` and trace `#2563eb`. Dark assets use
body and letters `#e5e7eb` and trace `#60a5fa`. The small icon uses the dark
colours on a `#1f2937` tile so it works on light and dark backgrounds.

The lockup letters are hand-drawn stroked paths. There is no `<text>` or font
dependency. There is no `currentColor` lockup because SVG embedded through
`<img>` does not inherit the page's text colour. The README selects the light
or dark lockup with `<picture>`.

## Regenerate the PNGs

From the repository root, use resvg-py 0.5.0 in a disposable environment.
This is a developer step, with no Rust dependency or CI generation step.

```sh
python3 -m venv /tmp/resvg
/tmp/resvg/bin/pip install resvg-py==0.5.0
for s in 16 32 48; do
  /tmp/resvg/bin/python -c 'import sys, resvg_py; s = int(sys.argv[1]); open(f"assets/icon-{s}.png", "wb").write(bytes(resvg_py.svg_to_bytes(svg_path="assets/icon.svg", width=s, height=s)))' "$s"
done
```

The PNGs are deterministic renders of `icon.svg`. Check dimensions and byte
identity after regeneration. At each size, the floor has 12 solid blue cells,
the tail is two cells wide, and carriers above the floor have a background
cell between them and the tail. Check full marks at 256 px for 16 px minimum
margins and top/bottom margins within 4 px of each other.
