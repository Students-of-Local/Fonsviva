# Install the fonsviva CLI on Windows, with no admin rights:
#   irm https://raw.githubusercontent.com/baney75/Fonsviva/main/install.ps1 | iex
# Pick a channel (only dev is published today):
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/baney75/Fonsviva/main/install.ps1))) -Channel dev
# Builds are unsigned. This checks each download's sha256 against the channel's latest.json
# before extracting it, installs to %LOCALAPPDATA%\Fonsviva\bin and adds that folder to the
# user PATH (-NoModifyPath skips that).
# FONSVIVA_DOWNLOAD_BASE replaces the URL the feed is read from (tests only).
param(
    [string]$Channel = 'dev',
    [switch]$NoModifyPath
)

# Errors throw and are reported at the bottom, so `irm ... | iex` never exits the person's window.
function Install-Fonsviva {
    $ErrorActionPreference = 'Stop'
    # Invoke-WebRequest's progress bar makes downloads many times slower on Windows PowerShell 5.1.
    $ProgressPreference = 'SilentlyContinue'
    # Windows PowerShell 5.1 can default to TLS 1.0, which GitHub refuses.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    switch ($Channel) {
        'dev' {}
        { $_ -in 'stable', 'nightly' } { throw "The $Channel channel is not published yet. Run this again without -Channel to get the dev channel." }
        default { throw "Unknown channel '$Channel'. Fonsviva publishes only the dev channel today." }
    }
    if (-not $env:LOCALAPPDATA) { throw 'LOCALAPPDATA is not set, so there is no folder to install into.' }
    $base = $env:FONSVIVA_DOWNLOAD_BASE
    if (-not $base) { $base = "https://github.com/baney75/Fonsviva/releases/download/$Channel" }
    $base = $base.TrimEnd('/')

    # A 32-bit PowerShell on 64-bit Windows reports x86 here and the real CPU in PROCESSOR_ARCHITEW6432.
    $arch = $env:PROCESSOR_ARCHITEW6432
    if (-not $arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
    if ($arch -ne 'AMD64') { throw "Fonsviva has no build for Windows on $arch CPUs. Builds exist for x86_64 (AMD64)." }
    $target = 'x86_64-pc-windows-msvc'

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("fonsviva-" + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    $dir = Join-Path $env:LOCALAPPDATA 'Fonsviva\bin'
    $staged = Join-Path $dir 'fonsviva.new.exe'
    try {
        # GitHub serves release files as application/octet-stream, so parse the JSON here
        # rather than trust Invoke-RestMethod to recognize it.
        $json = Join-Path $tmp 'latest.json'
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$base/latest.json" -OutFile $json
            $feed = Get-Content -Raw -Path $json | ConvertFrom-Json
        } catch {
            throw "Couldn't read the $Channel feed at $base/latest.json. Check your connection and try again."
        }
        $asset = $feed.assets.$target
        if (-not $asset) { throw "The $Channel channel has no build for $target yet." }
        if (-not $asset.url) { throw "The $Channel feed lists $target without a download URL." }
        $want = "$($asset.sha256)".ToLowerInvariant()
        if ($want -notmatch '^[0-9a-f]{64}$') { throw "The $Channel feed lists $target without a valid sha256." }

        Write-Host "Downloading fonsviva $($feed.version) for $target"
        $zip = Join-Path $tmp 'fonsviva.zip'
        try {
            Invoke-WebRequest -UseBasicParsing -Uri $asset.url -OutFile $zip
        } catch {
            throw "Couldn't download $($asset.url). Check your connection and try again."
        }
        $got = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash.ToLowerInvariant()
        if ($got -ne $want) {
            throw "The download's sha256 is $got, but the $Channel feed says $want. It may be damaged or tampered with. Nothing was installed."
        }
        $x = Join-Path $tmp 'x'
        Expand-Archive -Path $zip -DestinationPath $x -Force
        $bin = Get-ChildItem -Path $x -Recurse -File -Filter 'fonsviva.exe' | Select-Object -First 1
        if (-not $bin) { throw "The archive for $target has no fonsviva.exe in it. Nothing was installed." }

        # Stage next to the target and check it runs before it replaces anything.
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
        Copy-Item -Path $bin.FullName -Destination $staged -Force
        try { & $staged --version *> $null; $runs = $LASTEXITCODE -eq 0 } catch { $runs = $false }
        if (-not $runs) { throw "The downloaded fonsviva.exe won't run on this machine. Nothing was installed." }
        $dest = Join-Path $dir 'fonsviva.exe'
        if (Test-Path $dest) {
            # A running fonsviva.exe (the daemon) can't be overwritten, but it can be renamed.
            $old = Join-Path $dir 'fonsviva.old.exe'
            Remove-Item -Path $old -Force -ErrorAction SilentlyContinue
            Move-Item -Path $dest -Destination $old -Force
        }
        Move-Item -Path $staged -Destination $dest -Force
    } finally {
        Remove-Item -Path $tmp -Recurse -Force -ErrorAction SilentlyContinue
        Remove-Item -Path $staged -Force -ErrorAction SilentlyContinue
    }

    $onPath = { param($p) ($p -split ';' | ForEach-Object { $_.TrimEnd('\') }) -contains $dir }
    if (-not $NoModifyPath) {
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        if (-not (& $onPath $userPath)) {
            $new = if ($userPath) { "$($userPath.TrimEnd(';'));$dir" } else { $dir }
            [Environment]::SetEnvironmentVariable('Path', $new, 'User')
        }
        if (-not (& $onPath $env:Path)) { $env:Path = "$($env:Path.TrimEnd(';'));$dir" }
    }
    $cmd = if (& $onPath $env:Path) { 'fonsviva' } else { "& `"$dest`"" }
    Write-Host "Installed fonsviva $($feed.version) ($Channel channel, unsigned, sha256 verified) at $dest"
    Write-Host 'Next, get a first local answer:'
    Write-Host "  $cmd setup --yes; $cmd ask `"Name three primary colors.`""
}

try {
    Install-Fonsviva
} catch {
    Write-Host "fonsviva install: $($_.Exception.Message)" -ForegroundColor Red
    # Run as a file (-File, or the tests): fail the process. Run through iex: leave the window open.
    if ($PSCommandPath) { exit 1 }
}
