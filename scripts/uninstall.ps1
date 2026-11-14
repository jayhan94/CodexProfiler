#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$InstallDir,
    [string]$CodexHome
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$OwnedFiles = @(
    'codex-profiler.exe', 'package.json', 'LICENSE', 'licenses/JetBrainsMono-OFL.txt',
    'scripts/install.ps1', 'scripts/import-hooks.ps1', 'scripts/uninstall.ps1'
)

# Preserve Windows paths when invoking the native hook uninstaller.
function ConvertTo-NativeArgument([string]$Value) {
    $escaped = [regex]::Replace($Value, '(\\*)"', '$1$1\"')
    return '"' + [regex]::Replace($escaped, '(\\+)$', '$1$1') + '"'
}

# Remove the native ownership entries before removing their executable.
function Invoke-HookRemoval([string]$Executable, [string]$ConfigurationDir) {
    $start = New-Object System.Diagnostics.ProcessStartInfo
    $start.FileName = $Executable
    $start.Arguments = '--uninstall-hooks --codex-home ' + (ConvertTo-NativeArgument $ConfigurationDir)
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
            throw "Hook removal failed (exit $($process.ExitCode)): $output $errorOutput"
        }
        $null = $output | ConvertFrom-Json
        Write-Information -InformationAction Continue $output
    }
    finally { $process.Dispose() }
}

# Stop only the installed GUI. Brief retries let active hook processes finish writing.
function Invoke-FileRemoval([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return }
    for ($attempt = 0; $attempt -lt 12; $attempt++) {
        try {
            Remove-Item -LiteralPath $Path -Force
            return
        }
        catch {
            if ($attempt -eq 11) { throw }
            Start-Sleep -Milliseconds 250
        }
    }
}

# Validate the fixed ownership list before deleting anything, even when the state was edited.
function Get-ValidatedState([string]$Destination) {
    $state = Get-Content -LiteralPath (Join-Path $Destination 'install-state.json') -Raw | ConvertFrom-Json
    if ($state.schema -ne 1 -or $state.application -cne 'CodexProfiler' -or
        @($state.files | Sort-Object -Unique).Count -ne $OwnedFiles.Count) { throw 'Unrecognized install-state.json.' }
    foreach ($file in $state.files) {
        if ($OwnedFiles -cnotcontains $file) { throw "Unexpected owned file: $file" }
    }
    foreach ($relative in @('', 'scripts', 'licenses', 'install-state.json') + $OwnedFiles) {
        $ownedPath = $Destination
        if ($relative) { $ownedPath = Join-Path $Destination $relative }
        if ((Test-Path -LiteralPath $ownedPath) -and
            ((Get-Item -LiteralPath $ownedPath -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw "Managed paths cannot be symbolic links or junctions: $ownedPath"
        }
    }
    return $state
}

try {
    if (-not $InstallDir) { $InstallDir = Split-Path -Parent $PSScriptRoot }
    $InstallDir = [IO.Path]::GetFullPath($InstallDir)
    $state = Get-ValidatedState $InstallDir
    if (-not $CodexHome) { $CodexHome = $state.codex_home }
    $CodexHome = [IO.Path]::GetFullPath($CodexHome)
    if ($CodexHome -ne [IO.Path]::GetFullPath($state.codex_home)) {
        throw 'This installation is associated with another Codex home. Remove extra hooks separately before uninstalling.'
    }
    $executable = Join-Path $InstallDir 'codex-profiler.exe'
    Invoke-HookRemoval $executable $CodexHome
    Get-Process -Name 'codex-profiler' -ErrorAction SilentlyContinue | ForEach-Object {
        if ($_.Path -eq $executable -and $_.MainWindowHandle -ne 0) {
            $null = $_.CloseMainWindow()
            if (-not $_.WaitForExit(3000)) {
                $_.Kill()
                $_.WaitForExit()
            }
        }
    }
    $expectedShortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'CodexProfiler.lnk'
    if ($state.shortcut -and $state.shortcut -eq $expectedShortcut -and (Test-Path -LiteralPath $expectedShortcut)) {
        $shell = New-Object -ComObject WScript.Shell
        $shortcut = $shell.CreateShortcut($expectedShortcut)
        if ($shortcut.TargetPath -eq $executable) { Invoke-FileRemoval $expectedShortcut }
    }
    # Keep the state until all owned files have been removed, allowing a failed attempt to be retried.
    foreach ($file in $state.files) {
        if ($file -ne 'codex-profiler.exe') { Invoke-FileRemoval (Join-Path $InstallDir $file) }
    }
    Invoke-FileRemoval $executable
    Invoke-FileRemoval (Join-Path $InstallDir 'install-state.json')
    Write-Information -InformationAction Continue "Uninstalled CodexProfiler from $InstallDir. Profiling data and unrelated files were preserved."
}
catch {
    [Console]::Error.WriteLine("Uninstall failed: $_")
    exit 1
}
