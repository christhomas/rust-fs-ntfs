/*
 * fs_ntfs.h — High-level C API bridging the ntfs Rust crate to Swift/FSKit.
 *
 * This is the ONLY header that the Swift bridging header needs to import.
 * It provides a clean, Swift-friendly interface that hides NTFS internals.
 *
 * MIT License — see LICENSE
 */

#ifndef FS_NTFS_H
#define FS_NTFS_H

#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>
#include <sys/types.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque handle to a mounted NTFS filesystem */
typedef struct fs_ntfs_fs fs_ntfs_fs_t;

/* File type enumeration */
typedef enum {
    FS_NTFS_FT_UNKNOWN  = 0,
    FS_NTFS_FT_REG_FILE = 1,
    FS_NTFS_FT_DIR      = 2,
    FS_NTFS_FT_SYMLINK  = 7,
    FS_NTFS_FT_JUNCTION = 8, /* NTFS mount point / junction (directory) */
} fs_ntfs_file_type_t;

/* File/directory attributes.
 *
 * Timestamps are split into a signed 64-bit seconds component
 * (UNIX epoch; negative values represent pre-1970 dates) and an
 * unsigned 32-bit nanoseconds component in [0, 1_000_000_000).
 * Layout: all four *_sec fields first (contiguous, 8-byte aligned),
 * then all four *_nsec fields (contiguous, 4-byte aligned) — no
 * padding between them.
 *
 * ABI note: this struct grew from 44 bytes (v0.1.2 layout with
 * uint32_t atime/mtime/ctime/crtime) to 76 bytes.  Recompile any
 * code that passes or stores fs_ntfs_attr_t by value.
 */
typedef struct {
    uint64_t file_record_number;
    uint64_t size;
    int64_t  atime_sec;     /* Access time — seconds since UNIX epoch */
    int64_t  mtime_sec;     /* Modification time */
    int64_t  ctime_sec;     /* MFT record change time */
    int64_t  crtime_sec;    /* Creation time */
    uint32_t atime_nsec;    /* Sub-second nanoseconds for atime */
    uint32_t mtime_nsec;
    uint32_t ctime_nsec;
    uint32_t crtime_nsec;
    uint16_t mode;          /* Synthesized POSIX mode bits */
    uint16_t link_count;
    fs_ntfs_file_type_t file_type;
    uint32_t attributes;    /* NTFS file attributes (hidden, system, etc.) */
} fs_ntfs_attr_t;

/*
 * Max bytes a filename can occupy in `fs_ntfs_dirent_t::name`,
 * including the trailing NUL. NTFS allows up to 255 UTF-16 code
 * units; worst-case UTF-8 is 4 bytes per code unit → 1020 bytes
 * of content + 1 NUL → 1024 (rounded up). Files with names that
 * exceed this surface with `name_len = 1023` and the buffer
 * truncated; callers that need to detect should compare
 * `name_len` against `FS_NTFS_DIRENT_NAME_BYTES - 1`.
 *
 * ABI note: this widened from 256 in v0.1.2; structs sized to the
 * old 256-byte layout will mis-read `name` past the new bounds.
 */
#define FS_NTFS_DIRENT_NAME_BYTES 1024

/* Directory entry (returned during iteration) */
typedef struct {
    uint64_t file_record_number;
    uint8_t  file_type;     /* fs_ntfs_file_type_t */
    uint16_t name_len;
    char     name[FS_NTFS_DIRENT_NAME_BYTES];     /* UTF-8, null-terminated */
} fs_ntfs_dirent_t;

/* Volume information */
typedef struct {
    char     volume_name[128];  /* UTF-8, null-terminated */
    uint32_t cluster_size;
    uint64_t total_clusters;
    uint16_t ntfs_version_major;
    uint16_t ntfs_version_minor;
    uint64_t serial_number;
    uint64_t total_size;
} fs_ntfs_volume_info_t;

/*
 * Extended volume info — v2.
 *
 * Every v1 field above lands at the same offset (compile-time
 * verified by `offset_of!` style tests on the Rust side). Callers
 * MUST allocate a fs_ntfs_volume_info_v2_t (or larger) buffer when
 * calling fs_ntfs_get_volume_info_v2 — passing a v1-sized buffer is
 * unsupported and will overrun memory.
 *
 * v2 adds:
 *   - volume_flags     : raw $VOLUME_INFORMATION.flags (incl. dirty bit)
 *   - is_dirty         : 1 iff (volume_flags & 0x0001), 0 otherwise
 *   - mft_record_size  : bytes per MFT record (typically 1024 or 4096)
 *   - bytes_per_sector : physical sector size (typically 512 or 4096)
 */
typedef struct {
    /* -- v1 fields, identical offsets ----------------------------------- */
    char     volume_name[128];
    uint32_t cluster_size;
    uint64_t total_clusters;
    uint16_t ntfs_version_major;
    uint16_t ntfs_version_minor;
    uint64_t serial_number;
    uint64_t total_size;
    /* -- v2 additions --------------------------------------------------- */
    uint16_t volume_flags;
    uint8_t  is_dirty;
    /* Full 5-byte gap between is_dirty (offset 170) and mft_record_size
     * (offset 176, u32 alignment). Make the whole gap explicit so the
     * layout is stable with no hidden compiler padding. */
    uint8_t  _pad[5];
    uint32_t mft_record_size;
    uint32_t bytes_per_sector;
} fs_ntfs_volume_info_v2_t;

/* ---- Block device callback interface ---- */

/*
 * Callback for reading from the device.
 * Must read exactly `length` bytes at `offset` into `buf`.
 * Returns 0 on success, non-zero on error.
 */
typedef int (*fs_ntfs_read_fn)(void *context, void *buf,
                                   uint64_t offset, uint64_t length);

/*
 * Callback for writing to the device.
 * Must write exactly `length` bytes from `buf` starting at `offset`.
 * Returns 0 on success, non-zero on error.
 *
 * Optional — set to NULL if the consumer mounts read-only. The write
 * callback is required by the recovery entry points
 * (`fs_ntfs_fsck_with_callbacks`) and by the handle-based mutation API
 * (`fs_ntfs_*_h`) when called against a handle produced by
 * `fs_ntfs_mount_with_callbacks`. `fs_ntfs_is_dirty_with_callbacks`
 * ignores this field.
 */
typedef int (*fs_ntfs_write_fn)(void *context, const void *buf,
                                    uint64_t offset, uint64_t length);

/*
 * Block device parameters for callback-based mounting and recovery.
 *
 * NOTE: `write` was added in v0.1.1 at the tail of the struct to keep
 * backward-compatible binary layout with v0.1.0 consumers. Existing
 * read-only callers that memset/zero-init their config are unaffected.
 */
typedef struct {
    fs_ntfs_read_fn  read;
    void            *context;
    uint64_t         size_bytes;
    fs_ntfs_write_fn write;           /* NEW in v0.1.1; NULL if read-only */
} fs_ntfs_blockdev_cfg_t;

/* ---- Lifecycle ---- */

/*
 * Mount an NTFS filesystem from the given device/image path.
 * Returns NULL on failure. Read-only.
 *
 * Dirty-volume note: this driver parses dirty volumes (the
 * VOLUME_IS_DIRTY flag is informational here, not a refusal). Stale
 * data is possible if the volume hasn't been cleanly dismounted —
 * a file mid-rename may surface with the old name pointing at the
 * new file reference, etc.
 *
 * Callers that need to detect this should invoke `fs_ntfs_is_dirty`
 * (or `fs_ntfs_is_dirty_with_callbacks`) AFTER a successful mount
 * and decide policy themselves: refuse to surface the volume,
 * surface read-only, or surface with a warning. The driver does NOT
 * auto-warn or auto-refuse — the quiet-by-default contract FSKit
 * relies on stays intact.
 */
fs_ntfs_fs_t *fs_ntfs_mount(const char *device_path);

/*
 * Mount an NTFS filesystem using callback-based I/O.
 * Returns NULL on failure.
 *
 * Producing a writable handle: pass a non-NULL `cfg->write`. The
 * resulting handle is then accepted by the `_h` mutation entry points
 * (see "Handle-based mutation API" below). Pass NULL to mount
 * read-only — `_h` mutators will then fail with -1 / EINVAL.
 *
 * Dirty-volume note: same contract as `fs_ntfs_mount`. Use
 * `fs_ntfs_is_dirty_with_callbacks` post-mount to decide policy.
 */
fs_ntfs_fs_t *fs_ntfs_mount_with_callbacks(
    const fs_ntfs_blockdev_cfg_t *cfg);

/*
 * Mount via an FsCoreDevice handle from a sister crate
 * (`qcow2_open` from am-img-qcow2, `partitions_open_slice` from
 * am-partitions, `fs_core_file_open` from am-fs-core).
 *
 * Read-only — mutator API calls on the resulting handle (`_h` family)
 * return EINVAL with "handle has no recorded mount source". For RW
 * use `fs_ntfs_mount_rw_with_fs_core_device` below.
 *
 * The handle's reference count is incremented internally; the caller
 * still owns their *FsCoreDevice and frees it via
 * `fs_core_device_close`. Closing the resulting handle via
 * `fs_ntfs_umount` drops the mount's own reference.
 *
 * Forward declared FsCoreDevice — full definition in `fs_core.h`.
 *
 * Returns NULL on failure; use fs_ntfs_last_error() for detail.
 */
struct FsCoreDevice;
fs_ntfs_fs_t *fs_ntfs_mount_with_fs_core_device(struct FsCoreDevice *handle);

/*
 * Mount via an FsCoreDevice handle, RW. Same shape as
 * `fs_ntfs_mount_with_fs_core_device` but the underlying device is
 * recorded as the handle's mount source so the `_h` mutator family
 * (`fs_ntfs_create_file_h`, `fs_ntfs_mkdir_h`,
 * `fs_ntfs_write_file_contents_h`, `fs_ntfs_unlink_h`, …) can write
 * through it.
 *
 * The supplied device should report `is_writable=true` (see
 * `fs_core_device_is_writable`); a non-writable device still mounts
 * (read paths work) but the first mutator call returns EINVAL with a
 * descriptive error string. The mount itself does not pre-flight
 * writability so callers can mount hybrid devices that gate
 * writability per-region.
 *
 * Reference-counting and ownership rules are identical to
 * `fs_ntfs_mount_with_fs_core_device`.
 *
 * Returns NULL on failure; use fs_ntfs_last_error() for detail.
 */
fs_ntfs_fs_t *fs_ntfs_mount_rw_with_fs_core_device(struct FsCoreDevice *handle);

/*
 * Unmount and free all resources.
 */
void fs_ntfs_umount(fs_ntfs_fs_t *fs);

/* ---- Volume info ---- */

int fs_ntfs_get_volume_info(fs_ntfs_fs_t *fs,
                                fs_ntfs_volume_info_t *info);

/*
 * Extended volume info — v2. Populates fs_ntfs_volume_info_v2_t with
 * everything v1 reports plus volume_flags / is_dirty / mft_record_size /
 * bytes_per_sector. Returns 0 on success, -1 on error. New callers
 * should prefer this; legacy callers can stay on v1.
 */
int fs_ntfs_get_volume_info_v2(fs_ntfs_fs_t *fs,
                                   fs_ntfs_volume_info_v2_t *info);

/*
 * Set the volume label on an unmounted NTFS image. Pass NULL or an
 * empty string to remove the $VOLUME_NAME attribute entirely. The
 * label is UTF-8; this function encodes to UTF-16 LE on disk. NTFS
 * conventionally caps labels at 32 UTF-16 code units; longer labels
 * are rejected with -1.
 *
 * IMPORTANT: the image must NOT be mounted by Windows / ntfs.sys
 * concurrently — same constraint as fs_ntfs_clear_dirty / fsck. Use
 * the mounted-handle API for live volumes (TODO; not yet provided).
 *
 * Returns 0 on success, -1 on error (call fs_ntfs_last_error for
 * details).
 */
int fs_ntfs_set_volume_label(const char *image, const char *label);

/*
 * Read the volume label from an unmounted NTFS image into out_buf
 * (UTF-8 bytes, NO trailing NUL written — caller may add their own).
 * Returns the number of bytes written, or -1 on error.
 *
 * Returns 0 when the volume has no label (the $VOLUME_NAME attribute
 * is absent or zero-length). If the on-disk label is longer than
 * out_buf_len, the result is silently truncated and the truncated
 * length is returned; no error. Allocate at least 128 bytes for the
 * full label (32 UTF-16 code units × up to 4 UTF-8 bytes each).
 */
int fs_ntfs_read_volume_label(const char *image, char *out_buf, size_t out_buf_len);

/* ---- File attributes ---- */

/*
 * Get attributes for a path (relative to mount root).
 * Path uses forward slashes: "/Windows/System32/notepad.exe"
 * Returns 0 on success.
 */
int fs_ntfs_stat(fs_ntfs_fs_t *fs, const char *path,
                     fs_ntfs_attr_t *attr);

/* ---- Directory listing ---- */

typedef struct fs_ntfs_dir_iter fs_ntfs_dir_iter_t;

fs_ntfs_dir_iter_t *fs_ntfs_dir_open(fs_ntfs_fs_t *fs,
                                              const char *path);

const fs_ntfs_dirent_t *fs_ntfs_dir_next(fs_ntfs_dir_iter_t *iter);

/*
 * How many index entries were silently skipped while opening this
 * iterator (e.g. malformed rows on a dirty volume). Returns -1 on a
 * NULL iterator, otherwise a count. A non-zero value means the
 * listing the caller is iterating is incomplete; common causes are
 * dirty-volume metadata damage or upstream parser failures on rare
 * NTFS shapes.
 */
int64_t fs_ntfs_dir_skipped(const fs_ntfs_dir_iter_t *iter);

void fs_ntfs_dir_close(fs_ntfs_dir_iter_t *iter);

/* ---- File reading ---- */

int64_t fs_ntfs_read_file(fs_ntfs_fs_t *fs, const char *path,
                              void *buf, uint64_t offset, uint64_t length);

/* ---- Symlink / Reparse points ---- */

int fs_ntfs_readlink(fs_ntfs_fs_t *fs, const char *path,
                         char *buf, size_t bufsize);

/* ---- Error reporting ---- */

const char *fs_ntfs_last_error(void);

/*
 * Companion to fs_ntfs_last_error. Returns a POSIX-style errno code
 * (ENOENT, EEXIST, ENOSPC, EINVAL, ENOTDIR, EISDIR, ENOTEMPTY, EPERM,
 * or EIO as a fallback). `0` means no error recorded on this thread.
 * Inferred heuristically from the error message content.
 */
int fs_ntfs_last_errno(void);

/*
 * Reset the thread-local error state.
 */
void fs_ntfs_clear_last_error(void);

/* ---- Recovery / fsck ---- */

/*
 * Check whether the volume's VOLUME_IS_DIRTY flag is set. Lightweight
 * probe that doesn't fully mount the volume. Returns:
 *   1  — dirty
 *   0  — clean
 *  -1  — error
 */
int fs_ntfs_is_dirty(const char *path);

/*
 * Count free clusters in the volume bitmap. Scans the whole $Bitmap.
 * Returns the count on success, -1 on error.
 */
int64_t fs_ntfs_free_clusters(const char *path);

/*
 * Count free MFT records in $MFT:$Bitmap. Returns the count on
 * success, -1 on error.
 */
int64_t fs_ntfs_mft_free_records(const char *path);

/*
 * Add a hard link `new_parent_path/new_basename` to an existing
 * regular file `existing_path`. Refuses directories. Returns 0 on
 * success, -1 on error.
 */
int fs_ntfs_link(const char *image,
                 const char *existing_path,
                 const char *new_parent_path,
                 const char *new_basename);

/*
 * Copy a file's 16-byte $OBJECT_ID (GUID) into out_buf. Returns:
 *    1  — file has an object ID, 16 bytes written to out_buf
 *    0  — file has no $OBJECT_ID attribute, out_buf untouched
 *   -1  — error
 * out_buf must be at least 16 bytes.
 */
int fs_ntfs_read_object_id(const char *image,
                           const char *path,
                           uint8_t *out_buf);

/*
 * Set a file's 16-byte $OBJECT_ID (GUID) from in_buf. Adds the
 * attribute if absent, replaces in place if present. The GUID is
 * stored verbatim — no byte-order reinterpretation. Returns 0 on
 * success, -1 on error. in_buf must point to at least 16 bytes.
 *
 * Extended fields (birth volume / object / domain IDs, MS-FSCC §2.4.6)
 * are NOT written by this entry point; only the mandatory 16-byte
 * prefix. Modern Windows volumes accept the short form for
 * FSCTL_GET_OBJECT_ID round-trips.
 */
int fs_ntfs_write_object_id(const char *image,
                            const char *path,
                            const uint8_t *in_buf);

/*
 * Write a full 64-byte $OBJECT_ID carrying the mandatory object_id
 * plus the three DLT (Distributed Link Tracking) Birth GUIDs per
 * MS-FSCC §2.4.6: birth_volume_id, birth_object_id, birth_domain_id.
 * All four pointers must point to at least 16 readable bytes. Adds
 * the attribute if absent, replaces in place if present. Returns 0
 * on success, -1 on error.
 */
int fs_ntfs_write_object_id_extended(const char *image,
                                     const char *path,
                                     const uint8_t *in_buf,
                                     const uint8_t *birth_volume,
                                     const uint8_t *birth_object,
                                     const uint8_t *birth_domain);

/*
 * Read the full $OBJECT_ID into out_buf, decoding Birth GUIDs when
 * present. Caller passes out_buf_len (must be >= 16; pass 64 to also
 * receive Birth GUIDs).
 *
 * Returns:
 *   16  — short form only (object_id); no Birth GUIDs on disk
 *   64  — extended form (object_id + 3x birth_*); out_buf_len was >= 64
 *    0  — file has no $OBJECT_ID attribute
 *   -1  — error
 *
 * If on-disk is extended (64 bytes) but out_buf_len < 64, only the
 * first 16 bytes are written and 16 is returned (Birth GUIDs are
 * silently dropped).
 */
int fs_ntfs_read_object_id_extended(const char *image,
                                    const char *path,
                                    uint8_t *out_buf,
                                    size_t out_buf_len);

/*
 * Remove a file's $OBJECT_ID attribute. Idempotent — returns 0
 * whether or not the attribute existed. Returns -1 on error.
 */
int fs_ntfs_remove_object_id(const char *image,
                             const char *path);

/*
 * Clear the VOLUME_IS_DIRTY flag on an NTFS image so Windows / FSKit /
 * other NTFS drivers will remount it. Must NOT be called on a volume
 * that is currently mounted.
 *
 * Returns:
 *   1  — flag was set and has been cleared
 *   0  — volume was already clean, no write performed
 *  -1  — error (call fs_ntfs_last_error for details)
 */
int fs_ntfs_clear_dirty(const char *path);

/*
 * Overwrite $LogFile with the NTFS "empty log" pattern (all 0xFF bytes).
 * Causes the NTFS driver on next mount to treat the log as having no
 * pending transactions and reinitialize it. In-progress transactions
 * are discarded — any uncommitted metadata changes are lost.
 *
 * Returns the number of bytes overwritten on success, -1 on error.
 */
int64_t fs_ntfs_reset_logfile(const char *path);

/*
 * Combined recovery: reset $LogFile and clear the dirty flag, in that
 * order. If either out-param is non-NULL it is filled on success with
 * the corresponding sub-result. Returns 0 on success, -1 on error.
 */
int fs_ntfs_fsck(const char *path,
                 uint64_t *out_logfile_bytes,
                 uint8_t *out_dirty_cleared);

/* ---- Filesystem creation ---- */

/*
 * Format a fresh NTFS filesystem on the device backed by the callbacks
 * in `cfg`. Both `cfg->read` and `cfg->write` MUST be non-NULL.
 *
 * Writes a v3.1 layout: boot sector + backup, $MFT, $MFTMirr, $LogFile
 * (filled with 0xFF — Windows / chkdsk treat this as "reinit on
 * mount"), $Bitmap, $UpCase (generated at runtime via Rust stdlib
 * uppercase mappings), $AttrDef, $Volume (no label by default),
 * $BadClus, $Secure (default-everyone-allow stub), $Boot, $Extend,
 * and an empty root directory. Default cluster size 4 KiB, MFT
 * record size 4 KiB. Random volume serial.
 *
 * Returns 0 on success, -1 on error (call fs_ntfs_last_error for
 * details).
 */
int fs_ntfs_mkfs(const fs_ntfs_blockdev_cfg_t *cfg);

/* ---- Recovery / fsck via callback-based I/O ---- */

/*
 * Check whether the volume's VOLUME_IS_DIRTY flag is set, using the
 * read callback in `cfg`. `cfg->write` is ignored.
 *
 * Returns:
 *   1  — dirty
 *   0  — clean
 *  -1  — error (call fs_ntfs_last_error for details)
 */
int fs_ntfs_is_dirty_with_callbacks(const fs_ntfs_blockdev_cfg_t *cfg);

/*
 * Progress callback for fs_ntfs_fsck_with_callbacks. Fires zero or more
 * times per phase:
 *   phase    — short identifier, e.g. "reset_logfile" or "clear_dirty".
 *   done     — bytes/units completed in this phase.
 *   total    — total bytes/units for this phase.
 *   context  — the `progress_ctx` passed to fs_ntfs_fsck_with_callbacks.
 *
 * Return value is currently ignored (reserved for future cancellation
 * semantics — consumers should return 0).
 */
typedef int (*fs_ntfs_fsck_progress_fn)(void *context, const char *phase,
                                        uint64_t done, uint64_t total);

/*
 * Combined recovery via callbacks: reset $LogFile + clear the dirty bit.
 *
 * `cfg->read` and `cfg->write` must both be set (fsck needs to
 * overwrite `$LogFile` and patch `$Volume`'s flags). `progress_cb` may
 * be NULL; if non-NULL, it is invoked during the long `reset_logfile`
 * phase (one tick per 64 KiB chunk plus start + end) and once around
 * the 2-byte `clear_dirty` write.
 *
 * `out_logfile_bytes` / `out_dirty_cleared` mirror the path-based
 * fs_ntfs_fsck: if non-NULL they are filled on success with the number
 * of bytes overwritten in $LogFile and `1` if the dirty bit was set
 * and cleared (`0` if the volume was already clean).
 *
 * Returns 0 on success, -1 on error.
 */
int fs_ntfs_fsck_with_callbacks(
    const fs_ntfs_blockdev_cfg_t *cfg,
    fs_ntfs_fsck_progress_fn progress_cb,
    void *progress_ctx,
    uint64_t *out_logfile_bytes,
    uint8_t  *out_dirty_cleared);

/*
 * fs_core counterparts of `fs_ntfs_is_dirty_with_callbacks` and
 * `fs_ntfs_fsck_with_callbacks`. Use these when the underlying device
 * is reached through an FsCoreDevice handle from a sister crate
 * (`qcow2_open_rw_on_device`, `partitions_open_slice`,
 * `fs_core_file_open`, ...) — there's no need to plumb individual
 * callbacks when the device is already an FsCoreDevice.
 *
 * The handle is borrowed (its inner Arc is cloned for the duration
 * of the call). The caller still owns the handle and frees it via
 * `fs_core_device_close`.
 *
 * Semantics + return values match the `_with_callbacks` siblings:
 *   is_dirty: 1 = dirty, 0 = clean, -1 = error.
 *   fsck:     0 = success, -1 = error. out_logfile_bytes /
 *             out_dirty_cleared (if non-NULL) filled on success.
 *
 * fsck requires the device to report `is_writable() == true`;
 * otherwise it fails up front with -1 and a descriptive error.
 */
int fs_ntfs_is_dirty_with_fs_core_device(struct FsCoreDevice *handle);
int fs_ntfs_fsck_with_fs_core_device(
    struct FsCoreDevice *handle,
    fs_ntfs_fsck_progress_fn progress_cb,
    void *progress_ctx,
    uint64_t *out_logfile_bytes,
    uint8_t  *out_dirty_cleared);

/* ---- In-place writes (phase W1) ---- */

/*
 * Set any combination of the four NTFS FILETIMEs (100 ns since
 * 1601-01-01 UTC) on `path` within the NTFS image at `image`. Pass
 * NULL for any pointer to leave that field unchanged. Returns 0 on
 * success, -1 on error.
 *
 * NOTE: this writes to the $STANDARD_INFORMATION copy of the times
 * only. Windows itself doesn't update the duplicate times in the
 * parent-directory $FILE_NAME index on most operations — same
 * semantics here.
 */
int fs_ntfs_set_times(const char *image, const char *path,
                      const int64_t *creation,
                      const int64_t *modification,
                      const int64_t *mft_record_modification,
                      const int64_t *access);

/*
 * Create an empty regular file inside `parent_path` named `basename`
 * (no slashes). Returns the new MFT record number on success, -1 on
 * error. In the W3 MVP, parents with $INDEX_ALLOCATION overflow are
 * rejected — the parent must hold its index entirely in
 * $INDEX_ROOT (typically true for small subdirectories).
 */
int64_t fs_ntfs_create_file(const char *image,
                            const char *parent_path,
                            const char *basename);

/*
 * Upsert a single NTFS Extended Attribute on `path`. `ea_name` is
 * NUL-terminated ASCII. `value` + `value_len` is the raw value bytes.
 * `flags` may have bit 0x80 (FILE_NEED_EA) set. Returns 0 on success,
 * -1 on error.
 */
int fs_ntfs_write_ea(const char *image, const char *path,
                     const char *ea_name,
                     const void *value, uint64_t value_len,
                     uint8_t flags);

/*
 * Remove a single Extended Attribute by name. Returns 0 on success,
 * -1 on error (e.g. not found).
 */
int fs_ntfs_remove_ea(const char *image, const char *path,
                      const char *ea_name);

/*
 * Enumerate the names (keys) of every Extended Attribute on `path`.
 * Writes names as a sequence of NUL-terminated byte strings packed
 * into out_buf (in on-disk order). EA names cannot contain NUL by
 * the EA wire format, so the NUL terminator is unambiguous. Always
 * writes the required total byte count to *out_total_len so callers
 * can size-query (pass out_buf=NULL, out_buf_len=0).
 *
 * Returns:
 *   N >= 0 — number of EAs (also = count of NUL terminators)
 *  -2      — at least one EA exists but out_buf_len was too small;
 *            *out_total_len holds the required size, names not copied
 *  -1      — error (see fs_ntfs_last_error)
 *
 * out_total_len must be non-NULL. out_buf may be NULL only when
 * out_buf_len == 0.
 */
int fs_ntfs_list_ea_keys(const char *image, const char *path,
                         uint8_t *out_buf, size_t out_buf_len,
                         size_t *out_total_len);

/*
 * Write a resident $REPARSE_POINT attribute with the given tag and
 * tag-specific data. Sets FILE_ATTRIBUTE_REPARSE_POINT on the file.
 * Returns 0 on success, -1 on error.
 */
int fs_ntfs_write_reparse_point(const char *image, const char *path,
                                uint32_t reparse_tag,
                                const void *buf, uint64_t len);

/*
 * Remove a file's $REPARSE_POINT attribute + clear the reparse flag.
 */
int fs_ntfs_remove_reparse_point(const char *image, const char *path);

/*
 * Read a file's $REPARSE_POINT attribute as raw (tag, data). Unlike
 * fs_ntfs_readlink (symlink/mount-point only), this exposes the raw
 * payload for any reparse type.
 *
 * On success: writes the 32-bit reparse tag to *out_tag, writes the
 * actual data length (excluding the 8-byte REPARSE_DATA_BUFFER header)
 * to *out_data_len, and — if out_data_len <= out_buf_len — copies the
 * data bytes into out_buf[0..out_data_len]. *out_data_len is always
 * written so callers can size-query (pass out_buf=NULL, out_buf_len=0).
 *
 * Returns:
 *    1 — attribute present, data fully copied
 *    2 — attribute present but out_buf_len too small (truncated)
 *    0 — no $REPARSE_POINT attribute on this file
 *   -1 — error (see fs_ntfs_last_error)
 *
 * out_tag and out_data_len must be non-NULL. out_buf may be NULL only
 * when out_buf_len == 0.
 */
int fs_ntfs_read_reparse_point(const char *image, const char *path,
                               uint32_t *out_tag,
                               void *out_buf, size_t out_buf_len,
                               size_t *out_data_len);

/*
 * Create a symlink at `parent_path/basename` pointing at `target`.
 * `relative` non-zero for a relative target. Returns the new MFT
 * record number on success, -1 on error.
 */
int64_t fs_ntfs_create_symlink(const char *image,
                               const char *parent_path,
                               const char *basename,
                               const char *target,
                               int relative);

/*
 * Create or replace a resident named $DATA stream (Alternate Data
 * Stream). Stream content must fit in the file's free MFT record
 * space (non-resident named streams are future work). Returns 0 on
 * success, -1 on error.
 */
int fs_ntfs_write_named_stream(const char *image, const char *path,
                               const char *stream_name,
                               const void *buf, uint64_t len);

/*
 * Delete a named $DATA stream. Returns 0 on success, -1 on error.
 */
int fs_ntfs_delete_named_stream(const char *image, const char *path,
                                const char *stream_name);

/*
 * Enumerate the names of every named $DATA stream (Alternate Data
 * Stream) on `path`, excluding the unnamed primary $DATA. Writes
 * names as a sequence of NUL-terminated UTF-8 strings packed into
 * out_buf (in on-disk MFT record order — sort on the caller side if
 * a canonical ordering is required). Always writes the required
 * total byte count to *out_total_len so callers can size-query (pass
 * out_buf=NULL, out_buf_len=0).
 *
 * Returns:
 *   N >= 0 — number of named streams (also = count of NUL terminators)
 *  -2      — at least one stream exists but out_buf_len was too small;
 *            *out_total_len holds the required size, names not copied
 *  -1      — error (see fs_ntfs_last_error)
 *
 * out_total_len must be non-NULL. out_buf may be NULL only when
 * out_buf_len == 0.
 */
int fs_ntfs_list_named_streams(const char *image, const char *path,
                               char *out_buf, size_t out_buf_len,
                               size_t *out_total_len);

/*
 * Write `len` bytes from `buf` as the entire contents of the file at
 * `path`. Transparently dispatches: stays resident if it fits in the
 * MFT record, otherwise allocates clusters and promotes $DATA to
 * non-resident. Returns bytes written, -1 on error.
 */
int64_t fs_ntfs_write_file_contents(const char *image,
                                    const char *path,
                                    const void *buf,
                                    uint64_t len);

/*
 * Delete an empty directory. Fails if the directory has any entries
 * or has overflowed to $INDEX_ALLOCATION (MVP limitation). Returns
 * 0 on success, -1 on error.
 */
int fs_ntfs_rmdir(const char *image, const char *path);

/*
 * Create a new empty directory `basename` inside `parent_path`.
 * Returns the new MFT record number on success, -1 on error.
 * Same MVP limitations as fs_ntfs_create_file re: parent index.
 */
int64_t fs_ntfs_mkdir(const char *image,
                      const char *parent_path,
                      const char *basename);

/*
 * Replace a file's resident $DATA contents with `len` bytes from
 * `buf`. Works only while the file's data can remain resident
 * (fits in free MFT record space). Larger writes require W2.2
 * promotion to non-resident — not yet implemented.
 *
 * Returns bytes written, -1 on error.
 */
int64_t fs_ntfs_write_resident_contents(const char *image,
                                        const char *path,
                                        const void *buf,
                                        uint64_t len);

/*
 * Delete a regular file. Removes the parent dir's index entry, frees
 * the data clusters in $Bitmap, clears IN_USE on the MFT record, and
 * frees the MFT record bit in $MFT:$Bitmap.
 *
 * Refuses directories. Returns 0 on success, -1 on error.
 */
int fs_ntfs_unlink(const char *image, const char *path);

/*
 * Rename a file. `new_basename` may differ in length from the
 * current name. Delegates to the fast same-length path when the
 * lengths match; otherwise remove + re-insert in the parent's
 * $INDEX_ROOT (parent must not have $INDEX_ALLOCATION overflow —
 * MVP limitation). Returns 0 on success, -1 on error.
 */
int fs_ntfs_rename(const char *image,
                   const char *old_path,
                   const char *new_basename);

/*
 * Rename a file in place. `new_name` is a basename (no slashes) with
 * the SAME UTF-16 length as the current name. Patches both the parent
 * directory's index entry and each $FILE_NAME attribute on the file's
 * MFT record. Traverses $INDEX_ALLOCATION if the index has overflowed
 * out of resident $INDEX_ROOT.
 *
 * Returns 0 on success, -1 on error.
 */
int fs_ntfs_rename_same_length(const char *image,
                               const char *old_path,
                               const char *new_name);

/*
 * Grow a non-resident $DATA to `new_size` bytes. Allocates contiguous
 * free clusters and appends them to the file's run list. Bytes in
 * the newly-allocated range read as zero per NTFS semantics.
 *
 * Returns the new size on success, -1 on error.
 */
int64_t fs_ntfs_grow(const char *image, const char *path,
                     uint64_t new_size);

/*
 * Shrink a file's non-resident $DATA to `new_size` bytes. Frees the
 * clusters past the new end in $Bitmap. Growing is not supported in
 * W2 MVP — calls with new_size > current_size return -1.
 *
 * Returns the new size on success, -1 on error.
 */
int64_t fs_ntfs_truncate(const char *image, const char *path,
                         uint64_t new_size);

/*
 * Rewrite `len` bytes of `path`'s non-resident $DATA attribute starting
 * at `offset`. Size-preserving only (W1 scope); fails with -1 if the
 * write would extend the file, hit a resident attribute, or cross a
 * sparse / compressed range. Returns bytes written on success.
 */
int64_t fs_ntfs_write_file(const char *image, const char *path,
                           uint64_t offset, const void *buf, uint64_t len);

/*
 * Modify the file_attributes field in $STANDARD_INFORMATION by adding
 * the bits in `add_flags` and removing the bits in `remove_flags`.
 * Bit values match Windows FILE_ATTRIBUTE_* (MS-FSCC 2.6) —
 * READONLY=0x01, HIDDEN=0x02, SYSTEM=0x04, ARCHIVE=0x20, etc.
 *
 * Overlap between add and remove is rejected. Returns 0 on success,
 * -1 on error.
 */
int fs_ntfs_set_file_attributes(const char *image, const char *path,
                   uint32_t add_flags, uint32_t remove_flags);

/*
 * Read the file's $STANDARD_INFORMATION.security_id (the index into
 * $Secure:$SDS / $Secure:$SII). Writes the 32-bit value to *out.
 * Returns:
 *    1  — security_id read into *out
 *    0  — file's $STANDARD_INFORMATION is the 48-byte v1.x form (no
 *         security_id field). *out is set to 0.
 *   -1  — error
 */
int fs_ntfs_read_security_id(const char *image, const char *path,
                             uint32_t *out);

/*
 * Full $STANDARD_INFORMATION value (MS-FSCC §2.4.2). The four
 * timestamps are NT 100-nanosecond intervals since 1601-01-01 UTC.
 * file_attributes is the FILE_ATTRIBUTE_* bitmask. The trailing
 * owner_id / security_id / quota / usn fields only have meaning
 * when has_v3 != 0 (72-byte 3.x form); the 48-byte 1.x form
 * leaves them zeroed.
 */
typedef struct {
    uint64_t creation_time;
    uint64_t modification_time;
    uint64_t mft_modification_time;
    uint64_t access_time;
    uint32_t file_attributes;
    uint32_t maximum_versions;
    uint32_t version_number;
    uint32_t class_id;
    uint32_t owner_id;
    uint32_t security_id;
    uint64_t quota;
    uint64_t usn;
    uint8_t  has_v3;
    uint8_t  _pad[7];
} fs_ntfs_standard_info_t;

/*
 * Read every field of a file's $STANDARD_INFORMATION. Unlike the
 * targeted fs_ntfs_read_security_id, this exposes the full common
 * header plus the optional NTFS 3.x trailer (when present).
 *
 * Returns 0 on success, -1 on error. out must be non-NULL and
 * point at a writable fs_ntfs_standard_info_t.
 */
int fs_ntfs_read_si_full(const char *image, const char *path,
                         fs_ntfs_standard_info_t *out);

/*
 * Point a file at an existing $Secure:$SDS entry by writing the
 * security_id field in its $STANDARD_INFORMATION. mkfs ships the
 * canonical system-files DACL at id 0x100; pointing a runtime-created
 * file there grants the same ACL. Adding new SD entries is a separate
 * (larger) piece of work — this writer only retargets.
 *
 * Requires the file's $STANDARD_INFORMATION to be in the 72-byte
 * NTFS 3.x form. System files written by mkfs use the 48-byte v1.x
 * form and cannot be retargeted via this API. Returns 0 on success,
 * -1 on error.
 */
int fs_ntfs_set_security_id(const char *image, const char *path,
                            uint32_t security_id);

/* ---- Handle-based mutation API (`_h` siblings) ---- */

/*
 * Handle-based mutation API for callback-mounted RW handles.
 *
 * Each function below mirrors a path-based mutator above but takes an
 * already-mounted `fs_ntfs_fs_t *` instead of a `const char *image`.
 * The path-based variants call `mount_rw(image)` internally and so
 * cannot be used from sandboxed FSKit hosts (which can only see the
 * device through the `FSBlockDeviceResource` callback bridge).
 *
 * For callback-mounted handles the volume must have been mounted via
 * `fs_ntfs_mount_with_callbacks` with a non-NULL `cfg.write` —
 * otherwise these calls fail with -1 (EINVAL-flavored error text). For
 * path-mounted handles the underlying file is re-opened RW per call;
 * the kernel page cache amortizes the open cost.
 *
 * Same return-code conventions as their path-based siblings.
 */

int64_t fs_ntfs_create_file_h(fs_ntfs_fs_t *fs,
                              const char *parent_path,
                              const char *basename);

int64_t fs_ntfs_write_file_contents_h(fs_ntfs_fs_t *fs,
                                      const char *path,
                                      const void *buf,
                                      uint64_t len);

int fs_ntfs_unlink_h(fs_ntfs_fs_t *fs, const char *path);

int fs_ntfs_rename_h(fs_ntfs_fs_t *fs,
                     const char *old_path,
                     const char *new_basename);

int64_t fs_ntfs_mkdir_h(fs_ntfs_fs_t *fs,
                        const char *parent_path,
                        const char *basename);

int fs_ntfs_rmdir_h(fs_ntfs_fs_t *fs, const char *path);

int64_t fs_ntfs_truncate_h(fs_ntfs_fs_t *fs,
                           const char *path,
                           uint64_t new_size);

int fs_ntfs_set_times_h(fs_ntfs_fs_t *fs, const char *path,
                        const int64_t *creation,
                        const int64_t *modification,
                        const int64_t *mft_record_modification,
                        const int64_t *access);

/*
 * Handle-based sibling of fs_ntfs_write_object_id_extended. Writes
 * the 64-byte extended $OBJECT_ID carrying the mandatory object_id
 * (16 bytes from in_buf) plus the three optional DLT Birth GUIDs
 * (16 bytes each from birth_volume, birth_object, birth_domain).
 *
 * All four GUID pointers must be non-NULL and reference at least
 * 16 readable bytes. Adds the attribute if absent, replaces in
 * place if present. Returns 0 on success, -1 on error.
 */
int fs_ntfs_set_object_id_extended_h(fs_ntfs_fs_t *fs, const char *path,
                                     const uint8_t *in_buf,
                                     const uint8_t *birth_volume,
                                     const uint8_t *birth_object,
                                     const uint8_t *birth_domain);

#ifdef __cplusplus
}
#endif

#endif /* FS_NTFS_H */
