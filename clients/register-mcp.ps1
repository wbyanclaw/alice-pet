<#
.SYNOPSIS
    把 codex-pet 注册成 MCP server（编译 + 按需写入各家 agent 的配置）。

.DESCRIPTION
    步骤：
      1. 编译 release 版（debug 版是 console 子系统，会弹 cmd 窗口）
      2. 把 clients/*.mcp.json 里的 ${CODEX_PET_EXE} 换成实际 exe 路径
      3. 按 -Client 注册到对应 agent 的用户级配置

    模板里的 ${CODEX_PET_EXE} 是占位符，各家客户端大多不会展开，
    所以注册前必须替换成真实绝对路径。

.EXAMPLE
    .\clients\register-mcp.ps1 -Client codex
    .\clients\register-mcp.ps1 -Client claude -SkipBuild
    .\clients\register-mcp.ps1 -Client all
#>
[CmdletBinding()]
param(
    [ValidateSet("codex", "codex-app", "claude", "codebuddy", "dsh", "trae", "qwenwork", "all")]
    [string]$Client = "all",
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
# 仓库里是 src-tauri\target\release\，发布包里 exe 就在脚本旁边——
# 两个位置都认，脚本才能同时在仓库和发布包里用。
$exe = Join-Path $repo "src-tauri\target\release\codex-pet.exe"
if (-not (Test-Path $exe)) {
    $exe = Join-Path $repo "codex-pet.exe"
}

function Write-Step([string]$msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Write-Ok([string]$msg) { Write-Host "    $msg" -ForegroundColor Green }
function Write-Warn([string]$msg) { Write-Host "    $msg" -ForegroundColor Yellow }

# ---- 1. 编译 ----
if (-not $SkipBuild) {
    $srcDir = Join-Path $repo "src-tauri"
    if (Test-Path (Join-Path $srcDir "Cargo.toml")) {
        Write-Step "编译 release 版"
        Push-Location $srcDir
        try {
            cargo build --release
            if ($LASTEXITCODE -ne 0) { throw "cargo build 失败" }
        } finally {
            Pop-Location
        }
    } else {
        Write-Step "发布包里没有源码，跳过编译"
    }
}

if (-not (Test-Path $exe)) {
    throw "找不到 $exe —— 先编译，或用 -SkipBuild 时确认路径正确"
}
Write-Ok "exe: $exe"

# ---- 2. 生成填好真实路径的模板 ----
Write-Step "生成配置"
$templates = @{
    codex     = "clients\claude.mcp.json"     # codex 与 claude 的 mcpServers 结构一致
    claude   = "clients\claude.mcp.json"
    codebuddy = "clients\codebuddy.mcp.json"
    dsh       = "clients\dsh.mcp.json"
    trae      = "clients\trae.mcp.json"
    qwenwork  = "clients\qwenwork.mcp.json"
}
$filled = @{}
# JSON 里的反斜杠要转义成 \\，否则 "d:\POC\..." 会被当成非法转义序列。
# 用 MatchEvaluator 避免正则替换对 $ 和反斜杠的二次处理。
$exeForJson = $exe.Replace("\", "\\")
foreach ($k in $templates.Keys) {
    $src = Join-Path $repo $templates[$k]
    $raw = Get-Content $src -Raw
    $filled[$k] = [regex]::Replace($raw, '\$\{CODEX_PET_EXE\}', { param($m) $exeForJson })
}
$outDir = Join-Path $repo "clients\generated"
New-Item -ItemType Directory -Path $outDir -Force | Out-Null
foreach ($k in $filled.Keys) {
    $p = Join-Path $outDir "$k.mcp.json"
    [System.IO.File]::WriteAllText($p, $filled[$k], (New-Object System.Text.UTF8Encoding($false)))
    Write-Ok "已生成 $p"
}

# ---- 3. 注册 ----
function Register-Codex {
    Write-Step "注册到 Codex CLI"
    $cfg = Join-Path $env:USERPROFILE ".codex\config.toml"
    if (-not (Test-Path $cfg)) {
        New-Item -ItemType Directory -Path (Split-Path $cfg) -Force | Out-Null
        [System.IO.File]::WriteAllText($cfg, "", (New-Object System.Text.UTF8Encoding($false)))
    }
    $raw = [System.IO.File]::ReadAllText($cfg)
    # TOML 里路径用单引号字面量串，反斜杠不用转义
    $block = @(
        "[mcp_servers.codex-pet]",
        "command = '$exe'",
        'args = ["--mcp"]',
        'default_tools_approval_mode = "approve"',
        ""
    )
    # 按行处理，不用正则替换——替换值里的 $ 和换行在正则里含义特殊，
    # 实测会把换行吃掉导致 TOML 段落粘连。
    $lines = [System.Collections.Generic.List[string]]::new()
    foreach ($l in ($raw -split "`r?`n")) { $lines.Add($l) }
    $start = -1
    for ($i = 0; $i -lt $lines.Count; $i++) {
        if ($lines[$i].Trim() -eq "[mcp_servers.codex-pet]") { $start = $i; break }
    }
    if ($start -ge 0) {
        # 删掉旧段：从 [mcp_servers.codex-pet] 到下一个 [ 开头的行之前
        $end = $lines.Count
        for ($i = $start + 1; $i -lt $lines.Count; $i++) {
            if ($lines[$i].Trim().StartsWith("[")) { $end = $i; break }
        }
        $lines.RemoveRange($start, $end - $start)
        $lines.InsertRange($start, [string[]]$block)
    } else {
        foreach ($l in $block) { $lines.Add($l) }
    }
    $out = ($lines -join "`r`n")
    [System.IO.File]::WriteAllText($cfg, $out, (New-Object System.Text.UTF8Encoding($false)))
    Write-Ok "已写入 $cfg"
}

function Register-Claude {
    Write-Step "注册到 Claude Code"
    if (Get-Command claude -ErrorAction SilentlyContinue) {
        claude mcp remove codex-pet --scope user 2>&1 | Out-Null
        claude mcp add codex-pet --scope user -- $exe --mcp
        Write-Ok "已用 claude mcp add 注册"
    } else {
        Write-Warn "没找到 claude 命令，手动把 clients\generated\claude.mcp.json 里的"
        Write-Warn "mcpServers 段合进 ~/.claude.json 即可"
    }
}

function Register-JsonFile([string]$path, [string]$label) {
    Write-Step "注册到 $label"
    $obj = Get-Content (Join-Path $outDir "$($script:CurrentTarget).mcp.json") -Raw | ConvertFrom-Json
    if (-not (Test-Path $path)) {
        New-Item -ItemType Directory -Path (Split-Path $path) -Force | Out-Null
        [System.IO.File]::WriteAllText($path, "{`n  `"mcpServers`": {}`n}", (New-Object System.Text.UTF8Encoding($false)))
    }
    $existing = $null
    try { $existing = Get-Content $path -Raw | ConvertFrom-Json } catch { $existing = $null }
    if ($null -eq $existing) { $existing = [pscustomobject]@{ mcpServers = [pscustomobject]@{} } }
    $existing.mcpServers | Add-Member -NotePropertyName "codex-pet" -NotePropertyValue $obj.mcpServers."codex-pet" -Force
    $json = $existing | ConvertTo-Json -Depth 10
    [System.IO.File]::WriteAllText($path, $json, (New-Object System.Text.UTF8Encoding($false)))
    Write-Ok "已写入 $path"
}

function Register-CodexApp {
    Write-Step "注册到 Codex App（本地插件市场）"
    $cfg = Join-Path $env:USERPROFILE ".codex\config.toml"
    if (-not (Test-Path $cfg)) { throw "找不到 $cfg" }
    $raw = [System.IO.File]::ReadAllText($cfg)
    $market = Join-Path $repo "plugin"
    $lines = [System.Collections.Generic.List[string]]::new()
    foreach ($l in ($raw -split "`r?`n")) { $lines.Add($l) }

    $block = @(
        "[marketplaces.codex-pet]",
        'source_type = "local"',
        "source = '\\?\$market'",
        ""
    )
    # 删掉旧段再插入，避免重复
    $start = -1
    for ($i = 0; $i -lt $lines.Count; $i++) {
        if ($lines[$i].Trim() -eq "[marketplaces.codex-pet]") { $start = $i; break }
    }
    if ($start -ge 0) {
        $end = $lines.Count
        for ($i = $start + 1; $i -lt $lines.Count; $i++) {
            if ($lines[$i].Trim().StartsWith("[")) { $end = $i; break }
        }
        $lines.RemoveRange($start, $end - $start)
    }
    $lines.AddRange([string[]]$block)
    [System.IO.File]::WriteAllText($cfg, ($lines -join "`r`n"), (New-Object System.Text.UTF8Encoding($false)))
    Write-Ok "已写入 marketplace 配置：$market"

    # 插件里的 .mcp.json 也要填真实路径（插件不展开环境变量）。
    # 用正则替换而不是 ConvertTo-Json 重写——后者会把缩进弄成乱糟糟的一堆空格。
    $mcp = Join-Path $market "plugins\codex-pet\.mcp.json"
    $raw = [System.IO.File]::ReadAllText($mcp)
    # JSON 字符串里的反斜杠要写成 \\，但替换值本身会被正则再处理一次，
    # 所以这里用 MatchEvaluator 避免二次转义。
    $escaped = $exe.Replace("\", "\\")
    $replacement = '"command": "' + $escaped.Replace('$', '$$') + '"'
    $raw = [regex]::Replace($raw, '"command"\s*:\s*"[^"]*"', { param($m) $replacement })
    [System.IO.File]::WriteAllText($mcp, $raw, (New-Object System.Text.UTF8Encoding($false)))
    Write-Ok "已更新 $mcp"
    Write-Warn "重启 Codex App 后生效"
}

$targets = if ($Client -eq "all") { @("codex", "codex-app", "claude", "codebuddy", "dsh", "trae", "qwenwork") } else { @($Client) }

foreach ($t in $targets) {
    $script:CurrentTarget = $t
    switch ($t) {
        "codex"     { Register-Codex }
        "codex-app" { Register-CodexApp }
        "claude"    { Register-Claude }
        "codebuddy" { Register-JsonFile (Join-Path $env:USERPROFILE ".codebuddy\.mcp.json") "CodeBuddy / WorkBuddy" }
        "dsh"       { Register-JsonFile (Join-Path $env:USERPROFILE ".dsh\mcp.json") "DSH" }
        "trae"      { Register-JsonFile (Join-Path $env:APPDATA "Trae CN\user_settings\mcp.json") "Trae" }
        "qwenwork"  { Write-Warn "千问办公要在图形界面里粘贴 clients\generated\qwenwork.mcp.json" }
    }
}

Write-Host ""
Write-Host "完成。先启动桌宠，再让 agent 调用 pet_state 验证：" -ForegroundColor Green
Write-Host "  .\src-tauri\target\release\codex-pet.exe" -ForegroundColor White
