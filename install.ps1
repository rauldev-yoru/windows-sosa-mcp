# windows-sosa-mcp installer for Windows x64. In PowerShell (no admin needed):
#   irm https://raw.githubusercontent.com/rauldev-yoru/windows-sosa-mcp/main/install.ps1 | iex
# From cmd:
#   powershell -NoProfile -c "irm https://raw.githubusercontent.com/rauldev-yoru/windows-sosa-mcp/main/install.ps1 | iex"
# Uninstall:
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/rauldev-yoru/windows-sosa-mcp/main/install.ps1))) -Uninstall
#
# Installs the release exe (checksum-verified against SHA256SUMS) to
# %LOCALAPPDATA%\Programs\windows-sosa-mcp and adds that folder to the user PATH.
# Re-running upgrades in place, also while MCP clients still run the old exe.
# Env:
#   WINDOWS_SOSA_MCP_VERSION         release tag to install, e.g. v0.1.1 (default: latest release)
#   WINDOWS_SOSA_MCP_INSTALL_DIR     install folder (default: %LOCALAPPDATA%\Programs\windows-sosa-mcp)
#   WINDOWS_SOSA_MCP_NO_MODIFY_PATH  set to 1 to leave the user PATH alone
param([switch]$Uninstall)

& {
    param([bool]$Uninstall)
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

    $repo  = 'rauldev-yoru/windows-sosa-mcp'
    $asset = 'windows-sosa-mcp.exe'
    $dir = $env:WINDOWS_SOSA_MCP_INSTALL_DIR
    if (-not $dir) { $dir = Join-Path $env:LOCALAPPDATA 'Programs\windows-sosa-mcp' }
    $dir = [IO.Path]::GetFullPath($dir)
    $target = Join-Path $dir $asset
    $touchPath = $env:WINDOWS_SOSA_MCP_NO_MODIFY_PATH -ne '1'

    function Say([string]$msg) { Write-Host "windows-sosa-mcp-install: $msg" }

    # The user PATH is edited in the registry (keeping its value kind) so
    # REG_EXPAND_SZ entries such as %USERPROFILE%\bin survive unexpanded.
    function Set-UserPath([scriptblock]$edit) {
        $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
        try {
            $old = [string]$key.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
            $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
            if ($old) { $kind = $key.GetValueKind('Path') }
            $parts = @($old -split ';' | Where-Object { $_ -ne '' })
            $new = (& $edit $parts) -join ';'
            if ($new -eq $old) { return $false }
            $key.SetValue('Path', $new, $kind)
        } finally {
            $key.Close()
        }
        # Setting a user variable through .NET broadcasts WM_SETTINGCHANGE, so
        # Explorer and new terminals pick up the PATH change.
        [Environment]::SetEnvironmentVariable('WINDOWS_SOSA_MCP_INSTALL_TMP', '1', 'User')
        [Environment]::SetEnvironmentVariable('WINDOWS_SOSA_MCP_INSTALL_TMP', $null, 'User')
        return $true
    }
    function Test-SameDir([string]$a, [string]$b) {
        $x = [Environment]::ExpandEnvironmentVariables($a).TrimEnd('\')
        return [string]::Equals($x, $b.TrimEnd('\'), [StringComparison]::OrdinalIgnoreCase)
    }
    function Remove-Stale {
        Get-ChildItem -Path $dir -Filter "$asset.old-*" -ErrorAction SilentlyContinue |
            ForEach-Object { Remove-Item -Force $_.FullName -ErrorAction SilentlyContinue }
    }

    if ($Uninstall) {
        if (Test-Path $target) {
            try {
                Remove-Item -Force $target
            } catch {
                throw "windows-sosa-mcp: $target is in use; close the MCP clients running it and retry"
            }
        }
        Remove-Stale
        if ((Test-Path $dir) -and -not (Get-ChildItem -Force $dir)) { Remove-Item -Force $dir }
        if ($touchPath -and (Set-UserPath { param($p) $p | Where-Object { -not (Test-SameDir $_ $dir) } })) {
            Say "removed $dir from the user PATH"
        }
        Say "uninstalled. Remove the windows-sosa entry from your MCP client configs."
        Say "State and audit logs stay in %LOCALAPPDATA%\windows-sosa-mcp (delete it by hand if unwanted)."
        return
    }

    if (-not [Environment]::Is64BitOperatingSystem) { throw 'windows-sosa-mcp: only Windows x64 is supported' }

    $tag = $env:WINDOWS_SOSA_MCP_VERSION
    if ($tag) {
        if ($tag -notmatch '^v') { $tag = "v$tag" }
        $base = "https://github.com/$repo/releases/download/$tag"
        Say "installing $tag"
    } else {
        $base = "https://github.com/$repo/releases/latest/download"
        Say 'installing the latest release'
    }

    $tmp = Join-Path $env:TEMP ('windows-sosa-mcp-install-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        $exe  = Join-Path $tmp $asset
        $sums = Join-Path $tmp 'SHA256SUMS'
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $exe
            Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS" -OutFile $sums
        } catch {
            throw "windows-sosa-mcp: download from $base failed (does that release exist?): $($_.Exception.Message)"
        }
        $line = Get-Content $sums | Where-Object { $_ -match " \*?$([regex]::Escape($asset))$" } | Select-Object -First 1
        if (-not $line) { throw "windows-sosa-mcp: $asset not listed in SHA256SUMS" }
        $want = ($line -split '\s+')[0].ToLower()
        $got  = (Get-FileHash -Algorithm SHA256 $exe).Hash.ToLower()
        if ($want -ne $got) { throw "windows-sosa-mcp: checksum mismatch for $asset" }
        Say 'checksum ok'

        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        Remove-Stale
        if (Test-Path $target) {
            # A running exe can't be overwritten but can be renamed: move it
            # aside so running MCP sessions keep working until they restart.
            $old = "$target.old-" + (Get-Date -Format 'yyyyMMddHHmmss')
            Move-Item -Force $target $old
            Remove-Item -Force $old -ErrorAction SilentlyContinue
            if (Test-Path $old) { Say "previous exe is in use; kept as $(Split-Path -Leaf $old) until the next run" }
        }
        Move-Item $exe $target
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }

    $version = (& $target --version) -join ''
    if ($LASTEXITCODE -ne 0) { throw "windows-sosa-mcp: installed exe failed to start (exit $LASTEXITCODE)" }
    Say "installed $version to $target"

    if ($touchPath) {
        if (Set-UserPath { param($p) if ($p | Where-Object { Test-SameDir $_ $dir }) { $p } else { @($p) + $dir } }) {
            Say "added $dir to the user PATH (new terminals and apps will see it)"
        }
        if (-not ($env:Path -split ';' | Where-Object { $_ -and (Test-SameDir $_ $dir) })) { $env:Path = "$env:Path;$dir" }
    }

    $json = $target.Replace('\', '\\')
    Write-Host ''
    Write-Host 'Add it to your MCP client, e.g. (Claude Desktop / Cursor / VS Code style):'
    Write-Host ''
    Write-Host '  {'
    Write-Host '    "mcpServers": {'
    Write-Host "      `"windows-sosa`": { `"command`": `"$json`" }"
    Write-Host '    }'
    Write-Host '  }'
    Write-Host ''
    Write-Host "Codex (~/.codex/config.toml):  [mcp_servers.windows-sosa]  command = '$target'"
    Write-Host 'More clients: https://github.com/rauldev-yoru/windows-sosa-mcp#configure-an-mcp-client'
} $Uninstall.IsPresent
