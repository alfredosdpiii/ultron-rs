//! Esc reaches the worker at once, even while the link thread is busy with another command: here `/share`, whose
//! `gh auth status` (a fake `gh`) does not return until the test lets it. Before, the abort waited in the link
//! thread's queue until the share was done.

use crate::tui_harness;

use tui_harness::{Mock, QUIET, Reply, TempDir, Tui, text_reply};

const MODEL: [&str; 2] = ["--model", "anthropic/claude-sonnet-4-5"];

#[test]
fn cli_interactive_abort_is_not_queued_behind_a_busy_link() {
    let mock = Mock::start(
        vec![Reply {
            hold_after: Some(1),
            ..text_reply("Working on it.")
        }],
        None,
    );
    let tmp = TempDir::new();
    let bin = tmp.0.join("fake-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let (started, release) = (tmp.0.join("gh-started"), tmp.0.join("gh-release"));
    std::fs::write(
        bin.join("gh"),
        format!(
            "#!/bin/sh\ntouch '{}'\nwhile [ ! -e '{}' ]; do sleep 0.05; done\nexit 1\n",
            started.display(),
            release.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(bin.join("gh"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let env: Vec<(&str, &str)> = QUIET.iter().copied().chain([("PATH", path.as_str())]).collect();
    let tui = Tui::start_at(&tmp.0, mock.port, 120, 40, &MODEL, &env);
    tui.wait("Session:");
    tui.send_text("go");
    tui.send_keys(&["Enter"]);
    mock.wait_held(1);
    tui.send_text("/share");
    tui.send_keys(&["Enter"]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while !started.exists() {
        assert!(std::time::Instant::now() < deadline, "gh never ran");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // The link thread waits on `gh`; Esc still aborts the run.
    tui.send_keys(&["Escape"]);
    tui.wait("This operation was aborted");
    std::fs::write(&release, "").unwrap();
    mock.release();
    drop(tui);
}
