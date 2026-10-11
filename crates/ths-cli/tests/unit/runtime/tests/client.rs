use super::*;

#[test]
fn responses_decode_or_report_their_status_and_body() {
    let (url, _requests) = serve(vec![
        ("200 OK", r#"{"blocks":1,"hashes":["tip"]}"#),
        ("409 Conflict", "already claimed"),
        ("200 OK", "not json"),
    ]);
    let client = reqwest::blocking::Client::new();
    let get = || client.get(&url).send().unwrap();

    let mined: MineResult = decode_response(get()).unwrap();
    assert_eq!(mined.hashes, ["tip"]);
    let rejected = decode_response::<MineResult>(get())
        .unwrap_err()
        .to_string();
    assert_eq!(rejected, "rejected (409 Conflict): already claimed");
    let malformed = format!("{:#}", decode_response::<MineResult>(get()).unwrap_err());
    assert!(malformed.starts_with("decoding response: "), "{malformed}");
}

#[test]
fn wallet_faucet_keeps_funding_after_a_failed_account_and_keeps_its_keys() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Runtime {
        root: dir.path().to_path_buf(),
    };
    let alpha = name("alpha");
    fs::create_dir_all(runtime.instance_dir(&alpha)).unwrap();
    let pending = r#"{"id":"a","kind":"faucet","from_account":null,"to_account":2,"source_pool":"ironwood","destination_pool":"ironwood","amount_zatoshi":100,"txid":"t","block_hash":null,"status":"broadcast"}"#;
    let funded = r#"{"id":"b","kind":"faucet","from_account":null,"to_account":3,"source_pool":"ironwood","destination_pool":"ironwood","amount_zatoshi":100,"txid":"u","block_hash":"b","status":"confirmed"}"#;
    let (url, requests) = serve(vec![
        ("422 Unprocessable Entity", "no funds"),
        ("200 OK", pending),
        ("200 OK", funded),
    ]);

    let error = runtime
        .wallet_faucet_with(
            &alpha,
            &url,
            &reqwest::blocking::Client::new(),
            &[1, 2, 3],
            100,
            "ironwood",
            true,
        )
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "2 of 3 faucet requests are pending or failed"
    );

    let requests: Vec<serde_json::Value> = requests
        .try_iter()
        .map(|request| serde_json::from_str(&request).unwrap())
        .collect();
    assert_eq!(requests.len(), 3);
    let journal = FaucetJournal::open(&runtime.instance_dir(&alpha)).unwrap();
    for (account, request) in (1..=3).zip(&requests) {
        assert_eq!(request["account_id"], account);
        // the unfinished batch keeps every key, so a retry submits the same ones.
        assert_eq!(
            request["idempotency_key"],
            journal
                .key_for(&format!("wallet:ironwood:100:1,2,3:{account}"))
                .unwrap()
        );
    }
}
