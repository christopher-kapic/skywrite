#![cfg(target_os = "linux")]
use skywrite::process::{args, run};
use std::{
    process::Command,
    time::{Duration, Instant},
};

#[test]
fn lifecycle_fixture() {
    let Ok(mode) = std::env::var("SKYWRITE_LIFECYCLE_FIXTURE") else {
        return;
    };
    if mode == "idle" {
        run(
            "/bin/sh",
            &args(&["-c", "printf ok"]),
            Duration::from_secs(1),
            128,
        )
        .unwrap();
        std::fs::write(std::env::var("SKYWRITE_LIFECYCLE_DIR").unwrap(), b"ready").unwrap();
        std::thread::sleep(Duration::from_secs(30));
        return;
    }
    if mode == "census" {
        let count = |path| std::fs::read_dir(path).unwrap().count();
        run(
            "/bin/sh",
            &args(&["-c", "printf ok"]),
            Duration::from_secs(1),
            128,
        )
        .unwrap();
        let before = (count("/proc/self/fd"), count("/proc/self/task"));
        for _ in 0..12 {
            let workers: Vec<_> = (0..3)
                .map(|_| {
                    std::thread::spawn(|| {
                        assert!(
                            run(
                                "/bin/sh",
                                &args(&["-c", "exit 7"]),
                                Duration::from_secs(1),
                                128
                            )
                            .unwrap_err()
                            .to_string()
                            .contains("exited")
                        );
                        assert!(
                            run(
                                "/bin/sh",
                                &args(&["-c", "sleep 2 & wait"]),
                                Duration::from_millis(20),
                                128
                            )
                            .unwrap_err()
                            .to_string()
                            .contains("timed out")
                        );
                        assert_eq!(
                            run(
                                "/bin/sh",
                                &args(&["-c", "printf ok"]),
                                Duration::from_secs(1),
                                128
                            )
                            .unwrap()
                            .stdout,
                            b"ok"
                        );
                    })
                })
                .collect();
            for worker in workers {
                worker.join().unwrap();
            }
        }
        assert_eq!(
            (count("/proc/self/fd"), count("/proc/self/task")),
            before,
            "repeated concurrent calls leaked descriptors or threads"
        );
        return;
    }
    let dir = std::env::var("SKYWRITE_LIFECYCLE_DIR").unwrap();
    let workers: Vec<_> = (0..3)
        .map(|index| {
            let script = format!(
                "sleep 30 & descendant=$!; printf '%s %s' $$ $descendant > '{dir}/{index}'; wait"
            );
            std::thread::spawn(move || {
                let _ = run(
                    "/bin/sh",
                    &args(&["-c", &script]),
                    Duration::from_secs(35),
                    128,
                );
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    std::thread::sleep(Duration::from_secs(2));
}
fn live(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    !stat
        .rsplit_once(')')
        .unwrap()
        .1
        .trim_start()
        .starts_with('Z')
}
fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline, "fixture condition timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn fixture(mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "lifecycle_fixture", "--nocapture"])
        .env("SKYWRITE_LIFECYCLE_FIXTURE", mode);
    command
}
#[test]
fn ordinary_signals_clean_all_concurrent_child_groups() {
    use std::os::unix::process::ExitStatusExt;
    for signal in [libc::SIGTERM, libc::SIGINT] {
        for _ in 0..3 {
            let dir = tempfile::tempdir().unwrap();
            let mut parent = fixture("signal")
                .env("SKYWRITE_LIFECYCLE_DIR", dir.path())
                .spawn()
                .unwrap();
            wait_until(|| (0..3).all(|i| dir.path().join(i.to_string()).exists()));
            let pids: Vec<i32> = (0..3)
                .flat_map(|i| {
                    std::fs::read_to_string(dir.path().join(i.to_string()))
                        .unwrap()
                        .split_whitespace()
                        .map(|s| s.parse().unwrap())
                        .collect::<Vec<_>>()
                })
                .collect();
            assert!(pids.iter().all(|pid| live(*pid)));
            assert_eq!(unsafe { libc::kill(parent.id() as i32, signal) }, 0);
            wait_until(|| parent.try_wait().unwrap().is_some());
            let status = parent.wait().unwrap();
            let abandoned: Vec<_> = pids.iter().copied().filter(|pid| live(*pid)).collect();
            for pid in &pids {
                unsafe {
                    libc::kill(*pid, libc::SIGKILL);
                }
            }
            assert_eq!(
                status.signal(),
                Some(signal),
                "default signal exit must be preserved"
            );
            assert!(
                abandoned.is_empty(),
                "ordinary cancellation abandoned child PIDs {abandoned:?}"
            );
        }
    }
}
#[test]
fn repeated_failure_timeout_and_success_do_not_leak() {
    assert!(fixture("census").status().unwrap().success());
}
#[test]
fn successful_capture_and_output_cap_remain_distinct() {
    assert_eq!(
        run(
            "/bin/sh",
            &args(&["-c", "printf abc; printf def >&2"]),
            Duration::from_secs(2),
            3
        )
        .unwrap()
        .stdout,
        b"abc"
    );
    let error = run(
        "/bin/sh",
        &args(&["-c", "printf abcdefghijklmnopqrstuvwxyz"]),
        Duration::from_secs(2),
        8,
    )
    .unwrap_err();
    let diagnostic = format!("{error:#}");
    assert!(
        diagnostic.contains("output") && diagnostic.contains("limit"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("timed out"));
}

#[test]
fn signal_after_helpers_finish_preserves_prompt_default_exit() {
    use std::os::unix::process::ExitStatusExt;
    let d = tempfile::tempdir().unwrap();
    let ready = d.path().join("ready");
    let mut parent = fixture("idle")
        .env("SKYWRITE_LIFECYCLE_DIR", &ready)
        .spawn()
        .unwrap();
    wait_until(|| ready.exists());
    let start = Instant::now();
    assert_eq!(unsafe { libc::kill(parent.id() as i32, libc::SIGTERM) }, 0);
    wait_until(|| parent.try_wait().unwrap().is_some());
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(parent.wait().unwrap().signal(), Some(libc::SIGTERM));
}

#[test]
fn exited_leader_with_descendant_capture_pipe_is_bounded() {
    let d = tempfile::tempdir().unwrap();
    let marker = d.path().join("pid");
    let script = format!("sleep 30 & printf %s $! > '{}'; exit 0", marker.display());
    let error = run(
        "/bin/sh",
        &args(&["-c", &script]),
        Duration::from_millis(100),
        128,
    )
    .unwrap_err();
    assert!(error.to_string().contains("timed out"));
    let pid = std::fs::read_to_string(marker).unwrap().parse().unwrap();
    assert!(
        !live(pid),
        "descendant retaining capture pipe survived timeout"
    );
}
