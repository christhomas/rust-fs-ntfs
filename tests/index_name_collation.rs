//! One directory index, one idea of what "the same name" means.
//!
//! Entries go into `$I30` ordered by NTFS's `COLLATION_FILE_NAME`, which
//! upcases both sides through the volume's own `$UpCase` table. The
//! lookups that read the index back compared UTF-16 code units for exact
//! equality, so the two halves of the same index disagreed:
//!
//! * the collision check before a create passed for `Foo.txt` in a
//!   directory that already held `foo.txt`, and an entry was inserted
//!   whose collation key equals an existing one — a duplicate key in an
//!   index NTFS requires to have unique ones;
//! * the index-entry lookup that unlink and rename use to detach a name
//!   missed a differently-cased name, even though path resolution
//!   (`read::resolve_path`, which has always been collation-aware) had
//!   just found the file.

use fs_ntfs::block_io::{BlockIo, PathIo};
use fs_ntfs::facade::Filesystem;
use fs_ntfs::mkfs::format_filesystem;
use std::path::Path;

const VOL_SIZE: u64 = 16 * 1024 * 1024;
const CLUSTER: u32 = 4096;

fn fresh_volume(tag: &str) -> String {
    std::fs::create_dir_all("test-disks").expect("create test-disks dir");
    let dst = format!("test-disks/_collate_{tag}.img");
    let f = std::fs::File::create(&dst).expect("create image");
    f.set_len(VOL_SIZE).expect("set_len");
    drop(f);
    let mut io = PathIo::open_rw(Path::new(&dst)).expect("open_rw");
    format_filesystem(&mut io, VOL_SIZE, CLUSTER, CLUSTER, Some("COLL"), Some(4))
        .expect("format_filesystem");
    <PathIo as BlockIo>::sync(&mut io).expect("sync");
    drop(io);
    dst
}

fn names_in(img: &str, dir: &str) -> Vec<String> {
    let fs = Filesystem::mount(img).expect("mount");
    // The metadata files ($MFT, $Bitmap, ...) and the . / .. entries
    // live in the root index too; this is about the names a caller put
    // there.
    let mut names: Vec<String> = fs
        .read_dir(dir)
        .expect("read_dir")
        .into_iter()
        .map(|e| e.name)
        .filter(|n| !n.starts_with('$') && n != "." && n != "..")
        .collect();
    names.sort();
    names
}

#[test]
fn creating_a_differently_cased_name_is_refused() {
    let img = fresh_volume("create");
    let fs = Filesystem::mount_rw(&img).expect("mount_rw");
    fs.create_file("/", "foo.txt").expect("create foo.txt");

    let second = fs.create_file("/", "Foo.txt");
    drop(fs);
    assert!(
        second.is_err(),
        "creating Foo.txt beside foo.txt was allowed; $I30 now holds two entries \
         with the same collation key: {:?}",
        names_in(&img, "/")
    );
    assert_eq!(
        names_in(&img, "/"),
        vec!["foo.txt".to_string()],
        "the directory must still hold exactly the one name"
    );
}

#[test]
fn creating_a_differently_cased_directory_is_refused() {
    let img = fresh_volume("mkdir");
    let fs = Filesystem::mount_rw(&img).expect("mount_rw");
    fs.mkdir("/", "Data").expect("mkdir Data");
    let second = fs.mkdir("/", "DATA");
    drop(fs);
    assert!(
        second.is_err(),
        "mkdir DATA beside Data was allowed: {:?}",
        names_in(&img, "/")
    );
}

#[test]
fn renaming_onto_a_differently_cased_name_is_refused() {
    let img = fresh_volume("rename");
    let fs = Filesystem::mount_rw(&img).expect("mount_rw");
    fs.create_file("/", "alpha.txt").expect("create alpha.txt");
    fs.create_file("/", "beta.txt").expect("create beta.txt");
    let clash = fs.rename("/beta.txt", "ALPHA.TXT");
    drop(fs);
    assert!(
        clash.is_err(),
        "renaming beta.txt to ALPHA.TXT beside alpha.txt was allowed: {:?}",
        names_in(&img, "/")
    );
}

#[test]
fn unlink_finds_the_entry_whatever_case_the_caller_used() {
    // resolve_path has always been collation-aware, so the record is
    // found; it was the index-entry lookup that detaches the name that
    // was not.
    let img = fresh_volume("unlink");
    let fs = Filesystem::mount_rw(&img).expect("mount_rw");
    fs.create_file("/", "readme.txt")
        .expect("create readme.txt");
    let removed = fs.unlink("/README.TXT");
    drop(fs);
    removed.expect("unlink /README.TXT should remove the entry named readme.txt");
    assert!(
        names_in(&img, "/").is_empty(),
        "the entry should be gone: {:?}",
        names_in(&img, "/")
    );
}

#[test]
fn an_ordinary_create_still_works_and_distinct_names_still_coexist() {
    // The comparator got looser; it must not have got so loose that two
    // genuinely different names collide.
    let img = fresh_volume("distinct");
    let fs = Filesystem::mount_rw(&img).expect("mount_rw");
    for name in ["one.txt", "two.txt", "three.txt", "ONE-B.txt"] {
        fs.create_file("/", name)
            .unwrap_or_else(|e| panic!("create {name}: {e:?}"));
    }
    drop(fs);
    assert_eq!(
        names_in(&img, "/"),
        vec![
            "ONE-B.txt".to_string(),
            "one.txt".to_string(),
            "three.txt".to_string(),
            "two.txt".to_string()
        ]
    );
}

#[test]
fn a_case_only_rename_is_allowed() {
    // The one case where the destination legitimately collates equal to
    // the source: the entry the collision check finds is the file's own.
    // Windows allows `ren readme.txt README.TXT`, and both rename paths
    // -- the same-length one and the variable-length one that delegates
    // to it -- have to.
    let img = fresh_volume("case_rename");
    let fs = Filesystem::mount_rw(&img).expect("mount_rw");
    fs.create_file("/", "readme.txt")
        .expect("create readme.txt");
    fs.rename("/readme.txt", "README.TXT")
        .expect("readme.txt -> README.TXT");
    drop(fs);
    assert_eq!(names_in(&img, "/"), vec!["README.TXT".to_string()]);

    let fs = Filesystem::mount_rw(&img).expect("remount");
    fs.rename_same_length("/README.TXT", "ReadMe.TxT")
        .expect("README.TXT -> ReadMe.TxT");
    drop(fs);
    assert_eq!(names_in(&img, "/"), vec!["ReadMe.TxT".to_string()]);

    // And in a subdirectory, which is where the fixture suite does it.
    let fs = Filesystem::mount_rw(&img).expect("remount");
    fs.mkdir("/", "Documents").expect("mkdir Documents");
    fs.create_file("/Documents", "readme.txt")
        .expect("create /Documents/readme.txt");
    fs.rename("/Documents/readme.txt", "README.TXT")
        .expect("/Documents/readme.txt -> README.TXT");
    drop(fs);
    assert_eq!(names_in(&img, "/Documents"), vec!["README.TXT".to_string()]);
}

#[test]
fn a_rename_onto_another_file_is_still_refused_whatever_the_case() {
    // The exemption is for the file's OWN entry and nothing else.
    let img = fresh_volume("case_rename_clash");
    let fs = Filesystem::mount_rw(&img).expect("mount_rw");
    fs.create_file("/", "one.txt").expect("create one.txt");
    fs.create_file("/", "two.txt").expect("create two.txt");
    let clash = fs.rename_same_length("/two.txt", "ONE.txt");
    drop(fs);
    assert!(
        clash.is_err(),
        "two.txt -> ONE.txt must still clash with one.txt: {:?}",
        names_in(&img, "/")
    );
}
