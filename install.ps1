# Install or update Luma on Windows
# Usage: irm https://raw.githubusercontent.com/nghyane/luma/master/install.ps1 | iex
# Compatible with Windows PowerShell 5.1 and PowerShell Core 7+.
# Requires Windows 10+ (uses built-in curl.exe and tar.exe).
$ErrorActionPreference = 'Stop'

# TLS 1.2 required by GitHub; Windows PowerShell 5.1 defaults to TLS 1.0.
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Repo = 'nghyane/luma'
if ($env:LUMA_INSTALL_DIR) { $InstallDir = $env:LUMA_INSTALL_DIR } else { $InstallDir = "$env:USERPROFILE\.local\bin" }
$Target = 'x86_64-pc-windows-msvc'

# --- Resolve version --------------------------------------------------------

if ($env:LUMA_VERSION) {
    $Tag = $env:LUMA_VERSION
} else {
    # Use curl.exe to avoid Invoke-RestMethod quirks across PS versions.
    $json = curl.exe --ssl-revoke-best-effort -s "https://api.github.com/repos/$Repo/releases?per_page=1"
    $Tag = ($json | ConvertFrom-Json)[0].tag_name
}

if (-not $Tag) {
    Write-Error 'Failed to detect latest version'
    exit 1
}

# --- Download & extract ------------------------------------------------------

$Url = "https://github.com/$Repo/releases/download/$Tag/luma-$Target.zip"

Write-Host "Installing luma $Tag ($Target)"
Write-Host "  from: $Url"
Write-Host "  to:   $InstallDir\luma.exe"

# Temp directory via .NET (works on all PS versions).
$TmpFile = [System.IO.Path]::GetTempFileName()
Remove-Item $TmpFile
$Tmp = New-Item -ItemType Directory -Path $TmpFile

try {
    $ZipPath = Join-Path $Tmp 'luma.zip'

    # Prefer curl.exe + tar.exe (built into Windows 10 1803+); no PS cmdlet issues.
    curl.exe --fail --location --progress-bar --output $ZipPath $Url
    if ($LASTEXITCODE -ne 0) { throw "Download failed (exit code $LASTEXITCODE)" }

    tar.exe xf $ZipPath -C $Tmp
    if ($LASTEXITCODE -ne 0) { throw "Extract failed (exit code $LASTEXITCODE)" }

    # Install
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    $Dest = Join-Path $InstallDir 'luma.exe'
    if (Test-Path $Dest) {
        # The running binary may hold a lock; Windows allows renaming a locked file.
        $Old = Join-Path $InstallDir 'luma.exe.old'
        if (Test-Path $Old) { Remove-Item -Force $Old -ErrorAction SilentlyContinue }
        Rename-Item -Path $Dest -NewName 'luma.exe.old' -Force -ErrorAction SilentlyContinue
    }
    Move-Item -Path (Join-Path $Tmp 'luma.exe') -Destination $Dest -Force

    # Clean up old binary (may fail if process still running; harmless).
    Remove-Item -Force (Join-Path $InstallDir 'luma.exe.old') -ErrorAction SilentlyContinue

    Write-Host "Installed luma $Tag"
} finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}

# --- PATH setup --------------------------------------------------------------

$User = [System.EnvironmentVariableTarget]::User
$UserPath = [System.Environment]::GetEnvironmentVariable('Path', $User)
if (";${UserPath};".ToLower() -notlike "*;$($InstallDir.ToLower());*") {
    [System.Environment]::SetEnvironmentVariable('Path', "${InstallDir};${UserPath}", $User)
    $env:PATH = "${InstallDir};${env:PATH}"
    Write-Host "Added $InstallDir to user PATH"

    # Broadcast WM_SETTINGCHANGE so Explorer, cmd.exe, and other apps pick up
    # the new PATH without requiring logoff.
    try {
        $Def = @'
[DllImport("user32.dll", SetLastError=true, CharSet=CharSet.Auto)]
public static extern IntPtr SendMessageTimeout(
    IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam,
    uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
'@
        $Broadcast = Add-Type -MemberDefinition $Def -Name 'Win32Env' -Namespace 'Luma' -PassThru -ErrorAction SilentlyContinue
        $result = [UIntPtr]::Zero
        $Broadcast::SendMessageTimeout([IntPtr]0xFFFF, 0x001A, [UIntPtr]::Zero, 'Environment', 2, 5000, [ref]$result) | Out-Null
    } catch {
        # Non-critical; user can restart terminal.
    }

    Write-Host ''
    Write-Host 'Restart your terminal, or run:'
    Write-Host "  cmd:        set ""PATH=$InstallDir;%PATH%"""
    Write-Host "  powershell: `$env:PATH = ""$InstallDir;`$env:PATH"""
}
