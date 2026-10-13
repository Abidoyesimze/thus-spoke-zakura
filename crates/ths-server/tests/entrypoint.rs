#[tokio::test]
async fn help_and_invalid_arguments_return_typed_errors() {
    for (argv, expected) in [
        (
            vec!["ths-server", "--help"],
            clap::error::ErrorKind::DisplayHelp,
        ),
        (
            vec!["ths-server", "--version"],
            clap::error::ErrorKind::DisplayVersion,
        ),
        (
            vec!["ths-server", "--not-a-ths-option"],
            clap::error::ErrorKind::UnknownArgument,
        ),
    ] {
        let error = ths_server::run_from(argv)
            .await
            .expect_err("the runner must return argument processing to its caller")
            .downcast::<clap::Error>()
            .expect("preserve the Clap error type");
        assert_eq!(error.kind(), expected);
    }
}

#[test]
fn binary_preserves_help_and_parse_error_exits() {
    let binary = env!("CARGO_BIN_EXE_ths-server");
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
