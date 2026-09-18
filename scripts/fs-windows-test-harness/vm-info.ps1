# scripts/win/vm-info.ps1 — emit a JSON blob describing the VM
# (Windows + ntfs.sys + chkdsk versions). Used by matrix-baseline.sh
# to populate the `vm` field in test-diagnostics/matrix-results.json.
#
# Invoked over SSH from the orchestrator; no parameters.

$os   = Get-CimInstance Win32_OperatingSystem
$ntfs = Get-Item C:/Windows/System32/drivers/ntfs.sys
$chk  = Get-Item C:/Windows/System32/chkdsk.exe

[ordered]@{
    os_caption          = $os.Caption
    os_build            = $os.BuildNumber
    os_version          = $os.Version
    os_arch             = $os.OSArchitecture
    ntfs_sys            = $ntfs.VersionInfo.FileVersion
    chkdsk_version      = $chk.VersionInfo.FileVersion
    powershell_version  = $PSVersionTable.PSVersion.ToString()
} | ConvertTo-Json
