# Times a repack's own installer against arc-unpack and checks both produce the same files.
# Run from an administrator PowerShell (repack installers and their codecs need it):
#
#   .\bench\compare.ps1 -Repacks 'D:\Downloads\Game A [FitGirl Repack]','D:\Downloads\Game B [FitGirl Repack]'
#   .\bench\compare.ps1 -Repacks ... -SkipOriginal -Runs 3      # a new arc-unpack version only
#   python bench\ledger.py add bench\results\*.json             # then add the runs to the ledger
#
# For each repack, one at a time:
#   1. Flush the file cache. Run the repack's setup.exe silently with the same parts arc-unpack
#      installs (no DirectX/VC++ installers). Timed until its post-install check (QuickSFV) starts,
#      then QuickSFV is timed. Setup is stopped there, before later post-install entries run.
#   2. Fingerprint the folder (MD5 of every file), keep the fingerprint, uninstall, delete.
#   3. Flush the file cache. Run `arc-unpack install` with default settings and the same parts.
#   4. Fingerprint, compare with the original installer's fingerprint, delete.
# Only <Work>\<n>-o and <Work>\<n>-a are created and deleted. bench\results is not committed:
# it holds game names and paths.
param(
    [Parameter(Mandatory = $true)][string[]]$Repacks,
    [string]$Parts,
    [string]$Exe = (Join-Path $PSScriptRoot '..\dist\arc-unpack.exe'),
    [string]$Work = 'C:\ab',
    [string]$Out = (Join-Path $PSScriptRoot 'results'),
    [switch]$SkipOriginal,
    [int]$Runs = 1
)
$ErrorActionPreference = 'Continue'
if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Error 'Run this from an administrator PowerShell.'; exit 1
}
$Exe = (Resolve-Path $Exe).Path
New-Item -ItemType Directory -Force $Out, (Join-Path $Out 'manifests'), $Work | Out-Null
Add-Type -Path (Join-Path $PSScriptRoot 'AbHelp.cs') -ReferencedAssemblies System.Core
$hosts = 'C:\Windows\System32\drivers\etc\hosts'
$shell = New-Object -ComObject WScript.Shell

function Get-Sample($t) {
    $cpu = (Get-CimInstance Win32_PerfFormattedData_PerfOS_Processor -Filter "Name='_Total'").PercentProcessorTime
    $os = Get-CimInstance Win32_OperatingSystem
    '{0},{1},{2}' -f [math]::Round($t, 1), $cpu, [math]::Round(($os.TotalVisibleMemorySize - $os.FreePhysicalMemory) / 1MB, 2)
}

function Get-SampleStats($file) {
    $rows = @(Import-Csv $file)
    if ($rows.Count -lt 2) { return @{ avg_cpu_pct = $null; ram_rise_gb = $null } }
    $cpu = ($rows | Select-Object -Skip 1 | Measure-Object -Property cpu_pct -Average).Average
    $peak = ($rows | Measure-Object -Property used_gb -Maximum).Maximum
    @{ avg_cpu_pct = [math]::Round($cpu, 1); ram_rise_gb = [math]::Round($peak - [double]$rows[0].used_gb, 1) }
}

function Get-Tree([int]$rootPid) {
    $all = @(Get-CimInstance Win32_Process -Property ProcessId, ParentProcessId, Name, KernelModeTime, UserModeTime)
    $ids = [System.Collections.Generic.HashSet[int]]::new()
    [void]$ids.Add($rootPid)
    do {
        $added = $false
        foreach ($p in $all) { if ($ids.Contains([int]$p.ParentProcessId) -and -not $ids.Contains([int]$p.ProcessId)) { [void]$ids.Add([int]$p.ProcessId); $added = $true } }
    } while ($added)
    @($all | Where-Object { $ids.Contains([int]$_.ProcessId) })
}

# Bytes read and written by the tree's processes since the last call (processes that started
# since count in full). $seen: pid -> last count, kept by the caller.
function Get-TreeIo([int]$rootPid, $seen) {
    $ids = @{}; foreach ($p in Get-Tree $rootPid) { $ids[[int]$p.ProcessId] = $true }
    $delta = 0
    foreach ($p in Get-CimInstance Win32_Process -Property ProcessId, ReadTransferCount, WriteTransferCount) {
        $id = [int]$p.ProcessId
        if (-not $ids.ContainsKey($id)) { continue }
        $n = [double]$p.ReadTransferCount + [double]$p.WriteTransferCount
        $before = if ($seen.ContainsKey($id)) { $seen[$id] } else { 0 }
        if ($n -gt $before) { $delta += $n - $before }
        $seen[$id] = $n
    }
    $delta
}

# For a hung or timed-out install: what still ran (with command lines), their windows' text and
# the end of the log, into <tag>-<kind>-hang.txt. Returns the file's name.
function Save-HangReport($tag, $kind, [int]$rootPid, $logFile) {
    $file = Join-Path $Out "$tag-$kind-hang.txt"
    $tree = Get-Tree $rootPid
    $lines = @("$kind install of $tag stopped as hung at $(Get-Date -Format s)", '', 'Processes still running:')
    foreach ($p in $tree) {
        $c = Get-CimInstance Win32_Process -Filter "ProcessId=$($p.ProcessId)" -ErrorAction SilentlyContinue
        $cpu = [math]::Round(([double]$p.KernelModeTime + [double]$p.UserModeTime) / 1e7, 1)
        $io = if ($c) { [math]::Round(([double]$c.ReadTransferCount + [double]$c.WriteTransferCount) / 1MB, 1) } else { '?' }
        $lines += "  $($p.Name) pid $($p.ProcessId) parent $($p.ParentProcessId), cpu $cpu s, io $io MB, started $($c.CreationDate)"
        if ($c -and $c.CommandLine) { $lines += "    $($c.CommandLine)" }
    }
    $w = [AbHelp]::WindowText([int[]]@($tree | ForEach-Object { [int]$_.ProcessId }))
    if ($w) { $lines += '', 'Window text:', "  $w" }
    if ($logFile -and (Test-Path -LiteralPath $logFile)) { $lines += '', "End of $(Split-Path $logFile -Leaf):"; $lines += @(Get-Content -LiteralPath $logFile -Tail 40 | ForEach-Object { "  $_" }) }
    Set-Content -LiteralPath $file $lines -Encoding UTF8
    Split-Path $file -Leaf
}

function Stop-Tree([int]$rootPid) {
    $tree = Get-Tree $rootPid
    [array]::Reverse($tree)
    foreach ($p in $tree) { Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue }
}

# The repack's installer: setup.exe, or (older repacks) setup-multi6.exe and the like.
function Get-SetupExe($repack) {
    $plain = Join-Path $repack 'setup.exe'
    if (Test-Path -LiteralPath $plain) { return $plain }
    $other = Get-ChildItem -LiteralPath $repack -Filter 'setup*.exe' -File | Sort-Object Name | Select-Object -First 1
    if ($other) { return $other.FullName }
    $plain
}

function Stop-SetupOnly([int]$setupPid) {
    # (Inno's second process is named after the installer: setup.tmp, setup-multi6.tmp, ...)
    Get-CimInstance Win32_Process -Filter "Name LIKE '%.tmp'" | Where-Object { $_.ParentProcessId -eq $setupPid } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    Stop-Process -Id $setupPid -Force -ErrorAction SilentlyContinue
}

function Read-Shared($path) {
    try {
        $fs = [IO.File]::Open($path, 'Open', 'Read', 'ReadWrite')
        $sr = New-Object IO.StreamReader($fs)
        $t = $sr.ReadToEnd(); $sr.Close(); return $t
    } catch { return $null }
}

function Clear-Cache { $r = [AbHelp]::Purge(); Start-Sleep -Seconds 20; $r }

function Get-ShortcutSnapshot {
    $items = @()
    foreach ($d in 'CommonDesktopDirectory', 'DesktopDirectory', 'CommonPrograms', 'Programs') {
        $p = [Environment]::GetFolderPath($d)
        if ($p -and (Test-Path -LiteralPath $p)) { $items += @(Get-ChildItem -LiteralPath $p -Recurse -Force -ErrorAction SilentlyContinue | ForEach-Object FullName) }
    }
    , $items
}

# The parts arc-unpack installs by default, without redistributable installers.
function Get-DefaultParts($repack) {
    $lines = & $Exe script $repack 2>$null | ForEach-Object { "$_" }
    $parts = foreach ($l in $lines) {
        if ($l -match '^\s+\[x\]\s+(\S+)\s*(.*)$') {
            $n = $Matches[1]; $d = $Matches[2]
            if ($n -match '^(directx|dx|vc_|vcredist|redist|physx|dotnet|netfw|xna|openal|oal)' -or $d -match 'DirectX|Redist|C\+\+|PhysX|\.NET|XNA|OpenAL') { continue }
            $n
        }
    }
    @($parts) -join ','
}

function Get-NeededGB($repack, $parts, $target) {
    $d = & $Exe install $repack $target --dry-run --no-elevate --components $parts 2>&1 | ForEach-Object { "$_" }
    $line = $d | Where-Object { $_ -match 'would install up to ([\d.]+) (GB|MB|KB)' } | Select-Object -First 1
    if (-not $line) { return $null }
    $null = $line -match 'would install up to ([\d.]+) (GB|MB|KB)'
    $v = [double]$Matches[1]
    switch ($Matches[2]) { 'GB' { $v } 'MB' { $v / 1000 } default { $v / 1e6 } }
}

function Invoke-Original($tag, $repack, $target, $parts) {
    $log = Join-Path $Out "$tag-inno.log"
    $samples = Join-Path $Out "$tag-orig-samples.csv"
    Set-Content $samples 't,cpu_pct,used_gb'
    $r = [ordered]@{ install_s = $null; verify_s = $null; total_s = $null; note = ''; window_text = ''; log_succeeded = $false }
    $psi = New-Object Diagnostics.ProcessStartInfo
    $psi.FileName = Get-SetupExe $repack
    $psi.WorkingDirectory = $repack
    $psi.Arguments = "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP- /LANG=en /COMPONENTS=`"$parts`" /DIR=`"$target`" /LOG=`"$log`""
    $psi.UseShellExecute = $false
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $setup = [Diagnostics.Process]::Start($psi)
    $tInstall = $null; $tVerify = $null
    $nextSample = 0; $nextLog = 0; $nextTree = 15; $treeCpu = -1; $treeSince = 0; $qCpu = -1; $qSince = 0
    while ($true) {
        $t = $sw.Elapsed.TotalSeconds
        if ($t -ge $nextSample) { Add-Content $samples (Get-Sample $t); $nextSample = $t + 5 }
        $q = @(Get-Process -Name QuickSFV -ErrorAction SilentlyContinue)
        if (-not $tInstall) {
            if ($q.Count -gt 0) { $tInstall = $t; Stop-SetupOnly $setup.Id; continue }
            if ($t -ge $nextLog) {
                $nextLog = $t + 1
                $txt = Read-Shared $log
                if ($txt -and $txt.Contains('-- Run entry --')) {
                    $after = $txt.Substring($txt.IndexOf('-- Run entry --'))
                    if ($after -match 'Filename:\s*([^\r\n]+)' -and $Matches[1] -notmatch 'QuickSFV') {
                        $tInstall = $t; $r.note = 'the first post-install entry was not QuickSFV; stopped there'
                        Stop-Tree $setup.Id; break
                    }
                }
            }
            if ($setup.HasExited) { $tInstall = $t; $r.note = "setup exited (code $($setup.ExitCode)) without starting QuickSFV"; break }
            if ($t -ge $nextTree) {
                $nextTree = $t + 15
                $tree = Get-Tree $setup.Id
                # The decoders' CPU only: Setup's own window keeps ticking while a decoder hangs.
                # (While no decoder runs, Setup's own CPU counts instead.)
                $workers = @($tree | Where-Object { $_.Name -notmatch '^setup(\.tmp|\.exe)?$' -and $_.Name -notmatch '\.tmp$' })
                $counted = if ($workers.Count -gt 0) { $workers } else { $tree }
                $c = ($counted | ForEach-Object { [double]$_.KernelModeTime + [double]$_.UserModeTime } | Measure-Object -Sum).Sum
                # An unpacker's error box ("An error occurred while decompressing: Archive header is
                # corrupt! Unarc.dll returned an error code: -14"): the install has failed, the box waits
                # for a click /SUPPRESSMSGBOXES doesn't give, and Setup's own CPU keeps the idle test
                # below from firing (The Last of Us Part I, 2026-11-28: 1 h 50 min on the box).
                $wt = [AbHelp]::WindowText([int[]]@($tree | ForEach-Object { [int]$_.ProcessId }))
                if ($wt -match 'error occurred while decompressing|returned an error code|archive is corrupt|CRC check failed|not enough (free )?(disk )?space') {
                    $one = ($wt -replace '\s+', ' ').Trim()
                    $r.window_text = $wt
                    $r.hang_report = Save-HangReport $tag 'original' $setup.Id $log
                    $tInstall = $t; $r.note = 'failed: the installer showed an error: ' + $one.Substring(0, [Math]::Min(200, $one.Length))
                    Stop-Tree $setup.Id; break
                }
                if ($c -ne $treeCpu) { $treeCpu = $c; $treeSince = $t }
                elseif (($t - $treeSince) -ge 600) {
                    $r.window_text = [AbHelp]::WindowText([int[]]@($tree | ForEach-Object { [int]$_.ProcessId }))
                    $r.hang_report = Save-HangReport $tag 'original' $setup.Id $log
                    $tInstall = $t; $r.note = 'hung: no CPU use for 10 min'
                    Stop-Tree $setup.Id; break
                }
            }
            if ($t -gt 7200) { $r.hang_report = Save-HangReport $tag 'original' $setup.Id $log; $tInstall = $t; $r.note = 'hung: timed out after 2 h'; Stop-Tree $setup.Id; break }
        } else {
            if ($q.Count -eq 0) { $tVerify = $t; $r.note = 'QuickSFV finished and closed itself'; break }
            $c = ($q | ForEach-Object { $_.TotalProcessorTime.TotalSeconds } | Measure-Object -Sum).Sum
            if ($c -ne $qCpu) { $qCpu = $c; $qSince = $t }
            elseif (($t - $qSince) -ge 15) {
                $tVerify = $qSince
                $r.window_text = [AbHelp]::WindowText([int[]]@($q | ForEach-Object { $_.Id }))
                $r.note = 'QuickSFV stayed open after finishing'; break
            }
            if (($t - $tInstall) -gt 3600) { $tVerify = $t; $r.note = 'QuickSFV still busy after 60 min'; break }
        }
        Start-Sleep -Milliseconds 500
    }
    Get-Process -Name QuickSFV -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    if (-not $setup.HasExited) { Stop-Tree $setup.Id }
    $txt = Read-Shared $log
    $r.log_succeeded = [bool]($txt -and $txt.Contains('Installation process succeeded.'))
    $r.install_s = [math]::Round($tInstall, 1)
    if ($tVerify) { $r.verify_s = [math]::Round($tVerify - $tInstall, 1); $r.total_s = [math]::Round($tVerify, 1) } else { $r.total_s = $r.install_s }
    $s = Get-SampleStats $samples
    $r.avg_cpu_pct = $s.avg_cpu_pct; $r.ram_rise_gb = $s.ram_rise_gb
    $r
}

function Remove-Original($target, $snapshot) {
    $log = @()
    $unins = Join-Path $target 'unins000.exe'
    if (Test-Path -LiteralPath $unins) {
        Start-Process -FilePath $unins -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART' -Wait
        $u = [Diagnostics.Stopwatch]::StartNew()
        while ((Get-Process | Where-Object { $_.Name -like '_iu*' }) -and $u.Elapsed.TotalMinutes -lt 10) { Start-Sleep -Seconds 2 }
        $log += 'uninstaller ran'
    }
    if (Test-Path -LiteralPath $target) { Remove-Item -LiteralPath $target -Recurse -Force -ErrorAction SilentlyContinue; $log += 'removed folder' }
    foreach ($k in 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall', 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall') {
        Get-ChildItem $k -ErrorAction SilentlyContinue | Where-Object { ($_.GetValue('InstallLocation') -as [string]) -like "$target*" } | ForEach-Object { Remove-Item $_.PSPath -Recurse -Force; $log += 'removed uninstall key' }
    }
    foreach ($k in 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers', 'HKCU:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers') {
        $item = Get-Item $k -ErrorAction SilentlyContinue
        if ($item) { foreach ($v in $item.GetValueNames()) { if ($v -like "$target*") { Remove-ItemProperty -Path $k -Name $v -Force; $log += 'removed compatibility flag' } } }
    }
    $now = Get-ShortcutSnapshot
    foreach ($p in $now) {
        if ($snapshot -contains $p) { continue }
        if ($p -like '*.lnk' -and (Test-Path -LiteralPath $p) -and $shell.CreateShortcut($p).TargetPath -like "$Work\*") { Remove-Item -LiteralPath $p -Force; $log += 'removed shortcut' }
    }
    foreach ($p in ($now | Sort-Object Length -Descending)) {
        if ($snapshot -contains $p) { continue }
        if ((Test-Path -LiteralPath $p -PathType Container) -and -not (Get-ChildItem -LiteralPath $p -Force -ErrorAction SilentlyContinue)) { Remove-Item -LiteralPath $p -Force; $log += 'removed empty menu folder' }
    }
    $log -join '; '
}

function Invoke-Arc($tag, $repack, $target, $parts) {
    $o = Join-Path $Out "$tag-arc.out"; $e = Join-Path $Out "$tag-arc.err"
    $samples = Join-Path $Out "$tag-arc-samples.csv"
    Set-Content $samples 't,cpu_pct,used_gb'
    $psi = New-Object Diagnostics.ProcessStartInfo
    $psi.FileName = $env:ComSpec
    $psi.Arguments = "/d /c `"`"$Exe`" install `"$repack`" `"$target`" --no-elevate --components $parts > `"$o`" 2> `"$e`"`""
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $p = [Diagnostics.Process]::Start($psi)
    $next = 0; $note = ''; $hang = $null
    # Hung: the tree read and wrote under 4 MB in 15 min (arc-unpack's own check, 10 min by
    # default, normally stops a stuck step first and says which). Or 2 h in all.
    $seen = @{}; $nextIo = 30; $ioSince = 0; $ioSum = 0
    while (-not $p.WaitForExit(500)) {
        $t = $sw.Elapsed.TotalSeconds
        if ($t -ge $next) { Add-Content $samples (Get-Sample $t); $next = $t + 5 }
        if ($t -ge $nextIo) {
            $nextIo = $t + 30
            $ioSum += Get-TreeIo $p.Id $seen
            if ($ioSum -ge 4MB) { $ioSum = 0; $ioSince = $t }
            elseif (($t - $ioSince) -ge 900) { $hang = Save-HangReport $tag 'arc' $p.Id $e; Stop-Tree $p.Id; $note = 'hung: no disk activity for 15 min'; break }
        }
        if ($t -gt 7200) { $hang = Save-HangReport $tag 'arc' $p.Id $e; Stop-Tree $p.Id; $note = 'hung: timed out after 2 h'; break }
    }
    # arc-unpack stopped a stuck step itself: count it as a hang too.
    if (-not $note -and (Select-String -LiteralPath $e -Pattern 'made no progress for' -SimpleMatch -Quiet -ErrorAction SilentlyContinue)) { $note = 'hung: arc-unpack stopped a step that made no progress' }
    $r = [ordered]@{ total_s = [math]::Round($sw.Elapsed.TotalSeconds, 1); exit = $p.ExitCode; note = $note; hang_report = $hang }
    $r.summary = @(Get-Content -LiteralPath $o -ErrorAction SilentlyContinue) -join ' | '
    if ($p.ExitCode -ne 0) { $r.error_tail = @(Get-Content -LiteralPath $e -Tail 8 -ErrorAction SilentlyContinue) -join ' | ' }
    $s = Get-SampleStats $samples
    $r.avg_cpu_pct = $s.avg_cpu_pct; $r.ram_rise_gb = $s.ram_rise_gb
    $r
}

function Read-Manifest($path) {
    $m = @{}
    if ($path -and (Test-Path -LiteralPath $path)) { foreach ($l in [IO.File]::ReadAllLines($path)) { $p = $l.Split("`t"); $m[$p[0]] = @([int64]$p[1], $p[2]) } }
    $m
}

# The installers' own uninstallers (Inno's unins000.*, arc-unpack's uninstall.*) aren't game files.
function Test-Uninstaller($k) { $k -in 'unins000.exe', 'unins000.dat', 'uninstall.exe', 'uninstall.dat' }

# Scripts write the install folder into some files (Assetto Corsa: SetIniString puts
# '{app}\AssettoCorsa.exe' into SmartSteamEmu.ini). The original installs into <Work>\<n>-o and
# arc-unpack into <Work>\<n>-a, so such a file differs by that one name. It counts as the same when
# the arc-unpack copy, with its folder replaced by the original's (<n> as in that run, which an
# older manifest may have had at another batch position), hashes like the original's.
function Test-SameButFolder($file, $targetA, $wantMd5) {
    $fi = Get-Item -LiteralPath $file -ErrorAction SilentlyContinue
    if (-not $fi -or $fi.Length -gt 4MB) { return $false }
    $latin = [Text.Encoding]::GetEncoding(28591)
    $text = $latin.GetString([IO.File]::ReadAllBytes($file))
    if ($text.IndexOf($targetA, [StringComparison]::OrdinalIgnoreCase) -lt 0) { return $false }
    $md5 = [Security.Cryptography.MD5]::Create()
    $leaf = Split-Path $targetA -Leaf
    $base = Split-Path $targetA -Parent
    $n = if ($leaf -match '^(\d+)-a$') { [int]$Matches[1] } else { 0 }
    $tries = @($n) + (1..60 | Where-Object { $_ -ne $n })
    foreach ($i in $tries) {
        $orig = Join-Path $base "$i-o"
        $swapped = [regex]::Replace($text, [regex]::Escape($targetA), { param($m) $orig }, 'IgnoreCase')
        $hash = ($md5.ComputeHash($latin.GetBytes($swapped)) | ForEach-Object { $_.ToString('x2') }) -join ''
        if ($hash -eq $wantMd5.ToLower()) { return $true }
    }
    $false
}

function Compare-Manifests($origPath, $arcPath, $targetA) {
    $a = Read-Manifest $origPath; $b = Read-Manifest $arcPath
    if ($a.Count -eq 0) { return $null }
    $same = 0; $folderOnly = 0; $differ = @(); $onlyOrig = @(); $onlyArc = @()
    foreach ($k in $a.Keys) {
        if (Test-Uninstaller $k) { continue }
        if (-not $b.ContainsKey($k)) { $onlyOrig += $k }
        elseif ($a[$k][1] -eq $b[$k][1]) { $same++ }
        elseif ($targetA -and (Test-SameButFolder (Join-Path $targetA $k) $targetA $a[$k][1])) { $same++; $folderOnly++ }
        else { $differ += $k }
    }
    foreach ($k in $b.Keys) { if (-not $a.ContainsKey($k) -and -not (Test-Uninstaller $k)) { $onlyArc += $k } }
    [ordered]@{
        identical = $same; differ = $differ.Count; only_original = $onlyOrig.Count; only_arc_unpack = $onlyArc.Count
        # (Counted in identical: files that differ only by the install folder's name.)
        install_folder_only = $folderOnly
        examples = (@($differ | Select-Object -First 3) + @($onlyOrig | Select-Object -First 3 | ForEach-Object { "only original: $_" }) + @($onlyArc | Select-Object -First 3 | ForEach-Object { "only arc-unpack: $_" })) -join '; '
    }
}

function Get-ManifestTotals($path) {
    $m = Read-Manifest $path
    $files = 0; $bytes = [int64]0
    foreach ($k in $m.Keys) { if (Test-Uninstaller $k) { continue }; $files++; $bytes += $m[$k][0] }
    @{ files = $files; bytes = $bytes }
}

# ---------------------------------------------------------------- main
$cpuInfo = Get-CimInstance Win32_Processor | Select-Object -First 1
$diskModel = $null
try { $diskModel = (Get-Disk -Number (Get-Partition -DriveLetter $Work.Substring(0, 1)).DiskNumber).FriendlyName } catch { }
$machine = [ordered]@{
    cpu = $cpuInfo.Name.Trim(); threads = [int]$cpuInfo.NumberOfLogicalProcessors
    ram_gb = [math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB)
    disk = $diskModel
    os = (Get-CimInstance Win32_OperatingSystem).Caption
    timer_tweak = [bool](Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\kernel' -Name GlobalTimerResolutionRequests -ErrorAction SilentlyContinue).GlobalTimerResolutionRequests
}
$version = (& $Exe --version) -replace '^arc-unpack\s+', ''
$commit = $null
try {
    $commit = (& git -C (Join-Path $PSScriptRoot '..') rev-parse --short HEAD 2>$null)
    # Only changes to what goes into the binary count.
    if ($commit -and (& git -C (Join-Path $PSScriptRoot '..') status --porcelain --untracked-files=no -- src Cargo.toml Cargo.lock build.rs build.ps1 xtool-timer 2>$null)) { $commit += '+changes' }
} catch { }
$n = 0
foreach ($repack in $Repacks) {
    $repack = (Resolve-Path -LiteralPath $repack).Path
    $n++
    $folder = Split-Path $repack -Leaf
    $name = $folder -replace '\s*\[FitGirl Repack\]\s*$', ''
    $safe = ($folder -replace '[^\w\-]+', '_').Trim('_')
    $targetO = Join-Path $Work "$n-o"; $targetA = Join-Path $Work "$n-a"
    if ((Test-Path -LiteralPath $targetO) -or (Test-Path -LiteralPath $targetA)) { Write-Warning "$targetO or $targetA already exists; skipping $name"; continue }
    # (PowerShell names ignore case: this must not be called $parts, or it would overwrite -Parts.)
    $useParts = if ($Parts) { $Parts } else { Get-DefaultParts $repack }
    if (-not $useParts) { Write-Warning "arc-unpack can't plan $name; skipping"; continue }
    $need = Get-NeededGB $repack $useParts $targetA
    $free = (Get-PSDrive $Work.Substring(0, 1)).Free / 1GB
    if ($need -and $free -lt $need + 8) { Write-Warning ("{0}: needs {1:N1} GB, {2:N1} GB free; skipping" -f $name, $need, $free); continue }
    $origManifest = Join-Path $Out "manifests\$safe.original.tsv"

    for ($run = 1; $run -le $Runs; $run++) {
        $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
        $tag = "$safe-$stamp"
        $res = [ordered]@{
            name = $name; repack_path = $repack; date = (Get-Date -Format s); machine = $machine; arc_unpack_version = $version; arc_unpack_commit = $commit
            components = $useParts; needed_gb = $need
            repack = [ordered]@{ archives = @(Get-ChildItem -LiteralPath $repack -Filter '*.bin' -File).Count; packed_bytes = (Get-ChildItem -LiteralPath $repack -Filter '*.bin' -File | Measure-Object Length -Sum).Sum }
            conditions = [ordered]@{ cache_flushed = $true; qbittorrent_running = [bool](Get-Process qbittorrent -ErrorAction SilentlyContinue) }
        }
        if (-not $SkipOriginal -and $run -eq 1) {
            Write-Host "$name : original installer"
            Copy-Item -LiteralPath $hosts (Join-Path $Out "$tag-hosts.backup") -Force
            $hostsHash = (Get-FileHash -LiteralPath $hosts).Hash
            $snapshot = Get-ShortcutSnapshot
            $tempBefore = @(Get-ChildItem $env:TEMP -Directory -Filter 'is-*.tmp' -ErrorAction SilentlyContinue | ForEach-Object FullName)
            [void](Clear-Cache)
            $res.original = Invoke-Original $tag $repack $targetO $useParts
            if ((Get-FileHash -LiteralPath $hosts).Hash -ne $hostsHash) { Copy-Item -LiteralPath (Join-Path $Out "$tag-hosts.backup") $hosts -Force; $res.original.note += '; it changed the hosts file (restored)' }
            try { [void][AbHelp]::Manifest($targetO, $origManifest) } catch { $res.original.manifest_error = $_.Exception.Message }
            $tot = Get-ManifestTotals $origManifest
            $res.original.files = $tot.files; $res.original.bytes = $tot.bytes
            $res.original.cleanup = Remove-Original $targetO $snapshot
            Get-ChildItem $env:TEMP -Directory -Filter 'is-*.tmp' -ErrorAction SilentlyContinue | Where-Object { $tempBefore -notcontains $_.FullName } | ForEach-Object { Remove-Item -LiteralPath $_.FullName -Recurse -Force -ErrorAction SilentlyContinue }
            Write-Host ("  {0} s (install {1} s, check {2} s)" -f $res.original.total_s, $res.original.install_s, $res.original.verify_s)
        }
        Write-Host "$name : arc-unpack $version (run $run of $Runs)"
        [void](Clear-Cache)
        $res.arc_unpack = Invoke-Arc $tag $repack $targetA $useParts
        $arcManifest = Join-Path $Out "$tag-arc.tsv"
        try { [void][AbHelp]::Manifest($targetA, $arcManifest) } catch { $res.arc_unpack.manifest_error = $_.Exception.Message }
        $tot = Get-ManifestTotals $arcManifest
        $res.arc_unpack.files = $tot.files; $res.arc_unpack.bytes = $tot.bytes
        $res.compare = Compare-Manifests $origManifest $arcManifest $targetA
        # Clean up with arc-unpack's own uninstaller (which tests it), then delete what's left.
        $un = Join-Path $targetA 'uninstall.exe'
        if (Test-Path -LiteralPath $un) {
            $u = Start-Process -FilePath $un -ArgumentList '/S' -PassThru -Wait
            Start-Sleep -Seconds 4
            $left = @(Get-ChildItem -LiteralPath $targetA -Recurse -File -Force -ErrorAction SilentlyContinue)
            $res.arc_unpack.uninstall = if (-not (Test-Path -LiteralPath $targetA)) { 'removed everything' } else { "exit $($u.ExitCode), left $($left.Count) files: $((@($left | Select-Object -First 3 | ForEach-Object { $_.FullName.Substring($targetA.Length + 1) })) -join ', ')" }
        } else {
            $res.arc_unpack.uninstall = 'no uninstaller'
        }
        Remove-Item -LiteralPath $targetA -Recurse -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath "$targetA.arc-unpack-staging" -Recurse -Force -ErrorAction SilentlyContinue
        $res | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $Out "result-$tag.json") -Encoding UTF8
        $same = if ($res.compare) { if ($res.compare.differ + $res.compare.only_original + $res.compare.only_arc_unpack -eq 0) { 'same files as the original' } else { 'FILES DIFFER from the original' } } else { 'no original fingerprint to compare with' }
        if ($res.compare -and $res.compare.install_folder_only -gt 0) { $same += " ($($res.compare.install_folder_only) differ only by the install folder's name, which the script writes into them)" }
        Write-Host ("  {0} s, exit {1}, {2}" -f $res.arc_unpack.total_s, $res.arc_unpack.exit, $same)
    }
}
if ((Test-Path $Work) -and -not (Get-ChildItem $Work -Force)) { Remove-Item $Work -Force }
Write-Host "Results in $Out. Add them to the ledger: python bench\ledger.py add `"$Out\result-*.json`""
