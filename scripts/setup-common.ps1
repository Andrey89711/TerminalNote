Set-StrictMode -Version Latest

function Get-TerminalNoteAbsolutePath {
    param([Parameter(Mandatory = $true)][string]$Path)
    # Reject drive-relative, device and root-relative paths on Windows/PowerShell 5.1.
    if ($Path -notmatch '^(?:[a-zA-Z]:[\\/]|\\\\[^\\/?\.][^\\/]*[\\/][^\\/]+(?:[\\/]|$))' -or $Path -match '[\x00-\x1f"*?]') {
        throw 'Choose an absolute Windows path, without wildcards, quotes or device prefixes.'
    }
    $full = [IO.Path]::GetFullPath($Path)
    $withoutDrive = $full -replace '^[a-zA-Z]:', ''
    if ($withoutDrive.Contains(':')) { throw 'Alternate data streams are not supported.' }
    $root = [IO.Path]::GetPathRoot($full)
    if ($full.TrimEnd('\', '/').Length -le $root.TrimEnd('\', '/').Length) { return $root }
    $full.TrimEnd('\', '/')
}

function Assert-TerminalNoteSafePath {
    param([Parameter(Mandatory = $true)][string]$Path, [switch]$Directory)
    $current = Get-TerminalNoteAbsolutePath -Path $Path
    $first = $true
    while ($current) {
        if (Test-Path -LiteralPath $current) {
            $item = Get-Item -LiteralPath $current -Force -ErrorAction Stop
            if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Refusing a link or junction: $current" }
            if ((!$first -or $Directory) -and !$item.PSIsContainer) { throw "Expected a directory: $current" }
            if ($first -and !$Directory -and $item.PSIsContainer) { throw "Expected a regular file: $current" }
        }
        $first = $false
        $parent = Split-Path -Path $current -Parent
        if (!$parent -or $parent -eq $current) { break }
        $current = $parent
    }
}

function Get-TerminalNoteInstallRoot {
    param([Parameter(Mandatory = $true)][string]$Path)
    $full = Get-TerminalNoteAbsolutePath -Path $Path
    if ($full.TrimEnd('\', '/').Length -le [IO.Path]::GetPathRoot($full).TrimEnd('\', '/').Length) { throw 'Do not use a drive or share root as the program directory.' }
    if ($full.IndexOfAny([char[]]';%') -ge 0) { throw 'The program directory cannot contain semicolons or percent signs.' }
    Assert-TerminalNoteSafePath -Path $full -Directory
    $full
}

function Assert-TerminalNoteSeparateNotes {
    param([Parameter(Mandatory = $true)][string]$NotesRoot, [Parameter(Mandatory = $true)][string]$InstallDirectory)
    $notes = (Get-TerminalNoteAbsolutePath -Path $NotesRoot).TrimEnd('\', '/')
    $install = (Get-TerminalNoteInstallRoot -Path $InstallDirectory).TrimEnd('\', '/')
    if ($notes -ieq $install -or $notes.StartsWith($install + '\', [StringComparison]::OrdinalIgnoreCase) -or $install.StartsWith($notes + '\', [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Keep notes and configuration outside the program directory, and do not install inside your notes directory.'
    }
}

function Get-TerminalNotePayloadFiles {
    # This list is the ONLY authority for copying and removing program files.
    @('note.exe', 'README.md', 'LICENSE', 'CHANGELOG.md', 'setup.cmd', 'setup.ps1',
      'uninstall.cmd', 'uninstall.ps1', 'scripts/setup-common.ps1',
      'scripts/uninstall-common.ps1', 'docs/RELEASING.md')
}

function Get-TerminalNoteBinaryVersion {
    param([Parameter(Mandatory = $true)][string]$BinaryPath)
    Assert-TerminalNoteSafePath -Path $BinaryPath
    if (!(Test-Path -LiteralPath $BinaryPath -PathType Leaf)) { throw 'No note.exe found. Extract the complete Windows release ZIP, or supply -BinaryPath with a built note.exe.' }
    $output = @(& $BinaryPath --version 2>&1)
    if ($LASTEXITCODE -ne 0 -or $output.Count -ne 1 -or [string]$output[0] -notmatch '^note (\d+\.\d+\.\d+)$') {
        throw 'Cannot read a compatible TerminalNote version with note.exe --version.'
    }
    $Matches[1]
}

function Get-TerminalNoteInstallMarker {
    param([Parameter(Mandatory = $true)][string]$InstallDirectory)
    $path = Join-Path $InstallDirectory '.terminalnote-install.json'
    Assert-TerminalNoteSafePath -Path $path
    $info = Get-Item -LiteralPath $path -Force -ErrorAction Stop
    if ($info.Length -gt 4096) { throw 'Invalid TerminalNote ownership marker.' }
    $marker = Get-Content -Raw -Encoding UTF8 -LiteralPath $path | ConvertFrom-Json -ErrorAction Stop
    if ($marker -isnot [pscustomobject] -or !$marker.PSObject.Properties['product'] -or !$marker.PSObject.Properties['schema'] -or !$marker.PSObject.Properties['version']) { throw 'Invalid TerminalNote ownership marker.' }
    if ($marker.product -cne 'TerminalNote' -or ($marker.schema -isnot [int] -and $marker.schema -isnot [long]) -or $marker.schema -ne 1 -or $marker.version -isnot [string] -or $marker.version -notmatch '^\d+\.\d+\.\d+$') { throw 'Unrecognized TerminalNote ownership marker or schema.' }
    $null = [version]$marker.version
    $marker
}

function Assert-TerminalNoteUnlocked {
    param([Parameter(Mandatory = $true)][string]$Path)
    if (Test-Path -LiteralPath $Path) {
        try {
            $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
            $stream.Dispose()
        } catch { throw 'A program file is locked or not writable. Close TerminalNote and retry. No program files were changed.' }
    }
}

function Remove-TerminalNoteEmptyDirectories {
    param([Parameter(Mandatory = $true)][string]$Root)
    # No recursive deletion, even for our private staging directory.
    foreach ($relative in @('scripts', 'docs', '')) {
        $path = if ($relative) { Join-Path $Root $relative } else { $Root }
        if (Test-Path -LiteralPath $path -PathType Container) {
            Assert-TerminalNoteSafePath -Path $path -Directory
            if (@(Get-ChildItem -LiteralPath $path -Force).Count -eq 0) { [IO.Directory]::Delete($path, $false) }
        }
    }
}

function Install-TerminalNotePayload {
    param(
        [Parameter(Mandatory = $true)][string]$SourceRoot,
        [Parameter(Mandatory = $true)][string]$BinaryPath,
        [Parameter(Mandatory = $true)][string]$Destination
    )
    $source = Get-TerminalNoteInstallRoot -Path $SourceRoot
    $binary = Get-TerminalNoteAbsolutePath -Path $BinaryPath
    $root = Get-TerminalNoteInstallRoot -Path $Destination
    $files = @(Get-TerminalNotePayloadFiles)
    $version = Get-TerminalNoteBinaryVersion -BinaryPath $binary
    foreach ($relative in $files) {
        $inputPath = if ($relative -eq 'note.exe') { $binary } else { Join-Path $source $relative }
        Assert-TerminalNoteSafePath -Path $inputPath
        if (!(Test-Path -LiteralPath $inputPath -PathType Leaf)) { throw "Incomplete package: $relative" }
    }
    $markerPath = Join-Path $root '.terminalnote-install.json'
    if (Test-Path -LiteralPath $markerPath) {
        $marker = Get-TerminalNoteInstallMarker -InstallDirectory $root
        if ([version]$marker.version -gt [version]$version) { throw "Downgrade from $($marker.version) to $version refused." }
    } elseif ((Test-Path -LiteralPath $root) -and @(Get-ChildItem -LiteralPath $root -Force).Count -gt 0) {
        throw 'The program directory is nonempty and has no TerminalNote ownership marker. Choose a new empty directory.'
    }
    # Preflight EVERY destination before replacing anything, including linked ancestors.
    foreach ($relative in ($files + @('.terminalnote-install.json'))) {
        $target = Join-Path $root $relative
        Assert-TerminalNoteSafePath -Path $target
        Assert-TerminalNoteUnlocked -Path $target
    }
    if (Test-Path -LiteralPath (Join-Path $root 'note.exe') -PathType Leaf) {
        # An interrupted earlier upgrade can leave a newer EXE with an older
        # marker. Check the executable too, without starting the interface.
        $installedVersion = $null
        try { $installedVersion = Get-TerminalNoteBinaryVersion -BinaryPath (Join-Path $root 'note.exe') }
        catch { Write-Warning 'The owned executable is damaged or has an unreadable version; repairing it from the selected package.' }
        if ($installedVersion -and [version]$installedVersion -gt [version]$version) { throw "Downgrade from installed executable $installedVersion to $version refused." }
    }
    [void][IO.Directory]::CreateDirectory($root)
    $stage = Join-Path $root ('.terminalnote-stage-' + [guid]::NewGuid().ToString('N'))
    [void][IO.Directory]::CreateDirectory($stage)
    try {
        foreach ($relative in $files) {
            $inputPath = if ($relative -eq 'note.exe') { $binary } else { Join-Path $source $relative }
            $staged = Join-Path $stage $relative
            [void][IO.Directory]::CreateDirectory((Split-Path $staged -Parent))
            [IO.File]::Copy($inputPath, $staged, $false)
        }
        $newMarker = Join-Path $stage '.terminalnote-install.json'
        [IO.File]::WriteAllText($newMarker, (@{ product = 'TerminalNote'; schema = 1; version = $version } | ConvertTo-Json), (New-Object Text.UTF8Encoding($false)))
        if (!(Test-Path -LiteralPath $markerPath)) {
            # Claim a NEW empty target only after the complete package was staged.
            # A power loss during the following moves must leave a recognizable,
            # repairable partial installation rather than an unowned directory.
            [IO.File]::Copy($newMarker, $markerPath, $false)
        }
        # Atomic per-file replacement, binary first. A running EXE fails without truncating it.
        # On upgrades, keep the previous marker until the complete payload is written.
        foreach ($relative in ($files + @('.terminalnote-install.json'))) {
            $target = Join-Path $root $relative
            Assert-TerminalNoteSafePath -Path $target
            [void][IO.Directory]::CreateDirectory((Split-Path $target -Parent))
            $staged = Join-Path $stage $relative
            if (Test-Path -LiteralPath $target) { [IO.File]::Replace($staged, $target, [NullString]::Value) }
            else { [IO.File]::Move($staged, $target) }
        }
    } finally {
        foreach ($relative in ($files + @('.terminalnote-install.json'))) {
            $staged = Join-Path $stage $relative
            if (Test-Path -LiteralPath $staged -PathType Leaf) {
                Assert-TerminalNoteSafePath -Path $staged
                [IO.File]::Delete($staged)
            }
        }
        Remove-TerminalNoteEmptyDirectories -Root $stage
    }
    Join-Path $root 'note.exe'
}

function Get-TerminalNoteUpdatedPath {
    param([AllowNull()][AllowEmptyString()][string]$CurrentPath, [Parameter(Mandatory = $true)][string]$InstallDirectory, [switch]$Remove)
    $directory = Get-TerminalNoteInstallRoot -Path $InstallDirectory
    $remaining = @()
    if ($null -ne $CurrentPath -and $CurrentPath.Length -gt 0) {
        foreach ($entry in $CurrentPath.Split([char]';')) {
            $normalized = [Environment]::ExpandEnvironmentVariables($entry.Trim().Trim('"')).TrimEnd('\', '/')
            if ([IO.Path]::IsPathRooted($normalized)) {
                try { $normalized = [IO.Path]::GetFullPath($normalized).TrimEnd('\', '/') } catch { }
            }
            # Comparison only: NEVER expand or reformat an unrelated raw entry.
            if ($normalized -ine $directory) { $remaining += $entry }
        }
    }
    if ($Remove) { return ($remaining -join ';') }
    (@($directory) + $remaining) -join ';'
}

function Set-TerminalNoteUserPath {
    param([Parameter(Mandatory = $true)][string]$InstallDirectory, [switch]$Remove)
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
    $changed = $false
    try {
        $raw = $key.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        $kind = if ($null -eq $raw) { [Microsoft.Win32.RegistryValueKind]::ExpandString } else { $key.GetValueKind('Path') }
        if ($kind -notin @([Microsoft.Win32.RegistryValueKind]::String, [Microsoft.Win32.RegistryValueKind]::ExpandString)) { throw 'User PATH has an unsupported registry type; it was not modified.' }
        $updated = Get-TerminalNoteUpdatedPath -CurrentPath $raw -InstallDirectory $InstallDirectory -Remove:$Remove
        if (($null -ne $raw -or !$Remove) -and $raw -cne $updated) {
            $key.SetValue('Path', $updated, $kind)
            $changed = $true
        }
    } finally { $key.Dispose() }
    $env:Path = Get-TerminalNoteUpdatedPath -CurrentPath $env:Path -InstallDirectory $InstallDirectory -Remove:$Remove
    if ($changed) {
        if (!('TerminalNote.InstallerEnvironment' -as [type])) {
            Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace TerminalNote {
    public static class InstallerEnvironment {
        [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        public static extern IntPtr SendMessageTimeout(IntPtr window, uint message, UIntPtr wparam,
            string lparam, uint flags, uint timeout, out UIntPtr result);
    }
}
'@
        }
        $result = [UIntPtr]::Zero
        [void][TerminalNote.InstallerEnvironment]::SendMessageTimeout([IntPtr]0xffff, 0x1a, [UIntPtr]::Zero, 'Environment', 2, 3000, [ref]$result)
    }
}

function Set-TerminalNoteDefaultRoot {
    param([Parameter(Mandatory = $true)][string]$BinaryPath, [Parameter(Mandatory = $true)][string]$NotesRoot, [string]$ConfigFile)
    Assert-TerminalNoteSafePath -Path $NotesRoot -Directory
    if (!(Test-Path -LiteralPath $NotesRoot -PathType Container)) { throw 'The notes directory must already exist.' }
    $arguments = @('--set-default', $NotesRoot)
    if ($ConfigFile) {
        $ConfigFile = Get-TerminalNoteAbsolutePath -Path $ConfigFile
        Assert-TerminalNoteSafePath -Path $ConfigFile
        $arguments += @('--config-file', $ConfigFile)
    }
    # The CLI validates configuration and preserves language. Never edit TOML here.
    & $BinaryPath @arguments | Out-Host
    if ($LASTEXITCODE -ne 0) { throw 'Saving the notes default failed. Program files remain installed; configuration was not replaced by setup.' }
}
