# MAN-157: a logo mark that reads at 16 px, and a wordmark lockup

## Decision

Keep the manta ray over a spectrum. Use two tiers of geometry. `assets/icon.svg`
is drawn on a 16-unit grid for 16–48 px display, with PNG exports at 16, 32 and
48 px. The existing full mark is for 64 px and above. Its paths stay unchanged,
inside a centred padding transform that leaves at least 16 units on every
edge of its 256-unit box.

The small icon has an opaque dark rounded tile, a light ray and a blue trace.
Its tail is two grid cells wide. Integer, axis-aligned edges keep the trace
and tail solid at each export size. A cell of background separates the tail
from the carriers above the spectrum floor. All ink stays inside the tile's
outer ring of grid cells.

Light and dark lockups pair the padded mark with hand-drawn stroked paths
spelling "manta". They need no font or external resource. The README heading
uses the lockup through a themed `<picture>` and keeps `alt="manta"`.
`assets/README.md` records the palette, usage and pinned resvg-py 0.5.0 render
recipe. Rendering is an offline developer step.

## Evidence

The MAN-157 research rendered the old mark at 16 px. The wings reached columns
0 and 15, and the spectrum floor's best-row alpha was 0.15. At 32 px it was
0.29, and at 48 px it was 0.46. The tail merged visually with the nearest tall
carrier. Increasing stroke width alone could not align the geometry with the
pixel grid. The small icon therefore has separate geometry rather than a
thicker version of the full mark.

The new PNGs have 12 cells of exact trace colour across the floor outside the
tail, four carrier columns above it, and a two-cell tail extending below the
wings. The full marks have measured margins of 16, 16, 31 and 33 px at 256 px.

## External steps

Tony judges legibility in real light and dark browser tab strips and approves
the lockup letterforms and spacing. Those judgments are outside agent acceptance.
Use outside this repository, `.ico`, Apple touch icons and social previews
remain outside this ticket's scope.
