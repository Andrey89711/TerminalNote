[CmdletBinding()]
param([Parameter(Mandatory = $true)][string]$BinaryPath)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($env:OS -ne 'Windows_NT') { throw 'Windows is required.' }
$repo = Split-Path $PSScriptRoot -Parent
. (Join-Path $PSScriptRoot 'setup-common.ps1')
# CI supplies a repository-relative build path; production installer paths stay strict.
$BinaryPath = Get-TerminalNoteAbsolutePath -Path (Resolve-Path -LiteralPath $BinaryPath).ProviderPath
$version = Get-TerminalNoteBinaryVersion -BinaryPath $BinaryPath
$engine = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
$tempBase = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'Temp\opencode'
$work = Join-Path $tempBase ('terminalnote-setup-test-' + [guid]::NewGuid().ToString('N'))
$junctions = @()
$previousAlias = $env:TERMINALNOTE_SETUP_TEST_DIR
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
function Invoke-TestSetup {
    param([string[]]$Extra, [switch]$ExpectFailure)
    # Every entrypoint run is isolated: NEVER drop these safety arguments.
    $previousPreference = $ErrorActionPreference
    try {
        # Windows PowerShell treats redirected native stderr as error records;
        # expected validation failures must be checked by exit code, not thrown here.
        $ErrorActionPreference = 'Continue'
        $output = @(& $engine -NoLogo -NoProfile -ExecutionPolicy Bypass -File (Join-Path $package 'setup.ps1') -NonInteractive -SkipShellIntegration @Extra 2>&1 | ForEach-Object { $_.ToString() })
        $code = $LASTEXITCODE
    } finally { $ErrorActionPreference = $previousPreference }
    if (($ExpectFailure -and $code -eq 0) -or (!$ExpectFailure -and $code -ne 0)) { Write-Host ($output -join [Environment]::NewLine) }
    if ($ExpectFailure) { Assert-Test ($code -ne 0) 'setup must reject unsafe input' }
    else { Assert-Test ($code -eq 0) 'isolated setup entrypoint succeeds' }
}
try {
    [void][IO.Directory]::CreateDirectory($work)
    $package = Join-Path $work 'package with spaces'
    [void][IO.Directory]::CreateDirectory($package)
    foreach ($relative in @(Get-TerminalNotePayloadFiles)) {
        $inputPath = if ($relative -eq 'note.exe') { $BinaryPath } else { Join-Path $repo $relative }
        $outputPath = Join-Path $package $relative
        [void][IO.Directory]::CreateDirectory((Split-Path $outputPath -Parent))
        [IO.File]::Copy($inputPath, $outputPath)
    }
    foreach ($relative in @('setup.ps1', 'scripts/setup-common.ps1', 'scripts/uninstall-common.ps1')) {
        # Simulate unsigned scripts extracted from a downloaded Windows ZIP.
        Set-Content -LiteralPath (Join-Path $package $relative) -Stream Zone.Identifier -Value "[ZoneTransfer]`r`nZoneId=3" -Encoding ASCII
    }
    Write-TestText (Join-Path $package 'unowned-secret.txt') 'not a payload file'
    $binary = Join-Path $package 'note.exe'
    $root = Join-Path $work 'installed app'
    $env:TERMINALNOTE_SETUP_TEST_DIR = $root
    $raw = '%USERPROFILE%\bin;; "C:\Unrelated Path" ;%TERMINALNOTE_SETUP_TEST_DIR%;"' + $root.ToUpperInvariant() + '\";C:\Other;'
    $preserved = '%USERPROFILE%\bin;; "C:\Unrelated Path" ;C:\Other;'
    $updated = Get-TerminalNoteUpdatedPath -CurrentPath $raw -InstallDirectory $root
    Assert-Test ($updated -ceq ($root + ';' + $preserved)) 'PATH add preserves literal unrelated entries, empties and order'
    Assert-Test ((Get-TerminalNoteUpdatedPath -CurrentPath $updated -InstallDirectory $root) -ceq $updated) 'PATH repair is idempotent'
    Assert-Test ((Get-TerminalNoteUpdatedPath -CurrentPath $raw -InstallDirectory $root -Remove) -ceq $preserved) 'PATH removal changes only our entries'
    Assert-Test ((Get-TerminalNoteUpdatedPath -CurrentPath $null -InstallDirectory $root -Remove) -ceq '') 'missing PATH removal is empty'
    Assert-Throws { Get-TerminalNoteUpdatedPath -CurrentPath '' -InstallDirectory (Join-Path $work 'bad;path') } 'reject PATH injection'
    Assert-Throws { Get-TerminalNoteInstallRoot -Path 'C:\' } 'reject drive root'
    Assert-Throws { Get-TerminalNoteInstallRoot -Path 'C:relative' } 'reject drive-relative target'

    $foreign = Join-Path $work 'foreign'
    [void][IO.Directory]::CreateDirectory($foreign)
    Write-TestText (Join-Path $foreign 'notes.md') 'keep foreign notes'
    Assert-Throws { Install-TerminalNotePayload -SourceRoot $package -BinaryPath $binary -Destination $foreign } 'reject unrelated nonempty target'
    Assert-Test ((Get-Content -Raw -LiteralPath (Join-Path $foreign 'notes.md')) -ceq 'keep foreign notes') 'foreign data unchanged'
    Assert-Test (!(Test-Path -LiteralPath (Join-Path $foreign 'note.exe'))) 'rejection installed nothing'

    $missingPayload = Join-Path $package 'LICENSE'
    [IO.File]::Move($missingPayload, ($missingPayload + '.saved'))
    $incomplete = Join-Path $work 'incomplete'
    Assert-Throws { Install-TerminalNotePayload -SourceRoot $package -BinaryPath $binary -Destination $incomplete } 'reject incomplete package before creating target'
    Assert-Test (!(Test-Path -LiteralPath $incomplete)) 'incomplete package left target absent'
    [IO.File]::Move(($missingPayload + '.saved'), $missingPayload)

    Invoke-TestSetup -Extra @('-InstallDirectory', $root)
    Assert-Test ((Get-TerminalNoteInstallMarker -InstallDirectory $root).version -ceq $version) 'product/schema/version marker written'
    foreach ($relative in @(Get-TerminalNotePayloadFiles)) { Assert-Test (Test-Path -LiteralPath (Join-Path $root $relative) -PathType Leaf) "payload exists: $relative" }
    Assert-Test (!(Test-Path -LiteralPath (Join-Path $root 'unowned-secret.txt'))) 'unallowlisted source files are not installed'
    $markerPath = Join-Path $root '.terminalnote-install.json'
    $markerText = Get-Content -Raw -LiteralPath $markerPath
    $readme = Join-Path $root 'README.md'
    Write-TestText $readme 'repair this owned file'
    Write-TestText (Join-Path $root 'personal.md') 'keep personal notes'
    Invoke-TestSetup -Extra @('-InstallDirectory', $root)
    Assert-Test ((Get-FileHash -LiteralPath $readme).Hash -ceq (Get-FileHash -LiteralPath (Join-Path $package 'README.md')).Hash) 'repeat install repairs owned files'
    Assert-Test ((Get-Content -Raw -LiteralPath (Join-Path $root 'personal.md')) -ceq 'keep personal notes') 'repair preserves unknown files'
    [IO.File]::Delete((Join-Path $root 'LICENSE'))
    $null = Install-TerminalNotePayload -SourceRoot $package -BinaryPath $binary -Destination $root
    Assert-Test (Test-Path -LiteralPath (Join-Path $root 'LICENSE')) 'repair restores missing owned file'

    $installedBinary = Join-Path $root 'note.exe'
    $beforeHash = (Get-FileHash -LiteralPath $installedBinary).Hash
    $beforeReadme = Get-Content -Raw -LiteralPath $readme
    Write-TestText $markerPath (@{product = 'TerminalNote'; schema = 1; version = '999.0.0'} | ConvertTo-Json)
    Assert-Throws { Install-TerminalNotePayload -SourceRoot $package -BinaryPath $binary -Destination $root } 'downgrade refused'
    Assert-Test ((Get-FileHash -LiteralPath $installedBinary).Hash -ceq $beforeHash) 'downgrade leaves binary unchanged'
    foreach ($badMarker in @('{"product":"Other","schema":1,"version":"0.1.0"}', '{"product":"TerminalNote","schema":2,"version":"0.1.0"}', '{"product":"TerminalNote","schema":1,"version":"oops"}', '{}')) {
        Write-TestText $markerPath $badMarker
        Assert-Throws { Install-TerminalNotePayload -SourceRoot $package -BinaryPath $binary -Destination $root } 'invalid marker refused'
        Assert-Test ((Get-Content -Raw -LiteralPath $readme) -ceq $beforeReadme) 'invalid marker leaves payload unchanged'
    }
    Write-TestText $markerPath $markerText
    $lock = [IO.File]::Open($installedBinary, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    try { Assert-Throws { Install-TerminalNotePayload -SourceRoot $package -BinaryPath $binary -Destination $root } 'locked binary refuses repair' }
    finally { $lock.Dispose() }
    Assert-Test ((Get-FileHash -LiteralPath $installedBinary).Hash -ceq $beforeHash) 'locked binary not truncated or removed'
    Assert-Test ((Get-Content -Raw -LiteralPath $markerPath) -ceq $markerText) 'locked binary leaves ownership unchanged'
    Assert-Test ((Get-Content -Raw -LiteralPath $readme) -ceq $beforeReadme) 'locked binary leaves other payload unchanged'
    $beforeTime = (Get-Item -LiteralPath $installedBinary).LastWriteTimeUtc
    $lock = [IO.File]::Open($readme, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    try { Assert-Throws { Install-TerminalNotePayload -SourceRoot $package -BinaryPath $binary -Destination $root } 'locked secondary file preflight rejects before replacing binary' }
    finally { $lock.Dispose() }
    Assert-Test ((Get-Item -LiteralPath $installedBinary).LastWriteTimeUtc -eq $beforeTime) 'full preflight leaves binary untouched'

    $notes = Join-Path $work 'isolated notes'
    [void][IO.Directory]::CreateDirectory($notes)
    $noteFile = Join-Path $notes 'private.md'
    Write-TestText $noteFile 'never open or modify these fixture notes'
    $config = Join-Path $work 'isolated config.toml'
    Write-TestText $config "language = 'ru'`r`n"
    Invoke-TestSetup -Extra @('-InstallDirectory', $root, '-NotesRoot', $notes, '-ConfigFile', $config)
    Assert-Test ((Get-Content -Raw -LiteralPath $config) -match 'language\s*=\s*"ru"') 'CLI default selection preserves language'
    Assert-Test ((Get-Content -Raw -LiteralPath $config) -match 'root\s*=') 'CLI saved isolated notes root'
    Assert-Test ((Get-Content -Raw -LiteralPath $noteFile) -ceq 'never open or modify these fixture notes') 'notes content unchanged'
    $configBefore = Get-Content -Raw -LiteralPath $config
    Invoke-TestSetup -Extra @('-InstallDirectory', $root)
    Assert-Test ((Get-Content -Raw -LiteralPath $config) -ceq $configBefore) 'ordinary repair does not change config'
    Invoke-TestSetup -Extra @('-InstallDirectory', $root, '-NotesRoot', $notes) -ExpectFailure
    Assert-Test ((Get-Content -Raw -LiteralPath $config) -ceq $configBefore) 'skip-integration requires explicit config for notes'
    $newNotes = Join-Path $work 'new explicit notes'
    Invoke-TestSetup -Extra @('-InstallDirectory', $root, '-NotesRoot', $newNotes, '-ConfigFile', $config) -ExpectFailure
    Assert-Test (!(Test-Path -LiteralPath $newNotes)) 'missing notes never created implicitly'
    Invoke-TestSetup -Extra @('-InstallDirectory', $root, '-NotesRoot', $newNotes, '-ConfigFile', $config, '-CreateNotesDirectory')
    Assert-Test (Test-Path -LiteralPath $newNotes -PathType Container) 'explicit create option creates only isolated selected folder'
    Assert-Test (@(Get-ChildItem -LiteralPath $newNotes -Force).Count -eq 0) 'no sample notes or TUI launch'
    Assert-Test ((Get-Content -Raw -LiteralPath $config) -match 'language\s*=\s*"ru"') 'language retained when selecting newly created directory'
    Invoke-TestSetup -Extra @('-InstallDirectory', $root, '-NotesRoot', $newNotes, '-ConfigFile', $config, '-CreateNotesDirectory') -ExpectFailure
    Invoke-TestSetup -Extra @('-InstallDirectory', $root, '-NotesRoot', $root, '-ConfigFile', $config) -ExpectFailure
    $badConfig = Join-Path $work 'invalid config.toml'
    Write-TestText $badConfig "language = 'unsupported'"
    Invoke-TestSetup -Extra @('-InstallDirectory', $root, '-NotesRoot', $notes, '-ConfigFile', $badConfig) -ExpectFailure
    Assert-Test ((Get-Content -Raw -LiteralPath $badConfig) -ceq "language = 'unsupported'") 'invalid config never overwritten by setup'

    $cmdRoot = Join-Path $work 'cmd installed app'
    $process = New-Object Diagnostics.Process
    $process.StartInfo.FileName = $env:ComSpec
    $process.StartInfo.Arguments = '/d /s /c ""' + (Join-Path $package 'setup.cmd') + '" -NonInteractive -SkipShellIntegration -InstallDirectory "' + $cmdRoot + '""'
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.RedirectStandardOutput = $true
    $process.StartInfo.RedirectStandardError = $true
    try {
        [void]$process.Start()
        $cmdOutput = $process.StandardOutput.ReadToEnd()
        $cmdError = $process.StandardError.ReadToEnd()
        $process.WaitForExit()
        if ($process.ExitCode -ne 0) { Write-Host ($cmdOutput + $cmdError) }
        Assert-Test ($process.ExitCode -eq 0) 'unsigned setup.cmd works with spaces and noninteractive arguments'
    } finally { $process.Dispose() }
    Assert-Test (Test-Path -LiteralPath (Join-Path $cmdRoot 'note.exe')) 'cmd wrapper installs isolated payload'

    $junction = Join-Path $work 'linked parent'
    $junctions += $junction
    New-Item -ItemType Junction -Path $junction -Target $foreign | Out-Null
    Assert-Throws { Install-TerminalNotePayload -SourceRoot $package -BinaryPath $binary -Destination (Join-Path $junction 'app') } 'reject junction ancestor'
    Assert-Test (!(Test-Path -LiteralPath (Join-Path $foreign 'app'))) 'junction target unchanged'
    Assert-Test (@(Get-ChildItem -LiteralPath $root -Filter '.terminalnote-stage-*' -Force).Count -eq 0) 'staging directories cleaned without recursive removal'
    Write-Host "PASS: $checks setup checks (Windows PowerShell 5.1 entrypoints; no real PATH, Apps entry or user config changes)."
} finally {
    $env:TERMINALNOTE_SETUP_TEST_DIR = $previousAlias
    foreach ($junction in $junctions) {
        if (Test-Path -LiteralPath $junction) { [IO.Directory]::Delete($junction, $false) }
    }
    # Only this newly-created, GUID-named test fixture under approved Temp/opencode.
    if (Test-Path -LiteralPath $work) { Remove-Item -LiteralPath $work -Recurse -Force }
}
# Expected native failures must not leak into GitHub Actions' pwsh exit wrapper.
# Unhandled exceptions still terminate before reaching this successful exit.
exit 0
