# Benchmarks

Machine for all runs: Core i9-13900K (32 threads), 32 GB RAM, NVMe SSD, Windows 11.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="benchmark-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="benchmark-light.svg">
    <img src="benchmark-light.svg" width="100%" alt="Original setup.exe: 4 min 47 s. arc-unpack: 1 min 24 s with every check on, 1 min 15 s with the archive check skipped. The table below has the same numbers.">
  </picture>
</p>

## End to end: a whole install

A 5-archive repack (4.7 GB → 8.1 GB, 3,489 files). Its installer unpacks an optional bonus archive and two sets of audio data, then rebuilds the audio with its own tools: 35 script steps in all. Every run started with the file cache flushed and installed into an empty folder.

| Run | Time | Speed-up | Avg CPU | What it covers |
|---|---:|---:|---:|---|
| `setup.exe /VERYSILENT` (bonus selected) | 199.8 s | | | unpacking and post-processing |
| … then its default "verify files" pass (QuickSFV) | +86.9 s | | | MD5 of all 3,484 listed files |
| **Original installer, total** | **286.7 s** | 1.0× | 31% | |
| arc-unpack, steps one after another | 123 s | 2.3× | | unpacking, post-processing and MD5 of every file |
| **arc-unpack `--no-verify`** | **75 s** | **3.8×** | 75% | the same, with early extraction and a second heavy job |
| **arc-unpack, default** | **84 s** | **3.4×** | | the same, plus an MD5 check of the 5 downloaded archives |

- **Throughput.** arc-unpack wrote 9.19 GB (the 8.1 GB game plus intermediate files the installer's tools consume and delete) at 122.8 MB/s in the 75 s run, and 109.5 MB/s in the 84 s run.
- **Memory.** RAM in use peaked 15.8 GB above its starting level under the original installer, and 11.9 GB above it under arc-unpack (the 75 s run).
- **Newer builds.** The current build installs the same repack in 82 s with default settings, measured through the window while a game-store download was running in the background.

**Correctness.** The arc-unpack install was compared file by file with the original installer's. All 3,489 files were byte-identical. The only files missing were Inno Setup's own uninstaller files. All 3,484 files on the repack's MD5 list matched in every arc-unpack run.

## Everything fits inside the largest archive

One after another, the parts of this install add up:

| Part | On its own |
|---|---:|
| Largest archive (xtool → srep → lolz-class; 3,069 files, 5.80 GB) | 70 s |
| Bonus archive (330 files, 1.91 GB) | 24 s |
| A third archive (537 MB) | 13 s |
| The installer's own tools and the two inner archives they produce | about 13 s |

With early extraction they overlap. In the 75 s run the largest archive took 72 s, and everything else finished inside that window: the other four archives, 28 runs of the installer's tools, and the MD5 of every file.

## How close to the limit

Each stage of the largest archive was measured on its own, to see what's left to gain:

| Measured on the largest archive | Result |
|---|---:|
| lolz-class decoder alone (4.0 GB → 9.5 GB) | 23.5 s |
| xtool alone, input ready (15 GB → 5.8 GB of game files) | about 62 s |
| … at 75% to 200% thread settings | 62–67 s |
| … at 50% | 79.6 s |
| All three stages connected, three runs | 72.1 s, 75.2 s, 76.1 s |
| **The whole install** | **75 s** |

- **xtool sets the pace.** It's the repack's own build, it keeps 18–19 of 32 threads busy at any thread setting from 75% up, and it needs about 62 s even when its input is instant.
- **The install is within about 13 s of that.** Process priorities, larger buffers between stages and larger transfer buffers were all measured and made no difference.
- **What could still help** is outside the decoding: unpacking each archive as soon as its download finishes.

## The decode engine alone

Before arc-unpack ran installers' scripts, this compared only the decoding, on a second repack. `bench/bench.ps1` extracts the same archives three ways:

- **Baseline:** each archive one after another through `unarc.dll`'s `FreeArcExtract`, which is what Inno Setup + ISDone installers call internally.
- **arc-unpack, `--heavy-jobs 1`:** one `extract` run with `-j 3 --heavy-jobs 1`.
- **arc-unpack, `--heavy-jobs 2`:** the same run with two heavy jobs allowed.

Every run starts with the archives in the file cache, samples CPU and RAM every second, and is checked to produce the same file count and bytes. Raw data: [`bench/results.json`](../bench/results.json).

| Run | Wall time | Avg CPU | Peak RAM in use |
|---|---:|---:|---:|
| Baseline: installer engine, archive by archive | 1,045.6 s (17 min 26 s) | 19% | 31.5 GB |
| arc-unpack, `--heavy-jobs 1` | 720.8 s (1.45×) | 16% | 26.5 GB |
| arc-unpack, `--heavy-jobs 2` | 682.8 s (11 min 23 s, 1.53×) | 19% | 28.6 GB |

Baseline time per archive (4 archives of a 7-archive repack, 8.8 GB → 12.1 GB / 7,204 files):

| Archive | Packed | Codec chain | Baseline |
|---|---:|---|---:|
| A | 5.97 GB | razor; mostly stored data | 10.4 s |
| B | 2.46 GB | xtool → precomp → srep → lolz-class | 954.6 s |
| C | 258 MB | srep → dispack → delta → razor (FreeArc built-ins, runs through `unarc.dll` in both) | 8.5 s |
| D | 134 MB | precomp → srep → lolz-class | 72.1 s |

What the numbers say:

- **Most of the gain is in archive B.** Its four codecs overlap in arc-unpack's pipeline, and the small archives finish in its shadow instead of after it.
- **The CPU is mostly idle in both.** Average use stays under 20% of 32 threads. lolz-class decoders are single-threaded, and that sets the floor.
- **The largest archive (36.5 GB) isn't in this table,** because one run takes hours. It uses the same codec types as archive B.

Reproduce it from an administrator PowerShell, with codecs prepared as described in [How it works](HOW-IT-WORKS.md#codec-configuration):

```powershell
.\bench\bench.ps1 -Repack D:\repack -Archives data-02.bin,data-03.bin -Codecs D:\codecs -Work C:\bw
```

Keep `-Work` short: the 32-bit `unarc.dll` baseline can't write paths over 260 characters.

The chart at the top is generated by [`tools/benchmark-chart.py`](../tools/benchmark-chart.py).
