[CmdletBinding()]
param([Parameter(Mandatory = $true)][string]$BinaryPath)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($env:OS -ne 'Windows_NT') { throw 'Windows is required.' }
$repo = Split-Path $PSScriptRoot -Parent
. (Join-Path $PSScriptRoot 'setup-common.ps1')
. (Join-Path $PSScriptRoot 'uninstall-common.ps1')
# Resolve the caller's build path before applying the strict installer validator.
$BinaryPath = Get-TerminalNoteAbsolutePath -Path (Resolve-Path -LiteralPath $BinaryPath).ProviderPath
$null = Get-TerminalNoteBinaryVersion -BinaryPath $BinaryPath
$engine = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
$tempBase = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'Temp\opencode'
$work = Join-Path $tempBase ('terminalnote-uninstall-test-' + [guid]::NewGuid().ToString('N'))
$junctions = @()
$checks = 0
function Assert-Test {
    param([bool]$Condition, [string]$Message)
    if (!$Condition) { throw "FAIL: $Message" }
    $script:checks++
}
function Assert-Throws {
    param([scriptblock]$Action, [string]$Message)
    $threw = $false
    try { & $Action | Out-Null } catch { $threw = $true }
    Assert-Test $threw $Message
}
function Write-TestText {
    param([string]$Path, [string]$Text)
    [IO.File]::WriteAllText($Path, $Text, (New-Object Text.UTF8Encoding($false)))
}
function New-TestInstall {
    param([string]$Name)
    $root = Join-Path $work $Name
    $null = Install-TerminalNotePayload -SourceRoot $repo -BinaryPath $BinaryPath -Destination $root
    $root
}
try {
    [void][IO.Directory]::CreateDirectory($work)
    $foreign = Join-Path $work 'foreign notes'
    [void][IO.Directory]::CreateDirectory($foreign)
    $foreignNote = Join-Path $foreign 'private.md'
    Write-TestText $foreignNote 'keep external notes'
    Assert-Throws { Remove-TerminalNotePayload -InstallDirectory $foreign } 'unmarked folder refused'
    Assert-Throws { Remove-TerminalNotePayload -InstallDirectory 'C:\' } 'drive root refused'
    Assert-Test ((Get-Content -Raw -LiteralPath $foreignNote) -ceq 'keep external notes') 'unmarked notes preserved'

    $root = New-TestInstall 'installed app'
    $marker = Join-Path $root '.terminalnote-install.json'
    $markerText = Get-Content -Raw -LiteralPath $marker
    $exe = Join-Path $root 'note.exe'
    $binaryHash = (Get-FileHash -LiteralPath $exe).Hash
    foreach ($badMarker in @('{"product":"Other","schema":1,"version":"0.1.0"}', '{"product":"TerminalNote","schema":2,"version":"0.1.0"}', '{"product":"TerminalNote","schema":"1","version":"0.1.0"}', '{}', 'not json')) {
        Write-TestText $marker $badMarker
        Assert-Throws { Remove-TerminalNotePayload -InstallDirectory $root } 'invalid ownership refused'
        Assert-Test ((Get-FileHash -LiteralPath $exe).Hash -ceq $binaryHash) 'invalid ownership deletes no binary'
        Assert-Test (Test-Path -LiteralPath (Join-Path $root 'uninstall.ps1')) 'invalid ownership deletes no scripts'
    }
    Write-TestText $marker $markerText
    $lock = [IO.File]::Open($exe, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    try { Assert-Throws { Remove-TerminalNotePayload -InstallDirectory $root } 'locked binary refuses uninstall' }
    finally { $lock.Dispose() }
    Assert-Test ((Get-FileHash -LiteralPath $exe).Hash -ceq $binaryHash) 'locked binary preserved'
    Assert-Test ((Get-Content -Raw -LiteralPath $marker) -ceq $markerText) 'locked failure retains marker for retry'
    Assert-Test (Test-Path -LiteralPath (Join-Path $root 'README.md')) 'locked failure leaves docs intact'
    $secondary = Join-Path $root 'CHANGELOG.md'
    $lock = [IO.File]::Open($secondary, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    try { Assert-Throws { Remove-TerminalNotePayload -InstallDirectory $root } 'all owned files preflighted before any removal' }
    finally { $lock.Dispose() }
    Assert-Test (Test-Path -LiteralPath $exe) 'secondary lock did not delete binary'
    $unexpectedDirectory = Join-Path $root 'LICENSE'
    [IO.File]::Delete($unexpectedDirectory)
    [void][IO.Directory]::CreateDirectory($unexpectedDirectory)
    Write-TestText (Join-Path $unexpectedDirectory 'notes.md') 'keep unexpected directory contents'
    Assert-Throws { Remove-TerminalNotePayload -InstallDirectory $root } 'directory in an owned file slot refused before deleting anything'
    Assert-Test (Test-Path -LiteralPath $exe) 'unexpected payload directory leaves binary intact'
    Assert-Test ((Get-Content -Raw -LiteralPath (Join-Path $unexpectedDirectory 'notes.md')) -ceq 'keep unexpected directory contents') 'unexpected directory content preserved'
    [IO.File]::Delete((Join-Path $unexpectedDirectory 'notes.md'))
    [IO.Directory]::Delete($unexpectedDirectory, $false)
    [IO.File]::Copy((Join-Path $repo 'LICENSE'), $unexpectedDirectory)

    $personal = Join-Path $root 'personal.md'
    $localConfig = Join-Path $root 'config.toml'
    $nested = Join-Path $root 'docs\my-notes'
    [void][IO.Directory]::CreateDirectory($nested)
    Write-TestText $personal 'keep installed-folder notes'
    Write-TestText $localConfig "language = 'ru'"
    Write-TestText (Join-Path $nested 'note.txt') 'keep nested unknown files'
    $externalConfig = Join-Path $work 'isolated config.toml'
    Write-TestText $externalConfig "language = 'ru'"
    $null = Set-TerminalNoteDefaultRoot -BinaryPath $exe -NotesRoot $foreign -ConfigFile $externalConfig
    $configBefore = Get-Content -Raw -LiteralPath $externalConfig
    # Execute a COPY of the installed uninstaller; never register PATH or Apps.
    $entryOutput = @(& $engine -NoLogo -NoProfile -ExecutionPolicy Bypass -File (Join-Path $root 'uninstall.ps1') -InstallDirectory $root -NonInteractive -SkipShellIntegration 2>&1)
    if ($LASTEXITCODE -ne 0) { Write-Host ($entryOutput -join [Environment]::NewLine) }
    Assert-Test ($LASTEXITCODE -eq 0) 'isolated Windows PowerShell 5.1 uninstall entrypoint succeeds'
    foreach ($relative in @(Get-TerminalNotePayloadFiles)) { Assert-Test (!(Test-Path -LiteralPath (Join-Path $root $relative))) "owned payload removed: $relative" }
    Assert-Test (!(Test-Path -LiteralPath $marker)) 'marker removed after payload'
    Assert-Test ((Get-Content -Raw -LiteralPath $personal) -ceq 'keep installed-folder notes') 'unknown markdown preserved'
    Assert-Test ((Get-Content -Raw -LiteralPath $localConfig) -ceq "language = 'ru'") 'unknown config preserved'
    Assert-Test ((Get-Content -Raw -LiteralPath (Join-Path $nested 'note.txt')) -ceq 'keep nested unknown files') 'nested notes and directories preserved'
    Assert-Test ((Get-Content -Raw -LiteralPath $foreignNote) -ceq 'keep external notes') 'external notes preserved'
    Assert-Test ((Get-Content -Raw -LiteralPath $externalConfig) -ceq $configBefore) 'external config including root/language preserved byte-for-byte'
    Assert-Throws { Remove-TerminalNotePayload -InstallDirectory $root } 'repeat unmarked removal refuses surviving user data'

    $empty = New-TestInstall 'clean removal'
    Remove-TerminalNotePayload -InstallDirectory $empty
    Assert-Test (!(Test-Path -LiteralPath $empty)) 'empty program directory removed nonrecursively'
    $partial = New-TestInstall 'partial install'
    [IO.File]::Delete((Join-Path $partial 'note.exe'))
    [IO.File]::Delete((Join-Path $partial 'LICENSE'))
    Remove-TerminalNotePayload -InstallDirectory $partial
    Assert-Test (!(Test-Path -LiteralPath $partial)) 'partial install can be uninstalled with valid marker'
    $launcher = New-TestInstall 'keep launcher'
    Remove-TerminalNotePayload -InstallDirectory $launcher -KeepLauncher
    Assert-Test (Test-Path -LiteralPath (Join-Path $launcher 'uninstall.cmd')) 'batch launcher kept only when requested'
    Assert-Test (@(Get-ChildItem -LiteralPath $launcher -Force).Count -eq 1) 'only launcher remains for cmd wrapper to delete'

    $cmdRoot = New-TestInstall 'cmd uninstall with spaces'
    Write-TestText (Join-Path $cmdRoot 'my-notes.md') 'keep notes during cmd self-removal'
    $process = New-Object Diagnostics.Process
    $process.StartInfo.FileName = $env:ComSpec
    $process.StartInfo.Arguments = '/d /s /c ""' + (Join-Path $cmdRoot 'uninstall.cmd') + '" -NonInteractive -SkipShellIntegration"'
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.RedirectStandardOutput = $true
    $process.StartInfo.RedirectStandardError = $true
    try {
        [void]$process.Start()
        $cmdOutput = $process.StandardOutput.ReadToEnd()
        $cmdError = $process.StandardError.ReadToEnd()
        $process.WaitForExit()
        if ($process.ExitCode -ne 0) { Write-Host ($cmdOutput + $cmdError) }
        Assert-Test ($process.ExitCode -eq 0) 'uninstall.cmd self-removal succeeds without prompts'
    } finally { $process.Dispose() }
    Assert-Test (!(Test-Path -LiteralPath (Join-Path $cmdRoot 'uninstall.cmd'))) 'cmd launcher removes itself only after success'
    Assert-Test ((Get-Content -Raw -LiteralPath (Join-Path $cmdRoot 'my-notes.md')) -ceq 'keep notes during cmd self-removal') 'cmd wrapper never recursively removes remaining notes'

    $linked = New-TestInstall 'linked directory attack'
    $docs = Join-Path $linked 'docs'
    [IO.File]::Delete((Join-Path $docs 'RELEASING.md'))
    [IO.Directory]::Delete($docs, $false)
    Write-TestText (Join-Path $foreign 'RELEASING.md') 'foreign release notes must survive'
    $junctions += $docs
    New-Item -ItemType Junction -Path $docs -Target $foreign | Out-Null
    Assert-Throws { Remove-TerminalNotePayload -InstallDirectory $linked } 'linked payload subdirectory refused before deletion'
    Assert-Test (Test-Path -LiteralPath (Join-Path $linked 'note.exe')) 'junction attack deletes no binary'
    Assert-Test ((Get-Content -Raw -LiteralPath (Join-Path $foreign 'RELEASING.md')) -ceq 'foreign release notes must survive') 'junction target untouched'
    $parentLink = Join-Path $work 'linked parent'
    $junctions += $parentLink
    New-Item -ItemType Junction -Path $parentLink -Target $work | Out-Null
    Assert-Throws { Remove-TerminalNotePayload -InstallDirectory (Join-Path $parentLink 'linked directory attack') } 'linked ancestor refused'
    Write-Host "PASS: $checks uninstall checks (allowlist only; no real PATH, Apps entry or user config changes)."
} finally {
    foreach ($junction in $junctions) {
        if (Test-Path -LiteralPath $junction) { [IO.Directory]::Delete($junction, $false) }
    }
    # Narrow cleanup: only our new GUID-named fixture under approved Temp/opencode.
    if (Test-Path -LiteralPath $work) { Remove-Item -LiteralPath $work -Recurse -Force }
}
# Expected native failures must not leak into GitHub Actions' pwsh exit wrapper.
# Unhandled exceptions still terminate before reaching this successful exit.
exit 0
