//! `RecordingEnvironment`: wraps any environment and writes down what crossed the
//! process's pipes, in the fixture layout AGENTS-3 used: `<phase>.{argv,stdin,
//! stdout,stderr,times,exit}`.
//!
//! Recordings are raw. They go outside any repo until scrubbed.

use futures::future::{BoxFuture, FutureExt};
use futures::io::{AsyncRead, AsyncWrite};
use plexus_harness::{Environment, LaunchPlan, Process, SpawnError};
use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

/// See the module docs. Each spawn is a phase: `1-claude`, `2-claude`, and so on.
pub struct RecordingEnvironment<E> {
    inner: E,
    dir: PathBuf,
    spawned: Arc<AtomicUsize>,
}

impl<E> RecordingEnvironment<E> {
    pub fn new(inner: E, dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            inner,
            dir,
            spawned: Arc::default(),
        })
    }
}

impl<E: Environment> Environment for RecordingEnvironment<E> {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn spawn(&self, plan: LaunchPlan) -> BoxFuture<'static, Result<Process, SpawnError>> {
        let n = self.spawned.fetch_add(1, Ordering::SeqCst) + 1;
        let base = self.dir.join(format!("{n}-claude"));
        let path = move |ext: &str| base.with_extension(ext);
        let argv = serde_json::to_string(&plan.argv[1.min(plan.argv.len())..]).unwrap_or_default();
        let started = self.inner.spawn(plan);
        async move {
            let io = |e: io::Error| SpawnError::Io(format!("recording: {e}"));
            std::fs::write(path("argv"), argv).map_err(io)?;
            let p = started.await?;
            let t0 = Instant::now();
            let stdin = TeeWrite {
                inner: p.stdin,
                log: File::create(path("stdin")).map_err(io)?,
            };
            let stdout = TeeRead {
                inner: p.stdout,
                log: File::create(path("stdout")).map_err(io)?,
                times: Some((File::create(path("times")).map_err(io)?, t0)),
            };
            let stderr = TeeRead {
                inner: p.stderr,
                log: File::create(path("stderr")).map_err(io)?,
                times: None,
            };
            let wait = p.wait;
            let exit_path = path("exit");
            Ok(Process {
                pid: p.pid,
                stdin: Box::pin(stdin),
                stdout: Box::pin(stdout),
                stderr: Box::pin(stderr),
                signal: p.signal,
                wait: async move {
                    let exit = wait.await;
                    let _ =
                        std::fs::write(exit_path, serde_json::to_string(&exit).unwrap_or_default());
                    exit
                }
                .boxed(),
                secrets: p.secrets,
            })
        }
        .boxed()
    }
}

struct TeeRead {
    inner: plexus_harness::BoxAsyncRead,
    log: File,
    times: Option<(File, Instant)>,
}

impl AsyncRead for TeeRead {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let r = this.inner.as_mut().poll_read(cx, buf);
        if let Poll::Ready(Ok(n)) = &r {
            let _ = this.log.write_all(&buf[..*n]);
            if let Some((times, t0)) = &mut this.times {
                let at = t0.elapsed().as_secs_f64();
                for _ in buf[..*n].iter().filter(|b| **b == b'\n') {
                    let _ = writeln!(times, "{at:.3}");
                }
            }
        }
        r
    }
}

struct TeeWrite {
    inner: plexus_harness::BoxAsyncWrite,
    log: File,
}

impl AsyncWrite for TeeWrite {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let r = this.inner.as_mut().poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &r {
            let _ = this.log.write_all(&buf[..*n]);
        }
        r
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().inner.as_mut().poll_flush(cx)
    }
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().inner.as_mut().poll_close(cx)
    }
}
