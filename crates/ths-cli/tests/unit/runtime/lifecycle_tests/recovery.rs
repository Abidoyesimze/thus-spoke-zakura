use super::*;

#[test]
fn unidentified_create_stays_uncertain_for_either_snapshot() {
    let recorded = ResourceRef {
        kind: ResourceKind::Container,
        name: "ths-alpha-app".into(),
        identity: Some("original-id".into()),
    };
    assert!(recovery::identity_matches(&recorded, "original-id"));
    assert!(!recovery::identity_matches(&recorded, "replacement-id"));
    assert!(!recovery::identity_matches(
        &ResourceRef {
            identity: None,
            ..recorded.clone()
        },
        "original-id"
    ));
    assert!(matches!(
        recovery::reconcile_unidentified_create(true),
        MutationOutcome::Uncertain(_)
    ));
    assert!(matches!(
        recovery::reconcile_unidentified_create(false),
        MutationOutcome::Uncertain(_)
    ));
    let allocated = AllocatedInstance {
        endpoints: Endpoints {
            dashboard: "http://127.0.0.1:1".into(),
            rpc: "http://127.0.0.1:2".into(),
            lightwalletd: "http://127.0.0.1:3".into(),
            p2p: "127.0.0.1:4".into(),
            network: "regtest".into(),
            tls: false,
        },
        resources: vec![recorded],
        app_container_id: "original-id".into(),
        zakura_container_id: "zakura-id".into(),
    };
    assert_eq!(
        allocated.resources[0].identity.as_deref(),
        Some("original-id")
    );
    assert_eq!(allocated.zakura_container_id, "zakura-id");
}

#[test]
fn pending_mutation_survives_without_endpoint_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lifecycle-recovery.json");
    let mut journal = RecoveryJournal::create(path.clone(), &name("alpha")).unwrap();
    journal
        .begin(
            ResourceRef {
                kind: ResourceKind::Container,
                name: "ths-alpha-app".into(),
                identity: None,
            },
            Vec::new(),
            MutationOperation::Create,
        )
        .unwrap();
    let recovered = RecoveryJournal::load(path, &name("alpha"))
        .unwrap()
        .unwrap();
    assert!(matches!(
        recovered.record().mutations[0].outcome,
        MutationOutcome::Pending
    ));
    assert_eq!(
        recovered.record().mutations[0].resource.name,
        "ths-alpha-app"
    );
    assert!(!dir.path().join("instance.json").exists());
}

#[test]
fn failed_rewrite_keeps_the_previous_record_and_skips_the_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(recovery::RECOVERY_FILE);
    let mut journal = RecoveryJournal::create(path.clone(), &name("alpha")).unwrap();
    journal.fail_next_write();
    let docker = StatefulDocker::new();
    let error = ensure_volume_with(
        "ths-alpha-wallet",
        &name("alpha"),
        &docker,
        &mut journal,
        Deadline::after(Duration::from_secs(1)),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("could not be rewritten"),
        "{error:#}"
    );
    assert!(
        docker
            .calls()
            .iter()
            .all(|call| !(call.first().map(String::as_str) == Some("volume")
                && call.get(1).map(String::as_str) == Some("create")))
    );
    let recovered = RecoveryJournal::load(path.clone(), &name("alpha"))
        .unwrap()
        .unwrap();
    assert!(recovered.record().mutations.is_empty());

    let mut journal = RecoveryJournal::load(path.clone(), &name("alpha"))
        .unwrap()
        .unwrap();
    journal
        .begin(
            ResourceRef {
                kind: ResourceKind::Volume,
                name: "ths-alpha-wallet".into(),
                identity: None,
            },
            Vec::new(),
            MutationOperation::Create,
        )
        .unwrap();
    journal.fail_next_write();
    let error = journal
        .finish(
            0,
            MutationOutcome::Acknowledged,
            Some("ths-alpha-wallet".into()),
        )
        .unwrap_err();
    assert!(error.to_string().contains("could not be rewritten"));
    let recovered = RecoveryJournal::load(path, &name("alpha"))
        .unwrap()
        .unwrap();
    assert!(matches!(
        recovered.record().mutations[0].outcome,
        MutationOutcome::Pending
    ));
}

#[test]
fn uncertain_service_creates_record_every_mounted_dependency() {
    struct LostCreateReply<'a>(&'a StatefulDocker);

    impl DockerResourceCommands for LostCreateReply<'_> {
        fn output(&self, args: &[&str]) -> Result<String> {
            if args.first().copied() == Some("create") {
                anyhow::bail!("timed out waiting for Docker create reply");
            }
            self.0.output(args)
        }

        fn run(&self, args: &[&str]) -> Result<()> {
            self.0.run(args)
        }
    }

    for (service, mounted_volumes) in [
        ("zakura", vec!["ths-alpha-chain", "ths-alpha-config"]),
        ("lightwalletd", vec!["ths-alpha-lightwalletd"]),
        ("app", vec!["ths-alpha-wallet"]),
    ] {
        let (runtime, docker, _) = owned_cleanup_fixture();
        let instance = name("alpha");
        docker
            .resources
            .lock()
            .unwrap()
            .retain(|resource| resource.name != format!("ths-alpha-{service}"));
        let mut journal =
            RecoveryJournal::create(runtime.recovery_path(&instance), &instance).unwrap();
        let network = ResourceRef {
            kind: ResourceKind::Network,
            name: "ths-alpha".into(),
            identity: Some("net-id".into()),
        };
        for resource in
            std::iter::once(network.clone()).chain(mounted_volumes.iter().map(|target| {
                ResourceRef {
                    kind: ResourceKind::Volume,
                    name: (*target).into(),
                    identity: Some((*target).into()),
                }
            }))
        {
            let index = journal
                .begin(resource.clone(), Vec::new(), MutationOperation::Create)
                .unwrap();
            journal
                .finish(index, MutationOutcome::Acknowledged, resource.identity)
                .unwrap();
        }
        let deadline = Deadline::after(short_policy().startup_docker);
        let ports = host_ports(0).unwrap();
        let timeout_docker = LostCreateReply(&docker);
        let dependencies = vec![network];
        let result = match service {
            "zakura" => ensure_zakura(
                "ths-alpha",
                &instance,
                &ports,
                &timeout_docker,
                &mut journal,
                deadline,
                dependencies,
            ),
            "lightwalletd" => ensure_lightwalletd(
                "ths-alpha",
                &instance,
                &ports,
                &timeout_docker,
                &mut journal,
                deadline,
                dependencies,
            ),
            _ => ensure_app(
                "ths-alpha",
                &instance,
                &ports,
                &timeout_docker,
                &mut journal,
                deadline,
                dependencies,
            ),
        };
        assert!(result.is_err());
        let mutation = recovery::unresolved_create(
            journal.record(),
            &ResourceKind::Container,
            &format!("ths-alpha-{service}"),
        )
        .unwrap();
        for target in std::iter::once("ths-alpha").chain(mounted_volumes) {
            assert!(
                mutation
                    .dependencies
                    .iter()
                    .any(|resource| resource.name == target),
                "{service} create omitted dependency {target}: {:?}",
                mutation.dependencies
            );
        }
    }
}

#[test]
fn unresolved_create_keeps_its_dependencies() {
    let (runtime, docker, metadata) = owned_cleanup_fixture();
    let mut journal =
        RecoveryJournal::create(runtime.recovery_path(&name("alpha")), &name("alpha")).unwrap();
    journal
        .begin(
            ResourceRef {
                kind: ResourceKind::Container,
                name: "ths-alpha-app".into(),
                identity: None,
            },
            vec![
                ResourceRef {
                    kind: ResourceKind::Volume,
                    name: "ths-alpha-wallet".into(),
                    identity: Some("ths-alpha-wallet".into()),
                },
                ResourceRef {
                    kind: ResourceKind::Network,
                    name: "ths-alpha".into(),
                    identity: Some("net-id".into()),
                },
            ],
            MutationOperation::Create,
        )
        .unwrap();
    drop(journal);
    let report = runtime
        .cleanup_with(&name("alpha"), &docker, true, &cleanup_policy())
        .unwrap();
    assert!(matches!(report.outcome, CleanupOutcome::Uncertain));
    let removals = docker.removal_calls();
    assert!(
        removals.iter().all(|call| !call.iter().any(|arg| {
            arg == "ths-alpha-app"
                || arg == "app-id"
                || arg == "ths-alpha-wallet"
                || arg == "ths-alpha"
                || arg == "net-id"
        })),
        "{removals:?}"
    );
    assert!(
        removals
            .iter()
            .any(|call| call.iter().any(|arg| arg == "ths-alpha-chain"))
    );
    assert!(metadata.exists());
    assert!(runtime.recovery_path(&name("alpha")).exists());
}

#[test]
fn uncertain_start_preserves_the_creation_dependencies() {
    let (runtime, docker, _) = owned_cleanup_fixture();
    let instance = name("alpha");
    let mut journal = RecoveryJournal::create(runtime.recovery_path(&instance), &instance).unwrap();
    let dependencies = vec![
        ResourceRef {
            kind: ResourceKind::Network,
            name: "ths-alpha".into(),
            identity: Some("net-id".into()),
        },
        ResourceRef {
            kind: ResourceKind::Volume,
            name: "ths-alpha-wallet".into(),
            identity: Some("ths-alpha-wallet".into()),
        },
    ];
    let index = journal
        .begin(
            ResourceRef {
                kind: ResourceKind::Container,
                name: "ths-alpha-app".into(),
                identity: None,
            },
            dependencies,
            MutationOperation::Create,
        )
        .unwrap();
    journal
        .finish(index, MutationOutcome::Acknowledged, Some("app-id".into()))
        .unwrap();
    assert!(
        start_container(
            "app-id",
            "ths-alpha-app",
            &docker,
            &mut journal,
            Deadline::after(Duration::from_secs(1))
        )
        .is_err()
    );
    let mutation = journal.record().mutations.last().unwrap();
    assert_eq!(mutation.operation, MutationOperation::Start);
    let names: Vec<_> = mutation
        .dependencies
        .iter()
        .map(|resource| resource.name.as_str())
        .collect();
    assert_eq!(names, ["ths-alpha", "ths-alpha-wallet"]);
    drop(journal);
    let report = runtime
        .cleanup_with(&instance, &docker, true, &cleanup_policy())
        .unwrap();
    assert!(!report.verified());
    assert!(docker.contains_id("app-id"));
    assert!(docker.contains_id("net-id"));
    assert!(docker.contains_id("ths-alpha-wallet"));
}
