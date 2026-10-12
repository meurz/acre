# Run model benchmarks with explicit inputs and keep results outside the source tree.
[CmdletBinding(DefaultParameterSetName = 'Synthetic')]
param(
    [string]$Executable = '',
    [Parameter(ParameterSetName = 'Synthetic')]
    [ValidateRange(1000, 100000000)][long]$Nodes = 1000000,
    [Parameter(ParameterSetName = 'Synthetic')]
    [ValidateRange(1, 8192)][int]$Width = 1580,
    [Parameter(ParameterSetName = 'Synthetic')]
    [ValidateRange(1, 8192)][int]$Height = 430,
    [Parameter(Mandatory, ParameterSetName = 'Real')][string]$ScanPath,
    [Parameter(ParameterSetName = 'Real')][switch]$Walk,
    [ValidateRange(1, 100)][int]$Runs = 3,
    [string]$Output = ''
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
if (-not $Executable) { $Executable = Join-Path $repo 'dist/model-bench.exe' }
if (-not $Output) { $Output = Join-Path $repo 'benchmarks/model.json' }
$Executable = (Resolve-Path $Executable -ErrorAction Stop).Path
$arguments = if ($PSCmdlet.ParameterSetName -eq 'Real') {
    @('real', (Resolve-Path $ScanPath -ErrorAction Stop).Path)
} else {
    @("$Nodes", "$Width", "$Height")
}
if ($Walk) { $arguments += 'walk' }
$results = @()
for ($i = 0; $i -lt $Runs; $i++) {
    Write-Host "Run $($i + 1)/$Runs"
    $raw = & $Executable @arguments | Out-String
    if ($LASTEXITCODE -ne 0) { throw "Benchmark failed with exit code $LASTEXITCODE." }
    $results += ($raw | ConvertFrom-Json)
}
$report = [ordered]@{
    recorded_at = [DateTime]::UtcNow.ToString('o')
    executable_sha256 = (Get-FileHash $Executable -Algorithm SHA256).Hash.ToLowerInvariant()
    mode = $PSCmdlet.ParameterSetName.ToLowerInvariant()
    arguments = $arguments
    runs = $results
}
$outputPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Output)
New-Item -ItemType Directory -Force -Path (Split-Path $outputPath -Parent) | Out-Null
[IO.File]::WriteAllText($outputPath, ($report | ConvertTo-Json -Depth 10) + "`n", [Text.UTF8Encoding]::new($false))
Write-Host "Results: $outputPath"
