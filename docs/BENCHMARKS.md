# Benchmarks

Ten FitGirl repacks, each installed by its own installer and by arc-unpack, one at a time on the same PC. **Version** is the arc-unpack version each game was last measured with.

| Game | Download | Original installer | arc-unpack | Faster | Version |
|---|---:|---:|---:|---:|---|
| Blitzkrieg 3 | 1.7 GB | 1 min 43 s | 26 s | **3.9×** | 0.1.0-beta.6 |
| Castlevania - Belmont's Curse | 5.1 GB | 3 min 40 s | 1 min 09 s | **3.2×** | 0.1.0-beta.6 |
| Slime Rancher 2 | 1.7 GB | 2 min 29 s | 50 s | **3.0×** | 0.1.0-beta.6 |
| Flick | 2.1 GB | 54 s | 19 s | **2.8×** | 0.1.0-beta.6 |
| Moadra | 3.3 GB | 50 s | 22 s | **2.3×** | 0.1.0-beta.6 |
| Dinkum | 0.9 GB | 1 min 07 s | 30 s | **2.2×** | 0.1.0-beta.6 |
| 771 - Smile and Work | 0.7 GB | 24 s | 12 s | **1.9×** | 0.1.0-beta.6 |
| Serious Sam Fusion | 4.6 GB | 2 min 29 s | 1 min 20 s | **1.9×** | 0.1.0-beta.6 |
| The Killing Stone | 4.4 GB | 1 min 58 s | 1 min 04 s | **1.9×** | 0.1.0-beta.6 |
| Tokyo Xtreme Racer | 8.3 GB | 3 min 34 s | 2 min 51 s | **1.3×** | 0.1.0-beta.6 |
| **All 10** | | **19 min 09 s** | **9 min 03 s** | **2.1×** | |

- **Original installer:** the repack's `setup.exe`, timed until its default file check (QuickSFV) finished.
- **arc-unpack:** default settings, which check the downloaded archives and every installed file.
- **Same result:** all ten installs were byte-identical to the original installer's.
- **Same conditions:** the same parts in both, and the file cache flushed before every run.

Test PC: Intel Core i9-13900K, 32 GB DDR5-5600, Samsung 990 PRO 2 TB NVMe SSD, Windows 11 Pro.

Every run is kept in [`bench/ledger.jsonl`](../bench/ledger.jsonl), so a new version only needs measuring on the games it should change. To measure, run `.\bench\compare.ps1 -Repacks <folders> -SkipOriginal` from an administrator PowerShell, then `python bench\ledger.py add bench\results\*.json`. `python bench\ledger.py table --doc` prints this table, and `python bench\ledger.py table` shows every version side by side.
