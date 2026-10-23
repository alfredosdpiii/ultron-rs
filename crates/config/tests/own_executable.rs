//! `config::paths::own_executable` (user report: every new durable session failed with ENOENT after an install
//! replaced the binary under the running server): a process whose binary was replaced still spawns its own image.

use config::paths::{own_executable, running_image};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const ROLE: &str = "OWN_EXECUTABLE_TEST_ROLE";

#[test]
fn config_own_executable_is_the_file_while_it_exists_else_the_running_image() {
    let (exe, image) = (PathBuf::from("/opt/u/ultron (deleted)"), PathBuf::from("/proc/7/exe"));
    let installed = PathBuf::from("/opt/u/ultron");
    assert_eq!(running_image(exe.clone(), image.clone(), &|p: &Path| p == image), image);
    assert_eq!(
        running_image(installed.clone(), image.clone(), &|p: &Path| p == installed
            || p == image),
        installed
    );
    assert_eq!(running_image(exe.clone(), image, &|_: &Path| false), exe);
}

/// A copy of this test binary deletes its own file, puts another file at its path (an install), and spawns
/// `own_executable()`: the probe runs (this same image), where `current_exe` names `<path> (deleted)`.
#[test]
fn config_own_executable_spawns_this_image_after_its_file_is_replaced() {
    match std::env::var(ROLE).as_deref() {
        Ok("probe") => {
            println!("probe-ran");
            return;
        }
        Ok("replaced") => {
            let path = std::env::current_exe().unwrap();
            std::fs::remove_file(&path).unwrap();
            std::fs::write(&path, "#!/bin/sh\nexit 3\n").unwrap();
            let stale = std::env::current_exe().unwrap();
            assert!(
                stale.display().to_string().ends_with(" (deleted)"),
                "{}",
                stale.display()
            );
            let exe = own_executable().unwrap();
            assert_eq!(exe, PathBuf::from(format!("/proc/{}/exe", std::process::id())));
            let out = Command::new(&exe)
                .args([
                    "--exact",
                    "config_own_executable_spawns_this_image_after_its_file_is_replaced",
                    "--nocapture",
                ])
                .env(ROLE, "probe")
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(String::from_utf8_lossy(&out.stdout).contains("probe-ran"), "{out:?}");
            println!("replaced-ok");
            return;
        }
        _ => {}
    }
    let tmp = tempfile::tempdir().unwrap();
    let copy = tmp.path().join("ultron");
    std::fs::copy(std::env::current_exe().unwrap(), &copy).unwrap();
    let out = Command::new(&copy)
        .args([
            "--exact",
            "config_own_executable_spawns_this_image_after_its_file_is_replaced",
            "--nocapture",
        ])
        .env(ROLE, "replaced")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("replaced-ok"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
