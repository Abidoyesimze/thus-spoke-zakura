use super::*;

#[test]
fn foreign_volume_collision_preserves_all_resources() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    let mut docker = RecordingDocker::new("ths-alpha-app", "ths-alpha-wallet", "");
    docker.inspect(
        "container",
        "ths-alpha-app",
        r#"[{"Id":"owned-container","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#,
    );
    docker.inspect(
        "volume",
        "ths-alpha-wallet",
        r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"other"}}]"#,
    );

    let error = runtime
        .delete_instance_resources_with(&name("alpha"), &docker)
        .unwrap_err();

    assert!(format!("{error:#}").contains("not owned"), "{error:#}");
    assert!(docker.runs.lock().unwrap().is_empty());
}

#[test]
fn foreign_container_collision_is_not_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    let mut docker = RecordingDocker::new("ths-alpha-app", "", "");
    docker.inspect(
        "container",
        "ths-alpha-app",
        r#"[{"Id":"foreign-container","Config":{"Labels":{"com.zakura.ths.instance":"other"}}}]"#,
    );

    let error = runtime
        .delete_instance_resources_with(&name("alpha"), &docker)
        .unwrap_err();

    assert!(format!("{error:#}").contains("not owned"), "{error:#}");
    assert!(docker.runs.lock().unwrap().is_empty());
}

#[test]
fn owned_resources_still_clean_up() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    let mut docker = RecordingDocker::new("ths-alpha-app", "ths-alpha-wallet", "ths-alpha");
    docker.inspect(
        "container",
        "ths-alpha-app",
        r#"[{"Id":"owned-container","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#,
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

    runtime
        .delete_instance_resources_with(&name("alpha"), &docker)
        .unwrap();

    assert_eq!(
        *docker.runs.lock().unwrap(),
        [
            "rm -f owned-container",
            "volume rm ths-alpha-wallet",
            "network rm owned-network",
        ]
    );
}

#[test]
fn unlabeled_network_collision_preserves_data_and_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    let metadata = runtime.instance_dir(&name("alpha"));
    fs::create_dir_all(&metadata).unwrap();
    fs::write(metadata.join("instance.json"), "important data").unwrap();
    let mut docker = RecordingDocker::new("ths-alpha-app", "ths-alpha-wallet", "ths-alpha");
    docker.inspect(
        "container",
        "ths-alpha-app",
        r#"[{"Id":"owned-container","Config":{"Labels":{"com.zakura.ths.instance":"alpha"}}}]"#,
    );
    docker.inspect(
        "volume",
        "ths-alpha-wallet",
        r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"alpha"}}]"#,
    );
    docker.inspect(
        "network",
        "ths-alpha",
        r#"[{"Id":"old-network","Labels":{}}]"#,
    );

    let error = runtime
        .delete_instance_resources_with(&name("alpha"), &docker)
        .unwrap_err();

    assert!(format!("{error:#}").contains("not owned"), "{error:#}");
    assert!(docker.runs.lock().unwrap().is_empty());
    assert_eq!(
        fs::read_to_string(metadata.join("instance.json")).unwrap(),
        "important data"
    );
}

#[test]
fn new_networks_are_labeled_for_owned_cleanup() {
    struct NewNetwork(AtomicBool);

    impl DockerResourceCommands for NewNetwork {
        fn output(&self, args: &[&str]) -> Result<String> {
            match args {
                ["network", "ls", "--format", "{{.Name}}"] => {
                    Ok(if self.0.load(Ordering::SeqCst) {
                        "ths-alpha".into()
                    } else {
                        String::new()
                    })
                }
                ["network", "inspect", "ths-alpha"] => Ok(
                    r#"[{"Id":"owned-network","Labels":{"com.zakura.ths.instance":"alpha"}}]"#
                        .into(),
                ),
                _ => bail!("unexpected Docker read: {args:?}"),
            }
        }

        fn run(&self, args: &[&str]) -> Result<()> {
            assert_eq!(
                args,
                [
                    "network",
                    "create",
                    "--label",
                    "com.zakura.ths.instance=alpha",
                    "ths-alpha"
                ]
            );
            self.0.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    let docker = NewNetwork(AtomicBool::new(false));

    let dir = tempfile::tempdir().unwrap();
    let mut journal =
        recovery::RecoveryJournal::create(dir.path().join(recovery::RECOVERY_FILE), &name("alpha"))
            .unwrap();
    ensure_network_with(
        "ths-alpha",
        &name("alpha"),
        &docker,
        &mut journal,
        Deadline::after(Duration::from_secs(1)),
    )
    .unwrap();
    assert!(docker.0.load(Ordering::SeqCst));
}

#[test]
fn duplicate_network_names_are_rejected() {
    let docker = RecordingDocker::new("", "", "ths-alpha\nths-alpha");

    let dir = tempfile::tempdir().unwrap();
    let mut journal =
        recovery::RecoveryJournal::create(dir.path().join(recovery::RECOVERY_FILE), &name("alpha"))
            .unwrap();
    let error = ensure_network_with(
        "ths-alpha",
        &name("alpha"),
        &docker,
        &mut journal,
        Deadline::after(Duration::from_secs(1)),
    )
    .unwrap_err();

    assert!(
        error.to_string().contains("multiple network resources"),
        "{error:#}"
    );
    assert!(docker.runs.lock().unwrap().is_empty());
}

#[test]
fn volume_create_does_not_accept_a_racing_foreign_volume() {
    struct RacingVolume(AtomicBool);

    impl DockerResourceCommands for RacingVolume {
        fn output(&self, args: &[&str]) -> Result<String> {
            match args {
                ["volume", "ls", "--format", "{{.Name}}"] => Ok(if self.0.load(Ordering::SeqCst) {
                    "ths-alpha-wallet".into()
                } else {
                    String::new()
                }),
                ["volume", "inspect", "ths-alpha-wallet"] => Ok(
                    r#"[{"Name":"ths-alpha-wallet","Labels":{"com.zakura.ths.instance":"other"}}]"#
                        .into(),
                ),
                _ => bail!("unexpected Docker read: {args:?}"),
            }
        }

        fn run(&self, args: &[&str]) -> Result<()> {
            assert_eq!(
                args,
                [
                    "volume",
                    "create",
                    "--label",
                    "com.zakura.ths.instance=alpha",
                    "ths-alpha-wallet"
                ]
            );
            self.0.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let mut journal =
        recovery::RecoveryJournal::create(dir.path().join(recovery::RECOVERY_FILE), &name("alpha"))
            .unwrap();
    let error = ensure_volume_with(
        "ths-alpha-wallet",
        &name("alpha"),
        &RacingVolume(AtomicBool::new(false)),
        &mut journal,
        Deadline::after(Duration::from_secs(1)),
    )
    .unwrap_err();

    assert!(error.to_string().contains("not owned"), "{error:#}");
}

#[test]
fn initial_cleanup_failure_does_not_retry_on_drop() {
    let (mut host, events) = RecordingHost::new();
    host.initial_delete_error = true;
    let (_sender, receiver) = std::sync::mpsc::channel();
    let shutdown = Shutdown::from_receiver(receiver);

    let error = runtime_for_tests()
        .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
        .unwrap_err();

    assert!(error.to_string().contains("resource collision"));
    assert_eq!(*events.lock().unwrap(), ["delete:alpha"]);
}

#[test]
fn readiness_failure_deletes_the_started_instance() {
    let (mut host, events) = RecordingHost::new();
    host.wait_ready_result = Err("dashboard did not become healthy".into());
    let (_sender, receiver) = std::sync::mpsc::channel();
    let shutdown = Shutdown::from_receiver(receiver);
    let err = runtime_for_tests()
        .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
        .unwrap_err();
    assert!(err.to_string().contains("dashboard did not become healthy"));
    let events = events.lock().unwrap().clone();
    let allocate_pos = events
        .iter()
        .position(|e| e == "allocate:alpha")
        .expect("allocate:alpha");
    assert!(
        events[allocate_pos + 1..]
            .iter()
            .any(|e| e == "delete:alpha"),
        "expected delete:alpha after allocate:alpha, got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| e.starts_with("delete:") && !e.ends_with("alpha"))
    );
    assert!(!events.iter().any(|e| e == "wait_for_shutdown"));
}
