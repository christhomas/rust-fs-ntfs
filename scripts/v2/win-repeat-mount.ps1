# scripts/v2/win-repeat-mount.ps1 -- stress the mount/dismount cycle.
#
# Mounts and dismounts the .vhdx wrapper N times in a row to surface
# any state leak in ntfs.sys's volume-recognition path or the VHD
# miniport. No I/O happens between cycles — only mount + dismount.
# Used by `repeat-mount(N)` scenarios, which sandwich this op between
# two chkdsk passes to verify the volume's NTFS structure survives
# repeated remounts.
#
# Args:
#   -ImagePath   Path on the VM to the .img file.
#   -Cycles      Number of mount-then-dismount cycles to run.
#   -KeepImage   `true` keeps .img + .vhdx for a follow-on op
#                (default `false`). The next op will mount on its own.
#   -Diag        Directory for repeat-mount-result.txt + per-cycle
#                error markers if any cycle fails.
#
# Exit code:
#   0 if all cycles complete
#   1 if Mount-DiskImage or Dismount-DiskImage raises on any cycle

param(
    [Parameter(Mandatory=$true)] [string]$ImagePath,
    [Parameter(Mandatory=$true)] [string]$Cycles,
    [Parameter(Mandatory=$true)] [string]$Diag,
    [string]$KeepImage = 'false'
)

$ErrorActionPreference = 'Stop'

. "$PSScriptRoot\_lib.ps1"

$KeepImageBool = $false
if ($KeepImage -and $KeepImage.Trim() -ne '') {
    if ($KeepImage -match '^(?i:true|1|yes)$') { $KeepImageBool = $true }
}

$cyclesInt = 0
if (-not [int]::TryParse($Cycles, [ref]$cyclesInt) -or $cyclesInt -lt 1) {
    [Console]::Error.WriteLine("win-repeat-mount: -Cycles must be a positive integer; got '$Cycles'")
    exit 2
}

New-Item -ItemType Directory -Path $Diag -Force | Out-Null

$Vhdx = Get-VhdxPathFor -ImagePath $ImagePath

try {
    # Initialize-VhdxFromImg leaves the wrapper dismounted, which is
    # the right starting state for the cycle loop below.
    $null = Initialize-VhdxFromImg -ImagePath $ImagePath -Diag $Diag

    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    for ($i = 1; $i -le $cyclesInt; $i++) {
        try {
            Mount-DiskImage -ImagePath $Vhdx | Out-Null
            Start-Sleep -Seconds 1
            Dismount-DiskImage -ImagePath $Vhdx | Out-Null
            Start-Sleep -Seconds 1
        } catch {
            "cycle $i failed: $($_.Exception.Message)" |
                Out-File "$Diag\repeat-mount-cycle-$('{0:D3}' -f $i)-error.txt" -Encoding UTF8
            throw
        }
    }
    $sw.Stop()

    "completed $cyclesInt mount/dismount cycles in $([math]::Round($sw.Elapsed.TotalSeconds,1))s" |
        Out-File "$Diag\repeat-mount-result.txt" -Encoding UTF8

    exit 0
} finally {
    # Pass through to the shared cleanup (handles dismount of any
    # leftover attached state from a mid-loop failure + file deletion
    # if KeepImage=false).
    Dismount-VhdxAndCleanup -Vhdx $Vhdx -ImagePath $ImagePath -KeepImage $KeepImageBool
}
