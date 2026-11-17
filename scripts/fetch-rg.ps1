# Fetch the pinned ripgrep used by windows_search_text and install it as <Dest>\rg.exe plus the
# receipt <Dest>\rg.sha256 checked by the server on every search.
param([Parameter(Mandatory = $true)][string]$Dest)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$version = '15.2.0'
$url = "https://github.com/BurntSushi/ripgrep/releases/download/$version/ripgrep-$version-x86_64-pc-windows-msvc.zip"
$archiveSha = '71b2fef860abe467217a538ff31de02f5258807c0129f771846f87bd029aafc5'
$exeSha = '14231169855ec5205cf5a1b6f1db358ff4aed4247c86b69ce8aae647c77f6680'
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("rg-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    $zip = Join-Path $tmp 'rg.zip'
    Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $zip
    if ((Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower() -ne $archiveSha) { throw 'archive sha256 mismatch' }
    Expand-Archive -Path $zip -DestinationPath $tmp
    $exe = Get-ChildItem -Path $tmp -Recurse -Filter rg.exe | Select-Object -First 1
    if ((Get-FileHash -Algorithm SHA256 $exe.FullName).Hash.ToLower() -ne $exeSha) { throw 'rg.exe sha256 mismatch' }
    New-Item -ItemType Directory -Force -Path $Dest | Out-Null
    Copy-Item $exe.FullName (Join-Path $Dest 'rg.exe') -Force
    [IO.File]::WriteAllText((Join-Path $Dest 'rg.sha256'), $exeSha)
    # ripgrep is dual-licensed MIT / Unlicense; ship its license next to the binary.
    foreach ($lic in 'LICENSE-MIT', 'UNLICENSE') {
        $f = Get-ChildItem -Path $exe.Directory.FullName -Filter $lic | Select-Object -First 1
        if ($f) { Copy-Item $f.FullName (Join-Path $Dest "rg-$lic.txt") -Force }
    }
    "installed rg $version -> $Dest"
} finally { Remove-Item -Recurse -Force $tmp }
