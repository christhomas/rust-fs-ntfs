# fs-ntfs

Pure-Rust NTFS filesystem driver with a stable C ABI (`fs_ntfs_*`)
designed to be linked from Swift, C, C++, Go (via CGo), or any other
language with FFI.

Matches the pattern of
[rust-fs-ext4](https://github.com/christhomas/rust-fs-ext4) — same
shape, different filesystem.

## Status

Read-only NTFS. Built on top of [ColinFinck/ntfs](https://github.com/ColinFinck/ntfs)
(depended on via `ntfs = "0.4"` from crates.io) with a C ABI wrapper so
non-Rust callers can mount, stat, readdir, and read files.

## Building

```sh
cargo build --release
# produces target/release/libfs_ntfs.a + the rlib
```

Universal macOS static lib (if you use `build.sh`):

```sh
./build.sh          # builds both archs + lipos into dist/libfs_ntfs.a
```

## Using from C / Swift

Link `libfs_ntfs.a` and include `fs_ntfs.h`:

```c
#include "fs_ntfs.h"

fs_ntfs_fs_t *fs = fs_ntfs_mount("/path/to/ntfs.img");
if (!fs) {
    fprintf(stderr, "%s\n", fs_ntfs_last_error());
    return 1;
}

fs_ntfs_attr_t attr;
if (fs_ntfs_stat(fs, "/readme.txt", &attr) == 0) {
    printf("size=%llu\n", attr.size);
}

fs_ntfs_umount(fs);
```

## Using from Rust

```toml
[dependencies]
fs-ntfs = "0.1"
```

```rust
// Rust consumers can use the underlying `ntfs` crate directly; fs-ntfs
// is primarily the FFI surface. If you want idiomatic Rust NTFS access,
// depend on `ntfs = "0.4"` from crates.io instead.
```

## Credits

This crate is a C-ABI wrapper around
[ColinFinck/ntfs](https://github.com/ColinFinck/ntfs) — the underlying
pure-Rust NTFS parser. All filesystem parsing logic is his; fs-ntfs only
adds the FFI layer so non-Rust callers can consume it.

## License

Dual-licensed under MIT ([LICENSE-MIT](LICENSE-MIT)) or Apache-2.0
([LICENSE-APACHE](LICENSE-APACHE)), matching the upstream `ntfs` crate.
