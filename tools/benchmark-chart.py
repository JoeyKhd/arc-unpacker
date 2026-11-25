"""Writes the README benchmark figure (stat tiles + bars) as two SVGs: GitHub light and dark.

Usage: python tools/benchmark-chart.py [output folder]      (default: docs)
The numbers are the measured ones from docs/BENCHMARKS.md. Colours are checked for
contrast against GitHub's page backgrounds (#ffffff and #0d1117).
"""
import pathlib
import sys

# Measured, in seconds and percent (docs/BENCHMARKS.md).
INSTALL, VERIFY = 199.8, 86.9      # original setup.exe: install, then its verify pass
ARC = 74.3                         # arc-unpack, default settings (median of three runs)
CPU_ORIGINAL, CPU_ARC = 31, 68     # average CPU use over the whole run
FILES = 3489

ORIGINAL = INSTALL + VERIFY
W = 860
FONT = "-apple-system, BlinkMacSystemFont, 'Segoe UI', 'Noto Sans', Helvetica, Arial, sans-serif"

THEMES = {
    "light": dict(ink="#1f2328", secondary="#59636e", rule="#d1d9e0", gray="#afb8c1", in_gray="#1f2328", accent="#2a78d6"),
    "dark": dict(ink="#f0f6fc", secondary="#9198a1", rule="#3d444d", gray="#57606a", in_gray="#f0f6fc", accent="#3987e5"),
}


def span(seconds):
    s = round(seconds)
    return f"{s // 60} min {s % 60} s"


SPEEDUP = f"{ORIGINAL / ARC:.1f}×"

TILES = [
    (SPEEDUP, "faster than the repack's", "own installer"),
    (span(ORIGINAL - ARC), "saved on a", f"{span(ORIGINAL)} install"),
    (f"{FILES:,} files", "all byte-identical to the", "original installer's"),
    (f"{CPU_ORIGINAL}% → {CPU_ARC}%", "average CPU use,", "original → arc-unpack"),
]

LABELS = [
    ("Original setup.exe", "install, then verify files"),
    ("arc-unpack", "default: every check on"),
]

X0, SCALE, BAR = 200, 1.75, 22
ROWS = [140, 198]
H = ROWS[-1] + 30


def bar(x, y, w, fill, round_end=True):
    """A bar growing right from x: square at the baseline, 4px rounded data end."""
    if not round_end:
        return f'<rect x="{x:.1f}" y="{y}" width="{w:.1f}" height="{BAR}" fill="{fill}"/>'
    r = 4
    return (f'<path d="M{x:.1f} {y}h{w - r:.1f}a{r} {r} 0 0 1 {r} {r}v{BAR - 2 * r}a{r} {r} 0 0 1 -{r} {r}h-{w - r:.1f}z" '
            f'fill="{fill}"/>')


def svg(t):
    o = []
    alt = (f"Benchmark, same repack on the same PC. Original setup.exe: {span(ORIGINAL)} (install {span(INSTALL)}, then "
           f"verify {span(VERIFY)}). arc-unpack with default settings, every check on: {span(ARC)}, "
           f"{ORIGINAL / ARC:.1f} times faster. All {FILES:,} files byte-identical. "
           f"Average CPU use {CPU_ORIGINAL}% versus {CPU_ARC}%.")
    o.append(f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-label="{alt}">')
    o.append(f'<title>{alt}</title>')
    o.append(f'<g font-family="{FONT}">')

    # stat tiles
    for i, (value, l1, l2) in enumerate(TILES):
        x = i * 215    # leaves room on the right for wider fallback fonts
        o.append(f'<text x="{x}" y="34" font-size="30" font-weight="600" fill="{t["ink"]}">{value}</text>')
        o.append(f'<text x="{x}" y="58" font-size="13" fill="{t["secondary"]}">{l1}</text>')
        o.append(f'<text x="{x}" y="75" font-size="13" fill="{t["secondary"]}">{l2}</text>')
    o.append(f'<rect x="0" y="98" width="{W}" height="1" fill="{t["rule"]}"/>')

    # row labels
    for c, (name, sub) in zip(ROWS, LABELS):
        o.append(f'<text x="0" y="{c - 2}" font-size="14" font-weight="600" fill="{t["ink"]}">{name}</text>')
        o.append(f'<text x="0" y="{c + 14}" font-size="12" fill="{t["secondary"]}">{sub}</text>')

    # baseline
    o.append(f'<rect x="{X0 - 1}" y="{ROWS[0] - 22}" width="1" height="{ROWS[-1] - ROWS[0] + 44}" fill="{t["rule"]}"/>')

    def value(x, c, text, extra=None):
        s = f'<text x="{x:.1f}" y="{c + 5}" font-size="15" font-weight="600" fill="{t["ink"]}">{text}'
        if extra:
            s += f'<tspan dx="12" font-size="13" font-weight="400" fill="{t["secondary"]}">{extra}</tspan>'
        return s + '</text>'

    # original installer: install | 2px gap | verify
    c = ROWS[0]
    gap = 2
    w1 = INSTALL * SCALE - gap / 2
    x2 = X0 + INSTALL * SCALE + gap / 2
    w2 = VERIFY * SCALE - gap / 2
    o.append(bar(X0, c - BAR // 2, w1, t["gray"], round_end=False))
    o.append(bar(x2, c - BAR // 2, w2, t["gray"]))
    o.append(f'<text x="{X0 + w1 / 2:.1f}" y="{c + 4}" font-size="12" text-anchor="middle" fill="{t["in_gray"]}">install · {span(INSTALL)}</text>')
    o.append(f'<text x="{x2 + w2 / 2:.1f}" y="{c + 4}" font-size="12" text-anchor="middle" fill="{t["in_gray"]}">verify · {span(VERIFY)}</text>')
    o.append(value(X0 + ORIGINAL * SCALE + 10, c, span(ORIGINAL)))

    # arc-unpack
    c = ROWS[1]
    o.append(bar(X0, c - BAR // 2, ARC * SCALE, t["accent"]))
    o.append(value(X0 + ARC * SCALE + 10, c, span(ARC), f"{SPEEDUP} faster"))

    o.append('</g>')
    o.append('</svg>')
    return "\n".join(o) + "\n"


out = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "docs")
out.mkdir(parents=True, exist_ok=True)
for name, theme in THEMES.items():
    (out / f"benchmark-{name}.svg").write_text(svg(theme), encoding="utf-8", newline="\n")
    print("wrote", out / f"benchmark-{name}.svg")
