<#
Benchmark: the installer's own engine vs arc-unpack, on the same archives and codecs.

  Baseline   - every archive extracted one after another through unarc.dll's FreeArcExtract,
               which is what repack installers (Inno Setup + ISDone) call internally.
  arc-unpack - the same archives in one `arc-unpack extract` run, at --heavy-jobs 1 and 2.

Archives are read once beforehand so every run starts from a warm file cache. CPU and RAM are
sampled every second. Each run's output is counted (files, bytes) to check the results agree.
Run from an elevated PowerShell (the repack codecs need administrator rights).

  .\bench.ps1 -Repack D:\repack -Archives data-02.bin,data-03.bin -Codecs D:\codecs -Work D:\benchwork
#>
param(
    [Parameter(Mandatory)] [string]$Repack,
    [Parameter(Mandatory)] [string[]]$Archives,
    [Parameter(Mandatory)] [string]$Codecs,          # codec folder with codecs.toml, unarc.dll, arc.ini
    [Parameter(Mandatory)] [string]$Work,            # scratch folder for outputs (emptied between runs)
    [string]$Dist = (Join-Path $PSScriptRoot '..\dist'),
    [string]$Results = (Join-Path $PSScriptRoot 'results.json'),
    # Which runs to do; others are kept from an existing results file.
    [string[]]$Runs = @('baseline', 'heavy1', 'heavy2')
)
# Keep $Work short: the 32-bit unarc.dll can't write paths longer than 260 characters, and
# repack contents can be deeply nested.
$ErrorActionPreference = 'Continue'
$Dist = (Resolve-Path $Dist).Path
$paths = $Archives | ForEach-Object { Join-Path $Repack $_ }

function Read-Warm([string[]]$files) {
    $buf = New-Object byte[] (16MB)
    foreach ($f in $files) { $s = [IO.File]::OpenRead($f); while ($s.Read($buf, 0, $buf.Length) -gt 0) {}; $s.Close() }
}

function Measure-Output([string]$dir) {
    $m = Get-ChildItem -LiteralPath $dir -Recurse -File -Force -ErrorAction SilentlyContinue | Measure-Object Length -Sum
    [pscustomobject]@{ files = [int]$m.Count; bytes = [int64]$m.Sum }
}

function Invoke-Measured([string]$name, [scriptblock]$body) {
    $out = Join-Path $Work $name
    if (Test-Path $out) { Remove-Item -LiteralPath $out -Recurse -Force }
    New-Item -ItemType Directory -Force $out | Out-Null
    Read-Warm $paths
    $csv = Join-Path $Work "$name.samples.csv"
    Remove-Item $csv -ErrorAction SilentlyContinue
    $sampler = Start-Job -ArgumentList $csv -ScriptBlock {
        param($f)
        while ($true) {
            # In use = total - available. "Available" includes the file cache, so cached archive
            # data doesn't count as used memory.
            $c = (Get-Counter '\Processor(_Total)\% Processor Time', '\Memory\Available MBytes').CounterSamples
            $cpu = $c[0].CookedValue
            $usedMB = [int]((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1MB - $c[1].CookedValue)
            "$cpu,$usedMB" | Add-Content -Path $f
        }
    }
    $sw = [Diagnostics.Stopwatch]::StartNew()
    & $body $out
    $secs = $sw.Elapsed.TotalSeconds
    Stop-Job $sampler; Remove-Job $sampler -Force
    $samples = @(Get-Content $csv -ErrorAction SilentlyContinue | ForEach-Object { $c, $m = $_ -split ','; [pscustomobject]@{ cpu = [double]$c; ram = [int]$m } })
    $result = Measure-Output $out
    $row = [pscustomobject]@{
        name        = $name
        seconds     = [math]::Round($secs, 1)
        files       = $result.files
        bytes       = $result.bytes
        avg_cpu_pct = if ($samples) { [math]::Round(($samples | Measure-Object cpu -Average).Average, 1) } else { $null }
        peak_ram_mb = if ($samples) { ($samples | Measure-Object ram -Maximum).Maximum } else { $null }
    }
    Write-Host ("{0,-28} {1,8:N1}s  files={2}  bytes={3:N0}  cpu~{4}%  peakRAM={5} MB" -f $row.name, $row.seconds, $row.files, $row.bytes, $row.avg_cpu_pct, $row.peak_ram_mb)
    Remove-Item -LiteralPath $out -Recurse -Force -ErrorAction SilentlyContinue
    $row
}

New-Item -ItemType Directory -Force $Work | Out-Null
$temp = Join-Path $Work 'tmp'; New-Item -ItemType Directory -Force $temp | Out-Null
$rows = @()
$script:baselineParts = @()

if ($Runs -contains 'baseline') { $rows += Invoke-Measured 'baseline-unarc-sequential' {
    param($out)
    Push-Location $Codecs   # unarc finds its external codecs relative to the current folder
    foreach ($a in $paths) {
        $name = [IO.Path]::GetFileName($a)
        $log = Join-Path $Work "unarc-$name.log"
        $t = [Diagnostics.Stopwatch]::StartNew()
        & "$Dist\unarc-host-x86.exe" "$Codecs\unarc.dll" x $a -o+ "-cfg$Codecs\arc.ini" "-dp$out" "-w$temp" *> $log
        $code = $LASTEXITCODE
        $script:baselineParts += [pscustomobject]@{ archive = $name; seconds = [math]::Round($t.Elapsed.TotalSeconds, 1); exit_code = $code }
        if ($code -ne 0) { Write-Warning "unarc failed on $a ($code), see $log" }
    }
    Pop-Location
} }

foreach ($heavy in 1, 2) {
    if ($Runs -notcontains "heavy$heavy") { continue }
    $rows += Invoke-Measured "arc-unpack-heavy$heavy" {
        param($out)
        & "$Dist\arc-unpack.exe" extract @paths -c "$Codecs\codecs.toml" -o $out -j 3 --heavy-jobs $heavy --temp-dir $temp *> (Join-Path $Work "arc-unpack-heavy$heavy.log")
        if ($LASTEXITCODE -ne 0) { Write-Warning "arc-unpack exited with $LASTEXITCODE" }
    }
}

# Partial re-runs replace only the runs they did.
$parts = $script:baselineParts
if (Test-Path $Results) {
    $old = Get-Content $Results -Raw | ConvertFrom-Json
    $names = $rows | ForEach-Object name
    $rows = @($old.runs | Where-Object { $names -notcontains $_.name }) + $rows
    if ($Runs -notcontains 'baseline') { $parts = $old.baseline_per_archive }
}

$cpuInfo = Get-CimInstance Win32_Processor | Select-Object -First 1
$machine = [pscustomobject]@{
    cpu      = $cpuInfo.Name.Trim()
    threads  = $cpuInfo.NumberOfLogicalProcessors
    ram_gb   = [math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB)
    os       = (Get-CimInstance Win32_OperatingSystem).Caption
    archives = $Archives
    packed_bytes = ($paths | ForEach-Object { (Get-Item -LiteralPath $_).Length } | Measure-Object -Sum).Sum
}
[pscustomobject]@{ machine = $machine; runs = $rows; baseline_per_archive = $parts; date = (Get-Date -Format s) } |
    ConvertTo-Json -Depth 4 | Set-Content -Path $Results -Encoding UTF8
Write-Host "results written to $Results"
