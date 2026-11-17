# windows-sosa-mcp package install job script. Bundled with the server, fixed.
# Runs as user.ps1 of a job; the job wrapper exposes $JobDir. Reads package.json (written by
# the server from validated arguments), runs winget, re-lists the package and prints a final
# machine-readable marker line. Never interpolates arguments into PowerShell source.
$ErrorActionPreference='Continue'
$ProgressPreference='SilentlyContinue'
$cfg=[IO.File]::ReadAllText((Join-Path $JobDir 'package.json'),[Text.Encoding]::UTF8) | ConvertFrom-Json
$w=[string]$cfg.winget_path
if (-not [IO.File]::Exists($w)) { [Console]::Error.WriteLine('winget not found: '+$w); exit 2 }
$argv=@($cfg.args | ForEach-Object { [string]$_ })
"WINDOWS_SOSA_MCP_PACKAGE_BEGIN "+(Get-Date).ToUniversalTime().ToString('o')+" winget "+($argv -join ' ')
$global:LASTEXITCODE=0
& $w @argv 2>&1 | ForEach-Object { "$_" }
$code=[int]$LASTEXITCODE
"WINDOWS_SOSA_MCP_PACKAGE_INSTALL_EXIT "+$code
$listArgs=@('list','--id',[string]$cfg.id,'--exact','--disable-interactivity','--accept-source-agreements')
if ($cfg.source) { $listArgs+=@('--source',[string]$cfg.source) }
$global:LASTEXITCODE=0
$list=@(& $w @listArgs 2>&1 | ForEach-Object { "$_" })
$listCode=[int]$LASTEXITCODE
$result=[ordered]@{install_exit_code=$code; list_exit_code=$listCode; list_lines=@($list | Select-Object -First 40); finished_at_utc=(Get-Date).ToUniversalTime().ToString('o')}
"WINDOWS_SOSA_MCP_PACKAGE_RESULT:"+($result | ConvertTo-Json -Compress -Depth 4)
exit $code
