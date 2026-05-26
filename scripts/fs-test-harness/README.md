# scripts/fs-test-harness/

PowerShell scripts that run on the Windows test VM. Shipped once during
the bootstrap phase by `run-tests.sh` via the `scripts_dir` key in
`fs-test-harness.toml`. Once shipped, all harness operations and result
collection use the copies at `{vm.workdir}/scripts/fs-test-harness/`.

## Files

### Shared library
- `_lib.ps1` — VHD mount/dismount helpers, drive-letter mutex
  (`Acquire-DriveLock` / `Release-DriveLock`). Dot-sourced by every
  op script.

### Test operation scripts
Invoked per scenario step by the harness runner (via SSH). Each maps
to one `[ops.win-*]` entry in `fs-test-harness.toml`.

- `win-chkdsk.ps1` — wrap `.img` in a VHD, mount, run chkdsk, dismount
- `win-enumerate.ps1` — mount and walk the volume root recursively
- `win-write.ps1` — write a single file (content or zero-fill)
- `win-write-many.ps1` — bulk-write N files in one mount cycle
- `win-delete.ps1` — delete a single file
- `win-delete-many.ps1` — bulk-delete files by index range
- `win-mkdir.ps1` — create a directory (with parents)
- `win-rename.ps1` — rename / move a file
- `win-modify.ps1` — overwrite a byte range inside an existing file
- `win-read.ps1` — read a byte range from a file
- `win-repeat-mount.ps1` — mount/dismount N times, no I/O
- `win-format.ps1` — format a blank `.img` with Windows NTFS

### Result-collection scripts
Called by `scripts/_matrix-collect-vm.sh` after each matrix run.

- `vm-info.ps1` — emit Windows build / ntfs.sys / chkdsk version as JSON
- `verdict-collect.ps1` — aggregate per-scenario `verdict.json` files
  from `{vm.workdir}/diag/v2/` into a single JSON blob
