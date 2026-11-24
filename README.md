# arc-unpacker

**A fast, parallel unpacker for FreeArc archives and the game-repack installers built on them.**

Repack installers usually unpack through a 32-bit Inno Setup process that runs one archive at a time and pipes data between codecs with small buffers. `arc-unpack` reads the same archives and runs the same codecs the installer ships, but schedules them like a build system: every codec stage runs at once, independent archives decode in parallel, and files are written by a thread pool.

> Use it only on archives you have the right to extract.

---

## Features

- **Concurrent codec pipeline.** Each method in a chain such as `xtool + srep + lolz` runs as its own process or thread. Stages are connected by large in-memory pipes, so no stage waits for the previous one to finish.
- **Parallel scheduling.** Blocks from different archives decode at the same time, largest first (`--jobs`).
- **Parallel file output.** A pool of writer threads creates, preallocates, checksums and timestamps files while decoding continues (`--writers`).
- **Native 64-bit.** 32-bit codec DLLs run in small helper processes (`cls-host-x86.exe`, `unarc-host-x86.exe`), so the rest of the pipeline isn't limited to 4 GB.
- **Scheduler-aware.** Codec processes are opted out of Windows EcoQoS throttling and can be pinned to performance cores on hybrid CPUs.
- **Bottleneck report.** After each block it prints CPU and wall time per codec process.
- **Guards against misbehaving codecs:**
  - Every process runs in a kill-on-close job object, so nothing is orphaned.
  - Codec error dialogs are closed automatically and their text is reported.
  - A stall watchdog stops hung blocks.
  - A failing stage tears down its block instead of deadlocking it.
- **Works with what installers ship.** It reads FreeArc's `arc.ini` (external compressors), loads CLS codec DLLs (`cls-*.dll`), and falls back to the installer's own `unarc.dll` for FreeArc built-in methods.
- **Safe extraction.** Archive paths are untrusted: `..`, absolute paths, drive letters, alternate data streams and device names are refused.

## How it works

```
                      ┌────────────── one block ──────────────┐
 archive ─► reader ─► │ codec N ─► … ─► codec 2 ─► codec 1    │─► splitter ─► writer pool ─► files
            (4 MiB)   │ (process / thread per stage, pipes)   │   (by size)   (N threads, CRC)
                      └───────────────────────────────────────┘
 Blocks from different archives run side by side (--jobs).
 Archives that need FreeArc built-ins run whole through unarc.dll, in parallel with the rest.
```

Each codec is resolved, in this order, to one of:

| Kind | Source | How it runs |
|---|---|---|
| External program | `[methods.x] cmd = …` (or imported from `arc.ini`) | Child process, piped stdin/stdout or temp files |
| CLS DLL | `cls-<name>.dll` in the codec folder | `cls-host-x86/x64.exe`, chosen to match the DLL's architecture |
| Built-in | `storing`, `lzma` | In-process thread |
| FreeArc built-in (`delta`, `dispack`, `4x4`, `tor`, …) | `unarc = 'unarc.dll'` | The whole archive goes through `unarc-host-x86.exe` |

## Requirements

- Windows 10 or 11, x64.
- To build: Rust 1.85+ with the `x86_64-pc-windows-msvc` and `i686-pc-windows-msvc` targets, plus the MSVC build tools.
- Some third-party codecs create `Global\` shared memory and only work when run as **administrator**. `arc-unpack` detects these and warns.

## Build

```powershell
.\build.ps1             # -> dist\arc-unpack.exe, cls-host-x64.exe, cls-host-x86.exe, unarc-host-x86.exe
cargo test --release    # 25 unit + end-to-end tests
```

## Usage

```powershell
# What's inside: blocks, codec chains, sizes
arc-unpack list   data-01.bin data-02.bin

# Point it at the installer's tools folder (contains arc.ini, cls-*.dll, codec programs)
arc-unpack import-arcini C:\setup-tools\arc.ini -o codecs.toml

# Check that every method has a decoder, then decode and verify without writing
arc-unpack check  *.bin -c codecs.toml
arc-unpack test   *.bin -c codecs.toml

# Extract
arc-unpack extract *.bin -c codecs.toml -o D:\Games\Title -j 2
```

| Flag | Meaning |
|---|---|
| `-j, --jobs N` | Blocks decoded at once. Heavy codecs use several GB each, so size this to your RAM. |
| `--writers N` | File writer threads per block. |
| `--stall-timeout S` | Stop a block after S seconds without progress (default 600, 0 = never). |
| `--skip-missing` | Extract what's possible when some methods have no decoder. |
| `--overwrite` | Replace existing files. |
| `--no-tune` | Leave process priority and power throttling alone. |

### Getting the codecs out of an installer

Repack codecs live inside the Inno Setup installer. `tools/innounp.py` is a read-only extractor for Inno Setup 5.5 Unicode installers: it never runs `setup.exe`, and it verifies every file against the SHA-1 stored in the installer.

```powershell
python -I tools\innounp.py setup.exe C:\setup-tools --list   # list
python -I tools\innounp.py setup.exe C:\setup-tools          # extract (tools land in C:\setup-tools\tmp)
```

### Codec configuration

```toml
codec_dir = 'C:\setup-tools\tmp'
unarc     = 'unarc.dll'            # whole-archive fallback for FreeArc built-ins

[methods.srep]                     # external program, piped
cmd = 'srep64.exe -d {options}- -'
header = false                     # arc.ini "header" flag byte

[methods.rzwb]                     # external program, temp files
cmd = 'rzw d {input} {output} 1023 1023'
header = false

[methods.magic2]                   # CLS DLL under another name
cls = 'cls-lolz_x64.dll'
performance_cores = true

[methods.fast]                     # alias to another chain
alias = 'lzma:d64m'
```

Placeholders follow FreeArc's: `{options}` (→ `-p1 -p2 `), `{X option Y}`, `{params}`, `{input}`, `{output}`, `{codec_dir}`. See [`codecs.example.toml`](codecs.example.toml).

## Checksums

FreeArc stores a CRC-32 per file. Some repack tools use a modified FreeArc with a different checksum. `arc-unpack` detects this per archive by checking the directory block's CRC and handles it as follows:

| Archive | Result |
|---|---|
| Standard CRC-32 | Every file is verified |
| Modified checksum | Files are reported as *complete but not CRC-checked*. Sizes and codec exit codes are still verified |
| Extracted through `unarc.dll` | Verified by `unarc.dll` itself |

## Project layout

```
src/format.rs        FreeArc container: footer, control blocks, directory, varints
src/config.rs        codecs.toml, method resolution, FreeArc option templates
src/stage.rs         stage runners: built-ins, external programs, CLS host processes
src/extract.rs       scheduling, splitting, writer pool, watchdog, path safety
src/guard.rs         job objects, codec dialog handling, CLS proxy support, elevation check
src/pipe.rs          chunked in-process pipe
src/perf.rs          EcoQoS opt-out, P-core CPU sets, per-process CPU time
src/arcini.rs        arc.ini → codecs.toml
src/bin/cls-host.rs  CLS DLL host (x86 + x64)
src/bin/unarc-host.rs unarc.dll host (x86)
tools/innounp.py     read-only Inno Setup 5.5 Unicode extractor
tests/synthetic.rs   builds FreeArc archives byte by byte and runs the real binary
```

## Acknowledgements

- FreeArc by Bulat Ziganshin, for the archive format, the CLS interface and `arc.ini` semantics (from the FreeArc 0.67 sources).
- innoextract by Daniel Scharrer, for its documentation of the Inno Setup data layout.
