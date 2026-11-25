<p align="center"><img src="docs/banner.svg" alt="arc-unpacker: fast, parallel installer for FreeArc-based repacks" width="100%"></p>

<p align="center">
  <a href="https://github.com/JoeyKhd/arc-unpacker/releases"><img alt="Release" src="https://img.shields.io/github/v/release/JoeyKhd/arc-unpacker?include_prereleases&label=release"></a>
  <img alt="Platform" src="https://img.shields.io/badge/platform-Windows%2010%2F11%20x64-0078d4">
  <img alt="Rust" src="https://img.shields.io/badge/built%20with-Rust-dea584">
</p>

# arc-unpacker

**Installs FitGirl-style repacks up to 3.9× faster than their own installer, with a byte-identical result.** It runs the installer's own install steps on a parallel engine, and checks every file while writing it.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/benchmark-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="docs/benchmark-light.svg">
    <img src="docs/benchmark-light.svg" width="100%" alt="Benchmark, same repack on the same PC. Original setup.exe: 4 min 47 s (install 3 min 20 s, then verify 1 min 27 s). arc-unpack with default settings, every check on: 1 min 14 s, 3.9 times faster. All 3,489 files byte-identical. Average CPU use 31% versus 68%.">
  </picture>
</p>

<sub>Same repack, same PC: 5 archives, 4.7 GB → 8.1 GB, audio rebuilt by the repack's own tools. Core i9-13900K, 32 GB RAM, NVMe, file cache flushed before each run; arc-unpack is the median of three runs. <a href="#benchmarks">The numbers</a></sub>

<p align="center"><img src="docs/screenshot.png" alt="arc-unpack after an install: what was installed, how long it took, and the verified file count" width="640"></p>

## Use it

1. Download **`arc-unpack-gui.exe`** from the [latest release](https://github.com/JoeyKhd/arc-unpacker/releases). It's a single file.
2. Run it. It asks for administrator rights, as repack installers do.
3. Drop the repack folder onto the window, pick the optional parts you want, and press **Install**.

From a terminal: `arc-unpack install "<repack folder>" "<game folder>"`.

> Use it only on archives you have the right to extract.

## Benchmarks

| Same repack, same PC | Time | Speed-up |
|---|---:|---:|
| Original `setup.exe`, default (install, then verify files) | 4 min 47 s | 1.0× |
| Original `setup.exe`, verify turned off | 3 min 20 s | 1.4× |
| **arc-unpack**, default (checks the downloads, installs, verifies every file) | **1 min 14 s** | **3.9×** |

- **Same result.** All 3,489 installed files are byte-identical to the original installer's.
- **More checks in less time.** arc-unpack also checks the downloaded archives, which the original doesn't, and is still 2.7× faster than the original with its verify step turned off.
- **The whole install takes as long as its largest archive.** That archive needs 72 s. The other four archives, 28 tool runs and every check happen inside the same 74 s.
- **Close to the decompressor's limit.** The repack's own xtool needs about 62 s for that archive even with its input ready, so little is left to gain on this repack.
- **Repeatable.** Three runs took 73 s, 74 s and 78 s.
- **A second repack, decoding only:** 17 min 26 s → 11 min 23 s (1.5×).

Method, raw numbers and how to reproduce them: [Benchmarks](docs/BENCHMARKS.md).

## Why it's faster

- **Archives are unpacked before the script asks for them.** While the installer's steps run one by one, the archives needed later are already being extracted, then moved into place at their step.
- **Every codec stage runs at once.** A chain like xtool → srep → lolz runs as concurrent processes instead of one after another. Memory-hungry stages share RAM safely.
- **Checking is free.** Files are hashed against the repack's MD5 list as they're written, so there's no second pass over the game.

More in [How it works](docs/HOW-IT-WORKS.md).

## Works with

- **Inno Setup + FreeArc repacks:** the format FitGirl uses "in 99.9% of cases". arc-unpack runs the installer's ISDone extraction steps and post-processing tools, with its CLS codecs (xtool, srep, lolz, …).
- **Check any repack in seconds, without installing:** `arc-unpack script "<repack folder>"` lists every step, or names what isn't supported.
- **Tested end to end** on two repacks with different post-processing. Every file was verified on both, and one was compared byte for byte with the original installer's output.

Not done: an uninstaller, Start-menu entries, registry entries, or DirectX / VC++ installers. Use **Create desktop shortcut** after installing, and delete the folder to uninstall.

## Build

```powershell
.\build.ps1              # -> dist\ : arc-unpack.exe, arc-unpack-gui.exe (self-contained)
cargo test --release     # unit and end-to-end tests
```

Requires Rust 1.85+ with the `x86_64-pc-windows-msvc` and `i686-pc-windows-msvc` targets, and the MSVC build tools.

## Acknowledgements

- [FreeArc](https://en.wikipedia.org/wiki/FreeArc) by Bulat Ziganshin, for the archive format, the CLS interface and `arc.ini` semantics (from the FreeArc 0.67 sources).
- [innoextract](https://constexpr.org/innoextract/) by Daniel Scharrer, for its documentation of the Inno Setup data layout.
- [RemObjects Pascal Script](https://github.com/remobjects/pascalscript), whose runtime defines the bytecode Inno Setup compiles install scripts into.
