# Package an already-built Windows x64 executable with its licenses and checksums.
[CmdletBinding()]
param([string]$Executable = 'dist/acre.exe', [string]$OutputDir = 'dist', [string]$Version = '')
$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
Push-Location $repo
$staging = $null
try {
    $match = Select-String -Path Cargo.toml -Pattern '^version = "([^"]+)"$'
    $manifestVersion = $match.Matches.Groups[1].Value
    if (-not $manifestVersion) { throw 'Cannot read the workspace version.' }
    if ($Version -and $Version -ne $manifestVersion) { throw "Expected version $manifestVersion, got $Version." }
    $Version = $manifestVersion
    $Executable = (Resolve-Path $Executable -ErrorAction Stop).Path
    $OutputDir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputDir)
    New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
    $staging = Join-Path ([IO.Path]::GetTempPath()) ('acre-package-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $staging | Out-Null
    Copy-Item $Executable (Join-Path $staging 'acre.exe')
    Copy-Item LICENSE-MIT, LICENSE-APACHE, THIRD_PARTY_NOTICES.md $staging
    Copy-Item third_party (Join-Path $staging 'third_party') -Recurse
    $archiveName = "acre-$Version-windows-x86_64.zip"
    $binaryName = "acre-$Version-windows-x86_64.exe"
    $archive = Join-Path $OutputDir $archiveName
    Compress-Archive -Path (Join-Path $staging '*') -DestinationPath $archive -Force
    Copy-Item $Executable (Join-Path $OutputDir $binaryName) -Force
    $lines = foreach ($name in @($archiveName, $binaryName)) {
        $hash = (Get-FileHash (Join-Path $OutputDir $name) -Algorithm SHA256).Hash.ToLowerInvariant()
        "$hash  $name"
    }
    [IO.File]::WriteAllText((Join-Path $OutputDir 'SHA256SUMS'), ($lines -join "`n") + "`n", [Text.UTF8Encoding]::new($false))
    Write-Host "Created $archiveName, $binaryName, and SHA256SUMS in $OutputDir"
} finally {
    if ($staging) { Remove-Item $staging -Recurse -Force -ErrorAction SilentlyContinue }
    Pop-Location
}
