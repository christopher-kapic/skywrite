//! Bounded subprocess execution. Commands are never passed to a shell.
use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::io::Read;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

#[cfg(unix)]
mod cancellation {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Condvar, Mutex, OnceLock, atomic::AtomicI32};

    pub struct State {
        pub children: Mutex<HashMap<u32, bool>>,
        drained: Condvar,
        first_signal: AtomicI32,
    }
    static STATE: OnceLock<std::result::Result<Arc<State>, String>> = OnceLock::new();

    // signal-hook leaves its low-level handler installed after unregistering.
    // Restore previous dispositions if setup fails before a signal owner exists.
    struct SignalSetup {
        dispositions: Vec<(i32, libc::sigaction)>,
        registrations: Vec<signal_hook::SigId>,
    }
    impl SignalSetup {
        fn capture() -> std::io::Result<Self> {
            let mut saved = Vec::new();
            for signal in [libc::SIGINT, libc::SIGTERM] {
                let mut action = unsafe { std::mem::zeroed() };
                // SAFETY: a null new action only queries this process's disposition.
                if unsafe { libc::sigaction(signal, std::ptr::null(), &mut action) } != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                saved.push((signal, action));
            }
            Ok(Self {
                dispositions: saved,
                registrations: Vec::new(),
            })
        }
    }
    impl Drop for SignalSetup {
        fn drop(&mut self) {
            for registration in self.registrations.drain(..) {
                signal_hook::low_level::unregister(registration);
            }
            for (signal, action) in &self.dispositions {
                // SAFETY: restore the exact disposition captured before setup.
                unsafe {
                    libc::sigaction(*signal, action, std::ptr::null_mut());
                }
            }
        }
    }

    pub fn state() -> Result<&'static Arc<State>> {
        STATE
            .get_or_init(|| {
                let mut rollback = SignalSetup::capture().map_err(|e| e.to_string())?;
                let state = Arc::new(State {
                    children: Mutex::new(HashMap::new()),
                    drained: Condvar::new(),
                    first_signal: AtomicI32::new(0),
                });
                for signal in [libc::SIGINT, libc::SIGTERM] {
                    let state = state.clone();
                    // SAFETY: the handler only performs a lock-free atomic operation.
                    // Register before iterator wakeups, which enumerate co-pending
                    // signals by number rather than by their delivery order.
                    let registration = unsafe {
                        signal_hook::low_level::register(signal, move || {
                            let _ = state.first_signal.compare_exchange(
                                0,
                                signal,
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                            );
                        })
                    }
                    .map_err(|e| e.to_string())?;
                    rollback.registrations.push(registration);
                }
                let mut signals = signal_hook::iterator::Signals::new([
                    signal_hook::consts::SIGINT,
                    signal_hook::consts::SIGTERM,
                ])
                .map_err(|e| e.to_string())?;
                let owner = state.clone();
                std::thread::Builder::new()
                    .name("child-signals".into())
                    .spawn(move || {
                        // A signal can arrive between installing the recording
                        // handlers and the iterator. In that case no wakeup exists.
                        if owner.cancelled() || signals.forever().next().is_some() {
                            let signal = owner.first_signal.load(Ordering::SeqCst);
                            let mut children =
                                owner.children.lock().unwrap_or_else(|e| e.into_inner());
                            for (pid, live) in children.iter() {
                                if *live {
                                    kill_group(*pid);
                                }
                            }
                            // Child owners reap children and join pipe readers before unregistering.
                            while !children.is_empty() {
                                children = owner
                                    .drained
                                    .wait(children)
                                    .unwrap_or_else(|e| e.into_inner());
                            }
                            drop(children);
                            #[cfg(test)]
                            super::tests::hold_signal_exit();
                            // Restore normal signal exit even when the caller is blocked in HTTP.
                            let _ = signal_hook::low_level::emulate_default_handler(signal);
                        }
                    })
                    .map_err(|e| e.to_string())?;
                rollback.dispositions.clear();
                rollback.registrations.clear();
                Ok(state)
            })
            .as_ref()
            .map_err(|e| anyhow::anyhow!("installing child signal supervisor: {e}"))
    }
    impl State {
        pub fn cancelled(&self) -> bool {
            self.first_signal.load(Ordering::SeqCst) != 0
        }
    }

    // Drop after the child owner and registration lock on EVERY run() return,
    // including spawn/read failures and unwind. Returning cancellation to main
    // could otherwise race its ordinary error exit against the signal owner.
    pub struct ReturnGuard(pub &'static Arc<State>);
    impl Drop for ReturnGuard {
        fn drop(&mut self) {
            if self.0.cancelled() {
                let mut children = self.0.children.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    children = self
                        .0
                        .drained
                        .wait(children)
                        .unwrap_or_else(|e| e.into_inner());
                }
            }
        }
    }

    pub fn kill_group(pid: u32) {
        // SAFETY: each registered child was spawned in its own process group.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    pub fn unregister(state: &State, pid: u32) {
        state
            .children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&pid);
        state.drained.notify_all();
    }
}

/// Owns child termination, reaping and reader joins on errors and unwind as well
/// as success. Lock order: registry -> spawn/kill/reap; no reader joins under
/// its lock. A reaped entry remains until readers join, but cannot be signalled.
struct ChildOwner {
    child: Child,
    cleaned: bool,
    readers: Vec<std::thread::JoinHandle<()>>,
    #[cfg(unix)]
    state: &'static Arc<cancellation::State>,
}
impl Drop for ChildOwner {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.cleanup();
        }
    }
}
impl ChildOwner {
    fn cleanup(&mut self) -> std::io::Result<std::process::ExitStatus> {
        #[cfg(unix)]
        let mut registry = self
            .state
            .children
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        #[cfg(unix)]
        cancellation::kill_group(self.child.id());
        let _ = self.child.kill();
        let status = self.child.wait();
        #[cfg(unix)]
        {
            registry.insert(self.child.id(), false);
            drop(registry);
        }
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
        #[cfg(unix)]
        cancellation::unregister(self.state, self.child.id());
        self.cleaned = true;
        status
    }
}

/// Run a child with bounded stdout/stderr and a deadline covering pipe draining.
pub fn run(
    program: &str,
    args: &[OsString],
    timeout: Duration,
    max_output: usize,
) -> Result<Output> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    let state = cancellation::state()?;
    #[cfg(unix)]
    let _return_guard = cancellation::ReturnGuard(state);
    #[cfg(unix)]
    let mut registration = {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        let registration = state.children.lock().unwrap_or_else(|e| e.into_inner());
        if state.cancelled() {
            bail!("subprocess cancelled");
        }
        registration
    };
    // Keep the registry locked through spawn and publication: a signal owner
    // cannot miss a new group even if delivery occurs inside Command::spawn.
    let child = command
        .spawn()
        .with_context(|| format!("could not start {program}; is it installed and on PATH?"))?;
    #[cfg(unix)]
    registration.insert(child.id(), true);
    let mut owner = ChildOwner {
        child,
        cleaned: false,
        readers: Vec::new(),
        #[cfg(unix)]
        state,
    };
    #[cfg(unix)]
    drop(registration);
    let exceeded = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let out = owner.child.stdout.take().context("missing child stdout")?;
    let err = owner.child.stderr.take().context("missing child stderr")?;
    for (index, pipe) in [
        (0, Box::new(out) as Box<dyn Read + Send>),
        (1, Box::new(err) as Box<dyn Read + Send>),
    ] {
        let tx = tx.clone();
        let exceeded = exceeded.clone();
        owner.readers.push(
            std::thread::Builder::new()
                .name("child-output".into())
                .spawn(move || {
                    let mut pipe = pipe;
                    let mut bytes = Vec::new();
                    let mut buf = [0u8; 8192];
                    let result = loop {
                        match pipe.read(&mut buf) {
                            Ok(0) => break Ok(bytes),
                            Ok(n) => {
                                if n > max_output.saturating_sub(bytes.len()) {
                                    exceeded.store(true, Ordering::Relaxed);
                                    break Err(std::io::Error::other(
                                        "subprocess output exceeded limit",
                                    ));
                                }
                                bytes.extend_from_slice(&buf[..n]);
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(e) => break Err(e),
                        }
                    };
                    let _ = tx.send((index, result));
                })
                .context("starting child output reader")?,
        );
    }
    drop(tx);
    let deadline = Instant::now() + timeout;
    let mut captured: [Option<Vec<u8>>; 2] = [None, None];
    loop {
        #[cfg(unix)]
        if state.cancelled() {
            bail!("{program} cancelled");
        }
        if exceeded.load(Ordering::Relaxed) {
            bail!("{program} exceeded its {max_output}-byte output limit");
        }
        while let Ok((index, result)) = rx.try_recv() {
            captured[index] = Some(result.with_context(|| format!("reading {program} output"))?);
        }
        // Linux WNOWAIT keeps the leader reserved until the group is killed and
        // reaped by its owner, preventing stale PGIDs from targeting reused PIDs.
        #[cfg(target_os = "linux")]
        let finished = {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    owner.child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result != 0 {
                return Err(std::io::Error::last_os_error()).context("checking child status");
            }
            unsafe { info.si_pid() != 0 }
        };
        #[cfg(not(target_os = "linux"))]
        let finished = owner
            .child
            .try_wait()
            .context("checking child status")?
            .is_some();
        if finished && captured.iter().all(Option::is_some) {
            let status = owner
                .cleanup()
                .with_context(|| format!("waiting for {program}"))?;
            let output = Output {
                status,
                stdout: captured[0].take().unwrap(),
                stderr: captured[1].take().unwrap(),
            };
            if !output.status.success() {
                let diagnostic: String = String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(4096)
                    .collect();
                bail!(
                    "{program} exited with {}: {}",
                    output.status,
                    diagnostic.trim()
                );
            }
            return Ok(output);
        }
        if Instant::now() >= deadline {
            bail!(
                "{program} timed out after {} seconds",
                timeout.as_secs_f64()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only test binaries expose this scheduler gate. Production always proceeds
    // directly from complete registry drain to default signal emulation.
    #[cfg(unix)]
    pub(super) fn hold_signal_exit() {
        if let Some(dir) = std::env::var_os("SKYWRITE_SIGNAL_EXIT_GATE") {
            let dir = std::path::PathBuf::from(dir);
            std::fs::write(dir.join("drained"), b"ready").unwrap();
            let deadline = Instant::now() + Duration::from_secs(8);
            while !dir.join("release").exists() {
                assert!(Instant::now() < deadline, "signal exit gate timed out");
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cancellation_exit_fixture() {
        let Some(dir) = std::env::var_os("SKYWRITE_SIGNAL_EXIT_GATE") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let (tx, rx) = mpsc::channel();
        for index in 0..3 {
            let tx = tx.clone();
            let marker = dir.join(index.to_string());
            std::thread::spawn(move || {
                let script = format!(
                    "printf %s $$ > '{}'; exec /usr/bin/ffmpeg -nostdin -v error -re -f lavfi -i sine=frequency=440:sample_rate=48000 -t 30 -f s16le pipe:1",
                    marker.display()
                );
                let result = run(
                    "/bin/sh",
                    &args(&["-c", &script]),
                    Duration::from_secs(35),
                    8 * 1024 * 1024,
                );
                let _ = tx.send(result.is_err());
            });
        }
        // Reproduce a CLI main that returns an error immediately after its
        // process runner returns, rather than masking the race with a sleep.
        let _ = rx.recv().unwrap();
        std::process::exit(1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cancellation_waits_for_signal_exit_after_all_real_ffmpeg_groups_drain() {
        use std::os::unix::process::ExitStatusExt;
        fn until(mut check: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(6);
            while !check() {
                assert!(Instant::now() < deadline, "cancellation fixture timed out");
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        for signal in [libc::SIGTERM, libc::SIGINT] {
            let dir = tempfile::tempdir().unwrap();
            let mut parent = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "process::tests::cancellation_exit_fixture",
                    "--nocapture",
                ])
                .env("SKYWRITE_SIGNAL_EXIT_GATE", dir.path())
                .spawn()
                .unwrap();
            until(|| {
                (0..3).all(|i| {
                    std::fs::read_to_string(dir.path().join(i.to_string()))
                        .ok()
                        .and_then(|pid| std::fs::read_to_string(format!("/proc/{pid}/comm")).ok())
                        .is_some_and(|name| name.trim() == "ffmpeg")
                })
            });
            let pids: Vec<String> = (0..3)
                .map(|i| std::fs::read_to_string(dir.path().join(i.to_string())).unwrap())
                .collect();
            assert_eq!(unsafe { libc::kill(parent.id() as i32, signal) }, 0);
            until(|| {
                if let Some(status) = parent.try_wait().unwrap() {
                    // A fault can let main exit before other owners clean up.
                    // Bound the deliberate-fault experiment to its own groups.
                    for pid in &pids {
                        unsafe {
                            libc::kill(pid.parse().unwrap(), libc::SIGKILL);
                        }
                    }
                    assert_eq!(
                        status.signal(),
                        Some(signal),
                        "run returned before all groups/readers drained"
                    );
                }
                dir.path().join("drained").exists()
            });
            // Owner is paused after ALL child waits and reader joins. An old
            // run() returns now and the fixture exits 1 before gate release.
            assert!(
                pids.iter()
                    .all(|pid| !std::path::Path::new(&format!("/proc/{pid}")).exists())
            );
            let other = if signal == libc::SIGTERM {
                libc::SIGINT
            } else {
                libc::SIGTERM
            };
            assert_eq!(unsafe { libc::kill(parent.id() as i32, other) }, 0);
            let deadline = Instant::now() + Duration::from_millis(100);
            while Instant::now() < deadline {
                assert!(
                    parent.try_wait().unwrap().is_none(),
                    "run returned before default signal exit"
                );
                std::thread::yield_now();
            }
            std::fs::write(dir.path().join("release"), b"go").unwrap();
            until(|| parent.try_wait().unwrap().is_some());
            assert_eq!(
                parent.wait().unwrap().signal(),
                Some(signal),
                "first actual signal must win"
            );
        }
    }

    #[test]
    fn executable_missing_is_actionable() {
        assert!(
            run(
                "skywrite-deliberately-missing-program",
                &[],
                Duration::from_secs(1),
                1024
            )
            .unwrap_err()
            .to_string()
            .contains("could not start")
        );
    }
    #[cfg(unix)]
    #[test]
    fn bounds_output_and_deadline() {
        let start = Instant::now();
        let error = run(
            "sh",
            &args(&["-c", "printf 'abcdefghijklmnopqrstuvwxyz'"]),
            Duration::from_secs(2),
            8,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("output") && format!("{error:#}").contains("limit"));
        assert!(start.elapsed() < Duration::from_secs(1));
        let start = Instant::now();
        let error = run(
            "sh",
            &args(&["-c", "sleep 30 & wait"]),
            Duration::from_millis(100),
            1024,
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(3));
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn owner_joins_readers_and_reaps_child_on_unwind() {
        use std::os::unix::process::CommandExt;
        let state = cancellation::state().unwrap();
        let mut registration = state.children.lock().unwrap_or_else(|e| e.into_inner());
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "sleep 30"])
            .process_group(0)
            .stdout(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        registration.insert(pid, true);
        let mut pipe = child.stdout.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
        });
        let owner = ChildOwner {
            child,
            cleaned: false,
            readers: vec![reader],
            state,
        };
        drop(registration);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _owner = owner;
                panic!("controlled owner unwind");
            }))
            .is_err()
        );
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "owned child must be reaped on unwind"
        );
        assert!(
            !state
                .children
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&pid)
        );
    }
}
