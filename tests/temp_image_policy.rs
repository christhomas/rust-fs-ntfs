//! Generated integration-test images must be isolated and panic-safe.

mod common;

use std::fs;
use std::path::Path;
use std::sync::{mpsc, Arc, Barrier};

fn rust_sources_below(directory: &Path, paths: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(directory).expect("read test source directory") {
        let path = entry.expect("read test source entry").path();
        if path.is_dir() {
            rust_sources_below(&path, paths);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            paths.push(path);
        }
    }
}

fn bypasses_temp_image_primitive(source: &str) -> bool {
    // Remove line comments and whitespace so a continued or multiline string
    // cannot hide the fixed-path convention from the scan.
    let code: String = source
        .lines()
        .map(|line| line.split_once("//").map_or(line, |(code, _)| code))
        .collect();
    let compact: String = code
        .chars()
        .filter(|character| !character.is_ascii_whitespace() && *character != '\\')
        .collect();
    let compact = compact.replace("test-disks/_does_not_exist.img", "");

    if compact.contains("test-disks/_") {
        return true;
    }

    // Also catch a generated filename joined to a dynamically supplied
    // directory, for example `format!("{TEST_DIR}/_scratch.img")`.
    code.split('"').skip(1).step_by(2).any(|literal| {
        let literal: String = literal
            .chars()
            .filter(|character| !character.is_ascii_whitespace() && *character != '\\')
            .collect();
        let filename = literal.rsplit('/').next().unwrap_or(&literal);
        filename.starts_with('_') && filename.ends_with(".img") && filename != "_does_not_exist.img"
    })
}

#[test]
fn generated_images_use_the_shared_temp_image_primitive() {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut violations = Vec::new();

    let mut sources = Vec::new();
    rust_sources_below(&tests, &mut sources);
    for path in sources {
        if path == tests.join("temp_image_policy.rs") || path == tests.join("common/mod.rs") {
            continue;
        }
        let source = fs::read_to_string(&path).expect("read integration test source");
        if bypasses_temp_image_primitive(&source) {
            violations.push(path.display().to_string());
        }
    }

    assert!(
        violations.is_empty(),
        "generated images bypass the shared temporary-image primitive:\n{}",
        violations.join("\n")
    );
}

#[test]
fn policy_scan_covers_nested_multiline_and_dynamic_paths() {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut sources = Vec::new();
    rust_sources_below(&tests, &mut sources);
    assert!(
        sources.contains(&tests.join("common/mod.rs")),
        "recursive scan did not descend into tests/common"
    );

    assert!(bypasses_temp_image_primitive(
        r#"let path = format!("test-disks/\
             _multiline.img");"#
    ));
    assert!(bypasses_temp_image_primitive(
        r#"let path = format!("{TEST_DIR}/_dynamic.img");"#
    ));
    assert!(bypasses_temp_image_primitive(
        r#"let missing = "test-disks/_does_not_exist.img";
           let generated = "test-disks/_fixed.img";"#
    ));
    assert!(!bypasses_temp_image_primitive(
        r#"let path = common::temp_image_path("safe");"#
    ));
}

#[test]
fn generated_image_names_are_unique_and_worker_exit_cleans_them() {
    let barrier = Arc::new(Barrier::new(3));
    let (send, receive) = mpsc::channel();
    let mut workers = Vec::new();

    for _ in 0..2 {
        let barrier = Arc::clone(&barrier);
        let send = send.clone();
        workers.push(std::thread::spawn(move || {
            let path = common::temp_image_path("policy_probe");
            fs::write(&path, b"probe").expect("create temporary image probe");
            send.send(path).expect("report temporary image path");
            barrier.wait();
        }));
    }
    drop(send);

    let paths: Vec<String> = (0..2)
        .map(|_| receive.recv().expect("receive temporary image path"))
        .collect();
    assert_eq!(paths.len(), 2);
    assert_ne!(paths[0], paths[1], "concurrent workers reused one path");
    let pid_marker = format!("_{}_", std::process::id());
    for path in &paths {
        assert!(path.contains(&pid_marker), "path omits process id: {path}");
        assert!(Path::new(path).is_file(), "probe was not created: {path}");
    }

    barrier.wait();
    for worker in workers {
        worker.join().expect("temporary-image worker panicked");
    }
    for path in paths {
        assert!(
            !Path::new(&path).exists(),
            "worker exit did not clean temporary image: {path}"
        );
    }
}

#[test]
fn panicking_worker_still_cleans_its_generated_image() {
    let (send, receive) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let path = common::temp_image_path("panic_probe");
        fs::write(&path, b"probe").expect("create panic-cleanup probe");
        send.send(path).expect("report panic-cleanup path");
        panic!("intentional panic exercises thread-local cleanup");
    });

    let path = receive.recv().expect("receive panic-cleanup path");
    assert!(worker.join().is_err(), "worker was expected to panic");
    assert!(
        !Path::new(&path).exists(),
        "panic did not clean temporary image: {path}"
    );
}

#[test]
fn generated_image_stems_cannot_escape_the_owned_directory() {
    let result = std::panic::catch_unwind(|| common::temp_image_path("../escape"));
    assert!(
        result.is_err(),
        "path traversal was accepted as an image stem"
    );
}
