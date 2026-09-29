# tideminer installer for Windows (x86-64), in PowerShell:
#
#   irm https://github.com/tidecoin/tideminer/releases/latest/download/install.ps1 | iex
#
# Downloads the Windows release, checks it against the release's SHA256SUMS, installs
# tideminer.exe into %LOCALAPPDATA%\Programs\tideminer and adds that folder to your user
# PATH (no administrator rights). Re-run to update.
#
# Environment:
#   TIDEMINER_VERSION      release tag to install, e.g. v0.2.0 (default: latest)
#   TIDEMINER_INSTALL_DIR  where to put tideminer.exe
#   TIDEMINER_REPO         GitHub owner/repo that publishes releases
#   TIDEMINER_BASE_URL     download from this URL instead of GitHub Releases

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'   # Invoke-WebRequest is far faster without it
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$repo = if ($env:TIDEMINER_REPO) { $env:TIDEMINER_REPO } else { 'tidecoin/tideminer' }
$base = if ($env:TIDEMINER_BASE_URL) { $env:TIDEMINER_BASE_URL }
        elseif ($env:TIDEMINER_VERSION) { "https://github.com/$repo/releases/download/$($env:TIDEMINER_VERSION)" }
        else { "https://github.com/$repo/releases/latest/download" }
$dir = if ($env:TIDEMINER_INSTALL_DIR) { $env:TIDEMINER_INSTALL_DIR }
       else { Join-Path $env:LOCALAPPDATA 'Programs\tideminer' }
$asset = 'tideminer-windows-x86_64.zip'

# throw, not exit: under `irm | iex`, exit would close the user's PowerShell window.
function Fail($message) { throw "tideminer install: $message" }

if (-not [Environment]::Is64BitOperatingSystem) { Fail 'needs 64-bit Windows' }
if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {
    Write-Host 'Windows on ARM: installing the x86-64 build (runs under emulation, slower).' -ForegroundColor Yellow
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("tideminer-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Write-Host "downloading $asset"
    Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile (Join-Path $tmp $asset)
    Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS')

    $line = Get-Content (Join-Path $tmp 'SHA256SUMS') |
        Where-Object { ($_ -split '\s+')[1] -in @($asset, "*$asset") } | Select-Object -First 1
    if (-not $line) { Fail "$asset is not listed in SHA256SUMS" }
    $expected = ($line -split '\s+')[0].ToLowerInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $tmp $asset)).Hash.ToLowerInvariant()
    if ($expected -ne $actual) { Fail "checksum mismatch for $asset (expected $expected, got $actual)" }
    Write-Host 'checksum ok'

    Expand-Archive -Path (Join-Path $tmp $asset) -DestinationPath $tmp -Force
    $exe = Join-Path $tmp 'tideminer.exe'
    if (-not (Test-Path $exe)) { Fail 'archive does not contain tideminer.exe' }

    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $target = Join-Path $dir 'tideminer.exe'
    try {
        Copy-Item $exe $target -Force
    } catch {
        Fail "could not replace $target (is tideminer running? stop it and re-run)"
    }
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

if (-not (Test-Path $target)) {
    Fail ("$target disappeared right after installing: Windows Defender probably quarantined it " +
          '(miners are often flagged as potentially unwanted). To allow it, run as administrator: ' +
          "Add-MpPreference -ExclusionPath '$dir'  and re-run this installer.")
}

& $target self-test | Out-Null
if ($LASTEXITCODE -ne 0) { Fail 'installed binary failed its self-test' }
Write-Host "installed $(& $target --version) to $target (self-test passed)" -ForegroundColor Green

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$parts = if ($userPath) { $userPath -split ';' } else { @() }
if ($parts -notcontains $dir) {
    [Environment]::SetEnvironmentVariable('Path', (($parts + $dir) -join ';').Trim(';'), 'User')
    $env:Path = "$env:Path;$dir"
    Write-Host "added $dir to your user PATH (new terminals pick it up)"
}

Write-Host ''
Write-Host 'start mining:'
Write-Host '  tideminer -o POOL_HOST:PORT --tls -u YOUR_TDC_ADDRESS.rig'
Write-Host 'tune for this machine first (optional, a few minutes):'
Write-Host '  tideminer tune'
Write-Host ''
Write-Host 'Windows Defender may flag or remove tideminer.exe as a potentially unwanted coin miner.'
Write-Host "If that happens, allow the folder (administrator PowerShell): Add-MpPreference -ExclusionPath '$dir'"
