use super::*;

#[test]
fn validates_instance_names() {
    for valid in ["default", "project-2", "a"] {
        assert!(valid.parse::<InstanceName>().is_ok());
    }
    for invalid in ["", "UPPER", "with space", "-start", "end-"] {
        assert!(invalid.parse::<InstanceName>().is_err());
    }
}

#[test]
fn shutdown_reports_interrupt_from_channel() {
    let (sender, receiver) = std::sync::mpsc::channel();
    let shutdown = Shutdown::from_receiver(receiver);
    assert!(!shutdown.try_interrupted());
    sender.send(()).unwrap();
    assert!(shutdown.try_interrupted());
    assert!(shutdown.try_interrupted());
    shutdown.wait().unwrap();
}

#[test]
fn default_offset_uses_stable_loopback_ports() {
    let ports = host_ports(0).unwrap();
    assert_eq!(ports.dashboard, 32805);
    assert_eq!(ports.rpc, 18232);
    assert_eq!(ports.p2p, 18233);
    assert_eq!(ports.lightwalletd, 9067);
    assert_eq!(loopback_publish(ports.rpc, 18232), "127.0.0.1:18232:18232");
    assert_eq!(
        loopback_publish(ports.dashboard, 8080),
        "127.0.0.1:32805:8080"
    );
}

#[test]
fn port_offset_shifts_all_four_hosts_by_the_same_stride() {
    let base = host_ports(0).unwrap();
    let shifted = host_ports(10).unwrap();
    assert_eq!(shifted.dashboard, base.dashboard + 10);
    assert_eq!(shifted.rpc, base.rpc + 10);
    assert_eq!(shifted.p2p, base.p2p + 10);
    assert_eq!(shifted.lightwalletd, base.lightwalletd + 10);
}

#[test]
fn port_offset_rejects_values_that_are_not_multiples_of_ten() {
    let err = host_ports(1).unwrap_err();
    assert!(err.to_string().contains('1'));
}

#[test]
fn port_offset_accepts_the_largest_value_that_keeps_every_port_in_range() {
    let ports = host_ports(32730).unwrap();
    assert_eq!(ports.dashboard, 65535);
}

#[test]
fn port_offset_rejects_values_that_overflow_a_host_port() {
    let err = host_ports(32740).unwrap_err();
    assert!(err.to_string().contains("overflow"));
}

#[test]
fn invalid_port_offset_is_rejected_before_any_deletion_or_allocation() {
    let (mut host, events) = RecordingHost::new();
    host.initial_delete_error = true;
    let (_sender, receiver) = std::sync::mpsc::channel();
    let shutdown = Shutdown::from_receiver(receiver);

    let error = runtime_for_tests()
        .start_with(&name("alpha"), false, false, 32740, &host, &shutdown)
        .unwrap_err();

    assert!(error.to_string().contains("overflow"));
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn reports_the_conflicting_loopback_port() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let err = require_free_loopback(port).unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains(&port.to_string()),
        "error should name port {port}, got {message}"
    );
}

#[test]
fn endpoints_json_includes_regtest_and_plaintext_lightwalletd() {
    let json = serde_json::to_value(endpoints_for(&host_ports(0).unwrap())).unwrap();
    assert_eq!(json["dashboard"], "http://127.0.0.1:32805");
    assert_eq!(json["rpc"], "http://127.0.0.1:18232");
    assert_eq!(json["lightwalletd"], "http://127.0.0.1:9067");
    assert_eq!(json["p2p"], "127.0.0.1:18233");
    assert_eq!(json["network"], "regtest");
    assert_eq!(json["tls"], false);
}

#[test]
fn endpoints_json_without_network_fields_still_deserializes() {
    let parsed: Endpoints = serde_json::from_str(
        r#"{"dashboard":"http://127.0.0.1:1","rpc":"http://127.0.0.1:2","lightwalletd":"http://127.0.0.1:3","p2p":"127.0.0.1:4"}"#,
    )
    .unwrap();
    assert_eq!(parsed.network, "regtest");
    assert!(!parsed.tls);
}
