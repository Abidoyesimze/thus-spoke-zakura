#[test]
fn help_and_invalid_arguments_return_typed_errors() {
    for (argv, expected) in [
        (vec!["ths", "--help"], clap::error::ErrorKind::DisplayHelp),
        (
            vec!["ths", "--version"],
            clap::error::ErrorKind::DisplayVersion,
        ),
        (
            vec!["ths", "--not-a-ths-option"],
            clap::error::ErrorKind::UnknownArgument,
        ),
    ] {
        let error = thus_spoke_zakura::run_from(argv)
            .expect_err("the runner must return argument processing to its caller")
            .downcast::<clap::Error>()
            .expect("preserve the Clap error type");
        assert_eq!(error.kind(), expected);
    }
}

#[test]
fn binary_preserves_help_and_parse_error_exits() {
    let binary = env!("CARGO_BIN_EXE_ths");
    let help = std::process::Command::new(binary)
        .arg("--help")
        .output()
        .unwrap();
    assert_eq!(help.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&help.stdout).contains("Usage:"));
    assert!(help.stderr.is_empty());
    let invalid = std::process::Command::new(binary)
        .arg("--not-a-ths-option")
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(2));
    assert!(invalid.stdout.is_empty());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("unexpected argument"));
}

#[test]
fn json_run_reaches_dispatch_before_runtime_discovery() {
    let error = thus_spoke_zakura::run_from(["ths", "--json", "run", "--", "echo", "ready"])
        .expect_err("run owns stdout");
    assert!(
        error
            .to_string()
            .contains("--json cannot be used with `run`"),
        "{error:#}"
    );
    assert!(error.downcast_ref::<clap::Error>().is_none());
}
