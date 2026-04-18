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

#ifdef __cplusplus
}
#endif

#endif /* FS_NTFS_H */
