use super::*;
#[test]
fn run_timeout_after_allocation_skips_child_and_cleans_its_instance() {
    let (mut host, events) = RecordingHost::new();
    host.allocate_delay = Duration::from_millis(80);
    let mut context = LifecycleContext::new(LifecyclePolicy::default());
    context.overall_deadline = Some(Deadline::after(Duration::from_millis(30)));
    let (_sender, shutdown) = Shutdown::channel();
    let runtime = runtime_for_tests();
    let error = runtime
        .run_with_policy(
            &name("alpha"),
            &[OsString::from("true")],
            &runtime.root,
            &host,
            &shutdown,
            &context,
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("timeout"), "{error:#}");
    let events = events.lock().unwrap();
    assert!(events.contains(&"allocate:alpha".to_owned()));
    assert!(events.contains(&"delete:alpha".to_owned()));
    assert!(
        !events
            .iter()
            .any(|event| event == "wait_ready" || event.starts_with("child:"))
    );
}

#[test]
fn settlement_does_not_accept_a_late_status_response() {
    let (_sender, shutdown) = Shutdown::channel();
    let started = Instant::now();
    let error = wait_until_settled(
        &|_| {
            std::thread::sleep(Duration::from_millis(80));
            Ok(RuntimeStatus {
                node_height: Some(1),
                lightwalletd_height: Some(1),
                wallet_scanned_height: Some(1),
                wallet_sync_state: "ready".into(),
                settled: true,
                settlement_blocker: None,
            })
        },
        started + Duration::from_millis(30),
        Duration::from_millis(750),
        &shutdown,
        false,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("timeout"), "{error:#}");
    assert!(started.elapsed() < Duration::from_millis(250));
}

#[test]
fn run_preserves_child_exit_code_and_cleans_after_settlement() {
    let (mut host, events) = RecordingHost::new();
    host.run_status = Some(RuntimeStatus {
        node_height: Some(105),
        lightwalletd_height: Some(105),
        wallet_scanned_height: Some(105),
        wallet_sync_state: "ready".into(),
        settled: true,
        settlement_blocker: None,
    });
    let mut context = LifecycleContext::new(LifecyclePolicy::default());
    context.overall_deadline = Some(Deadline::after(Duration::from_secs(1)));
    let (_sender, shutdown) = Shutdown::channel();
    let runtime = runtime_for_tests();
    let code = runtime
        .run_with_policy(
            &name("alpha"),
            &[OsString::from("true")],
            &runtime.root,
            &host,
            &shutdown,
            &context,
        )
        .unwrap();
    assert_eq!(code, 42);
    let events = events.lock().unwrap();
    assert!(events.contains(&"runtime_status".to_owned()));
    assert!(events.contains(&"child:alpha".to_owned()));
    assert_eq!(events.last().map(String::as_str), Some("delete:alpha"));
}

#[test]
fn failed_cleanup_reports_error_without_replacing_child_exit_code() {
    let (mut host, _events) = RecordingHost::new();
    host.run_status = Some(RuntimeStatus {
        node_height: Some(105),
        lightwalletd_height: Some(105),
        wallet_scanned_height: Some(105),
        wallet_sync_state: "ready".into(),
        settled: true,
        settlement_blocker: None,
    });
    host.final_cleanup_failure = Some("Docker cleanup failed".into());
    let mut context = LifecycleContext::new(LifecyclePolicy::default());
    context.overall_deadline = Some(Deadline::after(Duration::from_secs(1)));
    let (_sender, shutdown) = Shutdown::channel();
    let runtime = runtime_for_tests();
    let code = runtime
        .run_with_policy(
            &name("alpha"),
            &[OsString::from("true")],
            &runtime.root,
            &host,
            &shutdown,
            &context,
        )
        .unwrap();
    assert_eq!(code, 42);
}

#[test]
fn run_refuses_existing_instance_without_cleanup() {
    let (host, events) = RecordingHost::new();
    let mut context = LifecycleContext::new(LifecyclePolicy::default());
    context.overall_deadline = Some(Deadline::after(Duration::from_secs(1)));
    let (_sender, shutdown) = Shutdown::channel();
    let runtime = runtime_for_tests();
    fs::create_dir_all(runtime.instance_dir(&name("alpha"))).unwrap();
    let error = runtime
        .run_with_policy(
            &name("alpha"),
            &[OsString::from("true")],
            &runtime.root,
            &host,
            &shutdown,
            &context,
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("already exists"));
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn stop_does_not_delete_an_instance_locked_by_another_command() {
    let runtime = runtime_for_tests();
    let lock = runtime.lock_instance(&name("alpha")).unwrap();
    let error = runtime.stop(&name("alpha")).unwrap_err();
    assert!(format!("{error:#}").contains("already in use"));
    drop(lock);
}

#[test]
fn child_receives_project_directory_and_regtest_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("child-environment");
    let command = vec![
        OsString::from("sh"),
        OsString::from("-c"),
        OsString::from("{ pwd; env; } > \"$1\""),
        OsString::from("sh"),
        output.as_os_str().to_owned(),
    ];
    let (_sender, shutdown) = Shutdown::channel();
    let outcome = run_child(
        &command,
        dir.path(),
        &name("alpha"),
        &endpoints_for(&host_ports(0).unwrap()),
        &shutdown,
        Duration::from_millis(50),
    )
    .unwrap();
    assert_eq!(outcome, ChildOutcome::Exited(0));
    let output = fs::read_to_string(output).unwrap();
    assert_eq!(
        output.lines().next(),
        fs::canonicalize(dir.path()).unwrap().to_str()
    );
    for expected in [
        "THS_INSTANCE=alpha",
        "THS_NETWORK=regtest",
        "THS_API_URL=http://127.0.0.1:32805",
        "THS_ZAKURA_RPC_URL=http://127.0.0.1:18232",
        "THS_LIGHTWALLETD_URL=http://127.0.0.1:9067",
        "THS_LIGHTWALLETD_TLS=false",
        "THS_P2P_ADDR=127.0.0.1:18233",
    ] {
        assert!(
            output.lines().any(|line| line == expected),
            "missing {expected}"
        );
    }
    assert!(!output.lines().any(|line| line.starts_with("THS_SEED=")));
}

#[test]
fn child_exit_code_is_preserved() {
    let command = [
        OsString::from("sh"),
        OsString::from("-c"),
        OsString::from("exit 7"),
    ];
    let (_sender, shutdown) = Shutdown::channel();
    let outcome = run_child(
        &command,
        Path::new("."),
        &name("alpha"),
        &endpoints_for(&host_ports(0).unwrap()),
        &shutdown,
        Duration::from_millis(50),
    )
    .unwrap();
    assert_eq!(outcome, ChildOutcome::Exited(7));
}

#[cfg(unix)]
#[test]
fn interrupted_child_terminates_its_background_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("descendant.pid");
    let command = vec![
        OsString::from("sh"),
        OsString::from("-c"),
        OsString::from("sleep 30 & echo $! > \"$1\"; wait"),
        OsString::from("sh"),
        pid_file.as_os_str().to_owned(),
    ];
    let (sender, shutdown) = Shutdown::channel();
    let signal_file = pid_file.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !signal_file.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        sender.send(Ended::Interrupted).unwrap();
    });
    let outcome = run_child(
        &command,
        dir.path(),
        &name("alpha"),
        &endpoints_for(&host_ports(0).unwrap()),
        &shutdown,
        Duration::from_millis(50),
    )
    .unwrap();
    assert_eq!(outcome, ChildOutcome::Signaled);
    let pid = fs::read_to_string(pid_file).unwrap();
    let alive = Command::new("kill")
        .args(["-0", pid.trim()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    assert!(!alive, "background descendant {} survived", pid.trim());
}

#[test]
fn stop_routes_through_a_running_run() {
    let runtime = runtime_for_tests();
    let root = runtime.root.clone();
    let received_signal = root.join("child-signal");
    let script = format!(
        "trap 'echo INT > {0}; exit 0' INT; trap 'echo TERM > {0}; exit 0' TERM; while :; do sleep 0.05; done",
        received_signal.display()
    );
    let (handoff, received) = mpsc::channel();
    let owner = thread::spawn(move || {
        let (mut host, events) = RecordingHost::new();
        host.real_child = true;
        host.run_status = Some(RuntimeStatus {
            node_height: Some(105),
            lightwalletd_height: Some(105),
            wallet_scanned_height: Some(105),
            wallet_sync_state: "ready".into(),
            settled: true,
            settlement_blocker: None,
        });
        handoff.send(events.clone()).unwrap();
        let mut context = LifecycleContext::new(LifecyclePolicy::default());
        context.overall_deadline = Some(Deadline::after(Duration::from_secs(30)));
        let (_sender, shutdown) = Shutdown::channel();
        let runtime = Runtime { root };
        let code = runtime
            .run_with_policy(
                &name("alpha"),
                &[
                    OsString::from("sh"),
                    OsString::from("-c"),
                    OsString::from(script),
                ],
                &runtime.root,
                &host,
                &shutdown,
                &context,
            )
            .map_err(|error| format!("{error:#}"));
        (code, events.lock().unwrap().clone())
    });
    let events = received.recv().unwrap();
    startup::wait_for_event(&events, "child:alpha");
    let started = Instant::now();

    let report = startup::stop_after_owner_with(
        &runtime,
        &events,
        "delete:alpha",
        &RecordingDocker::new("", "", ""),
    )
    .unwrap();

    assert!(report.verified());
    assert!(started.elapsed() < Duration::from_secs(10));
    let (code, events) = owner.join().unwrap();
    assert_eq!(code, Ok(1));
    assert_eq!(events.last().map(String::as_str), Some("delete:alpha"));
    assert!(startup::session_files(&runtime).is_empty());
    assert_eq!(
        fs::read_to_string(&received_signal).unwrap().trim(),
        "INT",
        "stop must reach the child the way Ctrl+C does"
    );
}
