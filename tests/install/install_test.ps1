# Tests install.ps1 against a fake dev feed served from a scratch dir, with scratch
# LOCALAPPDATA, APPDATA, USERPROFILE and HOME.
# usage: tests/install/install_test.ps1 -Binary <fonsviva.exe> -Version <version it was built with>
# CI builds the binary with FONSVIVA_VERSION=<version> so `--version` proves the stamp too.
# The case that edits the user PATH runs only on CI (GITHUB_ACTIONS=true) and restores it.
param(
    [Parameter(Mandatory)] [string]$Binary,
    [Parameter(Mandatory)] [string]$Version
)
$ErrorActionPreference = 'Stop'
$Binary = (Resolve-Path $Binary).Path
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$installer = Join-Path $repo 'install.ps1'
$target = 'x86_64-pc-windows-msvc'

$scratch = Join-Path ([IO.Path]::GetTempPath()) ("fv-install-test-" + [Guid]::NewGuid().ToString('N'))
$srv = Join-Path $scratch 'srv'
$name = "fonsviva-$Version-$target"
New-Item -ItemType Directory -Path (Join-Path $srv "pkg\$name"), (Join-Path $srv 'dev'), (Join-Path $srv 'bad') | Out-Null
Copy-Item $Binary (Join-Path $srv "pkg\$name\fonsviva.exe")
$zip = Join-Path $srv "$name.zip"
Compress-Archive -Path (Join-Path $srv "pkg\$name") -DestinationPath $zip
$sum = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLowerInvariant()

$l = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
$l.Start(); $port = $l.LocalEndpoint.Port; $l.Stop()
$url = "http://127.0.0.1:$port"
function Write-Feed($dir, $sha) {
    [ordered]@{
        channel = 'dev'; version = $Version; sha = ('0' * 40); published_at = '2000-01-01T00:00:00Z'
        assets = @{ $target = [ordered]@{ url = "$url/$name.zip"; sha256 = $sha } }
    } | ConvertTo-Json -Depth 5 | Set-Content -Encoding ascii (Join-Path $srv "$dir\latest.json")
}
Write-Feed 'dev' $sum
Write-Feed 'bad' ('0' * 64)

$python = (Get-Command python -ErrorAction SilentlyContinue) ?? (Get-Command python3)
$server = Start-Process -FilePath $python.Source -ArgumentList '-m', 'http.server', '--bind', '127.0.0.1', '--directory', $srv, $port -PassThru -WindowStyle Hidden
foreach ($i in 1..50) {
    try { Invoke-WebRequest -UseBasicParsing "$url/dev/latest.json" | Out-Null; break } catch { Start-Sleep -Milliseconds 100 }
}

$script:fails = 0
function Pass($what) { Write-Host "ok   $what" }
function Fail($what) { Write-Host "FAIL $what"; $script:fails++ }

# Runs install.ps1 in a fresh process with scratch user dirs. Returns @{ code; out; root }.
function Invoke-Installer($case, $feed, $shell, [string[]]$installerArgs, [hashtable]$extraEnv = @{}) {
    $root = Join-Path $scratch $case
    New-Item -ItemType Directory -Force -Path $root | Out-Null
    $vars = @{
        LOCALAPPDATA = "$root\LocalAppData"; APPDATA = "$root\AppData"; USERPROFILE = $root; HOME = $root
        FONSVIVA_HOME = "$root\fonsviva"; FONSVIVA_PORT = '0'; FONSVIVA_DOWNLOAD_BASE = "$url/$feed"
    } + $extraEnv
    # Set and restore through the env: drive. A $null passed to a .NET string parameter
    # arrives as "", so [Environment]::SetEnvironmentVariable can't tell unset from empty.
    $saved = @{}
    foreach ($k in $vars.Keys) {
        $saved[$k] = (Get-Item "env:$k" -ErrorAction SilentlyContinue).Value
        if ($vars[$k]) { Set-Item "env:$k" $vars[$k] } else { Remove-Item "env:$k" -ErrorAction SilentlyContinue }
    }
    $eap = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $out = & $shell -NoProfile -ExecutionPolicy Bypass -File $installer @installerArgs 2>&1 | Out-String
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $eap
        foreach ($k in $saved.Keys) {
            if ($null -ne $saved[$k]) { Set-Item "env:$k" $saved[$k] } else { Remove-Item "env:$k" -ErrorAction SilentlyContinue }
        }
    }
    @{ code = $code; out = $out; root = $root }
}

$hostArch = $env:PROCESSOR_ARCHITECTURE
Write-Host "host CPU: PROCESSOR_ARCHITECTURE=$env:PROCESSOR_ARCHITECTURE PROCESSOR_ARCHITEW6432=$env:PROCESSOR_ARCHITEW6432"
try {
    # 1. The good feed installs a binary that runs and reports the feed's version, under the
    #    Windows PowerShell a clean machine has and under pwsh.
    foreach ($shell in 'powershell.exe', 'pwsh') {
        $r = Invoke-Installer "good-$shell" 'dev' $shell @('-NoModifyPath')
        $exe = Join-Path $r.root 'LocalAppData\Fonsviva\bin\fonsviva.exe'
        if ($r.code -eq 0) { Pass "$shell exits 0" } else { Fail "$shell exited $($r.code): $($r.out)" }
        if ((Test-Path $exe) -and ((Get-FileHash $exe).Hash -eq (Get-FileHash $Binary).Hash)) { Pass "$shell puts fonsviva.exe in LOCALAPPDATA\Fonsviva\bin" } else { Fail "$shell left no matching fonsviva.exe" }
        $got = if (Test-Path $exe) { (& $exe --version | Out-String).Trim() } else { '' }
        if ($got -eq "fonsviva $Version") { Pass "$shell : --version prints fonsviva $Version" } else { Fail "$shell : --version printed '$got'" }
        if ($r.out -match [regex]::Escape("& `"$exe`" setup --yes; & `"$exe`" ask")) { Pass "$shell prints the next step with the full path" } else { Fail "$shell next step missing: $($r.out)" }
        if (-not (Test-Path (Join-Path $r.root 'LocalAppData\Fonsviva\bin\fonsviva.new.exe'))) { Pass "$shell leaves no staged file" } else { Fail "$shell left fonsviva.new.exe" }
    }
    # Installing again over an existing copy works.
    $r = Invoke-Installer 'good-powershell.exe' 'dev' 'powershell.exe' @('-NoModifyPath')
    if ($r.code -eq 0) { Pass 'reinstall replaces the binary' } else { Fail "reinstall failed: $($r.out)" }

    # 2. A sha256 that doesn't match is refused and nothing is installed.
    $r = Invoke-Installer 'bad' 'bad' 'powershell.exe' @('-NoModifyPath')
    if ($r.code -ne 0) { Pass 'bad sha256 exits non-zero' } else { Fail 'bad sha256 was accepted' }
    if ($r.out -match 'sha256' -and $r.out -match 'Nothing was installed') { Pass 'bad sha256 says why' } else { Fail "bad sha256 message: $($r.out)" }
    if (-not (Test-Path (Join-Path $r.root 'LocalAppData\Fonsviva\bin\fonsviva.exe'))) { Pass 'bad sha256 installs nothing' } else { Fail 'bad sha256 installed fonsviva.exe' }

    # 3. An unknown CPU gets a plain error before any download.
    $r = Invoke-Installer 'arch' 'dev' 'powershell.exe' @('-NoModifyPath') @{ PROCESSOR_ARCHITECTURE = 'IA64'; PROCESSOR_ARCHITEW6432 = '' }
    if ($r.code -ne 0) { Pass 'unknown arch exits non-zero' } else { Fail 'unknown arch was accepted' }
    if ($r.out -match 'no build for Windows on IA64') { Pass 'unknown arch says why' } else { Fail "unknown arch message: $($r.out)" }
    if (-not (Test-Path (Join-Path $r.root 'LocalAppData\Fonsviva'))) { Pass 'unknown arch installs nothing' } else { Fail 'unknown arch created the install dir' }

    if ($env:PROCESSOR_ARCHITECTURE -eq $hostArch) { Pass 'host CPU variable restored' } else { Fail "PROCESSOR_ARCHITECTURE is '$env:PROCESSOR_ARCHITECTURE' after the arch case" }

    # 4. Channels that aren't published refuse plainly.
    foreach ($ch in 'stable', 'nightly') {
        $r = Invoke-Installer $ch 'dev' 'powershell.exe' @('-Channel', $ch, '-NoModifyPath')
        if ($r.code -ne 0 -and $r.out -match "$ch channel is not published yet") { Pass "-Channel $ch refuses" } else { Fail "-Channel $ch : $($r.code) $($r.out)" }
    }

    # 5. Without -NoModifyPath the install dir joins the user PATH once. CI runners only.
    if ($env:GITHUB_ACTIONS -eq 'true') {
        $before = [Environment]::GetEnvironmentVariable('Path', 'User')
        try {
            $r = Invoke-Installer 'path' 'dev' 'powershell.exe' @()
            $null = Invoke-Installer 'path' 'dev' 'powershell.exe' @()
            $dir = Join-Path $r.root 'LocalAppData\Fonsviva\bin'
            $hits = @([Environment]::GetEnvironmentVariable('Path', 'User') -split ';' | Where-Object { $_ -eq $dir }).Count
            if ($r.code -eq 0 -and $hits -eq 1) { Pass 'user PATH gains the install dir once' } else { Fail "user PATH has the install dir $hits times: $($r.out)" }
            if ($r.out -match 'fonsviva setup --yes; fonsviva ask') { Pass 'next step uses plain fonsviva once on PATH' } else { Fail "next step: $($r.out)" }
        } finally {
            [Environment]::SetEnvironmentVariable('Path', $before, 'User')
        }
    } else {
        Write-Host 'skip user PATH case (runs only on CI)'
    }
} finally {
    Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $scratch -ErrorAction SilentlyContinue
}

if ($script:fails -ne 0) { Write-Host "$($script:fails) install test(s) failed"; exit 1 }
Write-Host 'all install tests passed'
