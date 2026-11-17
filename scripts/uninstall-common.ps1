Set-StrictMode -Version Latest

function Get-TerminalNoteRemovalPlan {
    param([Parameter(Mandatory = $true)][string]$InstallDirectory)
    $root = Get-TerminalNoteInstallRoot -Path $InstallDirectory
    $null = Get-TerminalNoteInstallMarker -InstallDirectory $root
    $files = @()
    foreach ($relative in (@(Get-TerminalNotePayloadFiles) + @('.terminalnote-install.json'))) {
        $path = Join-Path $root $relative
        Assert-TerminalNoteSafePath -Path $path
        if (Test-Path -LiteralPath $path) {
            Assert-TerminalNoteUnlocked -Path $path
            if ($relative -ne '.terminalnote-install.json') { $files += $path }
        }
    }
    [pscustomobject]@{ Root = $root; Files = $files; Marker = (Join-Path $root '.terminalnote-install.json') }
}

function Remove-TerminalNotePayload {
    param([Parameter(Mandatory = $true)][string]$InstallDirectory, [switch]$KeepLauncher)
    $plan = Get-TerminalNoteRemovalPlan -InstallDirectory $InstallDirectory
    foreach ($path in $plan.Files) {
        if ($KeepLauncher -and $path -ieq (Join-Path $plan.Root 'uninstall.cmd')) { continue }
        Assert-TerminalNoteSafePath -Path $path
        [IO.File]::Delete($path)
    }
    # Keep ownership on any earlier failure, allowing retry. Never enumerate user
    # content for deletion or remove directories recursively, even inside install.
    [IO.File]::Delete($plan.Marker)
    Remove-TerminalNoteEmptyDirectories -Root $plan.Root
}

function Assert-TerminalNoteRegistration {
    param([Parameter(Mandatory = $true)][string]$InstallDirectory)
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Software\Microsoft\Windows\CurrentVersion\Uninstall\TerminalNote')
    if ($null -ne $key) {
        try {
            if ($key.GetValue('InstallLocation', '') -ine $InstallDirectory -or $key.GetValue('DisplayName', '') -cne 'TerminalNote') {
                throw 'Another TerminalNote installation owns the Apps entry. Uninstall it first, or use -SkipShellIntegration.'
            }
        } finally { $key.Dispose() }
    }
}

function Register-TerminalNoteUninstaller {
    param([Parameter(Mandatory = $true)][string]$InstallDirectory)
    $root = Get-TerminalNoteInstallRoot -Path $InstallDirectory
    Assert-TerminalNoteRegistration -InstallDirectory $root
    $marker = Get-TerminalNoteInstallMarker -InstallDirectory $root
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Software\Microsoft\Windows\CurrentVersion\Uninstall\TerminalNote')
    try {
        $powershell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
        $command = '"' + $powershell + '" -NoLogo -NoProfile -ExecutionPolicy Bypass -File "' + (Join-Path $root 'uninstall.ps1') + '" -InstallDirectory "' + $root + '"'
        $key.SetValue('DisplayName', 'TerminalNote')
        $key.SetValue('DisplayVersion', $marker.version)
        $key.SetValue('Publisher', 'TerminalNote project')
        $key.SetValue('InstallLocation', $root)
        $key.SetValue('UninstallString', $command)
        $key.SetValue('QuietUninstallString', ($command + ' -NonInteractive'))
        $key.SetValue('DisplayIcon', (Join-Path $root 'note.exe'))
        $key.SetValue('NoModify', 1, [Microsoft.Win32.RegistryValueKind]::DWord)
        $key.SetValue('NoRepair', 1, [Microsoft.Win32.RegistryValueKind]::DWord)
    } finally { $key.Dispose() }
}

function Remove-TerminalNoteIntegrations {
    param([Parameter(Mandatory = $true)][string]$InstallDirectory)
    $root = Get-TerminalNoteInstallRoot -Path $InstallDirectory
    # Only remove registration owned by this location, never a newer/other install.
    $keyPath = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\TerminalNote'
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($keyPath)
    $owned = $false
    if ($null -ne $key) {
        try { $owned = $key.GetValue('InstallLocation', '') -ieq $root -and $key.GetValue('DisplayName', '') -ceq 'TerminalNote' }
        finally { $key.Dispose() }
    }
    Set-TerminalNoteUserPath -InstallDirectory $root -Remove
    if ($owned) { [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKey($keyPath, $false) }
}
