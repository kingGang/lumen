# Lumen shell integration：发射 OSC 133 命令边界标记，供终端构建命令块。
# 标记：A=提示符开始  B=命令输入开始  C=命令输出开始  D;<exit>=命令结束
# 注入方式：pwsh -NoExit -Command . <本文件>（在用户 profile 之后执行，
# 包装而非替换用户已有的 prompt / PSConsoleHostReadLine 定制）。

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
    # 上面的语句已把 $? 重置为 True；用户原 prompt（starship/posh-git
    # 等）第一行就读 $? 显示成败——失败时还原 $?（VS Code 同款手法）。
    if (-not $__ok) { Write-Error "failure" -ErrorAction Ignore }
    $e = [char]27
    $b = [char]7
    "$e]133;D;$__ec$b$e]133;A$b" + (& $Global:__LumenPrompt) + "$e]133;B$b"
}

# 包装 ReadLine：用户按下回车、命令即将执行时发 C（输出开始）。
if ($function:PSConsoleHostReadLine) {
    $Global:__LumenReadLine = $function:PSConsoleHostReadLine
    function PSConsoleHostReadLine {
        $line = & $Global:__LumenReadLine
        [Console]::Write("$([char]27)]133;C$([char]7)")
        $line
    }
}
