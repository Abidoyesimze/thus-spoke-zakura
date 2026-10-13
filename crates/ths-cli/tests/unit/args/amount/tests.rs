use super::*;

#[test]
fn parses_exact_zec_amounts() {
    assert_eq!("0.00000001".parse::<ZecAmount>().unwrap().zatoshi(), 1);
    assert_eq!("5".parse::<ZecAmount>().unwrap().zatoshi(), 500_000_000);
    for invalid in ["0", "5.00000001", "1.000000001", "-1", "1e2", ".5"] {
        assert!(invalid.parse::<ZecAmount>().is_err(), "accepted {invalid}");
    }
}
