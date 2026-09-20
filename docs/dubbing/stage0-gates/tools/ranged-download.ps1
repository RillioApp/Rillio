# Ranged parallel download for hosts that cap per-connection speed AND drop
# long connections: N parts in parallel, each part fetched as sequential
# 64 MB pieces (a dropped connection costs one piece, never the part), each
# piece retried against the ORIGINAL url (expired CDN tokens re-resolve).
# Joined and size-verified at the end.
param(
    [Parameter(Mandatory)] [string] $Url,
    [Parameter(Mandatory)] [string] $Out,
    [int] $Parts = 4,
    [int] $PieceMB = 64
)
$ErrorActionPreference = 'Stop'
$head = curl.exe -sIL $Url | Select-String -Pattern '^content-length:\s*(\d+)' | Select-Object -Last 1
if (-not $head) { throw "no Content-Length for $Url" }
$size = [int64]$head.Matches[0].Groups[1].Value
"$Out : $([math]::Round($size/1MB)) MB in $Parts parts of $PieceMB MB pieces"
$chunk = [math]::Ceiling($size / $Parts)
$piece = [int64]$PieceMB * 1MB
$jobs = @()
for ($i = 0; $i -lt $Parts; $i++) {
    $a = [int64]$i * $chunk
    $b = [math]::Min($size - 1, $a + $chunk - 1)
    $part = "$Out.part$i"
    $jobs += Start-Job -ScriptBlock {
        param($Url, $part, $a, $b, $piece)
        # Resume: whatever the part file already holds is kept.
        $have = if (Test-Path $part) { (Get-Item $part).Length } else { 0 }
        $pos = $a + $have
        while ($pos -le $b) {
            $end = [math]::Min($b, $pos + $piece - 1)
            $tmp = "$part.piece"
            $ok = $false
            for ($try = 1; $try -le 40 -and -not $ok; $try++) {
                Remove-Item $tmp -Force -ErrorAction SilentlyContinue
                # A connection the CDN has silently stalled must die fast:
                # under 200 kB/s for 15 s aborts the piece, and the retry
                # opens a fresh connection (fresh token too).
                & curl.exe -L -sS --max-time 300 --speed-limit 200000 --speed-time 15 -r "$pos-$end" -o $tmp $Url 2>$null
                if ((Test-Path $tmp) -and (Get-Item $tmp).Length -eq ($end - $pos + 1)) { $ok = $true } else { Start-Sleep 5 }
            }
            if (-not $ok) { throw "piece $pos-$end failed 40 times" }
            $in = [IO.File]::OpenRead($tmp); $outS = [IO.File]::Open($part, 'Append')
            try { $in.CopyTo($outS) } finally { $in.Close(); $outS.Close() }
            Remove-Item $tmp -Force
            $pos = $end + 1
        }
    } -ArgumentList $Url, $part, $a, $b, $piece
}
Wait-Job $jobs | Out-Null
$jobs | ForEach-Object { if ($_.State -ne 'Completed') { Receive-Job $_; throw "part job $($_.Id) $($_.State)" } }
$fs = [IO.File]::Create("$Out.joining")
$total = 0
try {
    for ($i = 0; $i -lt $Parts; $i++) {
        $part = "$Out.part$i"
        $expected = [math]::Min($size - 1, [int64]$i * $chunk + $chunk - 1) - [int64]$i * $chunk + 1
        $len = (Get-Item $part).Length
        if ($len -ne $expected) { throw "part $i is $len bytes, expected $expected" }
        $in = [IO.File]::OpenRead($part)
        try { $in.CopyTo($fs) } finally { $in.Close() }
        $total += $len
    }
} finally { $fs.Close() }
if ($total -ne $size) { throw "joined $total bytes, expected $size" }
Move-Item "$Out.joining" $Out -Force
for ($i = 0; $i -lt $Parts; $i++) { Remove-Item "$Out.part$i" -Force }
"done: $Out $([math]::Round((Get-Item $Out).Length/1MB)) MB"
