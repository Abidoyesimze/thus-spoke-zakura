use super::*;

#[test]
fn builds_docker_logs_arguments() {
    assert_eq!(
        logs_args("ths-default-app", false, None),
        ["logs", "ths-default-app"]
    );
    assert_eq!(
        logs_args("ths-default-zakura", true, None),
        ["logs", "--follow", "ths-default-zakura"]
    );
    assert_eq!(
        logs_args("ths-default-app", false, Some(200)),
        ["logs", "--tail", "200", "ths-default-app"]
    );
    assert_eq!(
        logs_args("ths-default-lightwalletd", true, Some(200)),
        [
            "logs",
            "--follow",
            "--tail",
            "200",
            "ths-default-lightwalletd"
        ]
    );
}

#[test]
fn copies_only_the_requested_leading_lines() {
    let mut out = Vec::new();
    let mut input = std::io::Cursor::new("one\ntwo\nthree\n");
    assert_eq!(copy_lines(&mut input, &mut out, 2).unwrap(), 2);
    assert_eq!(out, b"one\ntwo\n");
    assert_eq!(input.position(), 8, "read past the requested lines");

    let mut out = Vec::new();
    let input = std::io::Cursor::new("one\ntwo");
    assert_eq!(copy_lines(input, &mut out, 5).unwrap(), 2);
    assert_eq!(out, b"one\ntwo");
}
