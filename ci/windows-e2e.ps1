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

Step 'ports with their processes, LAN devices and the gateway'
$hello = '{"id":1,"method":"hello","params":{"protoMin":1,"protoMax":2,"client":"ci"}}'
$deadline = (Get-Date).AddSeconds(90)
do {
    Start-Sleep -Seconds 5
    $r = @(Bridge @($hello, '{"id":2,"method":"ports.list"}', '{"id":3,"method":"net.devices"}'))
    $ports = ($r[1] | ConvertFrom-Json).result
    $net = ($r[2] | ConvertFrom-Json).result
} while (($null -eq $ports -or $ports.at -eq 0 -or $null -eq $net -or $net.at -eq 0) -and (Get-Date) -lt $deadline)
$rpc = @($ports.listeners | Where-Object { $_.proto -eq 'tcp' -and $_.port -eq 135 })
if ($rpc.Count -lt 1 -or -not $rpc[0].process) { Fail "ports.list: $($r[1])" }
Write-Host "listeners $(@($ports.listeners).Count), 135 is $($rpc[0].process)"
if ($null -eq $net -or $net.at -eq 0 -or -not $net.gateway) { Fail "net.devices: $($r[2])" }
Write-Host "gateway $($net.gateway), devices $(@($net.devices).Count)"

Step 'usage per service and per process'
$deadline = (Get-Date).AddSeconds(60)
do {
    Start-Sleep -Seconds 5
    $r = @(Bridge @($hello, '{"id":2,"method":"usage.latest"}'))
    $u = ($r[1] | ConvertFrom-Json).result
    $svc = @($u.items | Where-Object { $_.kind -eq 'service' })
} while ($svc.Count -lt 5 -and (Get-Date) -lt $deadline)
if ($svc.Count -lt 5 -or -not $u.cgroups) { Fail "usage.latest: $($r[1])" }
$me = @($svc | Where-Object { $_.name -eq 'nightgrid-node' })
if ($me.Count -ne 1 -or $me[0].mem -le 0) { Fail "the node's own service is missing from usage: $($r[1])" }
$procs = @($u.items | Where-Object { $_.kind -eq 'process' })
if ($procs.Count -lt 5) { Fail "usage processes: $($r[1])" }
Write-Host "services $($svc.Count), top process $($procs[0].name) $($procs[0].cpu)%, node $([math]::Round($me[0].mem / 1MB, 1)) MB"

Step 'a change to the hosts file becomes a file event'
$hosts = Join-Path $env:SystemRoot 'System32\drivers\etc\hosts'
$saved = [IO.File]::ReadAllBytes($hosts)
Add-Content -Path $hosts -Value '# nightgrid ci'
$deadline = (Get-Date).AddSeconds(60)
do {
    Start-Sleep -Seconds 5
    $ev = (& $bin events --count 20) -join "`n"
} while ($ev -notmatch 'changed .*hosts' -and (Get-Date) -lt $deadline)
[IO.File]::WriteAllBytes($hosts, $saved)
if ($ev -notmatch 'changed .*hosts') { Fail "no file event for hosts: $ev" }
Write-Host ($ev -split "`n" | Select-String 'hosts' | Select-Object -First 1)

Step 'allow eventlog: failed network logons become login events'
$j = & $bin allow eventlog --json
Write-Host $j
if ($LASTEXITCODE -ne 0) { Fail 'allow eventlog' }
$doc = (& $bin doctor --json) -join ''
if ($doc -notmatch '"check":"eventlog","level":"ok"') { Fail "doctor after allow eventlog: $doc" }
$ip = (Get-NetIPAddress -AddressFamily IPv4 | Where-Object { $_.IPAddress -notlike '127.*' -and $_.IPAddress -notlike '169.254.*' } | Select-Object -First 1).IPAddress
Write-Host "failing three network logons against $ip"
# Through cmd: Windows PowerShell turns a native command's stderr into a
# terminating error under ErrorActionPreference Stop.
foreach ($i in 1..3) { cmd /c "net use \\$ip\IPC$ /user:ngnobody Wrong-pass-1 >nul 2>&1" }
$global:LASTEXITCODE = 0
$deadline = (Get-Date).AddSeconds(100)
do {
    Start-Sleep -Seconds 5
    $ev = (& $bin events --count 20) -join "`n"
} while ($ev -notmatch 'failed login' -and (Get-Date) -lt $deadline)
if ($ev -notmatch 'failed login') {
    Get-WinEvent -FilterHashtable @{ LogName = 'Security'; Id = 4625 } -MaxEvents 2 -ErrorAction SilentlyContinue | Format-List TimeCreated, Message
    Fail "no failed-login event: $ev"
}
Write-Host ($ev -split "`n" | Select-String 'failed login' | Select-Object -First 1)

Step 'allow helper: a listed service restarts through the admin helper'
$cfg = Join-Path $etc 'config.toml'
Add-Content -Path $cfg -Value "`n[actions]`nallow = [`"unit.restart`", `"power.reboot`"]`nunits = [`"W32Time`"]`n"
Set-Service W32Time -StartupType Manual
Start-Service W32Time
Restart-Service nightgrid-node
$j = (& $bin allow helper --json) -join ''
Write-Host $j
if ($LASTEXITCODE -ne 0) { Fail 'allow helper' }
$h = Get-CimInstance Win32_Service -Filter "Name='nightgrid-node-helper'"
if ($null -eq $h -or $h.StartMode -ne 'Manual' -or $h.StartName -ne 'LocalSystem') { Fail "helper service: $($h | Out-String)" }
$privs = (sc.exe qprivs nightgrid-node-helper) -join ' '
if ($privs -notmatch 'SeShutdownPrivilege' -or $privs -match 'SeDebug|SeTcb|SeBackup|SeImpersonate') { Fail "helper privileges: $privs" }
if (-not $fw.ServiceRestriction.ServiceRestricted('nightgrid-node-helper', $bin)) { Fail 'no hardening rule for the helper' }
$sd = (sc.exe sdshow nightgrid-node-helper) -join ''
$nodeSid = (New-Object Security.Principal.NTAccount('NT SERVICE\nightgrid-node')).Translate([Security.Principal.SecurityIdentifier]).Value
if ($sd -notmatch [regex]::Escape("(A;;LCRP;;;$nodeSid)")) { Fail "helper service DACL: $sd" }
$doc = (& $bin doctor --json) -join ''
if ($doc -notmatch '"check":"helper","level":"ok"') { Fail "doctor after allow helper: $doc" }
$t0 = Get-Date
$deadline = (Get-Date).AddSeconds(30)
do {
    Start-Sleep -Seconds 2
    $r = @(Bridge @($hello, '{"id":2,"method":"actions.policy"}'))
    $pol = ($r[1] | ConvertFrom-Json).result
} while (($null -eq $pol -or -not $pol.helper) -and (Get-Date) -lt $deadline)
if (-not $pol.helper -or $pol.allowed -notcontains 'unit.restart' -or $pol.units -notcontains 'W32Time') { Fail "actions.policy: $($r[1])" }
if ($pol.allowed -contains 'firewall.block') { Fail "Windows offers firewall.block: $($r[1])" }
$r = @(Bridge @($hello,
    '{"id":2,"method":"actions.run","params":{"action":"unit.restart","target":"W32Time"}}',
    '{"id":3,"method":"actions.run","params":{"action":"unit.restart","target":"Spooler"}}',
    '{"id":4,"method":"actions.run","params":{"action":"power.reboot","dryRun":true}}'))
Write-Host ($r -join "`n")
$a = ($r[1] | ConvertFrom-Json).result
if ($a.outcome -ne 'ok' -or $a.detail -notmatch 'restarted W32Time') { Fail "unit.restart W32Time: $($r[1])" }
$stopped = @(Get-WinEvent -FilterHashtable @{ LogName = 'System'; ProviderName = 'Service Control Manager'; Id = 7036; StartTime = $t0 } -ErrorAction SilentlyContinue |
    Where-Object { $_.Message -match 'Windows Time.*stopped' })
if ((Get-Service W32Time).Status -ne 'Running' -or $stopped.Count -lt 1) { Fail "W32Time was not restarted (stops seen: $($stopped.Count))" }
if (($r[2] | ConvertFrom-Json).result.outcome -ne 'refused') { Fail "an unlisted service was not refused: $($r[2])" }
if (($r[3] | ConvertFrom-Json).result.outcome -ne 'dryRun') { Fail "power.reboot dry run: $($r[3])" }
if ((Get-Service nightgrid-node-helper).Status -ne 'Running') { Fail 'the node did not start the helper' }

Step 'only the node and administrators may use the helper pipe'
try { $pipeSd = [IO.Directory]::GetAccessControl('\\.\pipe\nightgrid-node-helper') } catch { Fail "read the helper pipe's ACL: $_" }
$who = $pipeSd.Access | ForEach-Object { $_.IdentityReference.Value }
Write-Host "helper pipe: $($who -join ', ')"
if ($who -match 'Users|Everyone|Authenticated|Nightgrid Node Users') { Fail "helper pipe ACL: $($who -join ', ')" }

Step 'deny helper removes it; actions say so'
$j = (& $bin deny helper --json) -join ''
if ($LASTEXITCODE -ne 0 -or (Get-Service nightgrid-node-helper -ErrorAction SilentlyContinue)) { Fail "deny helper: $j" }
$r = @(Bridge @($hello, '{"id":2,"method":"actions.run","params":{"action":"unit.restart","target":"W32Time"}}'))
if (($r[1] | ConvertFrom-Json).result.outcome -ne 'refused') { Fail "unit.restart without the helper: $($r[1])" }
& $bin allow helper --json | Out-Null
if ($LASTEXITCODE -ne 0) { Fail 'allow helper again' }

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
$re = (& $Exe install --json) -join ''
if ($LASTEXITCODE -ne 0 -or $re -match 'firewall') { Fail "reinstall: $re" }
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
if (Get-Service nightgrid-node-helper -ErrorAction SilentlyContinue) { Fail 'helper service still there' }
if (-not (Test-Path (Join-Path $etc 'data\node.db'))) { Fail 'history removed by a plain uninstall' }
$re = (& $Exe install --json) -join ''
if ($LASTEXITCODE -ne 0 -or $re -notmatch 'blocked all network traffic') {
    Get-ChildItem 'HKLM:\SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\RestrictedServices' -Recurse -ErrorAction SilentlyContinue |
        ForEach-Object { $_.Name; $_.GetValueNames() | Where-Object { $_ -match 'nightgrid' -or "$($_)" -eq '' } }
    Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\RestrictedServices\*\*' -ErrorAction SilentlyContinue |
        Out-String -Stream | Select-String -Pattern 'nightgrid'
    Fail "install after uninstall: $re"
}
$pu = (& $Exe uninstall --purge --json) -join ''
Write-Host $pu
if ($LASTEXITCODE -ne 0) { Fail 'uninstall --purge' }
if ($pu -match '"warnings":\["') { Fail "uninstall --purge warned: $pu" }
if (Test-Path $etc) { Fail "$etc still there" }
if (@(Get-NetFirewallRule -Group 'Nightgrid Node' -ErrorAction SilentlyContinue).Count -ne 0) { Fail 'firewall rules left' }
if (Get-LocalGroup -Name 'Nightgrid Node Users' -ErrorAction SilentlyContinue) { Fail 'group left' }
$readers = @(Get-LocalGroupMember -SID 'S-1-5-32-573' -ErrorAction SilentlyContinue | Where-Object { $_.Name -match 'nightgrid-node' })
if ($readers.Count -ne 0) { Fail 'service left in Event Log Readers' }

Write-Host 'Windows e2e passed'
