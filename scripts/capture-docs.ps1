#Requires -Version 7.0
<#
.SYNOPSIS
Capture the actual Ratatui UI with temporary demo notes, then export PNGs offline.
.EXAMPLE
pwsh -NoProfile -File .\scripts\capture-docs.ps1
.EXAMPLE
# Rasterize existing SVGs only; does not invoke Cargo or touch application data.
pwsh -NoProfile -File .\scripts\capture-docs.ps1 -RasterizeOnly
.EXAMPLE
# Generate SVGs without launching a browser.
pwsh -NoProfile -File .\scripts\capture-docs.ps1 -SvgOnly
.NOTES
Uses installed Edge, no downloads. Edge receives a unique disposable profile under
%LOCALAPPDATA%\Temp\opencode, never your browser profile. Only docs/images outputs
and this script's own temporary directory are written. No real notes/config are read.
Run only after source integration is complete. SVG size determines PNG dimensions.
#>
[CmdletBinding()]
param(
    [switch] $RasterizeOnly,
    [switch] $SvgOnly,
    [string] $EdgePath
)
$ErrorActionPreference = 'Stop'
if ($RasterizeOnly -and $SvgOnly) { throw 'Choose -RasterizeOnly or -SvgOnly, not both.' }
$root = Split-Path -Parent $PSScriptRoot
$names = @('notes', 'editor', 'commands', 'links')

function Assert-Png([string] $Path, [int] $Width, [int] $Height) {
    if (!(Test-Path -LiteralPath $Path -PathType Leaf)) { throw "PNG was not created: $Path" }
    $bytes = [IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 33 -or [Convert]::ToHexString($bytes[0..7]) -ne '89504E470D0A1A0A' -or
        [Text.Encoding]::ASCII.GetString($bytes, 12, 4) -ne 'IHDR') {
        throw "Invalid PNG output: $Path"
    }
    $actualWidth = [uint32]$bytes[16] * 16777216 + [uint32]$bytes[17] * 65536 + [uint32]$bytes[18] * 256 + $bytes[19]
    $actualHeight = [uint32]$bytes[20] * 16777216 + [uint32]$bytes[21] * 65536 + [uint32]$bytes[22] * 256 + $bytes[23]
    if ($actualWidth -ne $Width -or $actualHeight -ne $Height) {
        throw "PNG dimensions ${actualWidth}x${actualHeight}; expected ${Width}x${Height}: $Path"
    }
}

Push-Location $root
try {
    if (!$RasterizeOnly) {
        # This ignored test invokes App::draw with synthetic notes under Temp\opencode.
        cargo test --locked --offline capture_documentation -- --ignored --test-threads=1
        if ($LASTEXITCODE -ne 0) { throw 'Ratatui screenshot capture failed. Existing PNGs were not changed.' }
    }
    if ($SvgOnly) { return }
    if (!$EdgePath) {
        $EdgePath = @(
            "${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe",
            "$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe",
            "$env:LOCALAPPDATA\Microsoft\Edge\Application\msedge.exe"
        ) | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
    }
    if (!$EdgePath -or !(Test-Path -LiteralPath $EdgePath -PathType Leaf)) {
        throw 'Microsoft Edge not found. SVG captures remain available in docs/images. Use -EdgePath for an installed executable.'
    }
    if (!$env:LOCALAPPDATA) { throw 'LOCALAPPDATA is required for an isolated, approved temporary Edge profile.' }
    $temporaryBase = Join-Path $env:LOCALAPPDATA 'Temp\opencode'
    [IO.Directory]::CreateDirectory($temporaryBase) | Out-Null
    $run = Join-Path $temporaryBase ('terminalnote-docs-' + [Guid]::NewGuid().ToString('N'))
    [IO.Directory]::CreateDirectory($run) | Out-Null
    $profile = Join-Path $run 'edge-profile'
    try {
        foreach ($name in $names) {
            $svg = (Resolve-Path -LiteralPath "docs/images/$name.svg").Path
            # Disable DTD resolution even though these SVGs are generated locally.
            $settings = [Xml.XmlReaderSettings]::new()
            $settings.DtdProcessing = [Xml.DtdProcessing]::Prohibit
            $settings.XmlResolver = $null
            $reader = [Xml.XmlReader]::Create($svg, $settings)
            try {
                $document = [Xml.XmlDocument]::new()
                $document.XmlResolver = $null
                $document.Load($reader)
            } finally { $reader.Dispose() }
            $width = 0
            $height = 0
            if ($document.DocumentElement.LocalName -ne 'svg' -or
                ![int]::TryParse($document.DocumentElement.GetAttribute('width'), [ref]$width) -or
                ![int]::TryParse($document.DocumentElement.GetAttribute('height'), [ref]$height) -or
                $width -lt 500 -or $height -lt 200 -or $width -gt 4096 -or $height -gt 4096) {
                throw "Invalid SVG capture dimensions: $svg"
            }
            # Accept only the exporter's inert, self-contained drawing vocabulary.
            # Do not turn an edited SVG with external resources into a network request.
            foreach ($element in $document.SelectNodes('//*')) {
                if ($element.LocalName -notin @('svg', 'title', 'rect', 'g', 'text')) { throw "Unsupported SVG element: $($element.LocalName)" }
                foreach ($attribute in $element.Attributes) {
                    if ($attribute.LocalName -match '^(on|href|style)' -or $attribute.Value -match '(?i)url\s*\(') {
                        throw "Active or external SVG content is not supported: $svg"
                    }
                }
            }
            $png = Join-Path $run "$name.png"
            $stdout = Join-Path $run "$name.stdout.log"
            $stderr = Join-Path $run "$name.stderr.log"
            $arguments = @(
                '--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
                '--hide-scrollbars', '--force-device-scale-factor=1', '--disable-background-networking',
                '--disable-component-update', '--disable-sync', '--disable-extensions', '--disable-default-apps',
                '--metrics-recording-only', '--no-proxy-server', '--host-resolver-rules="MAP * ~NOTFOUND"',
                '--virtual-time-budget=2000', '--run-all-compositor-stages-before-draw',
                "--window-size=$width,$height", "--user-data-dir=`"$profile`"", "--screenshot=`"$png`"",
                ([Uri]$svg).AbsoluteUri
            )
            $process = Start-Process -FilePath $EdgePath -ArgumentList $arguments -PassThru -NoNewWindow -RedirectStandardOutput $stdout -RedirectStandardError $stderr
            try {
                if (!$process.WaitForExit(60000)) {
                    $process.Kill($true)
                    $process.WaitForExit()
                    throw "Edge timed out while capturing $name."
                }
                if ($process.ExitCode -ne 0) {
                    throw "Edge failed for $name (exit $($process.ExitCode)): $([IO.File]::ReadAllText($stderr))"
                }
            } finally { $process.Dispose() }
            Assert-Png $png $width $height
            Write-Host "Rendered $name.png (${width}x${height})"
        }
        # Promote only after every fresh PNG passes validation; stale output cannot
        # masquerade as a successful capture. Existing SVGs are retained for review.
        foreach ($name in $names) {
            Copy-Item -LiteralPath (Join-Path $run "$name.png") -Destination (Join-Path $root "docs/images/$name.png") -Force
        }
    } finally {
        # Delete ONLY the unique directory created above, never a browser/user root.
        if (Test-Path -LiteralPath $run) { Remove-Item -LiteralPath $run -Recurse -Force -ErrorAction SilentlyContinue }
    }
} finally { Pop-Location }
