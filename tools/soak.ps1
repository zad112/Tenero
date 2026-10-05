# A timed local soak test of real Tenero nodes, each with a miner of its own. EXPERIMENTAL; nothing on the test network has value.
#
#   powershell -ExecutionPolicy Bypass -File tools\soak.ps1 [-Minutes 60] [-Visible] [-Bin FOLDER] [-Dir FOLDER]
#
# SHA-256 `test` network (a CPU finds a block in an instant). It checks the PROGRAMS and the PEER-TO-PEER behaviour over time: four nodes, each with its own miner
# (paying its own wallet), so blocks come from four places at once and the chain forks and re-joins all the time; a fifth node joins late; node 2 is stopped cleanly
# and restarted; node 3 is killed outright (a crash) and restarted; wallets pay two payee wallets that never mine, and the payees' balances must equal what was sent.
# It does NOT test the real proof of work, its memory or its speed (that needs the `dev` or `alpha` network and a GPU), and not Linux.
#
# Timeline (fractions of -Minutes; at 60: late node at 15, clean restart of node 2 at 25, crash of node 3 at 40, convergence checkpoints at 12, 21, 33, 48,
# payments about every 3 minutes, miners stopped 3 minutes before the end, then the final checks).
# Every node and miner is started by this script and stopped by it, by exact process id (or `tenerod stop`); nothing is deleted. -Visible gives each its own
# console window (the owner's choice for the long run); without it they are hidden and their output goes to files.
# Exit code 0 = every check passed, 1 = at least one failed.

param(
    [int]$Minutes = 60,
    [switch]$Visible,
    [int]$MinerPace = 5,
    [int]$SampleSeconds = 30,
    [string]$Bin = (Join-Path $PSScriptRoot '..\target\release'),
    [string]$Dir = (Join-Path $env:TEMP ('tenero-soak-' + (Get-Date -Format 'yyyyMMdd-HHmmss')))
)

$ErrorActionPreference = 'Stop'
$node = Join-Path $Bin 'tenerod.exe'; $wallet = Join-Path $Bin 'tenero-wallet.exe'; $miner = Join-Path $Bin 'tenero-miner.exe'
foreach ($f in $node, $wallet, $miner) { if (-not (Test-Path $f)) { throw "missing $f (build with: cargo build --release -p tenero-app)" } }
if ($Minutes -lt 6) { throw '-Minutes must be at least 6' }
New-Item -ItemType Directory -Force $Dir | Out-Null

$script:failed = 0
$script:procs = @{}            # name -> process id, for everything this script started
$script:memMax = @{}           # node name -> largest working set seen (MB)
$start = Get-Date
function Elapsed { [math]::Round(((Get-Date) - $start).TotalMinutes, 1) }
function Say([string]$t) { Write-Host ("[{0,5} min] {1}" -f (Elapsed), $t) }
function Check([string]$name, [bool]$ok, [string]$detail = '') {
    if ($ok) { Say ("PASS  $name") } else { Say ("FAIL  $name  $detail"); $script:failed++ }
}
# each node is in its OWN network group (127.i.0.1: a /16 each), like machines in different places; with one shared group a seed gives every asker the same
# cached answer for 24 hours and a node keeps at most 2 outbound peers per group, so the nodes would not find each other
function Addr([int]$i) { "127.$i.0.1" }
function DataOf([int]$i) { Join-Path $Dir "n$i" }
function Ctl([int]$i) { "$(Addr $i):18332" }
function WaitFor([int]$seconds, [scriptblock]$cond) {
    $end = (Get-Date).AddSeconds($seconds)
    while ((Get-Date) -lt $end) { if (& $cond) { return $true }; Start-Sleep -Milliseconds 700 }
    return $false
}

function Launch([string]$name, [string]$exe, [string[]]$arguments) {
    if ($Visible) {
        $p = Start-Process -FilePath $exe -ArgumentList $arguments -WindowStyle Normal -PassThru
    } else {
        $p = Start-Process -FilePath $exe -ArgumentList $arguments -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $Dir "$name-out.txt") -RedirectStandardError (Join-Path $Dir "$name-err.txt")
    }
    $script:procs[$name] = $p.Id
    return $p
}
function StartNode([int]$i, [string[]]$more = @()) {
    $a = @('--data', (DataOf $i), '--network', 'test', '--listen', "$(Addr $i):18331", '--control', (Ctl $i), '--log_file', (Join-Path $Dir "n$i.log")) + $more
    return (Launch "n$i" $node $a)
}
function StartMiner([int]$i, [string]$address) {
    $a = @('--data', (DataOf $i), '--control', (Ctl $i), '--address', $address, '--backend', 'sha256', '--pace', "$MinerPace", '--log-file', (Join-Path $Dir "m$i.log"))
    return (Launch "m$i" $miner $a)
}
# How it works on the real network: node 1 is the ONLY seed (the one address every other node is given). Every node tells its peers the address it can be reached at
# (`--advertise`), so the seed learns where its peers are and tells the next node that asks; that node dials them, and so on. Nobody is given the full list.
function NetArgs([int]$i) { @('--advertise', "$(Addr $i):18331") + @(if ($i -ne 1) { '--seed'; "$(Addr 1):18331" }) }
function PeersAre([int[]]$ids, [int]$n) { @($ids | Where-Object { (Status $_).Peers -ne $n }).Count -eq 0 }
function PeerList([int[]]$ids) { ($ids | ForEach-Object { (Status $_).Peers }) -join '/' }
function Alive([string]$name) { $script:procs.ContainsKey($name) -and [bool](Get-Process -Id $script:procs[$name] -ErrorAction SilentlyContinue) }

function Status([int]$i) {
    $old = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
    $t = (& $node status --data (DataOf $i) --control (Ctl $i) 2>$null | Out-String)
    $ErrorActionPreference = $old
    if ($t -match 'height (\d+) \((\w+)\)') {
        $height = [int]$Matches[1]; $tip = $Matches[2]
        $peers = 0; if ($t -match 'peers (\d+)') { $peers = [int]$Matches[1] }
        return [pscustomobject]@{ Up = $true; Height = $height; Tip = $tip; Peers = $peers }
    }
    return [pscustomobject]@{ Up = $false; Height = -1; Tip = ''; Peers = 0 }
}
function AllOnOneTip([int[]]$ids) {
    $s = $ids | ForEach-Object { Status $_ }
    if (@($s | Where-Object { -not $_.Up }).Count -gt 0) { return $false }
    if (@($s | Where-Object { $_.Tip -notmatch '^[0-9a-f]{8}$' }).Count -gt 0) { return $false }   # an empty tip must never count as "the same"
    return (@($s | Select-Object -ExpandProperty Tip -Unique).Count -eq 1)
}
function Number([string]$text, [string]$label) {
    if ($text -match ("(?m)^" + $label + "\s+([0-9.]+)")) { return [double]$Matches[1] }
    return -1
}
function WalletFile([string]$who) { Join-Path $Dir "$who.wallet" }
function Balance([string]$who, [int]$viaNode) {
    $old = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
    $t = (& $wallet balance --wallet (WalletFile $who) --data (DataOf $viaNode) --control (Ctl $viaNode) --passphrase-file $pass 2>$null | Out-String)
    $ErrorActionPreference = $old
    return $t
}

Say "folder: $Dir"
Say ("programs: " + ((& $node --version) -join ' ').Substring(0, 52))
$pass = Join-Path $Dir 'pass.txt'; Set-Content -Path $pass -Value 'soak-test-passphrase' -Encoding ascii

# wallets: four that mine (one per node) and two payees that never mine (their balances must equal exactly what was paid to them)
$names = 'alice', 'bob', 'carol', 'dave', 'eve', 'frank'
$addr = @{}
foreach ($who in $names) {
    $out = (& $wallet create --wallet (WalletFile $who) --birth 0 --passphrase-file $pass | Out-String)
    if ($out -match 'address:\s*(tni1[0-9a-f]+)') { $addr[$who] = $Matches[1] }
}
Check 'six wallets made' (@($addr.Keys).Count -eq 6 -and @($addr.Values | Select-Object -Unique).Count -eq 6)

# four nodes, each with its own miner; node 1 is the seed the others start from
$null = StartNode 1 (NetArgs 1)
Check 'node 1 (the seed) answers' (WaitFor 40 { (Status 1).Up })
foreach ($i in 2..4) { $null = StartNode $i (NetArgs $i); Check "node $i answers" (WaitFor 40 { (Status $i).Up }); Start-Sleep -Seconds 8 }   # one at a time: each asks the seed after the earlier ones have told it where they are
Check 'every node found the others and has exactly 3 peers (told only about node 1)' (WaitFor 180 { PeersAre @(1, 2, 3, 4) 3 }) ("peers " + (PeerList @(1, 2, 3, 4)))
$minerOwner = @{ 1 = 'alice'; 2 = 'bob'; 3 = 'carol'; 4 = 'dave' }
foreach ($i in 1..4) { $null = StartMiner $i $addr[$minerOwner[$i]] }
Check 'the chain passes height 10' (WaitFor 120 { (Status 1).Height -ge 10 })
if ($Visible) {
    $handles = @($script:procs.Keys | ForEach-Object { (Get-Process -Id $script:procs[$_] -ErrorAction SilentlyContinue).MainWindowHandle } | Where-Object { $_ -ne 0 })
    Say ("windows with a handle: {0} of {1} processes" -f $handles.Count, $script:procs.Count)
}

# the plan, in minutes from now
$D = [double]$Minutes - 3.0                      # the miners stop 3 minutes before the end
$tLate = 0.25 * $Minutes; $tClean = 0.42 * $Minutes; $tCrash = 0.67 * $Minutes
$checkpoints = @(0.2, 0.35, 0.55, 0.8) | ForEach-Object { $_ * $Minutes }
$payEvery = [math]::Max(1.5, $Minutes / 20.0)
$nextPay = [math]::Max(1.0, 0.08 * $Minutes)
$done = @{}                                      # which one-off events have happened
$members = @(1, 2, 3, 4)                         # nodes expected to be on the one tip
$expectedPaid = @{ eve = 0.0; frank = 0.0 }
$payCount = 0; $payFailed = 0
$t0 = Get-Date
$csv = Join-Path $Dir 'samples.csv'; Add-Content $csv 'minutes,heights,tips_equal,peers,mem_mb_by_node'

function Sample {
    $ids = $script:members + @(if ($script:done['late']) { 5 })
    $ids = @($ids | Select-Object -Unique)
    $st = @{}; foreach ($i in $ids) { $st[$i] = Status $i }
    $mem = @{}
    foreach ($i in $ids) {
        $name = "n$i"
        if (Alive $name) {
            $p = Get-Process -Id $script:procs[$name] -ErrorAction SilentlyContinue
            if ($p) { $mb = [int]($p.WorkingSet64 / 1MB); $mem[$i] = $mb; if (-not $script:memMax.ContainsKey($name) -or $mb -gt $script:memMax[$name]) { $script:memMax[$name] = $mb } }
        }
    }
    $heights = ($ids | ForEach-Object { if ($st[$_].Up) { $st[$_].Height } else { 'down' } }) -join '/'
    $tips = @($ids | Where-Object { $st[$_].Up } | ForEach-Object { $st[$_].Tip } | Select-Object -Unique).Count
    $peers = ($ids | ForEach-Object { if ($st[$_].Up) { $st[$_].Peers } else { '-' } }) -join '/'
    $memtxt = ($ids | ForEach-Object { if ($mem.ContainsKey($_)) { $mem[$_] } else { '-' } }) -join '/'
    Add-Content $script:csv ("{0},{1},{2},{3},{4}" -f (Elapsed), $heights, ($tips -eq 1), $peers, $memtxt)
    Say ("heights {0}  tips {1}  peers {2}  MB {3}" -f $heights, $(if ($tips -eq 1) { 'same' } else { "$tips different" }), $peers, $memtxt)
}

function Pay {
    $from = @('alice', 'bob', 'carol', 'dave')[$script:payCount % 4]
    $fromNode = 1 + ($script:payCount % 4)
    $to = if ($script:payCount % 2 -eq 0) { 'eve' } else { 'frank' }
    if (-not (Alive "n$fromNode")) { $fromNode = 1 }       # the payer's node may be down at this moment: use node 1
    $old = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
    $null = (& $wallet pay --wallet (WalletFile $from) --data (DataOf $fromNode) --control (Ctl $fromNode) --to $addr[$to] --amount 0.5 --passphrase-file $pass 2>$null | Out-String)
    $code = $LASTEXITCODE
    $ErrorActionPreference = $old
    $script:payCount++
    if ($code -eq 0) { $script:expectedPaid[$to] += 0.5; Say ("paid 0.5 from $from (via node $fromNode) to $to") }
    else { $script:payFailed++; Say ("payment from $from via node $fromNode FAILED (exit $code)") }
}
function PayeesMatch([int[]]$viaNodes) {
    # each payee, asked through a node that is NOT the payer's, must hold exactly what was paid to it (they never mine)
    foreach ($who in 'eve', 'frank') {
        $got = Number (Balance $who $viaNodes[0]) 'total'
        if ([math]::Abs($got - $script:expectedPaid[$who]) -gt 0.0001) { return $false }
    }
    return $true
}
function Converge([string]$label, [int[]]$ids, [int]$seconds = 60) {
    Check "every node is on the same tip ($label)" (WaitFor $seconds { AllOnOneTip $ids })
}
# the miner's LOG FILE says "found a block at height N" and, after losing its node, "connected to the node" (the "mined in" wording is only on its screen).
# The test chain's difficulty adjusts toward one block a minute, so each of four miners finds one every few minutes: "it found a block soon" proves nothing, "it reconnected" does.
function MinerLines([int]$i) { @(Get-Content (Join-Path $Dir "m$i.log") -ErrorAction SilentlyContinue | Where-Object { $_ -match 'found a block at height' }).Count }
function MinerConnects([int]$i) { @(Get-Content (Join-Path $Dir "m$i.log") -ErrorAction SilentlyContinue | Where-Object { $_ -match 'miner: connected to the node' }).Count }
$foundAtEvent = @{}
$jobsAtEvent = @{}
# the miner's last status line in its log: "... | 60s 6.54M | ... | jobs 145 | solutions 53"; jobs = how many times it took a new job, rate = attempts per second over the last minute (0 if none)
function MinerStatus([int]$i) {
    $l = @(Get-Content (Join-Path $Dir "m$i.log") -ErrorAction SilentlyContinue | Where-Object { $_ -match 'status: attempts/s' }) | Select-Object -Last 1
    $jobs = -1; $rate = 0.0
    if ($l -match 'jobs (\d+)') { $jobs = [int]$Matches[1] }
    if ($l -match '60s ([0-9.]+)M') { $rate = [double]$Matches[1] }
    [pscustomobject]@{ Jobs = $jobs; Rate = $rate }
}

# ---- the long middle ----
$pending = $checkpoints | Sort-Object
$lastSample = Get-Date
# the loop runs until the time is up AND the three events have happened (a long wait inside one event must never make a later one be skipped); a hard stop 20 minutes late
while (((Get-Date) -lt $t0.AddMinutes($D)) -or ((-not ($done['late'] -and $done['clean'] -and $done['crash'])) -and ((Get-Date) -lt $t0.AddMinutes($D + 20)))) {
    Start-Sleep -Seconds 2
    $m = ((Get-Date) - $t0).TotalMinutes
    if (((Get-Date) - $lastSample).TotalSeconds -ge $SampleSeconds) { Sample; $lastSample = Get-Date }

    if ($m -ge $nextPay) { Pay; $nextPay += $payEvery }

    if ((-not $done['late']) -and $m -ge $tLate) {
        $done['late'] = $true
        Say 'the late node (5) starts, with no miner'
        $null = StartNode 5 (NetArgs 5)
        Check 'node 5 (late) syncs to the same tip' (WaitFor 180 { AllOnOneTip @(1, 2, 3, 4, 5) })
        Check 'every node has exactly 4 peers with five nodes up' (WaitFor 120 { PeersAre @(1, 2, 3, 4, 5) 4 }) ("peers " + (PeerList @(1, 2, 3, 4, 5)))
    }
    if ((-not $done['clean']) -and $m -ge $tClean) {
        $done['clean'] = $true
        $before = MinerConnects 2
        $foundAtEvent[2] = MinerLines 2
        $jobsAtEvent[2] = (MinerStatus 2).Jobs
        Say 'node 2 is stopped cleanly (its miner keeps running)'
        $null = (& $node stop --data (DataOf 2) --control (Ctl 2) | Out-String)
        Check 'node 2 stops cleanly' (WaitFor 60 { -not (Alive 'n2') })
        Start-Sleep -Seconds 30
        Say 'node 2 starts again'
        $null = StartNode 2 (NetArgs 2)
        Check 'node 2 is back on the same tip' (WaitFor 180 { AllOnOneTip @(1, 2, 3, 4, 5) })
        Check 'every node is back to exactly 4 peers after node 2 restarts' (WaitFor 120 { PeersAre @(1, 2, 3, 4, 5) 4 }) ("peers " + (PeerList @(1, 2, 3, 4, 5)))
        Check "node 2's miner reconnects after the restart" (WaitFor 120 { (MinerConnects 2) -gt $before })
    }
    if ((-not $done['crash']) -and $m -ge $tCrash) {
        $done['crash'] = $true
        $before = MinerConnects 3
        $foundAtEvent[3] = MinerLines 3
        $jobsAtEvent[3] = (MinerStatus 3).Jobs
        Say 'node 3 is KILLED (a crash; its miner keeps running)'
        Stop-Process -Id $script:procs['n3'] -Force
        Start-Sleep -Seconds 30
        Say 'node 3 starts again'
        $null = StartNode 3 (NetArgs 3)
        Check 'node 3 is back on the same tip after the crash' (WaitFor 180 { AllOnOneTip @(1, 2, 3, 4, 5) })
        Check 'every node is back to exactly 4 peers after node 3 crashes and restarts' (WaitFor 120 { PeersAre @(1, 2, 3, 4, 5) 4 }) ("peers " + (PeerList @(1, 2, 3, 4, 5)))
        Check "node 3's miner reconnects after the crash" (WaitFor 120 { (MinerConnects 3) -gt $before })
        $log3 = Get-Content (Join-Path $Dir 'n3.log') -ErrorAction SilentlyContinue | Out-String
        Check "node 3's log reports no corruption" (-not ($log3 -match '(?i)corrupt'))
    }
    foreach ($c in @($pending)) {
        if ($m -ge $c) {
            $pending = @($pending | Where-Object { $_ -ne $c })
            $ids = @(1, 2, 3, 4) + @(if ($done['late']) { 5 })
            Converge ("checkpoint at {0:N0} min" -f $c) $ids 90
            $via = if ($done['late']) { 5 } else { 4 }
            if ($payCount -gt 0) { Check ("the payees hold exactly what was paid (asked through node $via), checkpoint at {0:N0} min" -f $c) (WaitFor 150 { PayeesMatch @($via) }) }
        }
    }
}

# ---- the end: freeze, compare, stop, check the stores ----
foreach ($i in 2, 3) {
    # NOT "it found a block again": with four miners and one block a minute, a miner can go 15 minutes without one by chance (about 1 run in 100; it did once,
    # and a replay of that miner on a copy of its data, killed and restarted, found one after 12 minutes). What shows it works is that it is hashing and keeps taking jobs.
    $s = MinerStatus $i
    Check ("miner $i is still hashing and taking new jobs after its node's restart/crash") (($s.Rate -gt 0) -and ($s.Jobs -ge $jobsAtEvent[$i] + 3)) "jobs $($s.Jobs) now, $($jobsAtEvent[$i]) at the event, rate $($s.Rate)M/s"
    Say ("note: miner $i found {0} block(s) after the event (luck, not checked)" -f ((MinerLines $i) - $foundAtEvent[$i]))
}
Say 'the miners stop (the chain is frozen so the nodes can be compared exactly)'
foreach ($i in 1..4) { if (Alive "m$i") { Stop-Process -Id $script:procs["m$i"] -Force } }
Start-Sleep -Seconds 30
$all = 1..5
Converge 'final, miners stopped' $all 120
Check 'the payees hold exactly what was paid (final)' (WaitFor 180 { PayeesMatch @(5) }) "paid: $($expectedPaid['eve']) and $($expectedPaid['frank']) in $payCount payments, $payFailed failed"
Check 'no payment command failed' ($payFailed -eq 0) "$payFailed of $payCount"
Sample
$final = Status 1
Say ("final: height {0}, tip {1}; {2} payments" -f $final.Height, $final.Tip, $payCount)

foreach ($i in $all) { $null = (& $node stop --data (DataOf $i) --control (Ctl $i) 2>$null | Out-String) }
Check 'every node stopped cleanly' (WaitFor 120 { @($all | Where-Object { Alive "n$_" }).Count -eq 0 })

# the stores, opened offline (a dry run changes nothing): each must open, and all must hold the same chain
$tips = @{}
foreach ($i in $all) {
    $t = (& $node rewind --data (DataOf $i) --network test --to 1 2>&1 | Out-String)
    if ($t -match 'the chain was at height (\d+) \(tip (\w+)\)') { $tips[$i] = "$($Matches[1])/$($Matches[2])" } else { $tips[$i] = 'could not open: ' + ($t -replace '\s+', ' ') }
}
Check 'every data folder opens offline' (@($tips.Values | Where-Object { $_ -notmatch '^\d+/[0-9a-f]{8}$' }).Count -eq 0) (($tips.GetEnumerator() | ForEach-Object { "n$($_.Key)=$($_.Value)" }) -join ' ')
Check 'every data folder holds the same chain' (@($tips.Values | Select-Object -Unique).Count -eq 1) (($tips.GetEnumerator() | ForEach-Object { "n$($_.Key)=$($_.Value)" }) -join ' ')

# the logs
foreach ($i in $all) {
    $log = Get-Content (Join-Path $Dir "n$i.log") -ErrorAction SilentlyContinue | Out-String
    Check "node $i's log has no panic and no corruption" (-not ($log -match '(?i)panick|corrupt'))
    $bans = @([regex]::Matches($log, 'bans (\d+)') | ForEach-Object { [int]$_.Groups[1].Value } | Sort-Object -Descending | Select-Object -First 1)
    Check "node $i banned no honest peer" ($bans.Count -eq 0 -or $bans[0] -eq 0) "bans reached $($bans[0])"
}
Say ("largest memory seen per node (MB): " + (($script:memMax.GetEnumerator() | Sort-Object Name | ForEach-Object { "$($_.Key)=$($_.Value)" }) -join ' '))
foreach ($k in $script:procs.Keys) { if (Alive $k) { Say "still running: $k (pid $($script:procs[$k]))" } }
Say "logs, samples.csv and data: $Dir"
if ($script:failed -eq 0) { Say 'ALL CHECKS PASSED'; exit 0 } else { Say ("{0} CHECK(S) FAILED" -f $script:failed); exit 1 }
