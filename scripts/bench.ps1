# Measures Gitust against the plan's performance budget on Windows.
#   pwsh scripts/bench.ps1 -Repo C:\path\to\repo [-Exe target\release\gitust.exe] [-Runs 5]
# Uses an isolated data dir so your real settings and recents are untouched.
param(
    [Parameter(Mandatory = $true)][string]$Repo,
    [string]$Exe = "target\release\gitust.exe",
    [int]$Runs = 5,
    [int]$IdleSeconds = 10
)
$ErrorActionPreference = "Stop"
$exePath = (Resolve-Path $Exe).Path
$data = Join-Path ([IO.Path]::GetTempPath()) "gitust-bench-$PID"
New-Item -ItemType Directory -Force $data | Out-Null
$env:GITUST_DATA_DIR = $data

$startups = @()
for ($i = 0; $i -lt $Runs; $i++) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $p = Start-Process -FilePath $exePath -ArgumentList "`"$Repo`"" -PassThru
    while ($p.MainWindowHandle -eq 0 -and $sw.ElapsedMilliseconds -lt 10000) {
        Start-Sleep -Milliseconds 5
        $p.Refresh()
    }
    $startups += $sw.ElapsedMilliseconds
    if ($i -lt $Runs - 1) { Stop-Process -Id $p.Id -Force; Start-Sleep -Milliseconds 300 }
}

# Last instance stays open: let the repo load, then measure idle.
Start-Sleep -Seconds 3
$p.Refresh()
$cpu0 = $p.TotalProcessorTime
Start-Sleep -Seconds $IdleSeconds
$p.Refresh()
$cpuMs = ($p.TotalProcessorTime - $cpu0).TotalMilliseconds

$size = (Get-Item $exePath).Length
$sorted = $startups | Sort-Object
[pscustomobject]@{
    "Binary size (MB)"              = [math]::Round($size / 1MB, 2)
    "Window visible, median (ms)"   = $sorted[[int][math]::Floor($sorted.Count / 2)]
    "Window visible, all runs (ms)" = ($startups -join ", ")
    "Idle private memory (MB)"      = [math]::Round($p.PrivateMemorySize64 / 1MB, 1)
    "Idle working set (MB)"         = [math]::Round($p.WorkingSet64 / 1MB, 1)
    "Idle CPU over ${IdleSeconds}s (%)" = [math]::Round(100 * $cpuMs / ($IdleSeconds * 1000 * [Environment]::ProcessorCount), 3)
    "Threads"                       = $p.Threads.Count
    "CPU"                           = (Get-CimInstance Win32_Processor | Select-Object -First 1).Name.Trim()
    "RAM (GB)"                      = [math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB)
} | Format-List

Stop-Process -Id $p.Id -Force
Remove-Item -Recurse -Force $data -ErrorAction SilentlyContinue
