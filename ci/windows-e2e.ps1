# End-to-end test of the Windows node on a real Windows Server runner
# (GitHub's windows-2022 and windows-2025 images, run as an Administrator):
# install as a service, talk to it through `bridge` like the app does over
# SSH, check the sandbox, refuse an account outside the group, uninstall.
#
#   powershell -File ci/windows-e2e.ps1 -Exe target\release\nightgrid-node.exe

param([Parameter(Mandatory = $true)][string]$Exe)

$ErrorActionPreference = 'Stop'
$Exe = (Resolve-Path $Exe).Path
$bin = Join-Path $env:ProgramFiles 'Nightgrid Node\nightgrid-node.exe'
$etc = Join-Path $env:ProgramData 'Nightgrid Node'

function Step($m) { Write-Host "==> $m" }
function Fail($m) {
    Write-Host "FAIL: $m"
    $log = Join-Path $etc 'data\log\node.log'
    if (Test-Path $log) { Write-Host '--- node.log ---'; Get-Content $log -Tail 80 }
    exit 1
}

# Runs the bridge with NDJSON lines on stdin and returns the answer lines.
function Bridge([string[]]$lines, [string]$exe = $bin) {
    $psi = New-Object Diagnostics.ProcessStartInfo
    $psi.FileName = $exe
    $psi.Arguments = 'bridge'
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $p = [Diagnostics.Process]::Start($psi)
    $out = New-Object Collections.Generic.List[string]
    foreach ($l in $lines) {
        $p.StandardInput.WriteLine($l)
        $p.StandardInput.Flush()
        $a = $p.StandardOutput.ReadLine()
        if ($null -eq $a) { break }
        $out.Add($a)
    }
    $p.StandardInput.Close()
    if (-not $p.WaitForExit(10000)) { $p.Kill() }
    return $out
}

Step 'version and help work without a service'
& $Exe version
if ($LASTEXITCODE -ne 0) { Fail 'version' }

Step 'bridge before install says not_running'
$r = @(Bridge @('{"id":1,"method":"hello","params":{"protoMin":1,"protoMax":2,"client":"ci"}}') $Exe)
if (-not ($r -join '' | Select-String '"not_running"')) { Fail "bridge before install: $r" }

Step 'install'
$json = & $Exe install --json
Write-Host $json
if ($LASTEXITCODE -ne 0) { Fail 'install failed' }
$rep = $json | ConvertFrom-Json
if ($rep.action -ne 'install' -or $rep.init -ne 'scm') { Fail "install report: $json" }
if (-not (Test-Path $bin)) { Fail "no $bin" }

Step 'the service runs as its virtual account, with one privilege and a restricted SID'
$svc = Get-CimInstance Win32_Service -Filter "Name='nightgrid-node'"
if ($svc.State -ne 'Running') { Fail "service state $($svc.State)" }
if ($svc.StartName -ne 'NT SERVICE\nightgrid-node') { Fail "runs as $($svc.StartName)" }
$sid = (sc.exe qsidtype nightgrid-node) -join ' '
if ($sid -notmatch 'RESTRICTED') { Fail "sid type: $sid" }
$privs = (sc.exe qprivs nightgrid-node) -join ' '
if ($privs -notmatch 'SeChangeNotifyPrivilege' -or $privs -match 'SeImpersonate|SeDebug|SeBackup') { Fail "privileges: $privs" }

Step 'the network is blocked for the service'
$fw = New-Object -ComObject HNetCfg.FwPolicy2
if (-not $fw.ServiceRestriction.ServiceRestricted('nightgrid-node', $bin)) { Fail 'no service hardening rule' }
$rules = @(Get-NetFirewallRule -Group 'Nightgrid Node' -ErrorAction SilentlyContinue)
if ($rules.Count -ne 2) { Fail "expected 2 firewall rules, found $($rules.Count)" }

Step 'the data folder is private'
$acl = (Get-Acl (Join-Path $etc 'data')).Access | ForEach-Object { $_.IdentityReference.Value }
if ($acl -match 'Users|Everyone|Authenticated') { Fail "data folder ACL: $($acl -join ', ')" }

Step 'the machine PATH has the install folder'
$path = [Environment]::GetEnvironmentVariable('Path', 'Machine')
if ($path -notmatch [regex]::Escape((Split-Path $bin))) { Fail 'not on PATH' }

Step 'hello, node.info and metrics through the bridge'
$lines = @(
    '{"id":1,"method":"hello","params":{"protoMin":1,"protoMax":2,"client":"ci"}}',
    '{"id":2,"method":"node.info"}'
)
$r = @(Bridge $lines)
if ($r.Count -lt 2) { Fail "bridge answers: $r" }
$w = $r[0] | ConvertFrom-Json
if ($w.result.proto -ne 2 -or -not $w.result.caps.'metrics.sync') { Fail "hello: $($r[0])" }
if ($w.result.caps.PSObject.Properties.Name -contains 'containers') { Fail 'advertises containers, which Windows lacks so far' }
$info = ($r[1] | ConvertFrom-Json).result
if ($info.node.hostname -eq '' -or $info.rssBytes -le 0) { Fail "node.info: $($r[1])" }
Write-Host "working set: $([math]::Round($info.rssBytes / 1MB, 1)) MB"

Step 'a sample with CPU, memory and disks arrives'
$deadline = (Get-Date).AddSeconds(40)
do {
    Start-Sleep -Seconds 3
    $r = @(Bridge @('{"id":1,"method":"hello","params":{"protoMin":1,"protoMax":2,"client":"ci"}}', '{"id":2,"method":"metrics.latest"}'))
    $s = ($r[1] | ConvertFrom-Json).result
} while (($null -eq $s -or $null -eq $s.cpu) -and (Get-Date) -lt $deadline)
if ($null -eq $s -or $null -eq $s.cpu) { Fail "no sample: $($r[1])" }
if ($s.memTotal -le 0 -or @($s.mounts).Count -lt 1 -or @($s.disks).Count -lt 1) { Fail "sample: $($r[1])" }
Write-Host "cpu $($s.cpu)% mem $($s.memUsed)/$($s.memTotal) mounts $(@($s.mounts).Count) disks $(@($s.disks).Count)"

Step 'status, doctor and events work'
& $bin status
if ($LASTEXITCODE -ne 0) { Fail 'status' }
$doc = & $bin doctor --json
Write-Host $doc
if ($LASTEXITCODE -ne 0) { Fail 'doctor reported a failure' }
& $bin events --count 5
if ($LASTEXITCODE -ne 0) { Fail 'events' }

Step 'an account outside the group is refused'
$pw = 'Ng-' + [guid]::NewGuid().ToString('N').Substring(0, 16) + '!a1'
$sec = ConvertTo-SecureString $pw -AsPlainText -Force
New-LocalUser -Name 'ngoutsider' -Password $sec -AccountNeverExpires | Out-Null
try {
    $cred = New-Object Management.Automation.PSCredential('ngoutsider', $sec)
    $outFile = Join-Path $env:TEMP 'ng-outsider.txt'
    $inFile = Join-Path $env:TEMP 'ng-outsider-in.txt'
    Set-Content -Path $inFile -Value '{"id":1,"method":"hello","params":{"protoMin":1,"protoMax":2,"client":"ci"}}'
    $started = $true
    try {
        $p = Start-Process -FilePath $bin -ArgumentList 'bridge' -Credential $cred -RedirectStandardInput $inFile `
            -RedirectStandardOutput $outFile -NoNewWindow -PassThru -Wait -WorkingDirectory $env:SystemRoot
    } catch {
        $started = $false
        Write-Host "warning: could not start a process as another account here: $_"
    }
    if ($started) {
        $o = Get-Content $outFile -Raw
        Write-Host "outsider got: $o"
        if ($o -notmatch '"no_access"') { Fail "an account outside the group got through: $o" }
    }
} finally {
    Remove-LocalUser -Name 'ngoutsider'
}

Step 'install again is an upgrade in place and keeps the node id'
$id1 = $info.node.nodeId
& $Exe install --json | Out-Null
if ($LASTEXITCODE -ne 0) { Fail 'reinstall' }
$r = @(Bridge @('{"id":1,"method":"hello","params":{"protoMin":1,"protoMax":2,"client":"ci"}}'))
if (($r[0] | ConvertFrom-Json).result.node.nodeId -ne $id1) { Fail 'node id changed on reinstall' }

Step 'the service comes back after a crash'
$pid1 = (Get-CimInstance Win32_Service -Filter "Name='nightgrid-node'").ProcessId
Stop-Process -Id $pid1 -Force
$deadline = (Get-Date).AddSeconds(30)
do {
    Start-Sleep -Seconds 1
    $svc = Get-CimInstance Win32_Service -Filter "Name='nightgrid-node'"
} while (($svc.State -ne 'Running' -or $svc.ProcessId -eq $pid1) -and (Get-Date) -lt $deadline)
if ($svc.State -ne 'Running') { Fail 'not restarted after a crash' }

Step 'uninstall keeps history; uninstall --purge removes everything'
& $Exe uninstall --json
if ($LASTEXITCODE -ne 0) { Fail 'uninstall' }
if (Get-Service nightgrid-node -ErrorAction SilentlyContinue) { Fail 'service still there' }
if (-not (Test-Path (Join-Path $etc 'data\node.db'))) { Fail 'history removed by a plain uninstall' }
& $Exe install --json | Out-Null
& $Exe uninstall --purge --json
if ($LASTEXITCODE -ne 0) { Fail 'uninstall --purge' }
if (Test-Path $etc) { Fail "$etc still there" }
if (@(Get-NetFirewallRule -Group 'Nightgrid Node' -ErrorAction SilentlyContinue).Count -ne 0) { Fail 'firewall rules left' }
if (Get-LocalGroup -Name 'Nightgrid Node Users' -ErrorAction SilentlyContinue) { Fail 'group left' }

Write-Host 'Windows e2e passed'
