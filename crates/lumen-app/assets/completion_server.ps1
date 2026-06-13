# Lumen command-completion sidecar (M4.4 batch 2).
# A persistent hidden pwsh process. Reads one JSON request per line from stdin,
# writes exactly one JSON response per line to stdout. Driven by Lumen over pipes.
#
# Request : {"id":<u64>,"line":"<cmdline>","col":<usize>,"cwd":"<dir?>"}
# Response: {"id":<u64>,"ri":<replIndex>,"rl":<replLen>,"items":[{"text":..,"type":..}]}
#   ri/rl = ReplacementIndex / ReplacementLength (byte? -> char index in $line).
#   type  = Command | ParameterName | ProviderItem | ProviderContainer | ...
#
# ASCII-only comments on purpose (avoid ANSI-decoding corruption like integration.ps1).
$ErrorActionPreference = 'SilentlyContinue'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

while ($null -ne ($line = [Console]::In.ReadLine())) {
    if ([string]::IsNullOrWhiteSpace($line)) { continue }
    $id = 0
    try {
        $req = $line | ConvertFrom-Json
        $id = [uint64]$req.id
        if ($req.cwd) { Set-Location -LiteralPath ([string]$req.cwd) -ErrorAction SilentlyContinue }
        $c = [System.Management.Automation.CommandCompletion]::CompleteInput(
            [string]$req.line, [int]$req.col, $null)
        # Cap at 100 to keep the JSON line small; Lumen further trims for display.
        $items = @($c.CompletionMatches | Select-Object -First 100 | ForEach-Object {
            @{ text = $_.CompletionText; type = $_.ResultType.ToString() }
        })
        $resp = @{ id = $id; ri = $c.ReplacementIndex; rl = $c.ReplacementLength; items = $items }
        [Console]::Out.WriteLine(($resp | ConvertTo-Json -Compress -Depth 4))
    } catch {
        # On any error, reply with an empty item set so Lumen can pair by id and degrade.
        [Console]::Out.WriteLine((@{ id = $id; ri = 0; rl = 0; items = @() } | ConvertTo-Json -Compress))
    }
}
