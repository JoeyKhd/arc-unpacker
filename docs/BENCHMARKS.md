# Benchmarks

Machine for all runs: Core i9-13900K (32 threads), 32 GB RAM, NVMe SSD, Windows 11.

## End to end: a whole install

A 5-archive repack (4.7 GB → 8.1 GB, 3,489 files). Its installer unpacks an optional bonus archive and two sets of audio data, then rebuilds the audio with its own tools in two chains of 12 steps. The file cache was flushed before each run.

| Run | Time | What it covers |
|---|---:|---|
| `setup.exe /VERYSILENT` (bonus selected) | 199.8 s | unpacking and post-processing |
| … then its default "verify files" pass (QuickSFV) | +86.9 s | MD5 of all 3,484 listed files |
| arc-unpack, steps one after another | 123 s | unpacking, post-processing and MD5 of every file |
| **arc-unpack `--no-verify`** (early extraction, 2nd heavy job) | **75 s** | the same |
| **arc-unpack, default** | **84 s** | the same, plus an MD5 check of the 5 downloaded archives first |

Where the time goes in the one-after-another run:

| Step | Time |
|---|---:|
| Largest archive | 70 s |
| Bonus archive | 24 s |
| A third archive | 13 s |
| The installer's tools, across 26 steps | about 13 s |

Early extraction overlaps all of that with the largest archive.

**Correctness.** The arc-unpack install was compared file by file with the original installer's. All 3,489 files were byte-identical. The only files missing were Inno Setup's own uninstaller files.

## The decode engine alone

Before arc-unpack ran installers' scripts, this compared only the decoding. `bench/bench.ps1` extracts the same archives three ways:

- **Baseline:** each archive one after another through `unarc.dll`'s `FreeArcExtract`, which is what Inno Setup + ISDone installers call internally.
- **arc-unpack, `--heavy-jobs 1`:** one `extract` run with `-j 3 --heavy-jobs 1`.
- **arc-unpack, `--heavy-jobs 2`:** the same run with two heavy jobs allowed.

Every run starts with the archives in the file cache, samples CPU and RAM every second, and is checked to produce the same file count and bytes. Raw data: [`bench/results.json`](../bench/results.json).

| Run | Wall time | Avg CPU | Peak RAM in use |
|---|---:|---:|---:|
| Baseline: installer engine, archive by archive | 1,045.6 s | 19% | 31.5 GB |
| arc-unpack, `--heavy-jobs 1` | 720.8 s (1.45×) | 16% | 26.5 GB |
| arc-unpack, `--heavy-jobs 2` | 682.8 s (1.53×) | 19% | 28.6 GB |

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
