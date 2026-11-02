//! `LocalEnvironment`: spawns on the host, with nothing inherited.
//!
//! A process gets exactly the env its plan names. An agent reached the `builder`
//! key through inherited access in session 80caa720; that can't happen here,
//! because the child's environment starts empty.

use futures::future::{self, BoxFuture, FutureExt};
use plexus_harness::{
    EnvName, EnvValue, Environment, ExitInfo, LaunchPlan, Need, PlanFile, Process, ProcessSignal,
    ResolvedSecret, SecretError, SecretRef, SecretResolver, Signal, SignalError, SpawnError,
    VersionPin,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::watch;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// The host, as an [`Environment`].
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalEnvironment;

impl LocalEnvironment {
    pub fn new() -> Self {
        Self
    }
}

impl Environment for LocalEnvironment {
    fn name(&self) -> &str {
        "local"
    }

    fn spawn(&self, plan: LaunchPlan) -> BoxFuture<'static, Result<Process, SpawnError>> {
        start(plan).boxed()
    }

    fn identity(&self, pid: u32) -> BoxFuture<'static, Option<String>> {
        async move { started_at(pid) }.boxed()
    }

    fn reap(&self, pid: u32, identity: &str) -> BoxFuture<'static, bool> {
        let identity = identity.to_string();
        async move {
            if started_at(pid).as_deref() != Some(&identity) {
                return false;
            }
            tokio::task::spawn_blocking(move || kill_tree(pid as i32))
                .await
                .is_ok_and(|r| r.is_ok())
        }
        .boxed()
    }
}

/// Reads host env vars. The only place a secret's value enters this crate.
struct HostSecrets;

impl SecretResolver for HostSecrets {
    fn resolve(&self, value: &EnvValue) -> BoxFuture<'static, Result<ResolvedSecret, SecretError>> {
        future::ready(resolve(value)).boxed()
    }
}

fn host_var(name: &EnvName) -> Result<ResolvedSecret, SecretError> {
    std::env::var(name.as_str())
        .map(ResolvedSecret::new)
        .map_err(|_| SecretError::Missing {
            name: name.to_string(),
        })
}

fn resolve(value: &EnvValue) -> Result<ResolvedSecret, SecretError> {
    match value {
        EnvValue::Literal { value } => Ok(ResolvedSecret::new(value.clone())),
        EnvValue::Host { name } => host_var(name),
        EnvValue::Secret {
            secret: SecretRef::Env { name },
        } => host_var(name),
        // `SecretRef` will grow with AGENTS-14.
        #[allow(unreachable_patterns)]
        other => Err(SecretError::Missing {
            name: format!("{other:?}"),
        }),
    }
}

fn find_program(name: &str, env: &BTreeMap<String, String>) -> Option<PathBuf> {
    if name.contains('/') {
        return Path::new(name).is_file().then(|| PathBuf::from(name));
    }
    let path = env
        .get("PATH")
        .cloned()
        .or_else(|| std::env::var("PATH").ok())?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

async fn check_version(
    program: &str,
    resolved: &Path,
    pin: &VersionPin,
    env: &BTreeMap<String, String>,
) -> Result<(), SpawnError> {
    let out = Command::new(resolved)
        .args(&pin.version_args)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| SpawnError::Io(format!("{program} version check: {e}")))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let found = text.split_whitespace().next().unwrap_or("").to_string();
    if pin.supported.contains(&found) {
        return Ok(());
    }
    Err(SpawnError::UnsupportedVersion {
        program: program.to_string(),
        found: if found.is_empty() {
            "(no version printed)".to_string()
        } else {
            found
        },
        supported: pin.supported.clone(),
    })
}

async fn meet(need: &Need, env: &BTreeMap<String, String>) -> Result<(), SpawnError> {
    match need {
        Need::Program { name, version } => {
            let Some(found) = find_program(name, env) else {
                return Err(SpawnError::NeedUnmet {
                    need: need.clone(),
                    detail: format!("{name} is not on PATH"),
                });
            };
            match version {
                Some(pin) => check_version(name, &found, pin, env).await,
                None => Ok(()),
            }
        }
        Need::Directory { path } => {
            std::fs::create_dir_all(path).map_err(|e| SpawnError::NeedUnmet {
                need: need.clone(),
                detail: e.to_string(),
            })
        }
        Need::ExistingDirectory { path } => {
            if Path::new(path).is_dir() {
                Ok(())
            } else {
                Err(SpawnError::NeedUnmet {
                    need: need.clone(),
                    detail: "the directory is gone".into(),
                })
            }
        }
    }
}

fn place(file: &PlanFile, cwd: Option<&str>) -> Result<(), SpawnError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let fail = |detail: String| SpawnError::File {
        path: file.path.clone(),
        detail,
    };
    let rel = Path::new(&file.path);
    let plain = rel
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::RootDir));
    if !plain {
        return Err(fail("must be a plain path".into()));
    }
    let dest = if rel.is_absolute() {
        rel.to_path_buf()
    } else {
        let base = cwd.ok_or_else(|| fail("the plan has no cwd".into()))?;
        Path::new(base).join(rel)
    };
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| fail(e.to_string()))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&dest)
        .map_err(|e| fail(e.to_string()))?;
    f.write_all(file.contents.as_bytes())
        .map_err(|e| fail(e.to_string()))
}

async fn start(plan: LaunchPlan) -> Result<Process, SpawnError> {
    let Some(program) = plan.argv.first().cloned() else {
        return Err(SpawnError::BadPlan("empty argv".into()));
    };
    let mut env = BTreeMap::new();
    for (name, value) in &plan.env {
        let v = resolve(value).map_err(SpawnError::Secret)?;
        env.insert(name.to_string(), v.expose().to_string());
    }
    for need in &plan.needs {
        meet(need, &env).await?;
    }
    for file in &plan.files {
        place(file, plan.cwd.as_deref())?;
    }
    let resolved = find_program(&program, &env).ok_or_else(|| SpawnError::NeedUnmet {
        need: Need::Program {
            name: program.clone(),
            version: None,
        },
        detail: "not found".into(),
    })?;

    let mut cmd = Command::new(resolved);
    cmd.args(&plan.argv[1..])
        .env_clear()
        .envs(&env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &plan.cwd {
        cmd.current_dir(cwd);
    }
    // Its own session and group, so a stop can't reach us and ours can't reach it.
    // SAFETY: setsid is async-signal-safe and nothing else runs before exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| SpawnError::Io(e.to_string()))?;
    let pid = child
        .id()
        .ok_or_else(|| SpawnError::Io("process exited during spawn".into()))?;
    let (stdin, stdout, stderr) =
        match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
            (Some(i), Some(o), Some(e)) => (i, o, e),
            _ => return Err(SpawnError::Io("missing stdio pipe".into())),
        };

    let exited = Arc::new(AtomicBool::new(false));
    let (exit_tx, mut exit_rx) = watch::channel(None::<ExitInfo>);
    let flag = exited.clone();
    tokio::spawn(async move {
        let status = child.wait().await;
        flag.store(true, Ordering::SeqCst);
        let _ = exit_tx.send(Some(status.map_or(
            ExitInfo {
                code: None,
                signal: None,
            },
            exit_info,
        )));
    });
    let wait = async move {
        loop {
            if let Some(exit) = *exit_rx.borrow_and_update() {
                return exit;
            }
            if exit_rx.changed().await.is_err() {
                return ExitInfo {
                    code: None,
                    signal: None,
                };
            }
        }
    }
    .boxed();

    Ok(Process {
        pid: Some(pid),
        stdin: Box::pin(stdin.compat_write()),
        stdout: Box::pin(stdout.compat()),
        stderr: Box::pin(stderr.compat()),
        signal: Arc::new(LocalSignal {
            pid: pid as i32,
            exited,
        }),
        wait,
        secrets: Arc::new(HostSecrets),
    })
}

/// When `pid` started, which tells it from a later process with the same pid.
fn started_at(pid: u32) -> Option<String> {
    let out = std::process::Command::new("/bin/ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn exit_info(status: ExitStatus) -> ExitInfo {
    use std::os::unix::process::ExitStatusExt;
    ExitInfo {
        code: status.code(),
        signal: status.signal(),
    }
}

struct LocalSignal {
    pid: i32,
    exited: Arc<AtomicBool>,
}

impl ProcessSignal for LocalSignal {
    fn signal(&self, signal: Signal) -> BoxFuture<'static, Result<(), SignalError>> {
        let (pid, exited) = (self.pid, self.exited.load(Ordering::SeqCst));
        async move {
            match signal {
                // The pid can be reused once it's reaped. Nothing to do anyway.
                Signal::Interrupt | Signal::Terminate if exited => Ok(()),
                Signal::Interrupt => send(pid, libc::SIGINT),
                Signal::Terminate => send(pid, libc::SIGTERM),
                Signal::KillTree => tokio::task::spawn_blocking(move || kill_tree(pid))
                    .await
                    .map_err(|e| SignalError(e.to_string()))?,
            }
        }
        .boxed()
    }
}

fn send(pid: i32, sig: i32) -> Result<(), SignalError> {
    // SAFETY: plain syscall; a stale pid gets ESRCH.
    if unsafe { libc::kill(pid, sig) } == 0 {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(SignalError(e.to_string()))
}

#[derive(Debug, Clone, Copy)]
struct Row {
    ppid: i32,
    pgid: i32,
    zombie: bool,
}

fn process_table() -> Result<HashMap<i32, Row>, SignalError> {
    let out = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,pgid=,stat="])
        .output()
        .map_err(|e| SignalError(format!("ps: {e}")))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut rows = HashMap::new();
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(pgid), Some(stat)) =
            (f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        if let (Ok(pid), Ok(ppid), Ok(pgid)) = (pid.parse(), ppid.parse(), pgid.parse()) {
            rows.insert(
                pid,
                Row {
                    ppid,
                    pgid,
                    zombie: stat.starts_with('Z'),
                },
            );
        }
    }
    Ok(rows)
}

fn descendants(table: &HashMap<i32, Row>, root: i32) -> BTreeSet<i32> {
    let mut out = BTreeSet::new();
    let mut todo = vec![root];
    while let Some(p) = todo.pop() {
        for (pid, row) in table {
            if row.ppid == p && out.insert(*pid) {
                todo.push(*pid);
            }
        }
    }
    out
}

/// Stops the tree first so it can't fork or reap past us, learns every group from
/// it while parents are alive to link them, then kills the groups until none has
/// a live member. A process that detached by double fork before this ran has no
/// link to the tree, so it isn't found: that's the limit of a pid-based kill.
fn kill_tree(root: i32) -> Result<(), SignalError> {
    // SAFETY: getpgrp has no preconditions.
    let own_group = unsafe { libc::getpgrp() };
    let mut pids = BTreeSet::new();
    let mut groups = BTreeSet::from([root]);
    let _ = send(root, libc::SIGSTOP);
    for _ in 0..2 {
        let table = process_table()?;
        for pid in descendants(&table, root) {
            if pids.insert(pid) {
                let _ = send(pid, libc::SIGSTOP);
            }
            if let Some(row) = table.get(&pid) {
                if row.pgid != own_group {
                    groups.insert(row.pgid);
                }
            }
        }
    }
    let mut left = 0;
    for _ in 0..50 {
        for g in &groups {
            // SAFETY: plain syscall; a gone group gets ESRCH.
            unsafe { libc::killpg(*g, libc::SIGKILL) };
        }
        for p in &pids {
            let _ = send(*p, libc::SIGKILL);
        }
        let _ = send(root, libc::SIGKILL);
        std::thread::sleep(Duration::from_millis(20));
        let table = process_table()?;
        left = table
            .iter()
            .filter(|(pid, row)| {
                !row.zombie && (groups.contains(&row.pgid) || pids.contains(pid) || **pid == root)
            })
            .count();
        if left == 0 {
            return Ok(());
        }
    }
    Err(SignalError(format!("{left} processes survived SIGKILL")))
}
