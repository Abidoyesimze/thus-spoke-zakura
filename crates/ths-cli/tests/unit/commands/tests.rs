use super::*;
use clap::Parser;

#[test]
fn checks_for_updates_only_during_human_readable_startup() {
    let default = Cli::try_parse_from(["ths"]).unwrap();
    assert!(should_check_for_updates(&default));

    let start = Cli::try_parse_from(["ths", "start", "--no-open"]).unwrap();
    assert!(should_check_for_updates(&start));

    let json = Cli::try_parse_from(["ths", "--json"]).unwrap();
    assert!(!should_check_for_updates(&json));

    let status = Cli::try_parse_from(["ths", "status"]).unwrap();
    assert!(!should_check_for_updates(&status));
}
