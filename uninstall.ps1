[CmdletBinding()]
param(
    [string]$InstallDirectory,
    [switch]$NonInteractive,
    [switch]$SkipShellIntegration,
    [switch]$KeepLauncher
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
try {
    if ($env:OS -ne 'Windows_NT') { throw 'This uninstaller supports Windows only.' }
    . (Join-Path $PSScriptRoot 'scripts/setup-common.ps1')
    . (Join-Path $PSScriptRoot 'scripts/uninstall-common.ps1')
    if (!$InstallDirectory) { $InstallDirectory = $PSScriptRoot }
    $InstallDirectory = Get-TerminalNoteInstallRoot -Path $InstallDirectory
    $null = Get-TerminalNoteRemovalPlan -InstallDirectory $InstallDirectory
    Write-Host "Remove TerminalNote program files from $InstallDirectory"
    Write-Host 'Notes, configuration and all unrecognized files will be kept.'
    if (!$NonInteractive -and (Read-Host 'Uninstall TerminalNote? [y/N]') -ine 'y') { throw 'Uninstall cancelled.' }
    # Removing a batch launcher while cmd.exe is reading it is unsafe. Its wrapper
    # removes that one file itself after successful uninstall.
    $keep = $KeepLauncher -and ($InstallDirectory -ieq [IO.Path]::GetFullPath($PSScriptRoot).TrimEnd('\', '/'))
    # Preflight above catches known locks before integration changes. If removing
    # integration fails, leave program files and the ownership marker available.
    if (!$SkipShellIntegration) { Remove-TerminalNoteIntegrations -InstallDirectory $InstallDirectory }
    Remove-TerminalNotePayload -InstallDirectory $InstallDirectory -KeepLauncher:$keep
    Write-Host 'TerminalNote uninstalled. Notes and configuration were not touched.'
} catch {
    Write-Error -Message $_.Exception.Message -ErrorAction Continue
    exit 1
}
