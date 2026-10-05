# A temporary local network of real Tenero nodes on this machine, for checking that the programs work together. EXPERIMENTAL; nothing on it has value.
#
#   powershell -ExecutionPolicy Bypass -File tools\localnet.ps1 [-Bin FOLDER] [-Nodes 4] [-Blocks 14]
#
# It uses the SHA-256 `test` network (a CPU mines a block in an instant; rings of 2, maturity of 1 block), so it checks the PROGRAMS and the PEER-TO-PEER
# behaviour (start-up, connecting, sync, relay, a wallet payment, a clean restart, a crash and recovery, a late joiner), NOT the real proof of work, its memory
# or its speed (use the `dev` or `alpha` network and a GPU for those; see docs/TESTING.md).
#
# What it does, in order, and checks at each step:
#   1. makes two throwaway wallets (alice, bob)           2. starts node 1 (the seed) and nodes 2..N, each on its own loopback address (127.0.0.i)
#   3. every node finds a peer                              4. starts a miner (its own process) that pays alice
#   5. every node reaches the same tip                      6. alice pays bob 1.5, and bob (asking a DIFFERENT node) sees it
#   7. a late node joins and syncs                          8. a node is stopped cleanly and restarted: it catches up
#   9. a node is killed (a crash) and restarted: its chain is intact and it catches up
#  10. everything is stopped; every process it started is checked to be gone.
# It stops only the processes IT started (by their exact process ids, or `tenerod stop`), and deletes nothing: the folder it used is printed at the end.
# Exit code 0 = every check passed, 1 = at least one failed.

param(
    [string]$Bin = (Join-Path $PSScriptRoot '..\target\release'),
    [int]$Nodes = 4,
    [int]$Blocks = 14,
    [switch]$BreakOnPurpose,   # a self-test of the checks: the late node gets no seed, so "a late node syncs" and the "same tip" checks after it MUST fail (exit code 1)
    [string]$Dir = (Join-Path $env:TEMP ('tenero-localnet-' + (Get-Date -Format 'yyyyMMdd-HHmmss')))
)

$ErrorActionPreference = 'Stop'
$node = Join-Path $Bin 'tenerod.exe'
$wallet = Join-Path $Bin 'tenero-wallet.exe'
$miner = Join-Path $Bin 'tenero-miner.exe'
foreach ($f in $node, $wallet, $miner) { if (-not (Test-Path $f)) { throw "missing $f (build with: cargo build --release -p tenero-app)" } }
if ($Nodes -lt 3) { throw '-Nodes must be at least 3' }
New-Item -ItemType Directory -Force $Dir | Out-Null

$script:failed = 0
$script:procs = @{}          # name -> process id, for everything this script started
function Check([string]$name, [bool]$ok, [string]$detail = '') {
    if ($ok) { Write-Host ("PASS  {0}" -f $name) } else { Write-Host ("FAIL  {0}  {1}" -f $name, $detail); $script:failed++ }
}
function Addr([int]$i) { "127.0.0.$i" }
function DataOf([int]$i) { Join-Path $Dir "n$i" }
function Ctl([int]$i) { "$(Addr $i):18332" }

function StartNode([int]$i, [string[]]$more = @()) {
    $a = @('--data', (DataOf $i), '--network', 'test', '--listen', "$(Addr $i):18331", '--control', (Ctl $i), '--log_file', (Join-Path $Dir "n$i.log")) + $more
    $p = Start-Process -FilePath $node -ArgumentList $a -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $Dir "n$i-out.txt") -RedirectStandardError (Join-Path $Dir "n$i-err.txt")
    $script:procs["n$i"] = $p.Id
    return $p
}
function Status([int]$i) {
    # a node that is not up yet makes `status` complain on stderr: that is expected while waiting, so it is not an error here
    $old = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
    $t = (& $node status --data (DataOf $i) --control (Ctl $i) 2>$null | Out-String)
    $ErrorActionPreference = $old
    # read each value with its own match (a second -match would overwrite $Matches)
    if ($t -match 'height (\d+) \((\w+)\)') {
        $height = [int]$Matches[1]; $tip = $Matches[2]
        $peers = 0; if ($t -match 'peers (\d+)') { $peers = [int]$Matches[1] }
        return [pscustomobject]@{ Up = $true; Height = $height; Tip = $tip; Peers = $peers }
    }
    return [pscustomobject]@{ Up = $false; Height = -1; Tip = ''; Peers = 0 }
}
function WaitFor([string]$what, [int]$seconds, [scriptblock]$cond) {
    $end = (Get-Date).AddSeconds($seconds)
    while ((Get-Date) -lt $end) { if (& $cond) { return $true }; Start-Sleep -Milliseconds 700 }
    return $false
}
function AllOnOneTip([int[]]$ids) {
    $s = $ids | ForEach-Object { Status $_ }
    if (@($s | Where-Object { -not $_.Up }).Count -gt 0) { return $false }
    # an empty or malformed tip must never count as "the same": a check that cannot fail proves nothing
    if (@($s | Where-Object { $_.Tip -notmatch '^[0-9a-f]{8}$' }).Count -gt 0) { return $false }
    return (@($s | Select-Object -ExpandProperty Tip -Unique).Count -eq 1)
}
function Number([string]$text, [string]$label) {
    if ($text -match ("(?m)^" + $label + "\s+([0-9.]+)")) { return [double]$Matches[1] }
    return -1
}

Write-Host "folder: $Dir"
Write-Host ("programs: " + ((& $node --version) -join ' ').Substring(0, 60))

# 1. throwaway wallets (the passphrase is a test value; the wallets hold nothing of value)
$pass = Join-Path $Dir 'pass.txt'; Set-Content -Path $pass -Value 'localnet-test-passphrase' -Encoding ascii
$addrs = @{}
foreach ($who in 'alice', 'bob') {
    $out = (& $wallet create --wallet (Join-Path $Dir "$who.wallet") --birth 0 --passphrase-file $pass | Out-String)
    if ($out -match 'address:\s*(tni1[0-9a-f]+)') { $addrs[$who] = $Matches[1] }
}
Check 'two wallets made' ($addrs.Count -eq 2 -and $addrs['alice'] -ne $addrs['bob'])

# 2 and 3. the seed, then the others, each pointing at the seed
$null = StartNode 1
Check 'the seed node answers' (WaitFor 'seed' 40 { (Status 1).Up })
foreach ($i in 2..$Nodes) { $null = StartNode $i @('--seed', "$(Addr 1):18331") }
$all = 1..$Nodes
Check "all $Nodes nodes answer" (WaitFor 'nodes' 60 { @($all | Where-Object { -not (Status $_).Up }).Count -eq 0 })
Check 'every node has at least one peer' (WaitFor 'peers' 60 { @($all | Where-Object { (Status $_).Peers -lt 1 }).Count -eq 0 })

# 4. a miner of its own, paying alice, asking node 1 for blocks
$mp = Start-Process -FilePath $miner -ArgumentList @('--data', (DataOf 1), '--control', (Ctl 1), '--address', $addrs['alice'], '--backend', 'sha256', '--pace', '1') -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $Dir 'miner-out.txt') -RedirectStandardError (Join-Path $Dir 'miner-err.txt')
$script:procs['miner'] = $mp.Id
Check "the chain reaches height $Blocks" (WaitFor 'height' 150 { (Status 1).Height -ge $Blocks })

# 5. one tip everywhere (the miner keeps going, so sample until a pass sees them all alike)
Check 'every node is on the same tip' (WaitFor 'tip' 60 { AllOnOneTip $all })

# 6. a payment, relayed to a node that is not the miner's
$bal = (& $wallet balance --wallet (Join-Path $Dir 'alice.wallet') --data (DataOf 1) --control (Ctl 1) --passphrase-file $pass | Out-String)
Check 'alice has spendable coins' ((Number $bal 'spendable') -gt 2) "balance output: $($bal -replace '\s+', ' ')"
$null = (& $wallet pay --wallet (Join-Path $Dir 'alice.wallet') --data (DataOf 1) --control (Ctl 1) --to $addrs['bob'] --amount 1.5 --passphrase-file $pass | Out-String)
Check 'the payment was accepted by node 1' ($LASTEXITCODE -eq 0)
$seen = WaitFor 'bob' 90 { $b = (& $wallet balance --wallet (Join-Path $Dir 'bob.wallet') --data (DataOf 3) --control (Ctl 3) --passphrase-file $pass | Out-String); (Number $b 'total') -ge 1.5 }
Check 'bob sees 1.5 coins, asking node 3' $seen

# 7. a late joiner
$late = $Nodes + 1
$lateSeed = @('--seed', "$(Addr 1):18331")
if ($BreakOnPurpose) { $lateSeed = @() }   # the self-test: a node that is never told where to connect cannot sync, and the checks must say so
$null = StartNode $late $lateSeed
Check 'a late node syncs to the same tip' (WaitFor 'late' 90 { AllOnOneTip ($all + $late) })

# 8. a clean stop and restart of node 2
$null = (& $node stop --data (DataOf 2) --control (Ctl 2) | Out-String)
$gone = WaitFor 'stop' 40 { -not (Get-Process -Id $script:procs['n2'] -ErrorAction SilentlyContinue) }
Check 'node 2 stops cleanly' $gone
Start-Sleep -Seconds 3
$null = StartNode 2 @('--seed', "$(Addr 1):18331")
Check 'node 2 restarts and is back on the tip' (WaitFor 'restart' 90 { AllOnOneTip ($all + $late) })

# 9. a crash: node 3 is killed outright (by its exact process id), then restarted
Stop-Process -Id $script:procs['n3'] -Force
Start-Sleep -Seconds 4
$null = StartNode 3 @('--seed', "$(Addr 1):18331")
Check 'node 3 restarts after a crash and is back on the tip' (WaitFor 'crash' 90 { AllOnOneTip ($all + $late) })
$log3 = Get-Content (Join-Path $Dir 'n3.log') -ErrorAction SilentlyContinue | Out-String
Check "node 3's log reports no corruption" (-not ($log3 -match '(?i)corrupt'))

# 10. everything is stopped and checked
$final = Status 1
Write-Host ("final: node 1 at height {0}, tip {1}" -f $final.Height, $final.Tip)
Stop-Process -Id $script:procs['miner'] -Force -ErrorAction SilentlyContinue
foreach ($i in (1..$late)) { $null = (& $node stop --data (DataOf $i) --control (Ctl $i) | Out-String) }
$left = $null
$cleared = WaitFor 'stop all' 60 { $script:left = @($script:procs.Values | Where-Object { Get-Process -Id $_ -ErrorAction SilentlyContinue }); $script:left.Count -eq 0 }
Check 'every process this script started is gone' $cleared ("still running: " + ($script:left -join ', '))
Write-Host ("logs and data: $Dir")
if ($script:failed -eq 0) { Write-Host 'ALL CHECKS PASSED'; exit 0 } else { Write-Host ("{0} CHECK(S) FAILED" -f $script:failed); exit 1 }
