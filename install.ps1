# ftc installer — Windows x64.
#
#   irm https://raw.githubusercontent.com/cyberd4ch/free-torrent-cli/main/install.ps1 | iex
#
# Environment overrides:
#   FTC_VERSION=v3.0.0   install a specific release (default: latest)
$ErrorActionPreference = 'Stop'

$repo = 'cyberd4ch/free-torrent-cli'
$version = if ($env:FTC_VERSION) { $env:FTC_VERSION } else { 'latest' }
$base = if ($version -eq 'latest') {
    "https://github.com/$repo/releases/latest/download"
} else {
    "https://github.com/$repo/releases/download/$version"
}
$asset = 'ftc-windows-x86_64.zip'

$tmp = Join-Path $env:TEMP ("ftc-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp -Force | Out-Null

try {
    $zip = Join-Path $tmp $asset
    Write-Host "-> $asset"
    Invoke-WebRequest -Uri "$base/$asset" -OutFile $zip -UseBasicParsing

    $sums = Join-Path $tmp 'SHA256SUMS'
    Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile $sums -UseBasicParsing
    $expected = (Select-String -Path $sums -Pattern ([regex]::Escape($asset) + '$') -SimpleMatch:$false).Line.Split()[0]
    $actual = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash.ToLower()
    if ($expected -and $expected -ne $actual) { throw "checksum mismatch for $asset" }
    Write-Host '-> sha256 ok'

    Expand-Archive -Path $zip -DestinationPath $tmp -Force

    $dest = Join-Path $env:LOCALAPPDATA 'Programs\ftc'
    New-Item -ItemType Directory -Path $dest -Force | Out-Null
    Copy-Item (Join-Path $tmp 'ftc.exe') (Join-Path $dest 'ftc.exe') -Force

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($userPath -notlike "*$dest*") {
        [Environment]::SetEnvironmentVariable('Path', ($userPath.TrimEnd(';') + ';' + $dest), 'User')
        Write-Host "-> added $dest to your user PATH (open a new terminal to use it)"
    }

    Write-Host "-> installed $dest\ftc.exe"
    & (Join-Path $dest 'ftc.exe') --version
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
