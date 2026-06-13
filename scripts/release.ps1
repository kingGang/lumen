<#
.SYNOPSIS
    Lumen 发版流水（F3 热更配套）：改版本号 → release 构建 → Inno Setup 打包
    →（可选）打 tag + 发布到 GitHub Release。

.DESCRIPTION
    需求池 F3 方案：**只发 GitHub**（不发 Gitee，海风哥 2026-06-13 拍板），
    安装器自替换，不依赖自建服务端。客户端启动查 GitHub latest Release API，
    下载本脚本产出的 Inno Setup 安装包。

    前置（一次性）：
      1) 安装 Inno Setup 6（含 ISCC.exe）。
      2) 安装 GitHub CLI（gh）并 `gh auth login`（-Publish 时用）。
      3) 确认 crates/lumen-app/src/update.rs 里 GITHUB_REPO 为真实仓库（jimhy/lumen）。

.PARAMETER Version
    语义版本号（如 0.2.0，不带 v 前缀）。

.PARAMETER Publish
    构建打包后，额外打 git tag 并发布到 GitHub Release。

.EXAMPLE
    pwsh scripts\release.ps1 -Version 0.2.0
    pwsh scripts\release.ps1 -Version 0.2.0 -Publish
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^\d+\.\d+\.\d+$')]
    [string]$Version,

    [switch]$Publish
)

$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent $PSScriptRoot
$Tag = "v$Version"
$InstallerName = "Lumen-Setup-$Version.exe"
$DistDir = Join-Path $RepoRoot 'dist'
$InstallerPath = Join-Path $DistDir $InstallerName

Write-Host "==> Lumen 发版 $Tag" -ForegroundColor Cyan

# 1) 同步 Cargo.toml 工作区版本号（CARGO_PKG_VERSION 须与 tag 一致，否则
#    客户端版本比较永远认为本地版本不变）。
$CargoToml = Join-Path $RepoRoot 'Cargo.toml'
$content = Get-Content -Raw -LiteralPath $CargoToml
# 仅替换 [workspace.package] 下行首的 version（依赖版本是内联/不同模式，不误伤）。
$updated = [regex]::Replace($content, '(?m)^version = "[^"]*"', "version = `"$Version`"")
if ($updated -ne $content) {
    Set-Content -LiteralPath $CargoToml -Value $updated -NoNewline
    Write-Host "  Cargo.toml 版本号 → $Version" -ForegroundColor Green
} else {
    Write-Host "  Cargo.toml 版本号已是 $Version（或未匹配，请核对）" -ForegroundColor Yellow
}

# 2) release 构建（此机 cargo 指纹坑：构建后务必核对产物时间戳）。
Write-Host "==> cargo build --release" -ForegroundColor Cyan
Push-Location $RepoRoot
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build 失败（exit $LASTEXITCODE）" }
} finally {
    Pop-Location
}
$Exe = Join-Path $RepoRoot 'target\release\lumen.exe'
if (-not (Test-Path $Exe)) { throw "未找到构建产物：$Exe" }

# 3) Inno Setup 打包。
Write-Host "==> Inno Setup 打包" -ForegroundColor Cyan
$Iscc = $null
foreach ($cand in @(
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "${env:ProgramFiles}\Inno Setup 6\ISCC.exe"
    )) {
    if ($cand -and (Test-Path $cand)) { $Iscc = $cand; break }
}
if (-not $Iscc) {
    $cmd = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if ($cmd) { $Iscc = $cmd.Source }
}
if (-not $Iscc) { throw "找不到 ISCC.exe，请先安装 Inno Setup 6" }

New-Item -ItemType Directory -Force -Path $DistDir | Out-Null
& $Iscc "/DMyAppVersion=$Version" (Join-Path $RepoRoot 'installer\lumen.iss')
if ($LASTEXITCODE -ne 0) { throw "ISCC 打包失败（exit $LASTEXITCODE）" }
if (-not (Test-Path $InstallerPath)) { throw "未找到安装包：$InstallerPath" }

# SHA256（发布说明里可附，供客户端校验；当前客户端未强制校验）。
$Sha = (Get-FileHash -Algorithm SHA256 -LiteralPath $InstallerPath).Hash.ToLower()
Write-Host "  产物：$InstallerPath" -ForegroundColor Green
Write-Host "  SHA256：$Sha" -ForegroundColor Green

if (-not $Publish) {
    Write-Host "==> 完成（未发布）。加 -Publish 可打 tag 并发布到 GitHub Release。" -ForegroundColor Cyan
    return
}

# 4) 打 tag。
Write-Host "==> git tag $Tag" -ForegroundColor Cyan
Push-Location $RepoRoot
try {
    git add Cargo.toml Cargo.lock 2>$null
    git commit -m "release: $Tag" 2>$null
    git tag -a $Tag -m "Lumen $Tag"
    git push origin HEAD --tags
    if ($LASTEXITCODE -ne 0) { throw "git push 失败（exit $LASTEXITCODE）" }
} finally {
    Pop-Location
}

# 5) GitHub Release（gh CLI；release body = 更新日志，客户端弹窗展示）。
#    发布只走 GitHub（不发 Gitee，海风哥 2026-06-13 拍板）。
Write-Host "==> GitHub Release" -ForegroundColor Cyan
$Notes = "Lumen $Tag`n`nSHA256: $Sha"
gh release create $Tag $InstallerPath --title "Lumen $Tag" --notes $Notes
if ($LASTEXITCODE -ne 0) { Write-Host "  GitHub 发布失败，请手动重试" -ForegroundColor Yellow }

Write-Host "==> 发版完成 $Tag" -ForegroundColor Cyan
