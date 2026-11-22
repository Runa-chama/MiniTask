# Build the MiniTask installer (Inno Setup 6).
# Usage: powershell -ExecutionPolicy Bypass -File installer\build.ps1 [-SkipBuild]
param([switch]$SkipBuild)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

# Read the version from Cargo.toml
$cargo = Get-Content (Join-Path $root 'Cargo.toml') -Raw
if ($cargo -match '(?m)^version\s*=\s*"([^"]+)"') {
    $version = $Matches[1]
} else {
    throw 'Cannot read version from Cargo.toml'
}
Write-Host "Building installer for MiniTask $version"

if (-not $SkipBuild) {
    Push-Location $root
    try {
        cargo build --release
        if ($LASTEXITCODE -ne 0) { throw 'cargo build --release failed' }
    } finally {
        Pop-Location
    }
}

# Locate ISCC.exe (also supports per-user winget installation)
$candidates = @(
    (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe'),
    (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'),
    (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe')
)
$iscc = $candidates | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $iscc) {
    throw 'ISCC.exe not found. Install Inno Setup 6 (winget install --id JRSoftware.InnoSetup)'
}
Write-Host "ISCC: $iscc"

$dist = Join-Path $root 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null
& $iscc "/DMyAppVersion=$version" (Join-Path $PSScriptRoot 'minitask.iss')
if ($LASTEXITCODE -ne 0) { throw 'ISCC failed' }

$setup = Join-Path $dist "MiniTask-$version-setup.exe"
Write-Host "Done: $setup"
