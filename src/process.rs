//! Process execution helpers: timing, streaming, env management.
//!
//! RustTurbo never uses a shell. Every command is an explicit argv vector, which keeps
//! quoting bugs (and thus silently-changed build invocations) impossible.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct Captured {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
}

impl Captured {
    pub fn success(&self) -> bool {
        self.status.success()
    }

    /// Combined output, useful for diagnostics and error messages.
    pub fn combined(&self) -> String {
        let mut s = String::new();
        if !self.stdout.trim().is_empty() {
            s.push_str(&self.stdout);
        }
        if !self.stderr.trim().is_empty() {
            if !s.is_empty() && !s.ends_with('\n') {
                s.push('\n');
            }
            s.push_str(&self.stderr);
        }
        s
    }
}

#[derive(Debug, Default, Clone)]
pub struct EnvPatch {
    pub set: BTreeMap<String, String>,
    pub remove: Vec<String>,
}

impl EnvPatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.set.insert(k.into(), v.into());
        self
    }

    pub fn remove(mut self, k: impl Into<String>) -> Self {
        self.remove.push(k.into());
        self
    }

    pub fn apply(&self, cmd: &mut Command) {
        for k in &self.remove {
            cmd.env_remove(k);
        }
        for (k, v) in &self.set {
            cmd.env(k, v);
        }
    }
}

/// Run a command to completion, capturing output.
pub fn capture(
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
    env: Option<&EnvPatch>,
) -> Result<Captured> {
    let start = Instant::now();
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    if let Some(env) = env {
        env.apply(&mut cmd);
    }
    let out = cmd
        .output()
        .with_context(|| format!("failed to spawn `{program}` (is it installed and on PATH?)"))?;
    Ok(Captured {
        status: out.status,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        duration: start.elapsed(),
    })
}

pub fn capture_simple(program: &str, args: &[&str]) -> Result<Captured> {
    let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    capture(program, &owned, None, None)
}

/// Minimal `which` implementation (avoids the `which` crate).
pub fn which(program: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(|s| s.to_ascii_lowercase())
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path_var) {
        for ext in &exts {
            let candidate = dir.join(format!("{program}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Run a command with inherited stdio (used to launch the user's program, so its
/// output is interactive and its exit status is observable).
pub fn run_inherit(
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
    env: Option<&EnvPatch>,
) -> Result<(ExitStatus, Duration)> {
    let start = Instant::now();
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    if let Some(env) = env {
        env.apply(&mut cmd);
    }
    let status = cmd
        .status()
        .with_context(|| format!("failed to spawn `{program}`"))?;
    Ok((status, start.elapsed()))
}

/// Run a command, streaming its stderr line-by-line through `sink` while capturing
/// stdout. Used for Cargo invocations so the user keeps progress feedback while we
/// attribute timings.
pub fn capture_streaming<F>(
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
    env: Option<&EnvPatch>,
    mut sink: F,
) -> Result<Captured>
where
    F: FnMut(&str),
{
    let start = Instant::now();
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    if let Some(env) = env {
        env.apply(&mut cmd);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn `{program}`"))?;

    let stderr = child.stderr.take();
    let mut collected = String::new();
    if let Some(stderr) = stderr {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            sink(&line);
            collected.push_str(&line);
            collected.push('\n');
        }
    }
    let out = child.wait_with_output()?;
    Ok(Captured {
        status: out.status,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: collected,
        duration: start.elapsed(),
    })
}

pub fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 1.0 {
        format!("{:.0} ms", s * 1000.0)
    } else if s < 60.0 {
        format!("{s:.2} s")
    } else {
        format!("{}m {:04.1}s", (s / 60.0) as u64, s % 60.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_stdout_and_duration() {
        let out = capture_simple("sh", &["-c", "echo hi; echo err >&2"]).unwrap();
        assert!(out.success());
        assert_eq!(out.stdout.trim(), "hi");
        assert!(out.combined().contains("err"));
        assert!(out.duration.as_nanos() > 0);
    }

    #[test]
    fn nonzero_exit_is_reported_not_panicked() {
        let out = capture_simple("sh", &["-c", "exit 3"]).unwrap();
        assert!(!out.success());
        assert_eq!(out.status.code(), Some(3));
    }

    #[test]
    fn env_patch_sets_and_removes() {
        std::env::set_var("RUSTURBO_TEST_PARENT", "parent");
        let patch = EnvPatch::new()
            .set("RUSTURBO_TEST_CHILD", "child")
            .remove("RUSTURBO_TEST_PARENT");
        let out = capture(
            "sh",
            &["-c".to_string(), "echo \"${RUSTURBO_TEST_CHILD}-${RUSTURBO_TEST_PARENT-unset}\"".to_string()],
            None,
            Some(&patch),
        )
        .unwrap();
        assert_eq!(out.stdout.trim(), "child-unset");
    }

    #[test]
    fn which_finds_sh() {
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-real-binary-xyz").is_none());
    }

    #[test]
    fn streaming_sink_sees_lines() {
        let mut seen = Vec::new();
        capture_streaming(
            "sh",
            &["-c".into(), "echo a >&2; echo b >&2".into()],
            None,
            None,
            |line| seen.push(line.to_string()),
        )
        .unwrap();
        assert_eq!(seen, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(fmt_dur(Duration::from_millis(250)), "250 ms");
        assert_eq!(fmt_dur(Duration::from_millis(1500)), "1.50 s");
        assert_eq!(fmt_dur(Duration::from_secs(75)), "1m 15.0s");
    }
}
