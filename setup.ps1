[CmdletBinding()]
param(
    [string]$InstallDirectory = (Join-Path $env:LOCALAPPDATA 'Programs\TerminalNote'),
    [string]$BinaryPath,
    [string]$NotesRoot,
    [string]$ConfigFile,
    [switch]$CreateNotesDirectory,
    [switch]$SkipShellIntegration,
    [switch]$NonInteractive
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
try {
    if ($env:OS -ne 'Windows_NT') { throw 'This setup supports Windows only.' }
    . (Join-Path $PSScriptRoot 'scripts/setup-common.ps1')
    . (Join-Path $PSScriptRoot 'scripts/uninstall-common.ps1')
    if (!$BinaryPath) { $BinaryPath = Join-Path $PSScriptRoot 'note.exe' }
    Write-Host 'TerminalNote per-user setup (no administrator rights required).'
    Write-Host 'Notes and configuration are kept when updating or uninstalling. The app will not be opened.'
    $InstallDirectory = Get-TerminalNoteInstallRoot -Path $InstallDirectory
    if (!$NonInteractive) {
        $entered = Read-Host "Program directory (Enter for $InstallDirectory)"
        if ($entered) { $InstallDirectory = Get-TerminalNoteInstallRoot -Path $entered }
        if (!$NotesRoot) {
            $NotesRoot = Read-Host 'Optional existing notes directory (Enter to keep your current default unchanged)'
        }
    }
    if ($CreateNotesDirectory -and !$NotesRoot) { throw '-CreateNotesDirectory requires -NotesRoot.' }
    if ($ConfigFile) { $ConfigFile = Get-TerminalNoteAbsolutePath -Path $ConfigFile }
    if ($SkipShellIntegration -and $NotesRoot -and !$ConfigFile) {
        throw 'With -SkipShellIntegration, selecting notes requires an explicit -ConfigFile. User configuration was not changed.'
    }
    if ($NotesRoot) {
        $NotesRoot = Get-TerminalNoteAbsolutePath -Path $NotesRoot
        Assert-TerminalNoteSeparateNotes -NotesRoot $NotesRoot -InstallDirectory $InstallDirectory
        Assert-TerminalNoteSafePath -Path $NotesRoot -Directory
        if (!(Test-Path -LiteralPath $NotesRoot -PathType Container)) {
            if (!$CreateNotesDirectory) {
                if ($NonInteractive -or (Read-Host "Directory does not exist. Explicitly create $NotesRoot ? [y/N]") -ine 'y') {
                    throw 'Notes directory does not exist. Use -CreateNotesDirectory only if you want to create it.'
                }
                $CreateNotesDirectory = $true
            }
        } elseif ($CreateNotesDirectory) {
            throw '-CreateNotesDirectory is only for a NEW directory; no existing notes were modified.'
        }
        $effectiveConfig = $ConfigFile
        if (!$effectiveConfig) {
            $effectiveConfig = if ($env:TERMINALNOTE_CONFIG) { $env:TERMINALNOTE_CONFIG } else { Join-Path ([Environment]::GetFolderPath('ApplicationData')) 'terminalnote\config.toml' }
        }
        $effectiveConfig = Get-TerminalNoteAbsolutePath -Path $effectiveConfig
        Assert-TerminalNoteSeparateNotes -NotesRoot $effectiveConfig -InstallDirectory $InstallDirectory
        Assert-TerminalNoteSafePath -Path $effectiveConfig
    }
    if (!$SkipShellIntegration) {
        $null = Get-TerminalNoteUpdatedPath -CurrentPath '' -InstallDirectory $InstallDirectory
        Assert-TerminalNoteRegistration -InstallDirectory $InstallDirectory
        Write-Host 'Setup will add note to your user PATH and register TerminalNote in Windows Settings > Apps.'
    }
    if (!$NonInteractive -and (Read-Host "Install/repair TerminalNote in $InstallDirectory ? [y/N]") -ine 'y') { throw 'Setup cancelled.' }
    $installed = Install-TerminalNotePayload -SourceRoot $PSScriptRoot -BinaryPath $BinaryPath -Destination $InstallDirectory
    if ($NotesRoot) {
        if ($CreateNotesDirectory) {
            Assert-TerminalNoteSafePath -Path $NotesRoot -Directory
            if (Test-Path -LiteralPath $NotesRoot) { throw 'Notes path appeared during setup. Nothing at that path was changed.' }
            [void][IO.Directory]::CreateDirectory($NotesRoot)
        }
        Set-TerminalNoteDefaultRoot -BinaryPath $installed -NotesRoot $NotesRoot -ConfigFile $effectiveConfig
    }
    if (!$SkipShellIntegration) {
        Set-TerminalNoteUserPath -InstallDirectory $InstallDirectory
        Register-TerminalNoteUninstaller -InstallDirectory $InstallDirectory
        Write-Host 'Ready. Open a NEW terminal and run: note'
    } else { Write-Host "Ready. Program: $installed (shell integration skipped)." }
    Write-Host 'Uninstall using Windows Settings > Apps > TerminalNote, or uninstall.cmd in the program directory.'
    Write-Host 'Your notes and configuration will be preserved.'
} catch {
    Write-Error -Message $_.Exception.Message -ErrorAction Continue
    exit 1
}
