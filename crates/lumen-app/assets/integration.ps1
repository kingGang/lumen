# Lumen shell integration：发射 OSC 133 命令边界标记，供终端构建命令块。
# 标记：A=提示符开始  B=命令输入开始  C=命令输出开始  D;<exit>=命令结束
# 另发射 OSC 9;9（ConEmu/Windows Terminal 约定）上报 cwd，供文件树跟随。
# 注入方式：pwsh -NoExit -Command . <本文件>（在用户 profile 之后执行，
# 包装而非替换用户已有的 prompt / PSConsoleHostReadLine 定制）。

# M4 输入编辑器前置驯化：
# - BellStyle None：蜂鸣干扰输入区感知，统一禁用。
# - PredictionSource 保持用户默认（海风哥第二十轮拍板恢复）：经典直通模式
#   下输入走 PSReadLine，其 History 联想是该模式的核心体验；编辑模式下
#   Lumen 一次性整行提交（text+\r），PSReadLine 无输入间隙渲染建议，
#   实测提交回显无 prediction 残影（残影回归点：块内命令行带灰色尾巴）。
# 每条独立 try/catch——兼容无 PSReadLine 环境及参数不兼容的旧版本。
try { Set-PSReadLineOption -BellStyle None -ErrorAction Stop } catch {}

$Global:__LumenPrompt = $function:prompt

function prompt {
    # 必须最先读取，函数体内的任何命令都会污染 $? / $LASTEXITCODE。
    $__ok = $?
    # $LASTEXITCODE 是粘滞的（仅原生命令更新）：成功时一律报 0，
    # 失败时才参考它（原生命令非零退出会把 $? 置 false，不漏报）。
    $__ec = if ($__ok) { 0 } else {
        if ($null -ne $Global:LASTEXITCODE -and $Global:LASTEXITCODE -ne 0) {
            $Global:LASTEXITCODE
        } else { 1 }
    }
    $e = [char]27
    $b = [char]7
    # cwd 上报（OSC 9;9）：带双引号发送（Windows Terminal 官方脚本同款，
    # 终端侧剥除，兼容含空格路径）。注册表等非文件系统 provider 不上报，
    # 终端保留上次的有效路径。
    $__loc = $ExecutionContext.SessionState.Path.CurrentLocation
    $__cwd = if ($__loc.Provider.Name -eq 'FileSystem') {
        "$e]9;9;`"$($__loc.ProviderPath)`"$b"
    } else { '' }
    # 上面的语句已把 $? 重置为 True；用户原 prompt（starship/posh-git
    # 等）第一行就读 $? 显示成败——失败时还原 $?（VS Code 同款手法）。
    if (-not $__ok) { Write-Error "failure" -ErrorAction Ignore }
    "$e]133;D;$__ec$b$e]133;A$b$__cwd" + (& $Global:__LumenPrompt) + "$e]133;B$b"
}

# 包装 ReadLine：用户按下回车、命令即将执行时发 C（输出开始）。
# M4.2：在 C 的私有参数位上挂 base64(UTF-8(命令行)) 作为「权威命令文本」，
# 供终端与编辑器本地记录对账、并作直通/Fallback 态命令文本来源（系统
# ConPTY 吞 OSC 633，故降级走 OSC 133 私参，设计稿 §3.3）。base64 为纯
# ASCII，跨机/西文系统/Windows PowerShell 5.1 均无编码风险；try/catch
# 兜底——编码异常绝不能吞掉命令本身的执行。
if ($function:PSConsoleHostReadLine) {
    $Global:__LumenReadLine = $function:PSConsoleHostReadLine
    function PSConsoleHostReadLine {
        $line = & $Global:__LumenReadLine
        $b64 = ''
        try {
            if ($line -is [string] -and $line.Length -gt 0) {
                $b64 = [Convert]::ToBase64String([System.Text.Encoding]::UTF8.GetBytes($line))
            }
        } catch {}
        [Console]::Write("$([char]27)]133;C;$b64$([char]7)")
        $line
    }
}
