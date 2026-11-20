# Compares Gitust with GitHub Desktop on this machine, same repository.
#   powershell -File scripts/compare.ps1 -Repo C:\path\to\repo [-Runs 3]
#
# For each app and run: launch, time until the window is visible, wait
# -SettleSeconds for the repository to load, then sum memory and CPU over
# -IdleSeconds across *all* of the app's processes (Electron runs several).
# Gitust uses an isolated data folder. GitHub Desktop is opened on -Repo with
# its own `--cli-open`, which adds the repository to its list if it isn't there.
# Close GitHub Desktop before running; the script closes both apps when done.
param(
    [Parameter(Mandatory = $true)][string]$Repo,
    [string]$Gitust = "target\release\gitust.exe",
    [string]$Desktop = "$env:LOCALAPPDATA\GitHubDesktop",
    [int]$Runs = 3,
    [int]$SettleSeconds = 20,
    [int]$IdleSeconds = 15
)
$ErrorActionPreference = "Stop"
$Repo = (Resolve-Path $Repo).Path
$gitustExe = (Resolve-Path $Gitust).Path
$appDir = Get-ChildItem $Desktop -Directory -Filter "app-*" | Sort-Object Name | Select-Object -Last 1
$desktopExe = Join-Path $appDir.FullName "GitHubDesktop.exe"
if (Get-Process GitHubDesktop -ErrorAction SilentlyContinue) { throw "Close GitHub Desktop first." }
$cores = [Environment]::ProcessorCount

function Get-Procs($name) { @(Get-Process -Name $name -ErrorAction SilentlyContinue) }

function Measure-App($name, [scriptblock]$launch) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    & $launch
    $visible = $null
    while ($sw.Elapsed.TotalSeconds -lt 60) {
        if (Get-Procs $name | Where-Object { $_.MainWindowHandle -ne 0 }) { $visible = $sw.ElapsedMilliseconds; break }
        Start-Sleep -Milliseconds 10
    }
    Start-Sleep -Seconds $SettleSeconds
    $before = @{}; Get-Procs $name | ForEach-Object { $before[$_.Id] = $_.TotalProcessorTime.TotalMilliseconds }
    Start-Sleep -Seconds $IdleSeconds
    $procs = Get-Procs $name
    $cpuMs = 0
    foreach ($p in $procs) {
        $start = if ($before.ContainsKey($p.Id)) { $before[$p.Id] } else { 0 }
        $cpuMs += $p.TotalProcessorTime.TotalMilliseconds - $start
    }
    $r = [pscustomobject]@{
        Visible   = $visible
        Private   = ($procs | Measure-Object PrivateMemorySize64 -Sum).Sum / 1MB
        WorkingSet = ($procs | Measure-Object WorkingSet64 -Sum).Sum / 1MB
        Cpu       = 100 * $cpuMs / ($IdleSeconds * 1000 * $cores)
        Processes = $procs.Count
    }
    $procs | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 3
    $r
}

function Median($xs) { $s = @($xs | Sort-Object); $s[[int][math]::Floor($s.Count / 2)] }

$data = Join-Path ([IO.Path]::GetTempPath()) "gitust-compare-$PID"
$results = @{ gitust = @(); desktop = @() }
for ($i = 1; $i -le $Runs; $i++) {
    Write-Host "Run ${i}/${Runs}: Gitust"
    $results.gitust += Measure-App "gitust" {
        $env:GITUST_DATA_DIR = $data
        Start-Process -FilePath $gitustExe -ArgumentList "`"$Repo`"" | Out-Null
    }
    Write-Host "Run ${i}/${Runs}: GitHub Desktop"
    $results.desktop += Measure-App "GitHubDesktop" {
        Start-Process -FilePath $desktopExe -ArgumentList "--cli-open=`"$Repo`"" | Out-Null
    }
}
Remove-Item -Recurse -Force $data -ErrorAction SilentlyContinue

$diskGitust = (Get-Item $gitustExe).Length / 1MB
$diskDesktop = (Get-ChildItem $appDir.FullName -Recurse -File | Measure-Object Length -Sum).Sum / 1MB
foreach ($k in "gitust", "desktop") {
    $r = $results[$k]
    [pscustomobject]@{
        App                    = if ($k -eq "gitust") { "Gitust" } else { "GitHub Desktop $($appDir.Name -replace 'app-','')" }
        "Window visible (ms)"  = Median ($r | ForEach-Object Visible)
        "Private memory (MB)"  = [math]::Round((Median ($r | ForEach-Object Private)), 1)
        "Working set (MB)"     = [math]::Round((Median ($r | ForEach-Object WorkingSet)), 1)
        "Idle CPU (%)"         = [math]::Round((Median ($r | ForEach-Object Cpu)), 2)
        "Processes"            = Median ($r | ForEach-Object Processes)
        "On disk (MB)"         = [math]::Round($(if ($k -eq "gitust") { $diskGitust } else { $diskDesktop }), 1)
        "All runs: private MB" = ($r | ForEach-Object { [math]::Round($_.Private, 1) }) -join ", "
        "All runs: CPU %"      = ($r | ForEach-Object { [math]::Round($_.Cpu, 2) }) -join ", "
        "All runs: visible ms" = ($r | ForEach-Object Visible) -join ", "
    }
}
[pscustomobject]@{
    CPU   = (Get-CimInstance Win32_Processor | Select-Object -First 1).Name.Trim()
    Cores = $cores
    RAM   = "{0} GB" -f [math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB)
    OS    = (Get-CimInstance Win32_OperatingSystem).Caption
}
