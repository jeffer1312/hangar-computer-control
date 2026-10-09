$ErrorActionPreference = 'Stop'
[Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$p = [Console]::In.ReadToEnd() | ConvertFrom-Json
if ($p.operation -eq 'prepare') {
    $dir = Join-Path $env:LOCALAPPDATA ('HangarComputerControl\' + $p.digest)
    New-Item -ItemType Directory -Force $dir | Out-Null
    $exe = Join-Path $dir 'windows-agent.exe'
    $valid = (Test-Path -LiteralPath $exe) -and ((Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLower() -eq $p.digest)
    if (-not $valid -and $p.shared_executable) {
        Copy-Item -LiteralPath $p.shared_executable -Destination $exe -Force
        $valid = (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLower() -eq $p.digest
    }
    @{executable=$exe; valid=$valid} | ConvertTo-Json -Compress
} elseif ($p.operation -eq 'start') {
    if ((Get-FileHash -LiteralPath $p.executable -Algorithm SHA256).Hash.ToLower() -ne $p.digest) { throw 'Hash do agente não confere' }
    $config = Join-Path (Split-Path $p.executable) ($p.task + '.json')
    $p.connection | ConvertTo-Json -Compress | Set-Content -LiteralPath $config -Encoding UTF8
    $action = New-ScheduledTaskAction -Execute $p.executable -Argument ('--config "' + $config + '"')
    $principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Highest
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Hours 1) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    Register-ScheduledTask -TaskName $p.task -Action $action -Principal $principal -Settings $settings | Out-Null
    Start-ScheduledTask -TaskName $p.task
    @{started=$true} | ConvertTo-Json -Compress
} elseif ($p.operation -eq 'stop') {
    $task = Get-ScheduledTask -TaskName $p.task -ErrorAction SilentlyContinue
    if ($task) {
        Stop-ScheduledTask -TaskName $p.task
        Unregister-ScheduledTask -TaskName $p.task -Confirm:$false
    }
    if ($p.pid -and $p.executable) {
        $child = Get-CimInstance Win32_Process -Filter ('ProcessId = ' + [int]$p.pid)
        if ($child -and $child.ExecutablePath -eq $p.executable -and $child.CommandLine.Contains($p.task + '.json')) {
            Stop-Process -Id $p.pid -Force
        }
    }
    if ($p.executable) {
        $config = Join-Path (Split-Path $p.executable) ($p.task + '.json')
        if (Test-Path -LiteralPath $config) { Remove-Item -LiteralPath $config }
    }
    @{stopped=$true} | ConvertTo-Json -Compress
} else { throw 'Operação inválida' }
