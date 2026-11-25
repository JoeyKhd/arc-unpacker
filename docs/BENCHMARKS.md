# Benchmarks

Ten FitGirl repacks, each installed by its own installer and by arc-unpack 0.1.0-beta.6, one at a time on the same PC.

| Game | Download | Original installer | arc-unpack | Faster |
|---|---:|---:|---:|---:|
| Blitzkrieg 3 | 1.7 GB | 1 min 43 s | 26 s | **3.9×** |
| Castlevania - Belmont's Curse | 5.1 GB | 3 min 40 s | 1 min 09 s | **3.2×** |
| Slime Rancher 2 | 1.7 GB | 2 min 29 s | 50 s | **3.0×** |
| Flick | 2.1 GB | 54 s | 19 s | **2.8×** |
| Moadra | 3.3 GB | 50 s | 22 s | **2.3×** |
| Dinkum | 0.9 GB | 1 min 07 s | 30 s | **2.2×** |
| 771 - Smile and Work | 0.7 GB | 24 s | 12 s | **1.9×** |
| Serious Sam Fusion | 4.6 GB | 2 min 29 s | 1 min 20 s | **1.9×** |
| The Killing Stone | 4.4 GB | 1 min 58 s | 1 min 04 s | **1.9×** |
| Tokyo Xtreme Racer | 8.3 GB | 3 min 34 s | 2 min 51 s | **1.3×** |
| **All 10** | | **19 min 09 s** | **9 min 03 s** | **2.1×** |

- **Original installer:** the repack's `setup.exe`, timed until its default file check (QuickSFV) finished.
- **arc-unpack:** default settings, which check the downloaded archives and every installed file.
- **Same result:** all ten installs were byte-identical to the original installer's.
- **Same conditions:** the same parts in both, and the file cache flushed before every run.

Test PC: Intel Core i9-13900K, 32 GB DDR5-5600, Samsung 990 PRO 2 TB NVMe SSD, Windows 11 Pro.

Every run is kept in [`bench/ledger.jsonl`](../bench/ledger.jsonl). To measure a new version on the same games, run `.\bench\compare.ps1 -Repacks <folders> -SkipOriginal` from an administrator PowerShell, then `python bench\ledger.py add bench\results\*.json` and `python bench\ledger.py table`.
