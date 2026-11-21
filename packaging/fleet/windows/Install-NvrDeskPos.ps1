<#
.SYNOPSIS
  Silent GPO / Intune deployer for NVRDesk on Windows POS terminals (Session 1 desktop capture).
.DESCRIPTION
  Installs the NVRDesk MSI, generates nvrdesk.toml from a fleet template, validates config,
  adds Windows Firewall inbound rules (RTSP 8554 / ONVIF 8080 / WS-Discovery UDP 3702),
  and registers a Scheduled Task running inside the logged-on POS cashier session.
#>
param(
  [string]$MsiPath = ".\NVRDesk-0.4.0.0-x86_64.msi",
  [string]$Template = "pos-cashier",
  [string]$Site = "store-01",
  [string]$TerminalId = $env:COMPUTERNAME,
  [string]$ConfigDir = "$env:ProgramData\NVRDesk",
  [string]$Username = "",
  [string]$Password = "",
  [string]$SrtTarget = ""
)

$ErrorActionPreference = "Stop"

Write-Host "Installing NVRDesk MSI from $MsiPath..."
Start-Process msiexec.exe -ArgumentList "/i `"$MsiPath`" /quiet /norestart" -Wait

New-Item -ItemType Directory -Force -Path $ConfigDir | Out-Null
$ConfigFile = Join-Path $ConfigDir "nvrdesk.toml"
$ExePath = "${env:ProgramFiles}\NVRDesk\bin\nvrdesk.exe"

$InitArgs = @("fleet", "init", $Template, "--out", $ConfigFile, "--site", $Site, "--terminal-id", $TerminalId)
if ($Username) { $InitArgs += @("--username", $Username) }
if ($Password) { $InitArgs += @("--password", $Password) }
if ($SrtTarget) { $InitArgs += @("--srt-target", $SrtTarget) }

& $ExePath @InitArgs
& $ExePath fleet validate --config $ConfigFile

# Open local LAN firewall ports for NVR/VMS discovery and streaming.
New-NetFirewallRule -DisplayName "NVRDesk RTSP" -Direction Inbound -Protocol TCP -LocalPort 8554 -Action Allow -ErrorAction SilentlyContinue | Out-Null
New-NetFirewallRule -DisplayName "NVRDesk ONVIF/HTTP" -Direction Inbound -Protocol TCP -LocalPort 8080 -Action Allow -ErrorAction SilentlyContinue | Out-Null
New-NetFirewallRule -DisplayName "NVRDesk WS-Discovery" -Direction Inbound -Protocol UDP -LocalPort 3702 -Action Allow -ErrorAction SilentlyContinue | Out-Null

# Run inside interactive user session (NOT Session 0) so WGC/DXGI desktop & input hooks work cleanly.
$Action = New-ScheduledTaskAction -Execute $ExePath -Argument "--config `"$ConfigFile`""
$Trigger = New-ScheduledTaskTrigger -AtLogOn
$Principal = New-ScheduledTaskPrincipal -GroupId "BUILTIN\Users" -RunLevel Limited
$Settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1)

Register-ScheduledTask -TaskName "NVRDeskPOSAgent" -Action $Action -Trigger $Trigger -Principal $Principal -Settings $Settings -Force | Out-Null
Write-Host "NVRDesk provisioned for $Site/$TerminalId ($ConfigFile)."
