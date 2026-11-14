#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$Version = 'latest',
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\CodexProfiler'),
    [string]$CodexHome,
    [string]$PackagePath,
    [string]$ChecksumPath,
    [switch]$SkipHooks,
    [switch]$NoShortcut,
    [switch]$NoLaunch
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$Repository = 'jayhan94/CodexProfiler'
$Application = 'CodexProfiler'
$PackageFiles = @(
    'codex-profiler.exe', 'package.json', 'LICENSE', 'licenses/JetBrainsMono-OFL.txt',
    'scripts/install.ps1', 'scripts/import-hooks.ps1', 'scripts/uninstall.ps1'
)

# Quote arguments for the Windows command-line parser, including spaces and trailing slashes.
function ConvertTo-NativeArgument([string]$Value) {
    $escaped = [regex]::Replace($Value, '(\\*)"', '$1$1\"')
    return '"' + [regex]::Replace($escaped, '(\\+)$', '$1$1') + '"'
}

# Redirected asynchronous reads also work for the Windows GUI-subsystem executable.
function Invoke-Profiler([string]$Executable, [string[]]$Arguments, [switch]$PlainText) {
    $start = New-Object System.Diagnostics.ProcessStartInfo
    $start.FileName = $Executable
    $start.Arguments = (($Arguments | ForEach-Object { ConvertTo-NativeArgument $_ }) -join ' ')
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $start.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
    $start.StandardErrorEncoding = [Text.UTF8Encoding]::new($false)
    $process = [System.Diagnostics.Process]::Start($start)
    try {
        $outputTask = $process.StandardOutput.ReadToEndAsync()
        $errorTask = $process.StandardError.ReadToEndAsync()
        $process.WaitForExit()
        $output = $outputTask.Result.Trim()
        $errorOutput = $errorTask.Result.Trim()
        if ($process.ExitCode -ne 0) {
            throw "Profiler failed (exit $($process.ExitCode)): $output $errorOutput"
        }
        if ($PlainText) { return $output }
        try { return ($output | ConvertFrom-Json) }
        catch { throw "Profiler returned invalid JSON: $output $errorOutput" }
    }
    finally { $process.Dispose() }
}

# Existing installations are owned explicitly; never overwrite an unrelated project folder.
function Get-InstallState([string]$Destination) {
    if (-not (Test-Path -LiteralPath $Destination)) { return $null }
    if (-not (Test-Path -LiteralPath $Destination -PathType Container)) {
        throw "InstallDir is not a directory: $Destination"
    }
    $statePath = Join-Path $Destination 'install-state.json'
    if (-not (Test-Path -LiteralPath $statePath)) {
        if (@(Get-ChildItem -LiteralPath $Destination -Force).Count -gt 0) {
            throw 'InstallDir is not empty and is not managed by CodexProfiler. Choose an empty directory.'
        }
        return $null
    }
    $state = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json
    if ($state.schema -ne 1 -or $state.application -cne $Application) {
        throw 'InstallDir has an unrecognized install-state.json.'
    }
    foreach ($file in $state.files) {
        if ($PackageFiles -cnotcontains $file) { throw "Unexpected owned file: $file" }
    }
    if (@($state.files | Sort-Object -Unique).Count -ne $PackageFiles.Count) {
        throw 'Incomplete install-state.json ownership list.'
    }
    foreach ($relative in @('', 'scripts', 'licenses', 'install-state.json') + $PackageFiles) {
        $ownedPath = $Destination
        if ($relative) { $ownedPath = Join-Path $Destination $relative }
        if ((Test-Path -LiteralPath $ownedPath) -and
            ((Get-Item -LiteralPath $ownedPath -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw "Managed application paths cannot be symbolic links or junctions: $ownedPath"
        }
    }
    return $state
}

# GitHub's release API selects the exact x64 package and its checksum list.
function Get-ReleasePackage([string]$RequestedVersion, [string]$DownloadDir) {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $endpoint = "https://api.github.com/repos/$Repository/releases/latest"
    if ($RequestedVersion -ne 'latest') {
        if ($RequestedVersion -notmatch '^v?\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$') {
            throw 'Version must be latest or a release version such as 0.2.0.'
        }
        $tag = 'v' + $RequestedVersion.TrimStart('v')
        $endpoint = "https://api.github.com/repos/$Repository/releases/tags/$tag"
    }
    Write-Information -InformationAction Continue "Reading release: $endpoint"
    $headers = @{
        'User-Agent' = 'CodexProfiler-Installer'
        Accept       = 'application/vnd.github+json'
    }
    $release = Invoke-RestMethod -Uri $endpoint -Headers $headers
    $releaseVersion = ([string]$release.tag_name).TrimStart('v')
    $assetName = "CodexProfiler-v$releaseVersion-windows-x64.zip"
    $archive = @($release.assets | Where-Object { $_.name -ceq $assetName })
    $checksums = @($release.assets | Where-Object { $_.name -ceq 'SHA256SUMS' })
    if ($archive.Count -ne 1 -or $checksums.Count -ne 1) {
        throw "Release $($release.tag_name) does not contain $assetName and SHA256SUMS."
    }
    $archivePath = Join-Path $DownloadDir $assetName
    $sumPath = Join-Path $DownloadDir 'SHA256SUMS'
    Invoke-WebRequest -UseBasicParsing -Uri $archive[0].browser_download_url -OutFile $archivePath -Headers $headers
    Invoke-WebRequest -UseBasicParsing -Uri $checksums[0].browser_download_url -OutFile $sumPath -Headers $headers
    return @{
        Archive   = $archivePath
        Checksums = $sumPath
        Version   = $releaseVersion
    }
}

# Checksum verification happens before opening or extracting any package entry.
function Test-PackageChecksum([string]$Archive, [string]$Checksums) {
    if (-not (Test-Path -LiteralPath $Checksums -PathType Leaf)) {
        throw "Checksum file is required: $Checksums"
    }
    $name = [IO.Path]::GetFileName($Archive)
    $pattern = '^([0-9A-Fa-f]{64})\s+\*?' + [regex]::Escape($name) + '$'
    $matchesForFile = @(Get-Content -LiteralPath $Checksums | Where-Object { $_ -match $pattern })
    if ($matchesForFile.Count -ne 1) { throw "Expected one SHA256SUMS entry for $name." }
    $null = $matchesForFile[0] -match $pattern
    $expected = $Matches[1]
    $actual = (Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash
    if ($actual -ne $expected) { throw "SHA256 mismatch for $name. No files were installed." }
    Write-Information -InformationAction Continue "SHA256 verified: $name"
}

# Only the published package layout is accepted; paths cannot escape the staging directory.
function Expand-VerifiedPackage([string]$Archive, [string]$Destination) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($Archive)
    $seen = New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
    try {
        foreach ($entry in $zip.Entries) {
            $name = $entry.FullName.Replace('\', '/')
            if ($name -in @('scripts/', 'licenses/')) { continue }
            if ($PackageFiles -cnotcontains $name -or -not $seen.Add($name)) {
                throw "Unexpected or duplicate archive entry: $name"
            }
            $target = Join-Path $Destination $name
            $null = New-Item -ItemType Directory -Force -Path ([IO.Path]::GetDirectoryName($target))
            [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $target, $false)
        }
    }
    finally { $zip.Dispose() }
    if ($seen.Count -ne $PackageFiles.Count) { throw 'The archive is missing required application files.' }
    $manifest = Get-Content -LiteralPath (Join-Path $Destination 'package.json') -Raw | ConvertFrom-Json
    if ($manifest.schema -ne 1 -or $manifest.application -cne $Application -or
        $manifest.platform -cne 'windows' -or $manifest.arch -cne 'x64' -or
        $manifest.version -notmatch '^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$') {
        throw 'The archive is not a supported CodexProfiler Windows x64 package.'
    }
    return $manifest
}

# Stop only an open GUI from this exact installation; do not interrupt in-flight hook writers.
function Invoke-GuiClose([string]$Executable) {
    Get-Process -Name 'codex-profiler' -ErrorAction SilentlyContinue | ForEach-Object {
        try {
            if ($_.Path -eq $Executable -and $_.MainWindowHandle -ne 0) {
                Write-Information -InformationAction Continue "Closing the installed Profiler window (PID $($_.Id)) for upgrade."
                $null = $_.CloseMainWindow()
                if (-not $_.WaitForExit(3000)) {
                    $_.Kill()
                    $_.WaitForExit()
                }
            }
        }
        catch { Write-Verbose "Unable to close process $($_.Id): $_" }
    }
}

# Short retries allow a completed hook writer to release the executable during an upgrade.
function Copy-WithRetry([string]$Source, [string]$Destination) {
    for ($attempt = 0; $attempt -lt 12; $attempt++) {
        try {
            Copy-Item -LiteralPath $Source -Destination $Destination -Force
            return
        }
        catch {
            if ($attempt -eq 11) { throw }
            Start-Sleep -Milliseconds 250
        }
    }
}

# Back up owned files before publishing so failed hook import restores the prior installation.
function Backup-Installation([string]$Destination, [string]$Backup) {
    $null = New-Item -ItemType Directory -Force -Path $Destination, $Backup
    foreach ($file in @($PackageFiles) + 'install-state.json') {
        $existing = Join-Path $Destination $file
        if (Test-Path -LiteralPath $existing -PathType Leaf) {
            $saved = Join-Path $Backup $file
            $null = New-Item -ItemType Directory -Force -Path ([IO.Path]::GetDirectoryName($saved))
            Copy-Item -LiteralPath $existing -Destination $saved
        }
    }
}

# Copy only application-owned paths after a complete backup is available.
function Publish-Package([string]$Staged, [string]$Destination) {
    foreach ($file in $PackageFiles) {
        $target = Join-Path $Destination $file
        $null = New-Item -ItemType Directory -Force -Path ([IO.Path]::GetDirectoryName($target))
        Copy-WithRetry (Join-Path $Staged $file) $target
    }
}

# Restore files individually; foreign files and profiling databases are never recursively removed.
function Restore-Installation([string]$Destination, [string]$Backup) {
    foreach ($file in @($PackageFiles) + 'install-state.json') {
        $target = Join-Path $Destination $file
        $saved = Join-Path $Backup $file
        if (Test-Path -LiteralPath $saved -PathType Leaf) { Copy-WithRetry $saved $target }
        elseif (Test-Path -LiteralPath $target -PathType Leaf) { Remove-Item -LiteralPath $target -Force }
    }
}

# Keep the shortcut per-user and preserve the selected Codex configuration directory.
function Invoke-ShortcutCreation([string]$Executable, [string]$ConfigurationDir) {
    $programs = [Environment]::GetFolderPath('Programs')
    $shortcutPath = Join-Path $programs 'CodexProfiler.lnk'
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($shortcutPath)
    $shortcut.TargetPath = $Executable
    $shortcut.Arguments = '--codex-home ' + (ConvertTo-NativeArgument $ConfigurationDir)
    $shortcut.WorkingDirectory = [IO.Path]::GetDirectoryName($Executable)
    $shortcut.Description = 'Codex hook timing and activity profiler'
    $shortcut.Save()
    return $shortcutPath
}

$temporary = $null
$published = $false
$committed = $false
try {
    # The OS registry value also identifies ARM64 when PowerShell runs under x64 emulation.
    $systemEnvironment = Get-ItemProperty -LiteralPath 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Environment'
    $osArchitecture = $systemEnvironment.PROCESSOR_ARCHITECTURE
    if (-not [Environment]::Is64BitOperatingSystem -or $osArchitecture -ne 'AMD64') {
        throw 'This package supports Windows x64 (AMD64) only. ARM64 and 32-bit Windows are unsupported.'
    }
    $InstallDir = [IO.Path]::GetFullPath($InstallDir)
    $previous = Get-InstallState $InstallDir
    if (-not $CodexHome -and $previous) { $CodexHome = $previous.codex_home }
    if (-not $CodexHome) {
        $CodexHome = $env:CODEX_HOME
        if (-not $CodexHome) { $CodexHome = Join-Path ([Environment]::GetFolderPath('UserProfile')) '.codex' }
    }
    $CodexHome = [IO.Path]::GetFullPath($CodexHome)
    if ($previous -and $CodexHome -ne [IO.Path]::GetFullPath($previous.codex_home)) {
        throw "This installation uses $($previous.codex_home). Uninstall it before changing -CodexHome."
    }
    $parent = [IO.Path]::GetDirectoryName($InstallDir)
    $null = New-Item -ItemType Directory -Force -Path $parent
    $temporary = Join-Path $parent ('.codex-profiler-' + [Guid]::NewGuid().ToString('N'))
    $null = New-Item -ItemType Directory -Path $temporary
    $staged = Join-Path $temporary 'package'
    $backup = Join-Path $temporary 'backup'
    $null = New-Item -ItemType Directory -Path $staged
    $expectedVersion = $Version.TrimStart('v')
    if ($PackagePath) {
        $PackagePath = (Resolve-Path -LiteralPath $PackagePath).Path
        if (-not $ChecksumPath) { $ChecksumPath = Join-Path ([IO.Path]::GetDirectoryName($PackagePath)) 'SHA256SUMS' }
    }
    else {
        $download = Get-ReleasePackage $Version $temporary
        $PackagePath = $download.Archive
        $ChecksumPath = $download.Checksums
        $expectedVersion = $download.Version
    }
    Test-PackageChecksum $PackagePath $ChecksumPath
    $manifest = Expand-VerifiedPackage $PackagePath $staged
    if ($expectedVersion -ne 'latest' -and $manifest.version -cne $expectedVersion) {
        throw "Package version $($manifest.version) does not match requested version $expectedVersion."
    }
    $binaryVersion = Invoke-Profiler (Join-Path $staged 'codex-profiler.exe') @('--version') -PlainText
    if ($binaryVersion -cne "CodexProfiler $($manifest.version)") { throw 'Executable version does not match package.json.' }
    $executable = Join-Path $InstallDir 'codex-profiler.exe'
    Invoke-GuiClose $executable
    # Backups complete before any replacement; rollback also covers partial copy failures.
    Backup-Installation $InstallDir $backup
    $published = $true
    Publish-Package $staged $InstallDir
    $shortcutPath = $null
    if ($previous -and $previous.shortcut) { $shortcutPath = $previous.shortcut }
    if (-not $NoShortcut) {
        $shortcutPath = Join-Path ([Environment]::GetFolderPath('Programs')) 'CodexProfiler.lnk'
    }
    $state = [ordered]@{
        schema      = 1
        application = $Application
        version     = $manifest.version
        files       = $PackageFiles
        codex_home  = $CodexHome
        shortcut    = $shortcutPath
    }
    $state | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $InstallDir 'install-state.json') -Encoding UTF8
    if (-not $SkipHooks) {
        $result = Invoke-Profiler $executable @('--install-hooks', '--codex-home', $CodexHome)
        Write-Information -InformationAction Continue ($result | ConvertTo-Json -Depth 8)
    }
    $committed = $true
    if (-not $NoShortcut) {
        try { $null = Invoke-ShortcutCreation $executable $CodexHome }
        catch { Write-Warning "Installed successfully, but could not create Start menu shortcut: $_" }
    }
    Write-Information -InformationAction Continue "Installed CodexProfiler $($manifest.version) at $InstallDir"
    if (-not $SkipHooks) {
        Write-Information -InformationAction Continue 'Review and enable hooks in Codex Settings > Hooks if prompted, then restart Codex.'
    }
    if (-not $NoLaunch) {
        try { Start-Process -FilePath $executable -ArgumentList ('--codex-home ' + (ConvertTo-NativeArgument $CodexHome)) }
        catch { Write-Warning "Installed successfully, but could not open the window: $_" }
    }
}
catch {
    $failure = $_
    if ($published -and -not $committed) {
        try { Restore-Installation $InstallDir $backup }
        catch {
            Write-Warning "Rollback needs attention: $_. Backup retained at $backup"
            $temporary = $null
        }
    }
    [Console]::Error.WriteLine("Installation failed: $failure")
    exit 1
}
finally {
    # Only our newly created staging directory can be recursively deleted.
    if ($temporary -and (Test-Path -LiteralPath $temporary)) {
        $resolvedTemporary = [IO.Path]::GetFullPath($temporary)
        if ([IO.Path]::GetDirectoryName($resolvedTemporary) -eq $parent -and
            [IO.Path]::GetFileName($resolvedTemporary) -like '.codex-profiler-*') {
            Remove-Item -LiteralPath $resolvedTemporary -Recurse -Force
        }
    }
}
