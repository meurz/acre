# Build Acre from a native Windows PowerShell session.
[CmdletBinding()]
param(
    [switch]$Check,
    [switch]$Test,
    [switch]$Bench,
    [ValidateRange(1, 128)][int]$Jobs = 2,
    [string]$OutputDir = 'dist',
    [string]$CorePath = ''
)
$ErrorActionPreference = 'Stop'
if (@($Check, $Test, $Bench).Where({ $_ }).Count -gt 1) {
    throw 'Choose only one of -Check, -Test, or -Bench.'
}
$cargo = (Get-Command cargo -ErrorAction Stop).Source
$overrideConfig = $null
Push-Location $PSScriptRoot
try {
    $targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $PSScriptRoot 'target' }
    $cargoArgs = @('--locked', "--jobs=$Jobs")
    if ($CorePath) {
        $core = (Resolve-Path $CorePath -ErrorAction Stop).Path
        if (-not (Test-Path (Join-Path $core 'Cargo.toml'))) {
            throw '-CorePath must point to the acre-core crate directory containing Cargo.toml.'
        }
        $overrideConfig = [IO.Path]::GetTempFileName()
        $toml = "[patch.'https://github.com/meurz/acre-core.git']`nacre-core = { path = " + ($core | ConvertTo-Json -Compress) + " }`n"
        [IO.File]::WriteAllText($overrideConfig, $toml, [Text.UTF8Encoding]::new($false))
        $cargoArgs = @("--jobs=$Jobs", '--config', $overrideConfig)
        Write-Warning 'A local core override may update Cargo.lock. Restore it before committing unrelated changes.'
    }

    if ($Check) { & $cargo check --release -p acre @cargoArgs }
    elseif ($Test) { & $cargo test --release -p acre-model -p acre-i18n @cargoArgs }
    elseif ($Bench) { & $cargo build --release -p acre-model --bin model-bench @cargoArgs }
    else { & $cargo build --release -p acre @cargoArgs }
    if ($LASTEXITCODE -ne 0) { throw "Cargo failed with exit code $LASTEXITCODE." }

    if (-not $Check -and -not $Test) {
        $releaseDir = if ($env:CARGO_BUILD_TARGET) {
            Join-Path (Join-Path $targetDir $env:CARGO_BUILD_TARGET) 'release'
        } else {
            Join-Path $targetDir 'release'
        }
        $name = if ($Bench) { 'model-bench.exe' } else { 'acre.exe' }
        New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
        Copy-Item (Join-Path $releaseDir $name) (Join-Path $OutputDir $name) -Force
        Get-FileHash (Join-Path $OutputDir $name) -Algorithm SHA256
    }
} finally {
    if ($overrideConfig) { Remove-Item $overrideConfig -Force -ErrorAction SilentlyContinue }
    Pop-Location
}
