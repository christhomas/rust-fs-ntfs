//! Generated integration-test images must be isolated and panic-safe.

mod common;

use std::fs;
use std::path::Path;
use std::sync::{mpsc, Arc, Barrier};

#[test]
fn generated_images_use_the_shared_temp_image_primitive() {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut violations = Vec::new();

    for entry in fs::read_dir(&tests).expect("read tests directory") {
        let path = entry.expect("read tests entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs")
            || path.file_name().and_then(|name| name.to_str()) == Some("temp_image_policy.rs")
        {
            continue;
        }
        let source = fs::read_to_string(&path).expect("read integration test source");
        for (index, line) in source.lines().enumerate() {
            let trimmed = line.trim_start();
            if line.contains("test-disks/_")
                && !trimmed.starts_with("//")
                && !line.contains("_does_not_exist.img")
            {
                violations.push(format!("{}:{}: {}", path.display(), index + 1, line.trim()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "generated images bypass the shared temporary-image primitive:\n{}",
        violations.join("\n")
    );
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
