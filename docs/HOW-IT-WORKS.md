# How it works

## It runs the installer's own steps

A repack's `setup.exe` is an Inno Setup program. Its real work happens in a compiled Pascal Script inside it: which archives to unpack, in what order, and which tools to run on the results (rebuilding audio banks, applying patches, running `.bat` scripts, merging file lists).

arc-unpack includes a small Pascal Script interpreter ([`src/pascal.rs`](../src/pascal.rs)) and runs that script itself, supplying the functions it calls ([`src/setup_script.rs`](../src/setup_script.rs)):

- **Archive extraction** (ISDone's `ISArcExtract`) goes through arc-unpack's parallel engine.
- **The repack's own tools** (`ISExec`, `Exec`) run exactly as the script says, in the same order, without windows.
- **RAR archives** some older repacks rebuild and then unpack (ISDone's `ISRarExtract`) go through the installer's own `Unrar.dll`, in a 32-bit helper like `unarc.dll`.
- **Codec settings** the script hands to ISDone (`PrecompInit`'s temp folder and memory) are written into the installer's `CLS.ini`, as ISDone does.
- **The installer's window**, progress bars and music are accepted and ignored.

The script runs under rules:

- It can only write and delete inside the game folder and its own temporary folder.
- The repack folder is read-only to it.
- It can only start programs from those folders, plus `cmd.exe`.
- It never starts the uninstaller. FitGirl's scripts run it to clean up after a failed step, and in a game folder made by the original installer that would remove the whole game.

## Optional parts

FitGirl's scripts extract every archive that is in the repack folder; the components only decide a few small files. arc-unpack decides per archive instead:

- **A part is an archive plus the steps after it,** up to the next repack archive: those steps process what it extracted. Leaving an archive out leaves its steps out too, including any file moves or deletes the script makes in the game folder meanwhile. Changes to the installer's own temporary files (codec settings) still happen, since later parts need them.
- **Unticked parts are left out.**
- **Untick the base game to add parts to an installed game.** The chosen folder must have this repack's game installed, by arc-unpack or by the original installer: every file on the repack's list must be there, and the game's own copy of the list must match (or, if `_Redist` was deleted, a few files are checked by MD5). Only the ticked parts' archives are needed; the main `.bin` files can be gone.
- **Adding a part** runs only that part's steps, adds its file list (`*.addon`) to the game's `_Redist\fitgirl.md5` the way the installer does, and checks the part's files against it. Game files a part replaces (a language pack, say) are moved to `_arc-unpack-backup\<part>` first.

From a terminal, list the parts to add without the base game's component: `arc-unpack install "<repack>" "<game folder>" --components bonus`.

Preview what an installer will do without changing anything:

```text
> arc-unpack script "D:\Downloads\Some Repack"
Components (default install + downloaded optional parts):
  [x] text  Main game files
  [x] bonus  Bonus Content
Steps:
    1. run cmd.exe /C fnr.exe arc.ini fgrplc -1
    2. extract D:\Downloads\Some Repack\fg-optional-bonus-content.bin -> C:\Games\Game
    3. run cmd.exe /C x2.exe 01.fgpack 01.fgpack.x2&&del 01.fgpack.x2 (in C:\Games\Game)
   ...
```

If a script needs a function arc-unpack doesn't provide, the install stops with a message naming it. `--no-script` extracts the archives directly instead, with no post-processing.

## What makes it fast

- **Early extraction.** arc-unpack plans the whole script first. While the script works through one step, the repack archives it needs later are already being extracted, into a staging folder next to the game folder. When the script reaches them, the files are moved into place (a rename on the same drive), so the order of effects is the script's own.
- **Checking while writing.** Every file named in the repack's MD5 list is hashed as it's written. Files changed by a later step are re-checked from disk at the end. There's no second read of the whole game.
- **Checking the downloads while installing.** The `.bin` files are hashed against the repack's list while they're being decoded, not in a pass before. A damaged archive stops the install with its name.
- **A small plugin inside xtool.** xtool, the precompressor most repacks use, loads every plugin next to it. arc-unpack places a 10 KB one there ([`xtool-timer/`](../xtool-timer/src/lib.rs)) that registers no codecs and does two things inside xtool's own process:
  - **Shorter waits.** xtool coordinates its threads with `Sleep(1)`, which lasts about 15 ms at Windows' default timer resolution; the plugin requests 1 ms. On PCs that already run a 1 ms timer it makes no difference.
  - **A steady view of free memory.** xtool keeps the streams it has to repeat (deduplication) partly in memory, sized by the free memory it sees, and partly spilled. If free memory is low for a while and then rises during the decode, xtool reads the spilled streams back from the wrong place and silently restores the largest repeated files wrong (found on AI LIMIT, whose 13 largest files came out wrong when other archives were decoding at the same time). The plugin gives xtool the memory figures from when it started, which avoids that. `ARC_UNPACK_XTOOL_LIVE_MEMORY=1` turns only this part off.

  Set `ARC_UNPACK_NO_XTOOL_TIMER=1` to leave the plugin out altogether.
- **xtool on several processes.** xtool restores its data in chunks of about 400 MB, one chunk at a time, and a chunk only ends when its slowest stream ends. When a few large images fill a chunk (PNGs rebuilt with preflate), most of its threads wait: on Factorio's main archive, xtool kept about 5 of 32 threads busy for 300 s. arc-unpack reads xtool's stream itself and gives each chunk to its own xtool process, several at once (as many as the CPU and free memory allow), then puts their output back in order. xtool still restores every byte, so the output is identical; that archive now takes 135 s. Streams in another layout, or whose chunks depend on each other (deduplicated streams), go to one xtool as before, and the log says why. `ARC_UNPACK_XTOOL_WORKERS=N` sets the number of processes (1 turns this off).
- **Every codec stage at once.** A chain like `xtool + precomp + srep + lolz` runs as four concurrent processes connected by large pipes, instead of one after another.
- **Memory-aware scheduling.** Jobs are classed by codec: the lolz family and large srep blocks are memory-heavy, the rest are light and run alongside. The largest archive gets its slot first, because it decides when the install ends. A second heavy job only starts while plenty of RAM is free, and the limit is machine-wide, so two runs at once can't exhaust RAM or corrupt each other.
- **A RAM limit you set.** FitGirl's installers have a "limit RAM usage to 2 GB" switch that lowers the decoders' memory settings. arc-unpack has the same switch with a number: **Limit RAM to** in the window, `--memory-limit <GB>` in a terminal. The lolz decoders are told to use at most half of it, srep a quarter and xtool's deduplication a fifth; below 8 GB xtool also gets fewer threads. One memory-hungry decoder runs at a time, and arc-unpack's own buffers shrink. It's slower, and it's a target, not a hard cap: data packed with a large dictionary still needs that dictionary.
- **A safe setting for an old decoder.** The lolz decoder of 2018 repacks (`cls-magic.dll`) never starts when it is allowed about 16 GB or more: it uses no CPU and waits forever. FitGirl's scripts only give it a limit when "limit RAM" is ticked, so on PCs with 20 or more CPU threads the repack's own installer hangs (A Way Out on a 32-thread PC). arc-unpack always gives it at most 12 GB, or 40% of RAM if that is less. Measured: 12 GB and 8 GB work, 16 GB and no limit hang; the game installs in 1 min 34 s.
- **Fast output.** A pool of writer threads creates, preallocates and timestamps files while decoding continues.
- **Never throttled.** Codec processes are opted out of Windows EcoQoS, so they aren't parked on efficiency cores.

The engine for one archive job:

```
                      ┌────────────── one job ───────────────┐
 archive ─► reader ─► │ codec N ─► … ─► codec 2 ─► codec 1   │─► splitter ─► writer pool ─► files
            (4 MiB)   │ (process / thread per stage, pipes)  │   (by size)   (N threads, CRC, MD5)
                      └──────────────────────────────────────┘
```

| Codec kind | Source | How it runs |
|---|---|---|
| External program | `arc.ini` / `[methods.x] cmd = …` | Child process, piped or through temp files |
| CLS DLL | `cls-<name>.dll` | `cls-host-x86/x64`, matched to the DLL's architecture |
| Built-in | `storing`, `lzma` | In-process thread |
| FreeArc `4x4` | built in | Its blocks decoded several at a time through the inner method (`4x4:b16mb:mpz`, `4x4:b128mb:mpzz`) |
| Other FreeArc built-ins (`delta`, `dispack`, `tor`, …) | `unarc.dll` | Whole archive via `unarc-host-x86`, in its own working folder |

## Runtimes

FitGirl's installers don't contain most runtimes. For each runtime component (`vc_2022_x64`, `dotnet48`, `physx`, ...), the setup wizard downloads the official installer (Inno Download Plugin, `IdpAddFileComp(url, file, component)`), and the Finish page offers to run it. DirectX's web installer comes inside setup.exe.

arc-unpack reads the same information: the runtime components a default install includes, the links the script registers, and the installers setup.exe carries. It checks whether this PC has each runtime (Visual C++ and .NET by their registry keys, DirectX and PhysX by their system DLLs). Only the missing ones are offered: in the window, a checkbox ticked by default (as FitGirl's installer ticks them); on the command line, only with `--runtimes`:

- **The file:** taken from setup.exe if it's there, otherwise downloaded with Windows' `curl.exe` from the link the script registers.
- **The check:** it runs only with a valid signature from Microsoft (NVIDIA for PhysX).
- **The install:** silent, after the game, one runtime at a time. "Already installed" and "restart needed" count as success.

`arc-unpack runtimes "<repack>"` lists them for a repack (only setup.exe is needed); `--install` installs the missing ones.

## Uninstaller

Every install gets `uninstall.exe` and its list `uninstall.dat` in the game folder, and an entry in Windows' list of installed apps (Settings > Apps; for all users when installed as administrator). arc-unpack's own uninstaller is used rather than the repack's `unins000.exe`, which needs a log in its exact Inno Setup build's format and runs the repack's script and DLLs.

- **What it removes:** the files and folders the install added. The game folder is listed before installing (kept until an install into it succeeds, so a retry after a stopped one still counts what that left) and again afterwards; the difference is the list. The shortcuts made, and Start menu folders made for them, are listed too.
- **What it keeps:** files that were in the folder before, and files the game makes later (saves, settings). Folders go once they are empty; the last dialog says what's left and offers to open the folder.
- **How it runs:** it copies itself to the temporary folder and runs from there, so the copy in the game folder can be removed too, and asks for administrator rights only if the entry or shortcuts are for all users. `/S` uninstalls without asking (as `QuietUninstallString`).
- **Adding parts** to a game it installed extends its list; a game installed by FitGirl's own installer keeps that installer's uninstaller.
- **Runtimes** (DirectX, Visual C++) are never removed: other programs may use them.

## Safety

- **unarc.dll only where needed.** FreeArc's old unarc.dll handles archives that need its built-in codecs. On Cities: Skylines' bonus content it corrupted a PDF in more than half the runs, even alone, while arc-unpack's own engine decoded the same block correctly every time; that archive only went to unarc.dll for its `4x4` block, which arc-unpack now decodes itself. Each unarc.dll job runs in its own working folder (hard links to the codec folder), because some codecs keep fixed-name temp files where they run.
- **Untrusted archive paths.** `..`, absolute paths, drive letters, alternate data streams and device names are refused.
- **No orphans.** Every codec process lives in a kill-on-exit job object.
- **No stuck dialogs.** Codec error dialogs are closed automatically and their text is reported.
- **No silent hangs.** A stall watchdog stops hung codecs.
- **Honest checksums.** Repacks that use a modified FreeArc checksum are detected. Their files are verified against the repack's MD5 list instead of producing false CRC errors.

## Command line

```powershell
arc-unpack install  <repack> <target>        # one-step install (asks for admin rights; --dry-run to preview)
arc-unpack script   <repack>                 # what the installer's script would do (changes nothing)
arc-unpack script   <repack> --disasm CurStepChanged   # its bytecode, for the curious
arc-unpack runtimes <repack> [--install]     # runtimes the game needs, and whether this PC has them
arc-unpack list     data-01.bin --files      # blocks, codec chains, sizes, files
arc-unpack extract-setup setup.exe -o tools  # unpack an Inno Setup 5.5 installer's files (never runs it)
arc-unpack import-arcini tools\tmp\arc.ini -o codecs.toml
arc-unpack check    *.bin -c codecs.toml     # does every method have a decoder?
arc-unpack test     *.bin -c codecs.toml     # decode and verify, write nothing
arc-unpack extract  *.bin -c codecs.toml -o D:\Games\Title
```

To measure a single codec: run `test` or `install` with `ARC_UNPACK_TEE_DIR=<folder>`, which saves every codec's input there, then `arc-unpack replay -c codecs.toml -m <method> <saved input>` runs that codec alone through the same engine and prints its time and the output's MD5. The logs show each codec's CPU time, including the programs it starts (a CLS proxy's decoder, unarc.dll's external codecs).

| Flag | Meaning |
|---|---|
| `--components a,b` | `install`: components to install (default: the installer's defaults plus downloaded optional parts). Optional parts left out aren't installed; without the base game's component, the parts are added to the game already in the target folder. |
| `--no-verify` | `install`: skip the MD5 check of the downloaded archives. |
| `--no-script` | `install`: don't run the installer's script; extract every archive into the target. |
| `--runtimes` | `install`: afterwards, install the runtimes the game needs that this PC is missing (downloads them; see Runtimes). |
| `--shortcuts` | `install`: create the installer's desktop shortcut and Start menu entry. |
| `--no-uninstaller` | `install`: don't add an uninstaller (see Uninstaller). |
| `-j, --jobs N` | Jobs at once. Light jobs fill in around heavy ones. |
| `--heavy-jobs N` | Memory-heavy jobs at once, machine-wide (default 2). The first always runs; the others start only while at least a quarter of RAM (and 8 GB) is free. |
| `--memory-limit GB` | `install`: keep RAM use to about this much (see *A RAM limit you set*). One memory-heavy job at a time. Slower. |
| `--writers N` | File writer threads per job. |
| `--stall-timeout S` | Stop a job after S seconds without progress (default 600, 0 = never). |
| `--overwrite`, `--skip-missing`, `--no-tune` | Replace existing files / extract what's decodable / leave priorities alone. |

## Codec configuration

`install` reads the codec setup from the installer itself. To write one by hand for `extract`/`test`:

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
```

Placeholders follow FreeArc's: `{options}`, `{X option Y}`, `{params}`, `{input}`, `{output}`, `{codec_dir}`. A `cls-<name>.dll` in `codec_dir` is used automatically. See [`codecs.example.toml`](../codecs.example.toml).

## Project layout

```
src/pascal.rs          Pascal Script (IFPS) bytecode interpreter
src/setup_script.rs    the Inno Setup / ISDone functions an install script calls; plan, components, shortcuts
src/inno.rs            read-only Inno Setup 5.5 Unicode reader (files, components, icons, compiled code)
src/install.rs         one-step install: script run, early extraction, MD5 checks
src/runtimes.rs        runtimes a game needs (DirectX, Visual C++, ...): detection, download, install
src/uninstall.rs       the uninstaller's list, the Settings > Apps entry, the removal
src/format.rs          FreeArc container: footer, control blocks, directory, varints
src/extract.rs         scheduling, splitting, writer pool, watchdog, path safety
src/stage.rs           stage runners: built-ins, external programs, CLS hosts
src/xtsplit.rs         xtool's stream cut into chunks for several xtool processes
src/memlimit.rs        the RAM limit: the decoders' memory settings scaled to it
src/config.rs          codecs.toml, method resolution, FreeArc option templates
src/guard.rs           job objects, dialog handling, cross-process locks, elevation, RAM checks
xtool-timer/           the xtool timer plugin (its own tiny crate)
src/events.rs          progress events (terminal and window are listeners)
src/bin/arc-unpack-gui.rs   the window (egui)
src/bin/arc-unpack-uninstall.rs   the uninstaller put in every game folder
src/bin/cls-host.rs    CLS DLL host (x86 + x64)
src/bin/unarc-host.rs  unarc.dll host (x86)
tests/synthetic.rs     builds FreeArc archives byte by byte and runs the real binary
bench/bench.ps1        the decode-engine benchmark
```
