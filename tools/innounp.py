"""Minimal read-only extractor for Inno Setup 5.5.x (Unicode) single-file installers.
Layouts follow innoextract's source (loader/offsets.cpp, stream/block.cpp, stream/chunk.cpp,
setup/header.cpp, setup/file.cpp, setup/data.cpp, stream/exefilter.hpp).
Every extracted file is verified against the SHA-1 stored in the installer.

usage: python -I innounp.py <setup.exe> <out_dir> [--list]
"""
import hashlib, lzma, os, struct, sys, zlib

MAGIC_515 = b"rDlPtS\xcd\xe6\xd7{\x0b*"


def u32(b, o): return struct.unpack_from("<I", b, o)[0]
def u64(b, o): return struct.unpack_from("<Q", b, o)[0]


def read_block(f, pos):
    """setup-0 block: crc32(4) | stored_size(4) | compressed(1) | [crc32(4) + <=4096 bytes]*"""
    f.seek(pos)
    hdr = f.read(9)
    crc, stored, comp = struct.unpack("<IIB", hdr)
    assert zlib.crc32(hdr[4:]) == crc, "block header CRC mismatch"
    raw = f.read(stored)
    data, o = bytearray(), 0
    while o < len(raw):
        c = u32(raw, o)
        piece = raw[o + 4:o + 4 + 4096]
        assert zlib.crc32(piece) == c, "block chunk CRC mismatch"
        data += piece
        o += 4 + len(piece)
    if comp:
        props = data[0]
        filt = {"id": lzma.FILTER_LZMA1, "lc": props % 9, "lp": (props % 45) // 9, "pb": props // 45,
                "dict_size": u32(data, 1)}
        data = lzma.LZMADecompressor(lzma.FORMAT_RAW, filters=[filt]).decompress(bytes(data[5:]))
    return bytes(data), pos + 9 + stored


class R:
    def __init__(self, b, o=0): self.b, self.o = b, o
    def u8(self): v = self.b[self.o]; self.o += 1; return v
    def u16(self): v = struct.unpack_from("<H", self.b, self.o)[0]; self.o += 2; return v
    def u32(self): v = u32(self.b, self.o); self.o += 4; return v
    def u64(self): v = u64(self.b, self.o); self.o += 8; return v
    def raw(self, n): v = self.b[self.o:self.o + n]; assert len(v) == n; self.o += n; return v
    def bstr(self):
        n = self.u32()
        assert n <= len(self.b) - self.o, "string past end"
        return self.raw(n)
    def wstr(self):
        s = self.bstr()
        assert len(s) % 2 == 0, "odd UTF-16 length"
        return s.decode("utf-16-le")


def parse_header_counts(b):
    r = R(b)
    for _ in range(27):  # binary strings up to close_applications_filter (5.5.0)
        r.bstr()
    for _ in range(3):   # license_text, info_before, info_after (ansi)
        r.bstr()
    r.bstr()             # compiled_code
    names = ["language", "message", "permission", "type", "component", "task", "directory", "file",
             "data_entry", "icon", "ini", "registry", "delete", "uninstall_delete", "run", "uninstall_run"]
    return {n: r.u32() for n in names}


def parse_file_entry(r, n_data):
    e = {}
    e["source"] = r.wstr()
    e["dest"] = r.wstr()
    for k in ("font", "assembly", "components", "tasks", "languages", "check", "after", "before"):
        e[k] = r.wstr()
    r.raw(20)  # windows version range
    e["location"] = r.u32()
    e["attributes"] = r.u32()
    e["external_size"] = r.u64()
    e["permission"] = struct.unpack("<h", r.raw(2))[0]
    e["flags"] = r.u32()   # 32 flags in 5.5.0 -> 4 bytes
    e["type"] = r.u8()
    assert e["type"] <= 2, "bad file type"
    assert e["dest"], "empty destination"
    assert e["location"] < n_data or e["location"] == 0xFFFFFFFF, "bad location"
    return e


def find_file_entries(b, n_files, n_data):
    """Scan for the start of the file entry table and validate by parsing all entries."""
    o = 0
    while True:
        o = b.find(b"{\x00", o + 1)
        if o < 0:
            raise SystemExit("file entries not found")
        start = o - 8  # dest length (4) + empty source string (4)
        if start < 0 or u32(b, start) != 0 or not (2 <= u32(b, start + 4) < 2048):
            continue
        try:
            r = R(b, start)
            entries = [parse_file_entry(r, n_data) for _ in range(n_files)]
            if all("\\" in e["dest"] or "}" in e["dest"] for e in entries):
                return entries, start, r.o
        except (AssertionError, UnicodeDecodeError, struct.error, IndexError):
            pass


def parse_data_entries(b, n):
    size = 74
    assert len(b) == n * size, f"data block is {len(b)} bytes, expected {n}x{size}"
    out = []
    for i in range(n):
        r = R(b, i * size)
        d = dict(first_slice=r.u32(), last_slice=r.u32(), chunk_offset=r.u32(), file_offset=r.u64(),
                 size=r.u64(), chunk_size=r.u64(), sha1=r.raw(20), filetime=r.u64(),
                 ver_ms=r.u32(), ver_ls=r.u32(), flags=r.u16())
        out.append(d)
    return out


def decode_call_filter_5309(data):
    out = bytearray(data)
    n, i = len(out), 0
    while i < n:
        b = out[i]
        i += 1
        if b not in (0xE8, 0xE9):
            continue
        if 0x10000 - ((i - 1) % 0x10000) < 5:
            continue
        if i + 4 > n:
            break
        i += 4
        if out[i - 1] in (0x00, 0xFF):
            rel = (out[i - 4] | out[i - 3] << 8 | out[i - 2] << 16)
            rel = (rel - (i & 0xFFFFFF)) & 0xFFFFFFFF
            out[i - 4], out[i - 3], out[i - 2] = rel & 0xFF, (rel >> 8) & 0xFF, (rel >> 16) & 0xFF
            if rel & 0x800000:
                out[i - 1] = (~out[i - 1]) & 0xFF
    return bytes(out)


def read_chunk(f, data_offset, d, cache):
    key = d["chunk_offset"]
    if key in cache:
        return cache[key]
    f.seek(data_offset + d["chunk_offset"])
    assert f.read(4) == b"zlb\x1a", "bad chunk magic"
    raw = f.read(d["chunk_size"])
    flags = d["flags"]
    assert not flags & (1 << 6), "chunk is encrypted"
    if flags & (1 << 7):
        p = raw[0]
        if p == 0x78:
            data = zlib.decompress(raw)
        elif p <= 40:
            dict_size = 0xFFFFFFFF if p == 40 else (2 | (p & 1)) << (p // 2 + 11)
            data = lzma.LZMADecompressor(lzma.FORMAT_RAW, filters=[{"id": lzma.FILTER_LZMA2, "dict_size": dict_size}]).decompress(raw[1:])
        else:
            filt = {"id": lzma.FILTER_LZMA1, "lc": p % 9, "lp": (p % 45) // 9, "pb": p // 45, "dict_size": u32(raw, 1)}
            data = lzma.LZMADecompressor(lzma.FORMAT_RAW, filters=[filt]).decompress(raw[5:])
    else:
        data = raw
    cache.clear()
    cache[key] = data
    return data


def safe_rel(dest):
    parts = []
    for p in dest.replace("/", "\\").split("\\"):
        if p in ("", "."):
            continue
        if p == ".." or ":" in p:
            raise ValueError(f"unsafe path {dest!r}")
        parts.append(p.replace("{", "").replace("}", ""))
    return os.path.join(*parts)


def main():
    exe, out_dir = sys.argv[1], sys.argv[2]
    list_only = "--list" in sys.argv
    with open(exe, "rb") as f:
        blob = f.read()
        m = blob.find(MAGIC_515)
        assert m >= 0, "Inno Setup 5.1.5+ offset table not found"
        vals = struct.unpack_from("<8I", blob, m + 12)
        assert zlib.crc32(blob[m:m + 12 + 28]) == vals[7], "offset table CRC mismatch"
        header_offset, data_offset = vals[5], vals[6]
        ver = blob[header_offset:header_offset + 64].split(b"\0")[0].decode("latin1")
        print(f"version: {ver}   headers @{header_offset}   data @{data_offset}")
        assert "(5.5." in ver and "(u)" in ver, "this extractor only knows Inno Setup 5.5.x Unicode"

        b1, end1 = read_block(f, header_offset + 64)
        b2, _ = read_block(f, end1)
        counts = parse_header_counts(b1)
        print("counts:", {k: v for k, v in counts.items() if v})
        data = parse_data_entries(b2, counts["data_entry"])
        files, fstart, fend = find_file_entries(b1, counts["file"], counts["data_entry"])
        print(f"file entries: {len(files)} at {fstart}..{fend} of {len(b1)}")

        cache, ok, bad = {}, 0, 0
        order = sorted(range(len(files)), key=lambda k: (data[files[k]["location"]]["chunk_offset"], data[files[k]["location"]]["file_offset"]) if files[k]["location"] != 0xFFFFFFFF else (-1, 0))
        for k in order:
            e = files[k]
            if e["location"] == 0xFFFFFFFF:
                print(f"  (external) {e['dest']}")
                continue
            d = data[e["location"]]
            if list_only:
                print(f"  {d['size']:>10}  {e['dest']}")
                continue
            chunk = read_chunk(f, data_offset, d, cache)
            content = chunk[d["file_offset"]:d["file_offset"] + d["size"]]
            assert len(content) == d["size"], f"{e['dest']}: chunk too short"
            if d["flags"] & (1 << 4):
                content = decode_call_filter_5309(content)
            good = hashlib.sha1(content).digest() == d["sha1"]
            ok += good
            bad += not good
            path = os.path.join(out_dir, safe_rel(e["dest"]))
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "wb") as o:
                o.write(content)
            print(f"  {'ok ' if good else 'BAD'} {d['size']:>10}  {e['dest']}")
        if not list_only:
            print(f"SHA-1 verified: {ok} ok, {bad} bad")


if __name__ == "__main__":
    main()
