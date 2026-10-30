//! Bounded subprocess execution. Commands are never passed to a shell.
use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

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
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("could not start {program}; is it installed and on PATH?"))?;
    let exceeded = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let out = child.stdout.take().context("missing child stdout")?;
    let err = child.stderr.take().context("missing child stderr")?;
    for (index, pipe) in [
        (0, Box::new(out) as Box<dyn Read + Send>),
        (1, Box::new(err) as Box<dyn Read + Send>),
    ] {
        let tx = tx.clone();
        let exceeded = exceeded.clone();
        std::thread::spawn(move || {
            let mut pipe = pipe;
            let mut bytes = Vec::new();
            let mut buf = [0u8; 8192];
            let result = loop {
                match pipe.read(&mut buf) {
                    Ok(0) => break Ok(bytes),
                    Ok(n) => {
                        if n > max_output.saturating_sub(bytes.len()) {
                            exceeded.store(true, Ordering::Relaxed);
                            break Err(std::io::Error::other("subprocess output exceeded limit"));
                        }
                        bytes.extend_from_slice(&buf[..n]);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => break Err(e),
                }
            };
            let _ = tx.send((index, result));
        });
    }
    drop(tx);
    let deadline = Instant::now() + timeout;
    let mut status = None;
    let mut captured: [Option<Vec<u8>>; 2] = [None, None];
    loop {
        while let Ok((index, result)) = rx.try_recv() {
            match result {
                Ok(bytes) => captured[index] = Some(bytes),
                Err(e) => {
                    terminate(&mut child);
                    return Err(e).with_context(|| format!("reading {program} output"));
                }
            }
        }
        if exceeded.load(Ordering::Relaxed) {
            terminate(&mut child);
            bail!("{program} exceeded its {max_output}-byte output limit");
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(value) => status = value,
                Err(e) => {
                    terminate(&mut child);
                    return Err(e).with_context(|| format!("waiting for {program}"));
                }
            }
        }
        if let Some(status) = status
            && captured.iter().all(Option::is_some)
        {
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
            terminate(&mut child);
            bail!(
                "{program} timed out after {} seconds",
                timeout.as_secs_f64()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn terminate(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // SAFETY: spawn put the child in its own process group. Negative PID
        // addresses only that group, including descendants holding capture pipes.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(
            run(
                "sh",
                &args(&["-c", "while :; do printf abcdefghijklmnop; done"]),
                Duration::from_secs(2),
                128
            )
            .is_err()
        );
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
}
