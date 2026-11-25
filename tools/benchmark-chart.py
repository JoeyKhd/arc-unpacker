"""Writes the README benchmark figure from the ledger (bench/ledger.jsonl) as two SVGs, for
GitHub's light and dark themes: headline numbers, then one pair of bars per repack.

Usage: python tools/benchmark-chart.py [output folder]      (default: docs)

Per repack: the original installer's median time (install + its own file check) and the newest
arc-unpack version's median time (default settings: every file checked), on the machine with
the most repacks in the ledger. Repacks are labelled by download size, never by name.
Colours are checked for contrast against GitHub's page backgrounds (#ffffff and #0d1117).
"""
import json
import pathlib
import re
import statistics
import sys
from xml.sax.saxutils import escape

ROOT = pathlib.Path(__file__).resolve().parent.parent
LEDGER = ROOT / "bench" / "ledger.jsonl"

W = 860
FONT = "-apple-system, BlinkMacSystemFont, 'Segoe UI', 'Noto Sans', Helvetica, Arial, sans-serif"
THEMES = {
    "light": dict(ink="#1f2328", secondary="#59636e", rule="#d1d9e0", gray="#afb8c1", accent="#2a78d6"),
    "dark": dict(ink="#f0f6fc", secondary="#9198a1", rule="#3d444d", gray="#57606a", accent="#3987e5"),
}


def version_key(v):
    return [int(p) if p.isdigit() else p for p in re.split(r"[.\-]", v.replace("beta", "b.")) if p]


def span(seconds):
    s = round(seconds)
    return f"{s // 60} min {s % 60:02d} s" if s >= 60 else f"{s} s"


def load_rows():
    runs = [json.loads(l) for l in LEDGER.read_text(encoding="utf-8").splitlines() if l.strip()]
    runs = [r for r in runs if r.get("settings") == "default"]
    machines = {}
    for r in runs:
        machines.setdefault(r["machine"], set()).add(r["repack"])
    machine = max(machines, key=lambda m: len(machines[m]))
    rows = []
    for rid in sorted(machines[machine]):
        mine = [r for r in runs if r["machine"] == machine and r["repack"] == rid]
        orig = [r for r in mine if r["tool"] == "original" and r.get("ok")]
        arc = [r for r in mine if r["tool"] == "arc-unpack"]
        if not orig or not arc:
            continue
        newest = max((r["version"] for r in arc), key=version_key)
        ok = [r for r in arc if r["version"] == newest and r.get("ok")]
        if not ok:
            continue
        verdicts = [r.get("identical_to_original") for r in ok if r.get("identical_to_original") is not None]
        rows.append(dict(
            original=statistics.median(r["seconds"] for r in orig),
            arc=statistics.median(r["seconds"] for r in ok),
            packed=mine[0]["packed_bytes"], archives=mine[0]["archives"],
            identical=bool(verdicts) and all(verdicts), version=newest,
        ))
    rows.sort(key=lambda r: -r["original"])
    return machine, rows


def bar(x, y, w, h, fill):
    """A bar growing right from x: square at the baseline, 3px rounded data end."""
    r = min(3, w / 2)
    return (f'<path d="M{x:.1f} {y:.1f}h{w - r:.1f}a{r} {r} 0 0 1 {r} {r}v{h - 2 * r}a{r} {r} 0 0 1 -{r} {r}'
            f'h-{w - r:.1f}z" fill="{fill}"/>')


def svg(t, machine, rows):
    n = len(rows)
    so, sa = sum(r["original"] for r in rows), sum(r["arc"] for r in rows)
    best = max(r["original"] / r["arc"] for r in rows)
    same = sum(r["identical"] for r in rows)
    versions = sorted({r["version"] for r in rows}, key=version_key)
    tiles = [
        (f"{so / sa:.1f}×", f"faster for all {n} repacks,", "installed one after another"),
        (f"up to {best:.1f}×", "on a single repack", ""),
        (span(so - sa), f"saved across {n} installs", f"({span(so)} → {span(sa)})"),
        (f"{same} of {n}", "installs byte-identical", "to the original installer's"),
    ]
    x0, pitch, bh = 170, 44, 12
    top = 150
    h = top + n * pitch + 8
    scale = (W - x0 - 150) / max(r["original"] for r in rows)
    alt = (f"{n} FitGirl repacks on the same PC. Original installers took {span(so)} in total, arc-unpack "
           f"{span(sa)}: {so / sa:.1f} times faster, up to {best:.1f} times on one repack. {same} of {n} installs "
           f"were byte-identical to the original installer's.")
    o = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{h}" viewBox="0 0 {W} {h}" role="img" aria-label="{escape(alt)}">',
         f"<title>{escape(alt)}</title>", f'<g font-family="{FONT}">']
    for i, (value, l1, l2) in enumerate(tiles):
        x = i * 215
        o.append(f'<text x="{x}" y="34" font-size="30" font-weight="600" fill="{t["ink"]}">{escape(value)}</text>')
        o.append(f'<text x="{x}" y="58" font-size="13" fill="{t["secondary"]}">{escape(l1)}</text>')
        if l2:
            o.append(f'<text x="{x}" y="75" font-size="13" fill="{t["secondary"]}">{escape(l2)}</text>')
    o.append(f'<rect x="0" y="96" width="{W}" height="1" fill="{t["rule"]}"/>')
    # legend
    ly = 124
    o.append(f'<rect x="0" y="{ly - 10}" width="12" height="12" rx="2" fill="{t["gray"]}"/>')
    o.append(f'<text x="18" y="{ly}" font-size="13" fill="{t["secondary"]}">Original installer (install, then its file check)</text>')
    o.append(f'<rect x="330" y="{ly - 10}" width="12" height="12" rx="2" fill="{t["accent"]}"/>')
    o.append(f'<text x="348" y="{ly}" font-size="13" fill="{t["secondary"]}">arc-unpack {escape(versions[-1])} (checks every file while installing)</text>')
    o.append(f'<rect x="{x0 - 1}" y="{top - 4}" width="1" height="{n * pitch}" fill="{t["rule"]}"/>')
    for i, r in enumerate(rows):
        y = top + i * pitch
        o.append(f'<text x="0" y="{y + 12}" font-size="14" font-weight="600" fill="{t["ink"]}">{r["packed"] / 1e9:.1f} GB download</text>')
        o.append(f'<text x="0" y="{y + 29}" font-size="12" fill="{t["secondary"]}">{r["archives"]} archives</text>')
        wo, wa = r["original"] * scale, r["arc"] * scale
        o.append(bar(x0, y + 1, wo, bh, t["gray"]))
        o.append(f'<text x="{x0 + wo + 8:.1f}" y="{y + 12}" font-size="12" fill="{t["secondary"]}">{span(r["original"])}</text>')
        o.append(bar(x0, y + bh + 4, wa, bh, t["accent"]))
        o.append(f'<text x="{x0 + wa + 8:.1f}" y="{y + bh + 15}" font-size="12" font-weight="600" fill="{t["ink"]}">{span(r["arc"])}'
                 f'<tspan dx="8" font-weight="400" fill="{t["secondary"]}">{r["original"] / r["arc"]:.1f}× faster</tspan></text>')
    o.append("</g>")
    o.append("</svg>")
    return "\n".join(o) + "\n"


def main():
    sys.stdout.reconfigure(encoding="utf-8")
    out = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / "docs")
    out.mkdir(parents=True, exist_ok=True)
    machine, rows = load_rows()
    for name, theme in THEMES.items():
        (out / f"benchmark-{name}.svg").write_text(svg(theme, machine, rows), encoding="utf-8", newline="\n")
        print("wrote", out / f"benchmark-{name}.svg")
    print(f"{len(rows)} repacks on {machine}")


if __name__ == "__main__":
    main()
