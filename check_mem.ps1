$samples = (Get-Counter '\Process(*)\ID Process', '\Process(*)\Working Set - Private' -ErrorAction SilentlyContinue).CounterSamples
$byInst = @{}
foreach ($s in $samples) {
  $inst = $s.Path -replace '^.*\\process\(([^)]*)\)\\.*$', '$1'
  if (-not $byInst[$inst]) { $byInst[$inst] = @{} }
  if ($s.Path -like '*\id process') { $byInst[$inst].pid = [int]$s.CookedValue } else { $byInst[$inst].pws = $s.CookedValue }
}
$pws = @{}
foreach ($v in $byInst.Values) { if ($v.pid) { $pws[$v.pid] = $v.pws } }

$procs = Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'chatdesk-web.exe' -or ($_.Name -eq 'msedgewebview2.exe' -and $_.CommandLine -like '*ChatDeskWeb*') }
$rows = foreach ($p in $procs) {
  $gp = Get-Process -Id $p.ProcessId -ErrorAction SilentlyContinue
  $t = if ($p.CommandLine -match '--type=(\S+)') { $matches[1] } elseif ($p.Name -eq 'chatdesk-web.exe') { 'shell' } else { 'browser' }
  if ($p.CommandLine -match '--utility-sub-type=(\S+)') { $t += ':' + $matches[1] }
  [pscustomobject]@{ Type=$t; WS_MB=[math]::Round($gp.WorkingSet64/1MB); Commit_MB=[math]::Round($gp.PrivateMemorySize64/1MB); TaskMgr_MB=[math]::Round($pws[[int]$p.ProcessId]/1MB) }
}
$rows | Format-Table -AutoSize | Out-String
"TOTAL: {0} procs | Task Manager (private WS) {1} MB | commit {2} MB | raw WS {3} MB" -f @($rows).Count, ($rows | Measure-Object TaskMgr_MB -Sum).Sum, ($rows | Measure-Object Commit_MB -Sum).Sum, ($rows | Measure-Object WS_MB -Sum).Sum
