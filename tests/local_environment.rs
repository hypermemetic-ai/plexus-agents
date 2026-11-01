//! AGENTS-31: `LocalEnvironment` gives a process only what its plan names.

use futures::io::AsyncReadExt;
use plexus_agents::LocalEnvironment;
use plexus_harness::{
    EnvName, EnvValue, Environment, LaunchPlan, Need, PlanFile, SecretRef, Signal, SpawnError,
    VersionPin,
};
use std::collections::BTreeMap;
use std::time::Duration;

fn plan(argv: &[&str]) -> LaunchPlan {
    LaunchPlan {
        argv: argv.iter().map(|s| s.to_string()).collect(),
        env: BTreeMap::new(),
        cwd: None,
        files: vec![],
        needs: vec![],
    }
}

async fn output(p: LaunchPlan) -> String {
    let mut process = LocalEnvironment.spawn(p).await.unwrap();
    let mut out = String::new();
    process.stdout.read_to_string(&mut out).await.unwrap();
    process.wait.await;
    out
}

fn names(env_output: &str) -> Vec<&str> {
    let mut v: Vec<&str> = env_output
        .lines()
        .filter_map(|l| l.split('=').next())
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn a_process_env_has_only_what_the_plan_names() {
    // SAFETY: only this test touches this name, and it sets it before spawning.
    unsafe { std::env::set_var("AGENTS31_SECRET_ON_HOST", "must-not-leak") };
    let mut p = plan(&["/usr/bin/env"]);
    p.env
        .insert(EnvName::new("NAMED"), EnvValue::literal("yes"));
    p.env.insert(
        EnvName::new("FROM_REF"),
        EnvValue::secret(SecretRef::env("AGENTS31_SECRET_ON_HOST")),
    );
    let out = output(p).await;
    println!("{out}");
    assert_eq!(names(&out), vec!["FROM_REF", "NAMED"]);
    assert!(out.contains("NAMED=yes"));
    assert!(
        out.contains("FROM_REF=must-not-leak"),
        "a named ref is resolved"
    );
}

#[tokio::test]
async fn an_empty_plan_means_an_empty_env() {
    assert_eq!(output(plan(&["/usr/bin/env"])).await, "");
}

#[tokio::test]
async fn a_missing_secret_fails_typed_and_spawns_nothing() {
    let mut p = plan(&["/usr/bin/true"]);
    p.env.insert(
        EnvName::new("X"),
        EnvValue::secret(SecretRef::env("AGENTS31_NOT_SET_ANYWHERE")),
    );
    let e = LocalEnvironment.spawn(p).await.unwrap_err();
    assert!(matches!(e, SpawnError::Secret(_)), "{e}");
    assert!(!e.to_string().contains("must-not-leak"));
}

#[tokio::test]
async fn a_missing_program_is_an_unmet_need() {
    let mut p = plan(&["/usr/bin/true"]);
    p.needs.push(Need::Program {
        name: "agents31-no-such-program".into(),
        version: None,
    });
    let e = LocalEnvironment.spawn(p).await.unwrap_err();
    assert!(matches!(e, SpawnError::NeedUnmet { .. }), "{e}");
}

#[tokio::test]
async fn a_version_outside_the_pin_fails_clearly() {
    let dir = std::env::temp_dir().join(format!("agents31-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cli = dir.join("cli");
    std::fs::write(&cli, "#!/bin/sh\necho '9.9.9 (Not Claude)'\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    let cli = cli.to_string_lossy().to_string();
    let mut p = plan(&[cli.as_str()]);
    let pin = VersionPin {
        version_args: vec!["--version".into()],
        supported: vec!["2.1.316".into()],
    };
    p.needs.push(Need::Program {
        name: cli.clone(),
        version: Some(pin),
    });
    let e = LocalEnvironment.spawn(p).await.unwrap_err();
    assert!(
        matches!(&e, SpawnError::UnsupportedVersion { found, .. } if found == "9.9.9"),
        "{e}"
    );
    assert!(e.to_string().contains("supported: 2.1.316"), "{e}");
}

#[tokio::test]
async fn files_land_in_cwd_and_cant_escape_it() {
    let dir = std::env::temp_dir().join(format!("agents31-files-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut p = plan(&["/bin/cat", "sub/f"]);
    p.cwd = Some(dir.to_string_lossy().to_string());
    p.files.push(PlanFile {
        path: "sub/f".into(),
        contents: "hello".into(),
    });
    assert_eq!(output(p.clone()).await, "hello");
    p.files[0].path = "../escape".into();
    let e = LocalEnvironment.spawn(p).await.unwrap_err();
    assert!(matches!(e, SpawnError::File { .. }), "{e}");
}

#[tokio::test]
async fn kill_tree_reaches_a_child_in_its_own_session() {
    let dir = std::env::temp_dir().join(format!("agents31-kill-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ticks = dir.join("ticks");
    let _ = std::fs::remove_file(&ticks);
    // A tool's shape from AGENTS-3: a child with its own session, ticking.
    let script = format!(
        "import os,time,subprocess,sys\n\
         tick='while :; do echo x >> {}; sleep 0.1; done'\n\
         subprocess.Popen(['/bin/sh','-c',tick],start_new_session=True)\n\
         print('up',flush=True)\n\
         time.sleep(60)\n",
        ticks.display()
    );
    let mut p = plan(&["/usr/bin/python3", "-I", "-c", script.as_str()]);
    p.cwd = Some(dir.to_string_lossy().to_string());
    let mut process = LocalEnvironment.spawn(p).await.unwrap();
    let mut buf = [0u8; 8];
    let _ = process.stdout.read(&mut buf).await.unwrap();
    let count = || {
        std::fs::read_to_string(&ticks)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    };
    for _ in 0..50 {
        if count() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(count() >= 2, "the tool never ticked");
    process.signal.signal(Signal::KillTree).await.unwrap();
    let exit = process.wait.await;
    assert_eq!(exit.signal, Some(9));
    let before = count();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(count(), before, "the tool outlived the kill");
}
