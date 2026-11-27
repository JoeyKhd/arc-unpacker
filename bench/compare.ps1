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
                if ($c -ne $treeCpu) { $treeCpu = $c; $treeSince = $t }
                elseif (($t - $treeSince) -ge 600) {
                    $r.window_text = [AbHelp]::WindowText([int[]]@($tree | ForEach-Object { [int]$_.ProcessId }))
                    $tInstall = $t; $r.note = 'stalled: no CPU use for 10 min'
                    Stop-Tree $setup.Id; break
                }
            }
            if ($t -gt 7200) { $tInstall = $t; $r.note = 'timed out after 2 h'; Stop-Tree $setup.Id; break }
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
    $next = 0; $note = ''
    while (-not $p.WaitForExit(500)) {
        $t = $sw.Elapsed.TotalSeconds
        if ($t -ge $next) { Add-Content $samples (Get-Sample $t); $next = $t + 5 }
        if ($t -gt 7200) { Stop-Tree $p.Id; $note = 'timed out after 2 h'; break }
    }
    $r = [ordered]@{ total_s = [math]::Round($sw.Elapsed.TotalSeconds, 1); exit = $p.ExitCode; note = $note }
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

function Compare-Manifests($origPath, $arcPath) {
    $a = Read-Manifest $origPath; $b = Read-Manifest $arcPath
    if ($a.Count -eq 0) { return $null }
    $same = 0; $differ = @(); $onlyOrig = @(); $onlyArc = @()
    foreach ($k in $a.Keys) {
        if ($k -eq 'unins000.exe' -or $k -eq 'unins000.dat') { continue }
        if (-not $b.ContainsKey($k)) { $onlyOrig += $k } elseif ($a[$k][1] -eq $b[$k][1]) { $same++ } else { $differ += $k }
    }
    foreach ($k in $b.Keys) { if (-not $a.ContainsKey($k)) { $onlyArc += $k } }
    [ordered]@{
        identical = $same; differ = $differ.Count; only_original = $onlyOrig.Count; only_arc_unpack = $onlyArc.Count
        examples = (@($differ | Select-Object -First 3) + @($onlyOrig | Select-Object -First 3 | ForEach-Object { "only original: $_" }) + @($onlyArc | Select-Object -First 3 | ForEach-Object { "only arc-unpack: $_" })) -join '; '
    }
}

function Get-ManifestTotals($path) {
    $m = Read-Manifest $path
    $files = 0; $bytes = [int64]0
    foreach ($k in $m.Keys) { if ($k -eq 'unins000.exe' -or $k -eq 'unins000.dat') { continue }; $files++; $bytes += $m[$k][0] }
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
        $res.compare = Compare-Manifests $origManifest $arcManifest
        Remove-Item -LiteralPath $targetA -Recurse -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath "$targetA.arc-unpack-staging" -Recurse -Force -ErrorAction SilentlyContinue
        $res | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $Out "result-$tag.json") -Encoding UTF8
        $same = if ($res.compare) { if ($res.compare.differ + $res.compare.only_original + $res.compare.only_arc_unpack -eq 0) { 'same files as the original' } else { 'FILES DIFFER from the original' } } else { 'no original fingerprint to compare with' }
        Write-Host ("  {0} s, exit {1}, {2}" -f $res.arc_unpack.total_s, $res.arc_unpack.exit, $same)
    }
}
if ((Test-Path $Work) -and -not (Get-ChildItem $Work -Force)) { Remove-Item $Work -Force }
Write-Host "Results in $Out. Add them to the ledger: python bench\ledger.py add `"$Out\result-*.json`""
