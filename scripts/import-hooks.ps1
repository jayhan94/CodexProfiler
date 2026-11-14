#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$ExecutablePath,
    [string]$CodexHome
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Preserve spaces, quotes, and trailing backslashes when launching the GUI-subsystem binary.
function ConvertTo-NativeArgument([string]$Value) {
    $escaped = [regex]::Replace($Value, '(\\*)"', '$1$1\"')
    return '"' + [regex]::Replace($escaped, '(\\+)$', '$1$1') + '"'
}

# Wait for the native atomic merge and surface its JSON result or precise failure.
function Invoke-HookImport([string]$Executable, [string[]]$Arguments) {
    $start = New-Object System.Diagnostics.ProcessStartInfo
    $start.FileName = $Executable
    $start.Arguments = (($Arguments | ForEach-Object { ConvertTo-NativeArgument $_ }) -join ' ')
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $start.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
    $start.StandardErrorEncoding = [Text.UTF8Encoding]::new($false)
    $process = [Diagnostics.Process]::Start($start)
    try {
        $outputTask = $process.StandardOutput.ReadToEndAsync()
        $errorTask = $process.StandardError.ReadToEndAsync()
        $process.WaitForExit()
        $output = $outputTask.Result.Trim()
        $errorOutput = $errorTask.Result.Trim()
        if ($process.ExitCode -ne 0) {
            throw "Hook import failed (exit $($process.ExitCode)): $output $errorOutput"
        }
        $null = $output | ConvertFrom-Json
        Write-Output $output
    }
    finally { $process.Dispose() }
}

try {
    if (-not $ExecutablePath) {
        $ExecutablePath = Join-Path (Split-Path -Parent $PSScriptRoot) 'codex-profiler.exe'
    }
    if (-not $CodexHome) {
        $CodexHome = $env:CODEX_HOME
        if (-not $CodexHome) { $CodexHome = Join-Path ([Environment]::GetFolderPath('UserProfile')) '.codex' }
    }
    $ExecutablePath = (Resolve-Path -LiteralPath $ExecutablePath).Path
    $CodexHome = [IO.Path]::GetFullPath($CodexHome)
    Invoke-HookImport $ExecutablePath @('--install-hooks', '--codex-home', $CodexHome)
    Write-Information -InformationAction Continue 'Hooks imported. Review and enable them in Codex Settings > Hooks if prompted, then restart Codex.'
}
catch {
    [Console]::Error.WriteLine("Could not import Profiler hooks: $_")
    exit 1
}
