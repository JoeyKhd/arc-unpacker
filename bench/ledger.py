"""The benchmark ledger: every measured install, so each new version can be compared with the
original installers and with earlier versions on the same repacks.

  python bench/ledger.py add <result.json>...   add runs measured by bench/compare.ps1
  python bench/ledger.py table [--names]        latest results, and arc-unpack's history per version
  python bench/ledger.py id "<repack folder>"   the IDs a repack is recorded under

Repacks are recorded by their torrent's info hash (found in qBittorrent's saved torrents) and by
the MD5 of the repack's own MD5 list (computable from any copy), never by name. Names stay in
bench/names.local.json on your PC (git ignores it); `table --names` uses them.
"""
import argparse
import glob
import hashlib
import json
import os
import pathlib
import re
import statistics
import sys

HERE = pathlib.Path(__file__).resolve().parent
LEDGER = HERE / "ledger.jsonl"
NAMES = HERE / "names.local.json"


# ------------------------------------------------------------------------------- repack IDs

def bdecode(data, i=0):
    """Decodes one bencoded value at i; returns (value, end). A dict's b"info" value also gets
    its byte span recorded under b"__info_span__" (the info hash is the SHA-1 of those bytes)."""
    c = data[i:i + 1]
    if c == b"i":
        j = data.index(b"e", i)
        return int(data[i + 1:j]), j + 1
    if c == b"l":
        i += 1
        out = []
        while data[i:i + 1] != b"e":
            v, i = bdecode(data, i)
            out.append(v)
        return out, i + 1
    if c == b"d":
        i += 1
        out = {}
        while data[i:i + 1] != b"e":
            k, i = bdecode(data, i)
            start = i
            v, i = bdecode(data, i)
            out[k] = v
            if k == b"info":
                out[b"__info_span__"] = (start, i)
        return out, i + 1
    colon = data.index(b":", i)
    n = int(data[i:colon])
    return data[colon + 1:colon + 1 + n], colon + 1 + n


def torrent_index():
    """Torrent name (lower case) -> v1 info hash, from qBittorrent's saved torrents."""
    index = {}
    base = os.environ.get("LOCALAPPDATA")
    folder = pathlib.Path(base, "qBittorrent", "BT_backup") if base else None
    if not folder or not folder.is_dir():
        return index
    for f in folder.glob("*.torrent"):
        try:
            data = f.read_bytes()
            meta, _ = bdecode(data)
            span = meta.get(b"__info_span__")
            name = meta[b"info"][b"name"].decode("utf-8", "replace")
        except Exception:
            continue
        if span:
            index[name.lower()] = hashlib.sha1(data[span[0]:span[1]]).hexdigest()
    return index


def repack_ids(folder, index=None):
    folder = pathlib.Path(folder)
    md5_list = folder / "MD5" / "fitgirl-bins.md5"
    ids = {
        "infohash": (index if index is not None else torrent_index()).get(folder.name.lower()),
        "md5_list": hashlib.md5(md5_list.read_bytes()).hexdigest() if md5_list.is_file() else None,
    }
    ids["repack"] = ids["infohash"] or (ids["md5_list"] and "md5list:" + ids["md5_list"])
    return ids


# ------------------------------------------------------------------------------- the ledger

def load():
    if not LEDGER.is_file():
        return []
    return [json.loads(line) for line in LEDGER.read_text(encoding="utf-8").splitlines() if line.strip()]


def load_names():
    return json.loads(NAMES.read_text(encoding="utf-8")) if NAMES.is_file() else {}


def scrub(text, name):
    """Keeps notes free of paths and the game's name."""
    text = re.sub(r"[A-Za-z]:\\[^;|()]*", "<path>", text or "")
    if name:
        text = re.sub(re.escape(name), "<game>", text, flags=re.IGNORECASE)
    return text.strip()


def machine_label(m):
    cpu = re.sub(r"\((R|TM)\)|\s*\d+-Core Processor", "", m.get("cpu", ""))
    cpu = re.sub(r"\d+th Gen |Intel |AMD |Core |Processor", "", cpu).strip()
    disk = re.sub(r"^Samsung SSD ", "Samsung ", m.get("disk") or "")
    return ", ".join(x for x in (cpu, f"{m.get('ram_gb')} GB RAM", disk) if x)


def records_from_result(res, ids):
    name = res.get("name", "")
    base = {
        "date": res["date"],
        "repack": ids["repack"],
        "infohash": ids["infohash"],
        "md5_list": ids["md5_list"],
        "archives": res["repack"]["archives"],
        "packed_bytes": res["repack"]["packed_bytes"],
        "parts": res["components"],
        "machine": machine_label(res["machine"]),
        "conditions": "; ".join(x for x in (
            "file cache flushed",
            "torrent client running" if res["conditions"].get("qbittorrent_running") else "",
            "1 ms system timer" if res["machine"].get("timer_tweak") else "",
        ) if x),
    }
    cmp = res.get("compare") or {}
    identical = None
    if cmp:
        identical = cmp.get("differ") == 0 and cmp.get("only_original") == 0 and cmp.get("only_arc_unpack") == 0 and cmp.get("identical", 0) > 0
    out = []
    o = res.get("original")
    if o and o.get("total_s"):
        out.append(dict(base, tool="original", version="setup.exe", settings="default",
                        seconds=o["total_s"], install_s=o["install_s"], verify_s=o.get("verify_s"),
                        files=o.get("files"), bytes=o.get("bytes"),
                        avg_cpu_pct=o.get("avg_cpu_pct"), ram_rise_gb=o.get("ram_rise_gb"),
                        ok=bool(o.get("log_succeeded")), note=scrub(o.get("note"), name)))
    a = res.get("arc_unpack")
    if a and a.get("total_s"):
        summary = a.get("summary") or ""
        out.append(dict(base, tool="arc-unpack", version=res["arc_unpack_version"], commit=res.get("arc_unpack_commit"), settings="default",
                        seconds=a["total_s"], files=a.get("files"), bytes=a.get("bytes"),
                        avg_cpu_pct=a.get("avg_cpu_pct"), ram_rise_gb=a.get("ram_rise_gb"),
                        ok=a.get("exit") == 0, identical_to_original=identical,
                        md5_checked=re.search(r"(\d[\d,]*) files match the repack's own MD5 list", summary) is not None,
                        note=scrub(a.get("note") or ("" if a.get("exit") == 0 else f"exit {a.get('exit')}"), name)))
    return out


def cmd_add(args):
    index = torrent_index()
    ledger = load()
    seen = {(r["repack"], r["tool"], r["version"], r["date"]) for r in ledger}
    names = load_names()
    added = 0
    # Windows shells don't expand wildcards: do it here.
    paths = [p for arg in args.results for p in (sorted(glob.glob(arg)) if any(c in arg for c in "*?[") else [arg])]
    for path in paths:
        res = json.loads(pathlib.Path(path).read_text(encoding="utf-8-sig"))
        if args.disk and not res["machine"].get("disk"):
            res["machine"]["disk"] = args.disk
        if res.get("error"):
            print(f"skipped {path}: {res['error']}")
            continue
        folder = res.get("repack_path") or str(pathlib.Path(args.repack_root, f"{res['name']} [FitGirl Repack]"))
        ids = repack_ids(folder, index)
        if not ids["repack"]:
            print(f"skipped {path}: no torrent or MD5 list found for {folder}")
            continue
        names[ids["repack"]] = res.get("name", "")
        for rec in records_from_result(res, ids):
            key = (rec["repack"], rec["tool"], rec["version"], rec["date"])
            if key in seen:
                continue
            ledger.append(rec)
            seen.add(key)
            added += 1
    with LEDGER.open("w", encoding="utf-8", newline="\n") as f:
        for r in sorted(ledger, key=lambda r: (r["date"], r["repack"], r["tool"])):
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    NAMES.write_text(json.dumps(names, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"added {added} runs; the ledger has {len(ledger)}")


# ------------------------------------------------------------------------------- tables

def version_key(v):
    parts = re.split(r"[.\-]", v.replace("beta", "b."))
    return [int(p) if p.isdigit() else p for p in parts if p]


def fmt_time(s):
    s = round(s)
    return f"{s // 60} min {s % 60:02d} s" if s >= 60 else f"{s} s"


def cmd_table(args):
    ledger = [r for r in load() if r.get("settings") == "default"]
    names = load_names() if args.names else {}
    label = lambda rid: names.get(rid) or ("repack " + rid.replace("md5list:", "")[:8])
    for machine in sorted({r["machine"] for r in ledger}):
        rows = [r for r in ledger if r["machine"] == machine]
        versions = sorted({r["version"] for r in rows if r["tool"] == "arc-unpack"}, key=version_key)
        repacks = sorted({r["repack"] for r in rows}, key=lambda rid: label(rid).lower())
        # Each repack's newest measured version (a newer build may not have been run on all of them).
        newest = {}
        for rid in repacks:
            vs = [r["version"] for r in rows if r["repack"] == rid and r["tool"] == "arc-unpack"]
            newest[rid] = max(vs, key=version_key) if vs else None
        shown = {v for v in newest.values() if v}
        head = f"arc-unpack {shown.pop()}" if len(shown) == 1 else "arc-unpack (newest version run)"
        print(f"### {machine}\n")
        print(f"| Repack | Archives | Download | Original installer | {head} | Speed-up | Same files |")
        print("|---|---:|---:|---:|---:|---:|:---:|")
        totals = []
        for rid in repacks:
            mine = [r for r in rows if r["repack"] == rid]
            orig = [r["seconds"] for r in mine if r["tool"] == "original" and r.get("ok", True)]
            arc = [r for r in mine if r["tool"] == "arc-unpack" and r["version"] == newest[rid]]
            arc_ok = [r["seconds"] for r in arc if r.get("ok")]
            info = mine[0]
            o = statistics.median(orig) if orig else None
            a = statistics.median(arc_ok) if arc_ok else None
            verdicts = [r.get("identical_to_original") for r in arc if r.get("ok") and r.get("identical_to_original") is not None]
            same = "–" if not verdicts else "yes" if all(verdicts) else "no"
            speed = f"**{o / a:.1f}×**" if o and a else "–"
            if o and a:
                totals.append((o, a))
            failed = len(arc) - len(arc_ok)
            cell = (fmt_time(a) + (f" ({len(arc_ok)} runs)" if len(arc_ok) > 1 else "")) if a else ("failed" if arc else "–")
            if a and failed:
                cell += f" ({failed} failed)"
            if len(shown) != 1 and newest[rid] and newest[rid] != (max(versions, key=version_key) if versions else None):
                cell += f" · {newest[rid]}"
            print(f"| {label(rid)} | {info['archives']} | {info['packed_bytes'] / 1e9:.2f} GB | {fmt_time(o) if o else '–'} | {cell} | {speed} | {same} |")
        if len(totals) > 1:
            so, sa = sum(t[0] for t in totals), sum(t[1] for t in totals)
            print(f"| **All {len(totals)}, one after another** | | | **{fmt_time(so)}** | **{fmt_time(sa)}** | **{so / sa:.1f}×** | |")
        if len(versions) > 1:
            print("\narc-unpack by version (median time, default settings):\n")
            print("| Repack | " + " | ".join(versions) + " |")
            print("|---|" + "---:|" * len(versions))
            for rid in repacks:
                cells = []
                for v in versions:
                    runs = [r for r in rows if r["repack"] == rid and r["tool"] == "arc-unpack" and r["version"] == v]
                    t = [r["seconds"] for r in runs if r.get("ok")]
                    failed = len(runs) - len(t)
                    cell = fmt_time(statistics.median(t)) if t else "failed" if runs else "–"
                    if t and failed:
                        cell += f" ({failed} of {len(runs)} runs failed)"
                    cells.append(cell)
                print(f"| {label(rid)} | " + " | ".join(cells) + " |")
        print()


def cmd_id(args):
    ids = repack_ids(args.folder)
    print(json.dumps(ids, indent=2))


def main():
    sys.stdout.reconfigure(encoding="utf-8")
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    a = sub.add_parser("add", help="add runs from compare.ps1 result files")
    a.add_argument("results", nargs="+")
    a.add_argument("--repack-root", default=str(pathlib.Path.home() / "Downloads"), help="where repack folders are, for results that don't name their folder")
    a.add_argument("--disk", help="the drive's model, for results that don't record it")
    t = sub.add_parser("table", help="print the comparison tables")
    t.add_argument("--names", action="store_true", help="show the game names kept in names.local.json")
    i = sub.add_parser("id", help="print a repack's IDs")
    i.add_argument("folder")
    args = p.parse_args()
    {"add": cmd_add, "table": cmd_table, "id": cmd_id}[args.cmd](args)


if __name__ == "__main__":
    main()
