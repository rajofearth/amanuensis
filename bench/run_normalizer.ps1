[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Transcript,
    [string]$Model = 'C:\Users\Yashraj\AppData\Local\Temp\opencode\bench_space\s1-mini-q4_k_m.gguf',
    [string]$LlamaCli = 'C:\Users\Yashraj\AppData\Local\Temp\opencode\bench_space\llama-cli.exe',
    [int]$Threads = 8,
    [int]$MaxNewTokens = 512,
    [int]$GpuLayers = 0,
    [string]$SystemPrompt = 'You are a text normalizer for speech-to-text transcripts. The input begins with a control line specifying the styling, structure, and context settings; clean the transcript to match those settings and output only the cleaned text.',
    [string]$ControlLine = '[Styling: semi-formal] [Structure: prose] [Context: general]',
    [switch]$Json
)

function Get-CleanText {
    param([string]$All, [string]$Transcript, [string]$ControlLine)
    $All = $All -replace "`r", ''
    $start = 0
    $boundary = '[^\r\n]*' + [regex]::Escape($ControlLine) + '[^\r\n]*\r?\n[^\r\n]*' + [regex]::Escape($Transcript) + '[ \t]*\r?\n'
    $m = [regex]::Match($All, $boundary)
    if ($m.Success) {
        $start = $m.Index + $m.Length
    } else {
        $idx = $All.LastIndexOf($Transcript + "`n")
        if ($idx -ge 0) {
            $start = $idx + $Transcript.Length
        } else {
            $a = $All.LastIndexOf('assistant')
            if ($a -ge 0) {
                $start = $a + 8
            } else {
                return $null
            }
        }
    }
    $text = $All.Substring($start)
    $text = $text -replace '(?s)^\s*(<\|im_start\|>)?\s*', ''
    $text = $text -replace '(?s)\s*Exiting\.\.\.\s*$', ''
    $text = $text -replace '(?s)(<\|im_start\|>)?\s*\[ (?:Prompt|Generation):[^\r\n]* \]\s*$', ''
    $text = $text -replace '(?s)\s*<\|im_end\|>\s*$', ''
    $text = $text -replace '(?s)\s*\[end of text\]\s*$', ''
    return $text.Trim()
}

$outFile = Join-Path ([IO.Path]::GetTempPath()) ("llama_out_" + [guid]::NewGuid().ToString('N') + '.txt')
$errFile = Join-Path ([IO.Path]::GetTempPath()) ("llama_err_" + [guid]::NewGuid().ToString('N') + '.txt')

$userMessage = "$ControlLine`n$Transcript"
$esc = { param($s) ($s -replace '"', '\"') }
$sysArg = '"' + (& $esc $SystemPrompt) + '"'
$userArg = '"' + (& $esc $userMessage) + '"'

$cmdline = @(
    "-m `"$Model`"",
    '--jinja',
    '--chat-template-kwargs "{\"enable_thinking\":false}"',
    '--temp 0',
    '-t ' + $Threads,
    '-c 2048',
    '-ngl ' + $GpuLayers,
    '-st',
    '-n ' + $MaxNewTokens,
    '-sys ' + $sysArg,
    '-p ' + $userArg
) -join ' '

$proc = $null
try {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $proc = Start-Process -FilePath $LlamaCli -ArgumentList $cmdline -PassThru -NoNewWindow -RedirectStandardOutput $outFile -RedirectStandardError $errFile

    $peakRss = 0L
    while (-not $proc.HasExited) {
        Start-Sleep -Milliseconds 20
        try { $ws = $proc.WorkingSet64 } catch { $ws = 0L }
        if ($ws -gt $peakRss) { $peakRss = $ws }
    }

    $proc.WaitForExit()
    $sw.Stop()
    $proc.Refresh()
    if ($proc.PeakWorkingSet64 -gt $peakRss) { $peakRss = $proc.PeakWorkingSet64 }
    $peakRssMb = [math]::Round($peakRss / 1MB, 1)
    $latencyMs = [math]::Round($sw.Elapsed.TotalMilliseconds)

    $stdout = ''
    $stderr = ''
    if (Test-Path -LiteralPath $outFile) { $stdout = Get-Content -LiteralPath $outFile -Raw }
    if (Test-Path -LiteralPath $errFile) { $stderr = Get-Content -LiteralPath $errFile -Raw }

    if ($proc.ExitCode -ne 0) {
        throw "llama-cli exited with code $($proc.ExitCode)"
    }
    if ($stderr -match 'error|failed') {
        throw 'llama-cli reported an error or failure on stderr'
    }

    $cleaned = Get-CleanText ($stdout + "`n" + $stderr) $Transcript $ControlLine
    if ([string]::IsNullOrWhiteSpace($cleaned)) {
        throw 'extraction produced empty cleaned text'
    }

    if ($Json) {
        [Console]::Error.WriteLine("max_new_tokens=$MaxNewTokens")
        [Console]::Error.WriteLine("model=$Model")
        [PSCustomObject]@{
            cleaned_text   = $cleaned
            latency_ms     = $latencyMs
            peak_rss_mb    = $peakRssMb
            threads        = $Threads
            max_new_tokens = $MaxNewTokens
        } | ConvertTo-Json -Compress
    } else {
        "rewrite   : $cleaned"
        "latency_ms: $latencyMs   | peak_rss_mb: $peakRssMb"
    }
}
finally {
    if ($proc -ne $null) { $proc.Dispose() }
    Remove-Item -LiteralPath $outFile, $errFile -Force -ErrorAction SilentlyContinue
}