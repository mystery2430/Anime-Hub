#Requires -Version 5.1
# Signs a single Windows artifact for the Tauri bundler.
#
# The bundler invokes this through bundle.windows.signCommand
# (src-tauri/tauri.windows-signing.conf.json) once per signable file: the app
# executable, the NSIS installer and the NSIS uninstaller (via the
# !uninstfinalize hook). The target path is passed as the only argument.
#
# Credential modes, checked in order:
#
#   1. WINDOWS_SIGN_CMD    - full vendor command template (jsign, SSL.com
#                            eSigner, trusted-signing-cli, ...). `%1` inside
#                            the template is replaced by the quoted target
#                            path; without `%1` the path is appended.
#                            Keep passwords OUT of the template: let the
#                            vendor tool read them from its own environment
#                            variables (e.g. SSL_COM_PASSWORD), which must be
#                            forwarded in the workflow env block.
#   2. WINDOWS_CERTIFICATE - base64-encoded .pfx plus
#                            WINDOWS_CERTIFICATE_PASSWORD, signed with
#                            signtool. Only works when the certificate private
#                            key is exportable; post-2023 CA/B hardware-token
#                            certificates usually need mode 1 instead.
#
# Timestamping always uses WINDOWS_TIMESTAMP_URL (RFC 3161, default DigiCert)
# so signatures stay valid after the certificate expires.
#
# The script fails hard when no credentials are configured or the resulting
# signature is not valid, so a release can never silently ship unsigned.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$FilePath
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path -LiteralPath $FilePath)) {
    throw "sign target does not exist: $FilePath"
}

$TimestampUrl = if ($env:WINDOWS_TIMESTAMP_URL) { $env:WINDOWS_TIMESTAMP_URL } else { 'http://timestamp.digicert.com' }

function Find-SignTool {
    $cmd = Get-Command 'signtool.exe' -ErrorAction SilentlyContinue
    if ($null -ne $cmd) {
        return $cmd.Source
    }
    $kitsKeys = @(
        'HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots',
        'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows Kits\Installed Roots'
    )
    foreach ($key in $kitsKeys) {
        if (-not (Test-Path -LiteralPath $key)) {
            continue
        }
        $root = Get-ItemPropertyValue -LiteralPath $key -Name 'KitsRoot10' -ErrorAction SilentlyContinue
        if (-not $root) {
            continue
        }
        $binDir = Join-Path -Path $root -ChildPath 'bin'
        if (-not (Test-Path -LiteralPath $binDir)) {
            continue
        }
        $candidate = Get-ChildItem -Path $binDir -Directory -ErrorAction SilentlyContinue |
            Sort-Object -Property Name -Descending |
            ForEach-Object { Join-Path -Path $_.FullName -ChildPath 'x64\signtool.exe' } |
            Where-Object { Test-Path -LiteralPath $_ } |
            Select-Object -First 1
        if ($candidate) {
            return $candidate
        }
    }
    throw 'signtool.exe not found (Windows SDK required)'
}

function Invoke-CustomSign([string]$Target) {
    $template = $env:WINDOWS_SIGN_CMD
    $quoted = '"' + $Target + '"'
    if ($template.IndexOf('%1') -ge 0) {
        $commandLine = $template.Replace('%1', $quoted)
    } else {
        $commandLine = $template + ' ' + $quoted
    }
    Write-Host "[sign_windows] custom sign command for $Target"
    cmd.exe /c $commandLine
    if ($LASTEXITCODE -ne 0) {
        throw "custom sign command failed with exit code $LASTEXITCODE"
    }
}

function Invoke-PfxSign([string]$Target) {
    $signtool = Find-SignTool
    $pfxPath = Join-Path -Path ([System.IO.Path]::GetTempPath()) -ChildPath ('animehub-sign-' + [Guid]::NewGuid().ToString('N') + '.pfx')
    try {
        $bytes = [System.Convert]::FromBase64String($env:WINDOWS_CERTIFICATE)
        [System.IO.File]::WriteAllBytes($pfxPath, $bytes)
        $signArgs = @('sign', '/fd', 'SHA256', '/tr', $TimestampUrl, '/td', 'SHA256', '/d', 'AnimeHub', '/f', $pfxPath)
        if ($env:WINDOWS_CERTIFICATE_PASSWORD) {
            $signArgs += @('/p', $env:WINDOWS_CERTIFICATE_PASSWORD)
        }
        $signArgs += @($Target)
        Write-Host "[sign_windows] signtool sign for $Target"
        & $signtool @signArgs
        if ($LASTEXITCODE -ne 0) {
            throw "signtool failed with exit code $LASTEXITCODE"
        }
    } finally {
        if (Test-Path -LiteralPath $pfxPath) {
            Remove-Item -LiteralPath $pfxPath -Force
        }
    }
}

if ($env:WINDOWS_SIGN_CMD) {
    Invoke-CustomSign -Target $FilePath
} elseif ($env:WINDOWS_CERTIFICATE) {
    Invoke-PfxSign -Target $FilePath
} else {
    throw 'no signing credentials: set WINDOWS_SIGN_CMD or WINDOWS_CERTIFICATE (+ WINDOWS_CERTIFICATE_PASSWORD)'
}

# Belt and braces: whatever tool signed the file, the result must carry a
# valid, timestamped Authenticode signature before it ships.
$signature = Get-AuthenticodeSignature -LiteralPath $FilePath
if ($signature.Status -ne 'Valid') {
    throw "signature status for $FilePath is $($signature.Status), expected Valid"
}
if ($null -eq $signature.TimeStamperCertificate) {
    throw "signature for $FilePath has no timestamp (WINDOWS_TIMESTAMP_URL: $TimestampUrl)"
}
Write-Host "[sign_windows] signed and timestamped: $FilePath"
