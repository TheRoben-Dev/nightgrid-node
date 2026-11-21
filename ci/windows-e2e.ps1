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
    $clog = Join-Path $etc 'data\log\checker\checker.log'
    if (Test-Path $clog) { Write-Host '--- checker.log ---'; Get-Content $clog -Tail 40 }
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
if (-not $w.result.caps.containers) { Fail 'does not advertise containers' }
if (-not $w.result.caps.checks) { Fail 'does not advertise checks' }
if (-not $w.result.caps.health) { Fail 'does not advertise health' }
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
# Proof of a restart: a new process when W32Time has its own svchost, else
# the SCM's 7036 "stopped" event (Server 2025 no longer logs those for
# every service, but it splits svchost there).
$w32pid = (Get-CimInstance Win32_Service -Filter "Name='W32Time'").ProcessId
$w32shared = @(Get-CimInstance Win32_Service -Filter "ProcessId=$w32pid").Count -gt 1
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
$w32now = (Get-CimInstance Win32_Service -Filter "Name='W32Time'").ProcessId
$restarted = if ($w32shared) { $stopped.Count -ge 1 } else { $w32now -ne $w32pid }
if ((Get-Service W32Time).Status -ne 'Running' -or -not $restarted) {
    Fail "W32Time was not restarted (pid $w32pid -> $w32now, shared $w32shared, stops seen $($stopped.Count))"
}
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

Step 'firewall.block makes an expiring Windows Firewall rule; expired ones go; unblock removes it'
$c = Get-Content $cfg -Raw
$c = $c -replace 'allow = \["unit.restart", "power.reboot"\]', 'allow = ["unit.restart", "power.reboot", "firewall.block", "firewall.unblock"]'
Set-Content -Path $cfg -Value $c -NoNewline
Restart-Service nightgrid-node
$fwp = New-Object -ComObject HNetCfg.FwPolicy2
$old = New-Object -ComObject HNetCfg.FWRule
$old.Name = 'Nightgrid Node block 198.51.100.7'; $old.Description = 'until 1'; $old.Grouping = 'Nightgrid Node blocks'
$old.Direction = 1; $old.Action = 0; $old.RemoteAddresses = '198.51.100.7'; $old.Enabled = $true
$fwp.Rules.Add($old)
Start-Sleep -Seconds 3
$r = @(Bridge @($hello,
    '{"id":2,"method":"actions.policy"}',
    '{"id":3,"method":"actions.run","params":{"action":"firewall.block","target":"203.0.113.9"}}',
    '{"id":4,"method":"actions.run","params":{"action":"firewall.block","target":"192.168.1.10"}}',
    '{"id":5,"method":"block.list"}'))
Write-Host ($r -join "`n")
if (($r[1] | ConvertFrom-Json).result.allowed -notcontains 'firewall.block') { Fail "policy lacks firewall.block: $($r[1])" }
$b = ($r[2] | ConvertFrom-Json).result
if ($b.outcome -ne 'ok' -or $b.detail -notmatch 'blocked 203.0.113.9') { Fail "firewall.block: $($r[2])" }
if (($r[3] | ConvertFrom-Json).result.outcome -ne 'refused' -and ($r[3] | ConvertFrom-Json).result.outcome -ne 'failed') { Fail "a private address was blocked: $($r[3])" }
if (($r[3] | ConvertFrom-Json).result.detail -notmatch 'private') { Fail "private address refusal: $($r[3])" }
$rules = @(Get-NetFirewallRule -Group 'Nightgrid Node blocks' -ErrorAction SilentlyContinue)
Write-Host "rules: $(@($rules | ForEach-Object { "$($_.DisplayName) [$($_.Description)] $($_.Direction) $($_.Action)" }) -join '; ')"
$rule = $rules | Where-Object { $_.DisplayName -eq 'Nightgrid Node block 203.0.113.9' }
if ($null -eq $rule -or $rule.Direction -ne 'Inbound' -or $rule.Action -ne 'Block' -or $rule.Description -notmatch '^until \d+$') { Fail 'no inbound block rule for 203.0.113.9' }
if ((($rule | Get-NetFirewallAddressFilter).RemoteAddress) -notcontains '203.0.113.9') { Fail "rule address: $(($rule | Get-NetFirewallAddressFilter).RemoteAddress)" }
if ($rules | Where-Object { $_.DisplayName -eq 'Nightgrid Node block 198.51.100.7' }) { Fail 'an expired block rule was not removed' }
if ($r[4] -notmatch '203\.0\.113\.9') { Fail "block.list: $($r[4])" }
$r = @(Bridge @($hello,
    '{"id":2,"method":"actions.run","params":{"action":"firewall.unblock","target":"203.0.113.9"}}',
    '{"id":3,"method":"actions.run","params":{"action":"firewall.block","target":"203.0.113.10"}}'))
Write-Host ($r -join "`n")
if (($r[1] | ConvertFrom-Json).result.outcome -ne 'ok') { Fail "firewall.unblock: $($r[1])" }
if (Get-NetFirewallRule -DisplayName 'Nightgrid Node block 203.0.113.9' -ErrorAction SilentlyContinue) { Fail 'the unblocked rule is still there' }
if (($r[2] | ConvertFrom-Json).result.outcome -ne 'ok') { Fail "second block: $($r[2])" }

Step 'self-healing: an autofix restarts a listed service that stopped with an error'
$fixes = Join-Path $etc 'fixes.toml'
Set-Content -Path $fixes -Value "[[autofix]]`nid = `"w32time`"`nenabled = true`ndry_run = false`nwhen = `"unit-failed`"`ntarget = `"W32Time`"`naction = `"unit.restart`"`nfor_minutes = 1`n"
$w32pid = (Get-CimInstance Win32_Service -Filter "Name='W32Time'").ProcessId
$w32shared = @(Get-CimInstance Win32_Service -Filter "ProcessId=$w32pid").Count -gt 1
$r = @(Bridge @($hello, '{"id":2,"method":"heal.status"}'))
Write-Host $r[1]
if ($w32shared) {
    Write-Host "W32Time shares its svchost here; killing it would take other services down, so only the rule is checked"
} else {
    # Automatic start, so a stop that isn't an administrator's counts as
    # failed; no SCM recovery, so only the node can bring it back.
    Set-Service W32Time -StartupType Automatic
    & sc.exe failure W32Time reset= 0 actions= '""' | Out-Null
    Stop-Process -Id $w32pid -Force
    Start-Sleep -Seconds 2
    $st = (sc.exe query W32Time) -join ' '
    Write-Host "after the kill: $st"
    $deadline = (Get-Date).AddSeconds(150)
    do {
        Start-Sleep -Seconds 5
        $svc = Get-CimInstance Win32_Service -Filter "Name='W32Time'"
    } while (($svc.State -ne 'Running' -or $svc.ProcessId -eq $w32pid) -and (Get-Date) -lt $deadline)
    $r = @(Bridge @($hello, '{"id":2,"method":"actions.log","params":{"after":0}}'))
    Write-Host $r[1]
    if ($svc.State -ne 'Running') { Fail "the autofix did not restart W32Time: $($r[1])" }
    if ($r[1] -notmatch 'autofix w32time') { Fail "no autofix entry in the action log: $($r[1])" }
    Set-Service W32Time -StartupType Manual
}
Remove-Item $fixes

Step 'fix menu: the same fix names, run through the admin helper'
Set-Content -Path $fixes -Value @'
allow = ["fix.package-cache", "fix.ssh-permissions", "fix.runbook"]

[[runbook]]
name = "hello"
description = "Says ok"
run = ['C:\Windows\System32\cmd.exe', '/c', 'echo ok']
timeout_secs = 30
'@
# A key file with the inherited ProgramData ACL (Users can read), which
# OpenSSH refuses; the fix must leave only SYSTEM and Administrators.
$sshDir = Join-Path $env:ProgramData 'ssh'
$madeSshDir = -not (Test-Path $sshDir)
if ($madeSshDir) { New-Item -ItemType Directory -Path $sshDir | Out-Null }
$keys = Join-Path $sshDir 'administrators_authorized_keys'
$hadKeys = Test-Path $keys
if (-not $hadKeys) {
    Set-Content -Path $keys -Value 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleKeyForCiOnly ci'
    $acl = Get-Acl $keys
    $acl.SetAccessRuleProtection($false, $false)
    $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule('BUILTIN\Users', 'Read', 'Allow')))
    Set-Acl -Path $keys -AclObject $acl
}
$out = (& $bin fix list --json) -join "`n"
if ($LASTEXITCODE -ne 0) { Fail "fix list: $out" }
$fl = $out | ConvertFrom-Json
$names = @($fl.fixes | ForEach-Object { $_.name }) -join ' '
Write-Host "fixes: $names; auto: $(@($fl.auto | ForEach-Object { $_.name }) -join ' ')"
if ($names -ne 'restart start packages prune ssh reboot run') { Fail "fix list names: $out" }
if (@($fl.fixes | Where-Object { $_.name -eq 'run' }).state -ne 'allowed') { Fail "runbook not allowed: $out" }
$out = (& $bin fix run hello -y) -join "`n"
Write-Host $out
if ($LASTEXITCODE -ne 0 -or $out -notmatch 'runbook hello done: ok') { Fail "fix run hello: $out" }
$out = (& $bin fix ssh $env:USERNAME -y) -join "`n"
Write-Host $out
if ($LASTEXITCODE -ne 0 -or $out -notmatch 'tightened|already tight') { Fail "fix ssh: $out" }
if (-not $hadKeys) {
    $acl = Get-Acl $keys
    $who = @($acl.Access | ForEach-Object { $_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value } | Sort-Object -Unique) -join ' '
    Write-Host "key file ACL: $who (protected: $($acl.AreAccessRulesProtected))"
    if (-not $acl.AreAccessRulesProtected -or $who -ne 'S-1-5-18 S-1-5-32-544') { Fail "key file ACL after the fix: $who" }
}
# A file in the download cache that the fix must remove; Windows Update's
# start type stays what the image set (the runner images disable it).
$wuStart = (Get-Service wuauserv).StartType
$dl = Join-Path $env:SystemRoot 'SoftwareDistribution\Download'
if (-not (Test-Path $dl)) { New-Item -ItemType Directory -Path $dl | Out-Null }
$junk = Join-Path $dl 'nightgrid-ci.bin'
[IO.File]::WriteAllBytes($junk, (New-Object byte[] 1048576))
$r = @(Bridge @($hello,
    '{"id":2,"method":"actions.run","params":{"action":"fix.package-cache"}}',
    '{"id":3,"method":"actions.run","params":{"action":"fix.ssh-permissions","target":"o''brien"}}',
    '{"id":4,"method":"actions.run","params":{"action":"fix.journal-vacuum"}}'))
Write-Host ($r -join "`n")
$a = ($r[1] | ConvertFrom-Json).result
if ($a.outcome -ne 'ok' -or $a.detail -notmatch 'cleaned the Windows Update download cache') { Fail "fix.package-cache: $($r[1])" }
if ($null -ne ($r[2] | ConvertFrom-Json).result -and ($r[2] | ConvertFrom-Json).result.outcome -eq 'ok') { Fail "a quoted account name ran: $($r[2])" }
if ($null -ne ($r[3] | ConvertFrom-Json).result -and ($r[3] | ConvertFrom-Json).result.outcome -eq 'ok') { Fail "journal-vacuum ran on Windows: $($r[3])" }
if ($a.detail -match '\(0 B freed\)' -or (Test-Path $junk)) { Fail "the cache file was not removed: $($a.detail)" }
if ((Get-Service wuauserv).StartType -ne $wuStart) { Fail "the cache fix changed wuauserv's start type to $((Get-Service wuauserv).StartType)" }
if (-not $hadKeys) { Remove-Item $keys -Force }
if ($madeSshDir) { Remove-Item $sshDir -Recurse -Force }
Remove-Item $fixes

Step 'rollbacks: a change is put back (contents and ACL) on revert and by the timer, kept on confirm'
$rb = 'C:\ngn-rollback-ci'
New-Item -ItemType Directory -Path $rb -Force | Out-Null
$app = Join-Path $rb 'app.conf'
$new = Join-Path $rb 'new.conf'
Set-Content -Path $app -Value 'v1' -NoNewline
& icacls.exe $app /inheritance:r /grant:r '*S-1-5-18:F' '*S-1-5-32-544:F' '*S-1-1-0:R' | Out-Null
function AclOf($f) {
    $a = Get-Acl $f
    "protected=$($a.AreAccessRulesProtected) " + (@($a.Access | ForEach-Object { "$($_.IdentityReference)|$($_.FileSystemRights)|$($_.AccessControlType)|$($_.IsInherited)" } | Sort-Object) -join '; ')
}
$dacl0 = AclOf $app
Write-Host "app.conf DACL: $dacl0"
$c = Get-Content $cfg -Raw
$c = $c -replace '(?m)^allow = \[', 'allow = ["files.rollback", '
$c = $c -replace '(?m)^paths = \[\]', "paths = ['$rb']"
Set-Content -Path $cfg -Value $c -NoNewline
Restart-Service nightgrid-node
Start-Sleep -Seconds 3
function Rpc($id, $method, $params) { @{ id = $id; method = $method; params = $params } | ConvertTo-Json -Compress -Depth 6 }
$r = @(Bridge @($hello,
    (Rpc 2 'changes.begin' @{ paths = @($app, $new); confirmWithinSecs = 60; note = 'ci revert' }),
    (Rpc 3 'changes.begin' @{ paths = @('C:\ProgramData\Nightgrid Node\config.toml'); confirmWithinSecs = 60 }),
    (Rpc 4 'changes.begin' @{ paths = @('C:\Windows\win.ini'); confirmWithinSecs = 60 })))
Write-Host ($r -join "`n")
$ch = ($r[1] | ConvertFrom-Json).result
if ($null -eq $ch -or $ch.state -ne 'pending' -or $ch.detail -notmatch 'saved 2 files \(1 new') { Fail "changes.begin: $($r[1])" }
if ($null -ne ($r[2] | ConvertFrom-Json).result) { Fail "a change to the node's own config was accepted: $($r[2])" }
if ($null -ne ($r[3] | ConvertFrom-Json).result) { Fail "a path outside [rollback] paths was accepted: $($r[3])" }
$snaps = Join-Path $env:ProgramData 'Nightgrid Node Rollback'
$sa = Get-Acl $snaps
$who = @($sa.Access | ForEach-Object { $_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value } | Sort-Object -Unique) -join ' '
Write-Host "snapshot folder ACL: $who (protected: $($sa.AreAccessRulesProtected))"
if (-not $sa.AreAccessRulesProtected -or $who -ne 'S-1-5-18 S-1-5-32-544') { Fail "snapshot folder ACL: $who" }
if (-not (Test-Path (Join-Path $snaps "$($ch.id)\manifest.json"))) { Fail 'no snapshot manifest' }
# The change: new contents, a looser ACL, a new file.
Set-Content -Path $app -Value 'v2 broken' -NoNewline
& icacls.exe $app /grant '*S-1-1-0:F' | Out-Null
Set-Content -Path $new -Value 'x'
$r = @(Bridge @($hello, (Rpc 2 'changes.revert' @{ id = $ch.id })))
Write-Host $r[1]
$rv = ($r[1] | ConvertFrom-Json).result
if ($rv.state -ne 'reverted') { Fail "changes.revert: $($r[1])" }
$dacl1 = AclOf $app
if ((Get-Content $app -Raw) -ne 'v1') { Fail "contents not put back: $(Get-Content $app -Raw)" }
if ($dacl1 -ne $dacl0) { Fail "ACL not put back: $dacl1 (was $dacl0)" }
if (Test-Path $new) { Fail 'the new file is still there after the revert' }
if (Test-Path (Join-Path $snaps $ch.id)) { Fail 'snapshot kept after a clean revert' }
# Confirm keeps the change and drops the snapshot.
$r = @(Bridge @($hello, (Rpc 2 'changes.begin' @{ paths = @($app); confirmWithinSecs = 60; note = 'ci confirm' })))
$ch = ($r[1] | ConvertFrom-Json).result
Set-Content -Path $app -Value 'v3' -NoNewline
$r = @(Bridge @($hello, (Rpc 2 'changes.confirm' @{ id = $ch.id })))
Write-Host $r[1]
if (($r[1] | ConvertFrom-Json).result.state -ne 'confirmed' -or (Get-Content $app -Raw) -ne 'v3') { Fail "changes.confirm: $($r[1])" }
if (Test-Path (Join-Path $snaps $ch.id)) { Fail 'snapshot kept after confirm' }
# The dead-man timer: nobody confirms, so it reverts by itself.
$r = @(Bridge @($hello, (Rpc 2 'changes.begin' @{ paths = @($app); confirmWithinSecs = 30; note = 'ci timer' })))
$ch = ($r[1] | ConvertFrom-Json).result
if ($null -eq $ch) { Fail "changes.begin (timer): $($r[1])" }
Set-Content -Path $app -Value 'v4 never confirmed' -NoNewline
$deadline = (Get-Date).AddSeconds(90)
do { Start-Sleep -Seconds 5 } while ((Get-Content $app -Raw) -ne 'v3' -and (Get-Date) -lt $deadline)
$r = @(Bridge @($hello, (Rpc 2 'changes.list' @{})))
Write-Host $r[1]
if ((Get-Content $app -Raw) -ne 'v3') { Fail "the timer did not revert: $(Get-Content $app -Raw)" }
$last = @(($r[1] | ConvertFrom-Json).result.changes | Where-Object { $_.id -eq $ch.id })
if ($last.Count -ne 1 -or $last[0].state -ne 'reverted') { Fail "changes.list after the timer: $($r[1])" }
Remove-Item $rb -Recurse -Force

Step 'allow docker: containers, their events and container actions through Docker''s pipe'
$dj = Join-Path $env:ProgramData 'docker\config\daemon.json'
$djOld = if (Test-Path $dj) { Get-Content $dj -Raw } else { $null }
Write-Host "daemon.json before: $djOld"
if ($djOld -and ($djOld | ConvertFrom-Json).group) {
    # A runner image with its own pipe group: take it out for the test, put it back after.
    $o = $djOld | ConvertFrom-Json; $o.PSObject.Properties.Remove('group'); $o | ConvertTo-Json | Set-Content $dj
}
$j = (& $bin allow docker --json) -join ''
Write-Host $j
if ($LASTEXITCODE -ne 0) { Fail "allow docker: $j" }
if ((Get-Content $dj -Raw | ConvertFrom-Json).group -ne 'NT SERVICE\nightgrid-node') { Fail "daemon.json group: $(Get-Content $dj -Raw)" }
$again = (& $bin allow docker --json) -join ''
if ($again -notmatch 'already lets the service') { Fail "allow docker twice: $again" }
$docker = Get-Service docker -ErrorAction SilentlyContinue
if ($null -eq $docker) {
    Write-Host 'No Docker Engine on this runner: only the settings are checked'
} else {
    Restart-Service docker
    $deadline = (Get-Date).AddSeconds(90)
    do { Start-Sleep -Seconds 3; & docker version --format '{{.Server.Version}}' 2>$null | Out-Null } while ($LASTEXITCODE -ne 0 -and (Get-Date) -lt $deadline)
    $tag = if ([Environment]::OSVersion.Version.Build -ge 26100) { 'ltsc2025' } else { 'ltsc2022' }
    $img = "mcr.microsoft.com/windows/nanoserver:$tag"
    & docker pull -q $img
    if ($LASTEXITCODE -ne 0) { Fail "docker pull $img" }
    & docker rm -f ngn-ci 2>$null | Out-Null
    & docker run -d --name ngn-ci $img ping -t localhost | Out-Null
    if ($LASTEXITCODE -ne 0) { Fail 'docker run' }
    $deadline = (Get-Date).AddSeconds(60)
    do {
        Start-Sleep -Seconds 5
        $r = @(Bridge @($hello, '{"id":2,"method":"containers.list"}'))
        $cl = ($r[1] | ConvertFrom-Json).result
        $mine = @($cl.containers | Where-Object { $_.name -eq 'ngn-ci' })
    } while (($cl.access -ne 'ok' -or $mine.Count -ne 1 -or $mine[0].state -ne 'running') -and (Get-Date) -lt $deadline)
    Write-Host $r[1]
    if ($cl.access -ne 'ok') { Fail "container access: $($r[1])" }
    if ($mine.Count -ne 1 -or $mine[0].state -ne 'running') { Fail "ngn-ci not listed as running: $($r[1])" }
    $c = Get-Content $cfg -Raw
    $c = $c -replace '(?m)^allow = \[', 'allow = ["container.restart", '
    Set-Content -Path $cfg -Value $c -NoNewline
    Restart-Service nightgrid-node
    Start-Sleep -Seconds 3
    $before = (& docker inspect -f '{{.State.StartedAt}}' ngn-ci)
    $deadline = (Get-Date).AddSeconds(60)
    do {
        Start-Sleep -Seconds 3
        $r = @(Bridge @($hello, '{"id":2,"method":"actions.run","params":{"action":"container.restart","target":"ngn-ci"}}'))
        $a = ($r[1] | ConvertFrom-Json)
    } while ($null -ne $a.error -and $a.error.message -match 'no access|no container' -and (Get-Date) -lt $deadline)
    Write-Host $r[1]
    if ($a.result.outcome -ne 'ok') { Fail "container.restart: $($r[1])" }
    $after = (& docker inspect -f '{{.State.StartedAt}}' ngn-ci)
    if ($after -eq $before) { Fail "ngn-ci was not restarted ($before)" }
    & docker stop -t 2 ngn-ci | Out-Null
    # Container events are held 120 s before they are written, so a die and
    # a start close together become one "restarted" entry.
    $deadline = (Get-Date).AddSeconds(240)
    do {
        Start-Sleep -Seconds 10
        $r = @(Bridge @($hello, '{"id":2,"method":"events.sync","params":{"after":0,"limit":500,"kinds":["container"]}}'))
        $seen = $r[1] -match 'ngn-ci'
    } while (-not $seen -and (Get-Date) -lt $deadline)
    if (-not $seen) { Fail "no event for ngn-ci: $($r[1])" }
    & docker rm -f ngn-ci | Out-Null
}
$j = (& $bin deny docker --json) -join ''
Write-Host $j
if ($LASTEXITCODE -ne 0) { Fail "deny docker: $j" }
if ((Get-Content $dj -Raw | ConvertFrom-Json).group) { Fail "group left in daemon.json: $(Get-Content $dj -Raw)" }
if ($null -eq $djOld) { Remove-Item $dj } else { Set-Content -Path $dj -Value $djOld -NoNewline }
if ($docker) { Restart-Service docker }

Step 'allow checks: a runner service with its own account checks HTTP and TCP, outbound only to [checks] networks'
$r = @(Bridge @($hello, '{"id":2,"method":"node.info"}'))
if ($r[1] -notmatch '"checks":"denied"') { Fail "checks should be denied before allow: $($r[1])" }
$cfgBefore = Get-Content $cfg -Raw
if ($cfgBefore -notmatch '(?m)^interval_secs = 60$') { Fail "no [checks] interval in the template: $cfgBefore" }
Set-Content -Path $cfg -Value ($cfgBefore -replace '(?m)^interval_secs = 60$', 'interval_secs = 30') -NoNewline
Restart-Service nightgrid-node
Start-Sleep -Seconds 3
$lan = (Get-NetIPAddress -AddressFamily IPv4 | Where-Object { $_.IPAddress -match '^(10\.|172\.(1[6-9]|2\d|3[01])\.|192\.168\.)' } | Select-Object -First 1).IPAddress
if (-not $lan) { $lan = '127.0.0.1' }
Write-Host "LAN address: $lan"
$srv = Start-Job {
    $t = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Any, 18767)
    $t.Start()
    $l = [System.Net.HttpListener]::new()
    $l.Prefixes.Add('http://127.0.0.1:18766/')
    $l.Start()
    while ($true) { $c = $l.GetContext(); $c.Response.StatusCode = 204; $c.Response.Close() }
}
Start-Sleep -Seconds 3
$list = @(
    @{ id = 'web'; name = 'web'; url = 'http://127.0.0.1:18766/' },
    @{ id = 'lan'; name = 'lan'; url = "tcp://${lan}:18767" },
    @{ id = 'gone'; name = 'gone'; url = 'tcp://127.0.0.1:1' },
    @{ id = 'out'; name = 'out'; url = 'http://1.1.1.1/' }
)
$r = @(Bridge @($hello, (Rpc 2 'checks.set' @{ checks = $list })))
if ($r[1] -notmatch '"id":"web"') { Fail "checks.set: $($r[1])" }
$r = @(Bridge @($hello, (Rpc 2 'checks.set' @{ checks = @(@{ id = 'x'; name = 'x'; url = "http://a/`r`nX: y" }) })))
if ($r[1] -notmatch 'bad characters') { Fail "a header injection URL was accepted: $($r[1])" }
$r = @(Bridge @($hello, (Rpc 2 'checks.set' @{ checks = $list })))
$j = (& $bin allow checks --json) -join ''
Write-Host $j
if ($LASTEXITCODE -ne 0) { Fail "allow checks: $j" }
if ($j -notmatch 'own service account') { Fail "no checks warning: $j" }
$cs = Get-CimInstance Win32_Service -Filter "Name='nightgrid-node-checker'"
if ($null -eq $cs -or $cs.StartName -ne 'NT SERVICE\nightgrid-node-checker' -or $cs.StartMode -ne 'Auto') { Fail "runner service: $($cs | Out-String)" }
$sidType = (sc.exe qsidtype nightgrid-node-checker) -join ' '
if ($sidType -notmatch 'RESTRICTED') { Fail "runner SID type: $sidType" }
$privs = (sc.exe qprivs nightgrid-node-checker) -join ' '
if ($privs -match 'SeShutdown|SeDebug|SeTcb|SeBackup|SeImpersonate|SeAssignPrimary') { Fail "runner privileges: $privs" }
$fw = New-Object -ComObject HNetCfg.FwPolicy2
if (-not $fw.ServiceRestriction.ServiceRestricted('nightgrid-node-checker', $bin)) { Fail 'no hardening rule for the runner' }
$wsh = @($fw.ServiceRestriction.Rules | Where-Object { $_.ServiceName -eq 'nightgrid-node-checker' -and $_.Action -eq 1 })
if ($wsh.Count -ne 1 -or $wsh[0].RemoteAddresses -notmatch '192\.168\.0\.0') { Fail "runner allow rule: $($wsh | Format-List Name,RemoteAddresses,Direction | Out-String)" }
Write-Host "runner may reach: $($wsh[0].RemoteAddresses)"
if (@(Get-NetFirewallRule -Group 'Nightgrid Node check runner' -ErrorAction SilentlyContinue).Count -ne 2) { Fail 'expected 2 visible runner rules' }
$deadline = (Get-Date).AddSeconds(90)
do {
    Start-Sleep -Seconds 3
    $r = @(Bridge @($hello, '{"id":2,"method":"checks.latest"}'))
    $cl = ($r[1] | ConvertFrom-Json).result
} while (($cl.access -ne 'ok' -or @($cl.readings).Count -lt 4) -and (Get-Date) -lt $deadline)
Write-Host $r[1]
if ($cl.access -ne 'ok') { Fail "runner never reported: $($r[1])" }
$by = @{}; foreach ($x in $cl.readings) { $by[$x.id] = $x.result }
if (-not $by.web.up -or $by.web.status -ne 204) { Fail "web: $($by.web | ConvertTo-Json -Compress)" }
if (-not $by.lan.up) { Fail "lan: $($by.lan | ConvertTo-Json -Compress)" }
if ($by.gone.up) { Fail 'a closed port counted as up' }
if ($by.out.error -notmatch 'outside \[checks\] networks') { Fail "out: $($by.out | ConvertTo-Json -Compress)" }
$r = @(Bridge @($hello, '{"id":2,"method":"checks.report","params":{"readings":[]}}'))
if ($r[1] -notmatch 'only the check runner') { Fail "an administrator could report checks: $($r[1])" }
$doc = (& $bin doctor --json) -join ''
if ($doc -notmatch '"check":"checks","level":"ok"') { Fail "doctor after allow checks: $doc" }

# The firewall, not only the runner's own filter: an address added to the
# config without allow checks again is let through by the runner, and
# Windows still blocks it.
$net = Test-NetConnection 1.1.1.1 -Port 443 -WarningAction SilentlyContinue
if ($net.TcpTestSucceeded) {
    $c = Get-Content $cfg -Raw
    Set-Content -Path $cfg -Value ($c -replace '(?m)^  "fe80::/10",$', "  `"fe80::/10`",`n  `"1.1.1.1/32`",") -NoNewline
    if ((Get-Content $cfg -Raw) -notmatch '1\.1\.1\.1/32') { Fail 'could not add 1.1.1.1 to [checks] networks' }
    $r = @(Bridge @($hello, (Rpc 2 'checks.set' @{ checks = @(@{ id = 'fw'; name = 'fw'; url = 'tcp://1.1.1.1:443' }) })))
    Restart-Service nightgrid-node-checker
    $deadline = (Get-Date).AddSeconds(90)
    do {
        Start-Sleep -Seconds 3
        $r = @(Bridge @($hello, '{"id":2,"method":"checks.latest"}'))
        $fwr = @(($r[1] | ConvertFrom-Json).result.readings | Where-Object { $_.id -eq 'fw' })
    } while ($fwr.Count -eq 0 -and (Get-Date) -lt $deadline)
    Write-Host $r[1]
    if ($fwr.Count -eq 0) { Fail "no reading for fw: $($r[1])" }
    if ($fwr[0].result.up) { Fail 'the runner reached an address outside the firewall rule' }
    if ($fwr[0].result.error -match 'outside \[checks\] networks') { Fail "the runner refused fw itself, so the firewall was not tested: $($r[1])" }
} else {
    Write-Host 'no internet on this runner: firewall check skipped'
}

$j = (& $bin deny checks --json) -join ''
Write-Host $j
if ($LASTEXITCODE -ne 0) { Fail "deny checks: $j" }
if (Get-Service nightgrid-node-checker -ErrorAction SilentlyContinue) { Fail 'runner service left after deny' }
if (@(Get-NetFirewallRule -Group 'Nightgrid Node check runner' -ErrorAction SilentlyContinue).Count -ne 0) { Fail 'runner rules left after deny' }
$fw = New-Object -ComObject HNetCfg.FwPolicy2
if (@($fw.ServiceRestriction.Rules | Where-Object { $_.ServiceName -eq 'nightgrid-node-checker' }).Count -ne 0) { Fail 'runner hardening rules left after deny' }
Stop-Job $srv; Remove-Job $srv -Force
Set-Content -Path $cfg -Value $cfgBefore -NoNewline
Restart-Service nightgrid-node
Start-Sleep -Seconds 3

Step 'allow health: disks and Storage Spaces through the read-only health helper'
$j = (& $bin allow health --json) -join ''
Write-Host $j
if ($LASTEXITCODE -ne 0) { Fail 'allow health' }
$hs = Get-CimInstance Win32_Service -Filter "Name='nightgrid-node-health'"
if ($null -eq $hs -or $hs.StartMode -ne 'Manual' -or $hs.StartName -ne 'LocalSystem') { Fail "health service: $($hs | Out-String)" }
$sidType = (sc.exe qsidtype nightgrid-node-health) -join ' '
if ($sidType -notmatch 'RESTRICTED') { Fail "health helper SID type: $sidType" }
$privs = (sc.exe qprivs nightgrid-node-health) -join ' '
if ($privs -match 'SeShutdown|SeDebug|SeTcb|SeBackup|SeImpersonate') { Fail "health helper privileges: $privs" }
if (-not $fw.ServiceRestriction.ServiceRestricted('nightgrid-node-health', $bin)) { Fail 'no hardening rule for the health helper' }
$deadline = (Get-Date).AddSeconds(150)
do {
    Start-Sleep -Seconds 5
    $r = @(Bridge @($hello, '{"id":2,"method":"health.latest"}'))
    $hl = ($r[1] | ConvertFrom-Json).result
} while (($null -eq $hl -or $null -eq $hl.smart.report -or $null -eq $hl.pools.report) -and (Get-Date) -lt $deadline)
Write-Host $r[1]
if ($hl.access -ne 'ok') { Fail "health access: $($r[1])" }
if ($null -eq $hl.smart.report -or @($hl.smart.report.disks).Count -lt 1) { Fail "no disks from the health helper: $($r[1])" }
if ($null -eq $hl.pools.report) { Fail "no pools report: $($r[1])" }
Write-Host "disks: $(@($hl.smart.report.disks | ForEach-Object { "$($_.name) $($_.protocol) $($_.health)" }) -join '; ')"
if ($hl.updates) { Write-Host "updates: $(@($hl.updates.report.updates).Count) pending, note $($hl.updates.report.note), error $($hl.updates.error)" }
$doc = (& $bin doctor --json) -join ''
if ($doc -notmatch '"check":"health","level":"ok"') { Fail "doctor after allow health: $doc" }

Step 'UPS on USB: "battery" reads through the health helper and the guard follows it'
$c = Get-Content $cfg -Raw
if ($c -notmatch '(?m)^ups = \[\]') { Fail "no ups line in the template: $c" }
$c = $c -replace '(?m)^ups = \[\]', 'ups = ["battery"]'
$c += "`n[ups]`nshutdown = true`ndry_run = true`nlow_battery = true`n"
Set-Content -Path $cfg -Value $c -NoNewline
Restart-Service nightgrid-node
$deadline = (Get-Date).AddSeconds(90)
do {
    Start-Sleep -Seconds 5
    $r = @(Bridge @($hello, '{"id":2,"method":"health.latest"}', '{"id":3,"method":"ups.guard"}'))
    $hl = ($r[1] | ConvertFrom-Json).result
} while (($null -eq $hl -or @($hl.ups).Count -lt 1) -and (Get-Date) -lt $deadline)
Write-Host ($r -join "`n")
if (-not $w.result.caps.ups) { Fail 'does not advertise ups' }
$u = @($hl.ups)[0]
if ($null -eq $u -or $u.target -ne 'battery') { Fail "no battery reading: $($r[1])" }
$hasBattery = @(Get-CimInstance Win32_Battery).Count -gt 0
if ($hasBattery) {
    if ($u.reading.error) { Fail "battery read failed on a host with a battery: $($r[1])" }
} elseif ($u.reading.error -notmatch 'no UPS or battery') { Fail "a host without a battery should say so: $($r[1])" }
$g = ($r[2] | ConvertFrom-Json).result
if (-not $g.enabled -or -not $g.dryRun -or $g.target -ne 'battery') { Fail "ups.guard: $($r[2])" }

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

Step 'upgrade refuses a wrong checksum and installs a verified exe in place'
$ErrorActionPreference = 'Continue'
$o = (& $bin upgrade $Exe --sha256 ('0' * 64) --json 2>&1 | Out-String)
$code = $LASTEXITCODE
$ErrorActionPreference = 'Stop'
if ($code -eq 0 -or $o -notmatch 'checksum mismatch') { Fail "a wrong checksum was not refused: $o" }
if (Test-Path (Join-Path (Split-Path $bin) '.nightgrid-node.new.exe')) { Fail 'the staged exe was left behind' }
$sha = (Get-FileHash $Exe -Algorithm SHA256).Hash.ToLower()
$up = (& $bin upgrade $Exe --sha256 $sha --json) -join ''
Write-Host $up
if ($LASTEXITCODE -ne 0) { Fail "upgrade: $up" }
$u = $up | ConvertFrom-Json
if (-not $u.ok -or $u.reverted) { Fail "upgrade report: $up" }
if (-not (Test-Path (Join-Path (Split-Path $bin) 'nightgrid-node.prev.exe'))) { Fail 'no previous exe kept' }
$r = @(Bridge @($hello))
if (($r[0] | ConvertFrom-Json).result.node.nodeId -ne $id1) { Fail 'node id changed on upgrade' }

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
if (Get-Service nightgrid-node-health -ErrorAction SilentlyContinue) { Fail 'health service still there' }
if (Get-NetFirewallRule -Group 'Nightgrid Node blocks' -ErrorAction SilentlyContinue) { Fail 'block rules left after uninstall' }
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
if (Test-Path (Join-Path $env:ProgramData 'Nightgrid Node Rollback')) { Fail 'rollback snapshots left after --purge' }
if (@(Get-NetFirewallRule -Group 'Nightgrid Node' -ErrorAction SilentlyContinue).Count -ne 0) { Fail 'firewall rules left' }
if (Get-LocalGroup -Name 'Nightgrid Node Users' -ErrorAction SilentlyContinue) { Fail 'group left' }
$readers = @(Get-LocalGroupMember -SID 'S-1-5-32-573' -ErrorAction SilentlyContinue | Where-Object { $_.Name -match 'nightgrid-node' })
if ($readers.Count -ne 0) { Fail 'service left in Event Log Readers' }

Write-Host 'Windows e2e passed'
