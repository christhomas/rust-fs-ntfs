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

/* File/directory attributes */
typedef struct {
    uint64_t file_record_number;
    uint64_t size;
    uint32_t atime;         /* Access time (UNIX epoch) */
    uint32_t mtime;         /* Modification time */
    uint32_t ctime;         /* MFT record change time */
    uint32_t crtime;        /* Creation time */
    uint16_t mode;          /* Synthesized POSIX mode bits */
    uint16_t link_count;
    fs_ntfs_file_type_t file_type;
    uint32_t attributes;    /* NTFS file attributes (hidden, system, etc.) */
} fs_ntfs_attr_t;

/* Directory entry (returned during iteration) */
typedef struct {
    uint64_t file_record_number;
    uint8_t  file_type;     /* fs_ntfs_file_type_t */
    uint16_t name_len;
    char     name[256];     /* UTF-8, null-terminated */
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

/* ---- Block device callback interface ---- */

/*
 * Callback for reading from the device.
 * Must read exactly `length` bytes at `offset` into `buf`.
 * Returns 0 on success, non-zero on error.
 */
typedef int (*fs_ntfs_read_fn)(void *context, void *buf,
                                   uint64_t offset, uint64_t length);

/*
 * Block device parameters for callback-based mounting.
 */
typedef struct {
    fs_ntfs_read_fn read;
    void   *context;
    uint64_t size_bytes;
} fs_ntfs_blockdev_cfg_t;

/* ---- Lifecycle ---- */

/*
 * Mount an NTFS filesystem from the given device/image path.
 * Returns NULL on failure. Read-only.
 */
fs_ntfs_fs_t *fs_ntfs_mount(const char *device_path);

/*
 * Mount an NTFS filesystem using callback-based I/O.
 * Returns NULL on failure. Read-only.
 */
fs_ntfs_fs_t *fs_ntfs_mount_with_callbacks(
    const fs_ntfs_blockdev_cfg_t *cfg);

/*
 * Unmount and free all resources.
 */
void fs_ntfs_umount(fs_ntfs_fs_t *fs);

/* ---- Volume info ---- */

int fs_ntfs_get_volume_info(fs_ntfs_fs_t *fs,
                                fs_ntfs_volume_info_t *info);

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
int fs_ntfs_chattr(const char *image, const char *path,
                   uint32_t add_flags, uint32_t remove_flags);

#ifdef __cplusplus
}
#endif

#endif /* FS_NTFS_H */
