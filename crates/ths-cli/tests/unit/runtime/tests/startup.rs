use super::*;

#[test]
fn partial_startup_cleanup_removes_only_owned_resources() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    let metadata = runtime.instance_dir(&name("alpha"));
    fs::create_dir_all(&metadata).unwrap();
    fs::write(metadata.join("instance.json"), "partial startup").unwrap();
    let mut docker = RecordingDocker::new("", "ths-alpha-chain\nths-alpha-wallet", "ths-alpha");
    docker.inspect(
        "volume",
        "ths-alpha-chain",
        r#"[{"Name":"ths-alpha-chain","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
    );
    docker.inspect(
        "volume",
        "ths-alpha-wallet",
        r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"other"}}]"#,
    );
    docker.inspect(
        "network",
        "ths-alpha",
        r#"[{"Id":"owned-network","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
    );

    let error = runtime
        .delete_instance_resources_with_mode(&name("alpha"), &docker, true)
        .unwrap_err();

    assert!(format!("{error:#}").contains("not owned"), "{error:#}");
    assert_eq!(
        *docker.runs.lock().unwrap(),
        ["volume rm ths-alpha-chain", "network rm owned-network"]
    );
    assert_eq!(
        fs::read_to_string(metadata.join("instance.json")).unwrap(),
        "partial startup"
    );
}

#[test]
fn shutdown_collision_removes_owned_resources_and_preserves_foreign_container() {
    let mut docker = RecordingDocker::new(
        "ths-alpha-app\nths-alpha-init",
        "ths-alpha-wallet",
        "ths-alpha",
    );
    docker.inspect(
        "container",
        "ths-alpha-app",
        r#"[{"Id":"owned-app","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#,
    );
    docker.inspect(
        "container",
        "ths-alpha-init",
        r#"[{"Id":"foreign-init","Config":{"Labels":{}}}]"#,
    );
    docker.inspect(
        "volume",
        "ths-alpha-wallet",
        r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
    );
    docker.inspect(
        "network",
        "ths-alpha",
        r#"[{"Id":"owned-network","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
    );
    let (mut host, events) = RecordingHost::new();
    host.shutdown_collision = Some(docker);
    let (_sender, shutdown) = Shutdown::channel();

    let error = runtime_for_tests()
        .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
        .unwrap_err();

    assert!(format!("{error:#}").contains("ths-alpha-init is not owned"));
    assert!(
        events
            .lock()
            .unwrap()
            .contains(&"delete_partial:alpha".into())
    );
    assert_eq!(
        *host
            .shutdown_collision
            .as_ref()
            .unwrap()
            .runs
            .lock()
            .unwrap(),
        [
            "rm -f owned-app",
            "volume rm ths-alpha-wallet",
            "network rm owned-network",
        ]
    );
}

#[test]
fn browser_open_failure_deletes_and_does_not_wait() {
    let (mut host, events) = RecordingHost::new();
    host.open_url_result = Err("opening http://127.0.0.1:1".into());
    let (_sender, shutdown) = Shutdown::channel();
    let err = runtime_for_tests()
        .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
        .unwrap_err();
    assert!(err.to_string().contains("opening"));
    let events = events.lock().unwrap().clone();
    assert!(events.iter().any(|e| e.starts_with("open_url:")));
    assert!(events.iter().any(|e| e == "delete:alpha"));
    assert!(!events.iter().any(|e| e.starts_with("wait_for_shutdown")));
}

#[test]
fn no_open_skips_browser_and_waits_for_shutdown() {
    let (host, events) = RecordingHost::new();
    let (sender, shutdown) = Shutdown::channel();
    let watched = std::sync::Arc::clone(&events);
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_secs(2) {
            let waiting = watched
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.starts_with("wait_for_shutdown"));
            if waiting {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        sender.send(Ended::Interrupted).unwrap();
    });
    runtime_for_tests()
        .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
        .unwrap();
    let events = events.lock().unwrap().clone();
    assert!(!events.iter().any(|e| e.starts_with("open_url:")));
    assert!(events.iter().any(|e| e.starts_with("wait_for_shutdown")));
    assert!(events.iter().any(|e| e == "delete:alpha"));
}

#[test]
fn startup_and_cleanup_failures_are_both_reported_once() {
    let (mut host, events) = RecordingHost::new();
    host.wait_ready_result = Err("dashboard readiness timed out".into());
    host.final_cleanup_failure = Some("Docker inspection failed".into());
    let (_sender, shutdown) = Shutdown::channel();
    let error = runtime_for_tests()
        .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("dashboard readiness timed out"));
    assert!(text.contains("Docker inspection failed"));
    assert_eq!(
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| *event == "final_cleanup:alpha")
            .count(),
        1
    );
}

#[test]
fn interrupt_before_ready_deletes_the_started_instance() {
    let (mut host, events) = RecordingHost::new();
    host.interrupt_before_ready = true;
    let (_sender, shutdown) = Shutdown::channel();
    let err = runtime_for_tests()
        .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
        .unwrap_err();
    assert!(err.to_string().contains("interrupted"));
    let events = events.lock().unwrap().clone();
    assert!(events.iter().any(|e| e == "allocate:alpha"));
    assert!(events.iter().any(|e| e == "delete:alpha"));
    assert!(!events.iter().any(|e| e.starts_with("wait_for_shutdown")));
}

#[test]
fn interrupt_during_allocate_deletes_only_the_named_instance() {
    let (host, events) = RecordingHost::new();
    let (sender, shutdown) = Shutdown::channel();
    sender.send(Ended::Interrupted).unwrap();
    let err = runtime_for_tests()
        .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
        .unwrap_err();
    assert!(err.to_string().contains("interrupted"));
    let events = events.lock().unwrap().clone();
    assert!(events.iter().any(|e| e == "delete:alpha"));
    assert!(!events.iter().any(|e| e.contains("beta")));
}

#[test]
fn shutdown_check_bails_when_latched() {
    let (sender, shutdown) = Shutdown::channel();
    shutdown.check().unwrap();
    sender.send(Ended::Interrupted).unwrap();
    let err = shutdown.check().unwrap_err();
    assert!(err.to_string().contains("interrupted"));
    let err = shutdown.check().unwrap_err();
    assert!(err.to_string().contains("interrupted"));
}

#[test]
fn shutdown_wait_timeout_wakes_on_signal() {
    let (sender, shutdown) = Shutdown::channel();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        sender.send(Ended::Interrupted).unwrap();
    });
    let started = Instant::now();
    let err = shutdown.wait_timeout(Duration::from_secs(2)).unwrap_err();
    assert!(err.to_string().contains("interrupted"));
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[test]
fn shutdown_wait_timeout_returns_on_idle() {
    let (_sender, shutdown) = Shutdown::channel();
    shutdown.wait_timeout(Duration::from_millis(20)).unwrap();
}

#[test]
fn shutdown_wait_returns_after_signal() {
    let (sender, shutdown) = Shutdown::channel();
    sender.send(Ended::Interrupted).unwrap();
    assert_eq!(shutdown.wait().unwrap(), Ended::Interrupted);
}

#[test]
fn wait_ready_aborts_when_shutdown_is_signaled() {
    let (sender, shutdown) = Shutdown::channel();
    sender.send(Ended::Interrupted).unwrap();
    let err = readiness::wait_ready(
        "http://127.0.0.1:1",
        "missing-app",
        Deadline::after(Duration::from_secs(5)),
        &shutdown,
        &LifecyclePolicy::default(),
        &IdleDocker,
    )
    .unwrap_err();
    assert!(err.to_string().contains("interrupted"));
}

#[test]
fn wait_for_zakura_tip_aborts_when_shutdown_is_signaled() {
    let (sender, shutdown) = Shutdown::channel();
    sender.send(Ended::Interrupted).unwrap();
    let err = readiness::wait_for_zakura_tip(
        "http://127.0.0.1:1",
        "missing-zakura",
        Deadline::after(Duration::from_secs(5)),
        &shutdown,
        &LifecyclePolicy::default(),
        &IdleDocker,
    )
    .unwrap_err();
    assert!(err.to_string().contains("interrupted"));
}

#[test]
fn deletion_elsewhere_ends_start_without_deleting_again() {
    let (mut host, events) = RecordingHost::new();
    host.deleted_elsewhere = true;
    let (_sender, shutdown) = Shutdown::channel();
    runtime_for_tests()
        .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
        .unwrap();
    let events = events.lock().unwrap().clone();
    let waited = events
        .iter()
        .position(|e| e == "wait_for_shutdown:app-1")
        .expect("waited on the app container");
    assert!(
        !events[waited..].iter().any(|e| e.starts_with("delete:")),
        "deleted again after another command deleted it: {events:?}"
    );
}
#[test]
fn shutdown_wait_returns_when_the_environment_is_gone() {
    let (sender, shutdown) = Shutdown::channel();
    sender.send(Ended::DeletedElsewhere).unwrap();
    assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
    assert!(!shutdown.try_interrupted());
}
#[test]
fn shutdown_wait_keeps_a_deletion_notice_that_arrived_first() {
    let (sender, shutdown) = Shutdown::channel();
    sender.send(Ended::DeletedElsewhere).unwrap();
    sender.send(Ended::Interrupted).unwrap();
    assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
    assert!(shutdown.try_interrupted());
}
