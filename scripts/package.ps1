#Requires -Version 5.1
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$BinaryPath,
    [Parameter(Mandatory = $true)][ValidatePattern('^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$')][string]$Version,
    [string]$OutputDir,
    [string]$SourceDir
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not $SourceDir) { $SourceDir = Split-Path -Parent $PSScriptRoot }
if (-not $OutputDir) { $OutputDir = Join-Path $SourceDir 'dist' }
$BinaryPath = (Resolve-Path -LiteralPath $BinaryPath).Path
$SourceDir = (Resolve-Path -LiteralPath $SourceDir).Path
$OutputDir = [IO.Path]::GetFullPath($OutputDir)
$null = New-Item -ItemType Directory -Force -Path $OutputDir
$stage = Join-Path $OutputDir ('.package-' + [Guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $stage
try {
    $null = New-Item -ItemType Directory -Path (Join-Path $stage 'licenses'), (Join-Path $stage 'scripts')
    Copy-Item -LiteralPath $BinaryPath -Destination (Join-Path $stage 'codex-profiler.exe')
    Copy-Item -LiteralPath (Join-Path $SourceDir 'LICENSE') -Destination (Join-Path $stage 'LICENSE')
    Copy-Item -LiteralPath (Join-Path $SourceDir 'assets/fonts/OFL.txt') -Destination (Join-Path $stage 'licenses/JetBrainsMono-OFL.txt')
    foreach ($script in @('install.ps1', 'import-hooks.ps1', 'uninstall.ps1')) {
        Copy-Item -LiteralPath (Join-Path $SourceDir "scripts/$script") -Destination (Join-Path $stage "scripts/$script")
    }
    [ordered]@{
        schema      = 1
        application = 'CodexProfiler'
        version     = $Version
        platform    = 'windows'
        arch        = 'x64'
    } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $stage 'package.json') -Encoding UTF8
    $archiveName = "CodexProfiler-v$Version-windows-x64.zip"
    $archive = Join-Path $OutputDir $archiveName
    if (Test-Path -LiteralPath $archive) { throw "Refusing to replace existing archive: $archive" }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [IO.Compression.ZipFile]::CreateFromDirectory($stage, $archive, [IO.Compression.CompressionLevel]::Optimal, $false)
    $hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $archiveName" | Set-Content -LiteralPath (Join-Path $OutputDir 'SHA256SUMS') -Encoding ASCII
    Write-Information -InformationAction Continue "Created $archive and SHA256SUMS"
}
finally {
    # The staging path is newly allocated directly beneath the requested output directory.
    $resolvedStage = [IO.Path]::GetFullPath($stage)
    if ([IO.Path]::GetDirectoryName($resolvedStage) -eq $OutputDir -and
        [IO.Path]::GetFileName($resolvedStage) -like '.package-*') {
        Remove-Item -LiteralPath $resolvedStage -Recurse -Force
    }
}
