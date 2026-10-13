# fix(payments)!: implementation and validation report

This branch extends main at `769795f242e2d527875443f83029c06c6c347138` (#219). It implements immediate durable acceptance, safe original-transaction recovery, bounded/fair progression, durable execution errors, and payment API compatibility documentation. It reuses the merged stores, wallet journal, queued-submission handling, and compatible client identity flows.

## Compatibility

Send, account faucet, and internal/external address faucet POSTs return the existing operation body with `202 Accepted` for pending operations and `200 OK` for terminal replays. `Location` contains the exact encoded-key status URL. Scripts must follow that URL with GET and distinguish pending from `confirmed`/`failed`; POST success no longer means payment completion. See the [payment API documentation](../../../README.md#payment-api-compatibility).

## Implementation and evidence map

The tables distinguish implementation evidence from integrated acceptance. Tests named below are present in the linked source and passed in the recorded local runs. They do not imply all original issue conditions have been exercised through real HTTP clients.

Source owners: [API and unit regressions](../../../crates/ths-server/src/api.rs), [wallet and journal regressions](../../../crates/ths-server/src/wallet.rs), [guarded store regressions](../../../crates/ths-server/src/db.rs), [live recovery scenarios](../../../crates/ths-server/tests/activity_recovery.rs), and [startup/readiness regressions](../../../crates/ths-server/src/main.rs).

| #196 criterion | Concrete evidence | Remaining acceptance limits |
| --- | --- | --- |
| Immediate POST acceptance; disconnect preserves execution | `all_payment_posts_admit_without_execution_while_payments_are_busy`, `payment_replays_use_stored_status_and_exact_encoded_location`, `expired_attempt_observes_preparation_and_keeps_guards_until_recording_finishes`; live `disconnected_payment_preparation_keeps_the_original_transaction` | The live pause is at the wallet's subtree-roots network call; it also extends past the dispatch deadline and checks same-key/conflicting replays. |
| Restart after claim without another payment POST | `payment_confirmation_loop_recovers_reopened_records_without_wallet_sync`; live `restart_recovers_claim_before_preparation_without_payment_post` | The live claim is seeded while the server is stopped, reproducing durable state after admission without the HTTP request itself. |
| Journal-backed repair after interrupted server persistence | `expired_wallet_journals_restore_both_server_rows_without_network`, `server_store_repairs_preparation_from_the_wallet_journal`; live `background_recovers_prepare_interrupted_before_store_recording` | Recovery now completes through GET only. A deterministic real-wallet preparation pause during an actual HTTP disconnect remains outstanding. |
| Lost response, queued submission, unavailable lookup, mining failure | `retry_submits_the_same_prepared_transaction_after_a_lost_response`, `a_lookup_failure_does_not_broadcast_or_change_status`, queued internal/external unit cases; both queued lost-response/restart live cases and `broadcast_recovers_after_auto_mine_failure` | The mining-failure live regression still mines directly through the node. Its pass proves background confirmation of the original, not wholly autonomous mining recovery. |
| Concurrent duplicates, deadline expiry, fingerprint conflicts | `payment_admission_replays_enforce_fingerprints_and_cross_store_identity`, `dispatch_deadline_bounds_waiting_for_payment_serialization`, `expired_attempt_observes_preparation_and_keeps_guards_until_recording_finishes`; live `concurrent_identical_sends_have_one_chain_effect` and `disconnected_payment_preparation_keeps_the_original_transaction` | The paused live worker retains the original journal and key after the ten-second budget; conflicting payload gets 409. |
| Durable terminal insufficient funds; later funding/new key | `insufficient_send_is_terminal_for_same_key_but_a_new_key_can_retry`, `insufficient_send_policy_waits_for_wallet_journal_and_checks_server_transaction`, `insufficient_send_keeps_the_uncertain_original_lookup_error_pending`; live `insufficient_send_stays_failed_after_restart_and_later_funding` | The live scenario checks the original failure, terminal replay, deliberate new key, and SDK destination balance. |
| Proven expiry, boundary inclusion, zero expiry, unknown presence, stale repair | `saved_original_inclusion_wins_over_expiry_only_on_the_selected_chain`, `a_transaction_seen_at_expiry_is_not_replaced`, `scanned_expiry_does_not_authorize_replacing_a_journal`, `replacement_authorization_rechecks_the_journal_inside_the_worker`, `matching_replacement_authorization_keeps_the_journal_after_proposal_failure`; live `expired_unseen_send_replaces_once_under_the_original_key` | The live case withholds broadcast, mines past expiry, then verifies replacement inclusion, stable key, and SDK balance. Boundary inclusion and unknown-presence branches remain seam-tested. |
| External progress, fair traversal, available sync guard | `slow_internal_lookup_does_not_starve_external_faucet_dispatch`, `slow_journal_height_lookup_preserves_recovery_and_external_dispatch_across_passes`, `attempt_bounds_treasury_and_wallet_sync_serialization_waits`, `automatic_mining_stages_share_the_remaining_attempt_budget`; live external/internal-address scenarios | These are scoped coverage, not every slow-stage/fairness and outage combination. |
| Integrated compatible CLI/dashboard and parent conditions | Existing launcher identity/resumption regressions passed; rebuilt app image; live CLI pending/resumption and Safari dashboard reload/two-tab check below | Browser validation used Safari against one disposable Regtest instance. |

| #176 parent condition | Evidence and boundary |
| --- | --- |
| Real preparation disconnect/restart; repair both stores | Live disconnected HTTP admission paused the wallet's subtree-roots request, held it past the dispatch budget, verified both prepared journals, restarted, and confirmed the original through GET. The separate journal-repair live case interrupted after the wallet write and before the server write. |
| Claim restart, lost/queued broadcast, lookup/mining recovery | Reopened-record seam plus selected live recovery cases; GET observation replaces synchronous POST completion. Direct-node-mining cases do not prove autonomous mining. |
| Expiry identity, one canonical payout, SDK balances | Unit/seam expiry policy, live duplicate/roundtrip assertions, and live expired-unseen replacement with canonical-chain and SDK-balance checks. |
| Terminal insufficient funds through restart/later funding | Guarded policy plus live failed-restart/fund/new-key scenario. |
| Duplicates and dispatch deadline without overlapping construction | Busy-admission/worker-deadline/fingerprint seams plus live duplicate payout and paused real-wallet/deadline cases. |
| CLI interruption and dashboard restart/two-tab identity | Live CLI Send returned pending during a lightwalletd outage, then resumed the same operation/key after service recovery. Safari kept one pending Send visible after reload and in a second tab; it confirmed without pressing Resume. |
| External progression and fair recovery outside wallet-sync guard | Common progression owner, bounded stages, persistent collection/cursors; evidence above. |
| Store-only reads during dependency outages and truthful readiness | `stored_payments_are_served_after_restart_while_dependencies_are_unavailable`, `stored_status_survives_repeated_rpc_response_disconnects`, `stored_status_reads_have_no_wallet_or_node_side_effects`, `health_and_chain_effects_wait_until_startup_finishes` passed. |

## Exact local commands and results

Validation used these environment overrides to limit disposable build-cache size; manifests were unchanged:

```bash
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_TEST_DEBUG=0
export CARGO_INCREMENTAL=0
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo test -p thus-spoke-zakura --features release-distribution
cargo run -p thus-spoke-zakura -- build --dev
```

- Formatting and full-workspace Clippy: exit 0 after final source edits.
- Workspace tests: exit 0; 222 server unit tests, 28 integration helpers, 185 launcher tests, 4 doctor tests. The workspace command left 13 live server scenarios and 4 launcher scenarios ignored.
- Release-distribution tests: exit 0; 185 launcher tests and 4 doctor tests, with 4 launcher scenarios ignored.
- Runtime image rebuild: exit 0. App image ID `sha256:394987c79e7cafcf159d9d299bcf08b5cda295bba57d580e946fba0ee2b4b732`; runtime lightwalletd ID `sha256:d7027c3bf927520ed17d0c74dcee157f09ea85ab00443f670dbb50c41da6f986`.
- README example: extracted script passed `bash -n`; it was not executed as a separate client.
- Diff whitespace check: exit 0. No production dependencies, pinned versions/revisions, image tags, workflows, or web source changed.

Selected live commands:

```bash
cargo test --locked --profile dev-runtime -p ths-server --test activity_recovery -- --ignored --skip large_reward_history_keeps_treasury_faucet_responsive --test-threads=1
cargo test --locked --profile dev-runtime -p ths-server --test activity_recovery -- --ignored --exact broadcast_recovers_after_auto_mine_failure
```

The broad selection recorded 11 passes and one setup failure in 104.19 seconds. The failure was the fixture's Linux-only signal registration on macOS. It was corrected to Unix Tokio signal registration, preserving scoped cleanup, and the broadcast scenario separately passed in 17.75 seconds. Twelve selected live scenarios therefore have passing local evidence; the entire selection was not rerun after that fixture correction.

Passing cases: `address_faucet_background_recovery_after_restart_pays_each_destination_once`, `concurrent_identical_sends_have_one_chain_effect`, `external_address_faucet_behavior_is_unchanged`, `internal_address_faucet_recovers_after_auto_mine_failure`, `internal_address_faucets_record_confirmed_activity`, `prepare_rolls_back_constructed_transaction_when_journal_write_fails`, `queued_external_faucet_retry_survives_lost_response_and_restart`, `queued_send_retry_survives_lost_response_and_restart`, `background_recovers_prepare_interrupted_before_store_recording`, `same_account_cross_pool_round_trip_is_replay_safe`, `startup_funding_retry_after_auto_mine_failure_has_one_payout`, and `broadcast_recovers_after_auto_mine_failure`.

Live cases exercised the current Cargo-built host server with `zakuracore/zakura:1.6.0` and `ths-recovery-lightwalletd:local`, loopback proxies, actual wallet stores, and fixture-scoped resources. They did not exercise the rebuilt app container. The 10,000-block reward-history performance case was intentionally not run. No performance claim is made. No standalone web lint/test commands or browser flow ran; web source is unchanged, and Docker rebuilt its assets. CI results are not included in this local evidence.

## Follow-up acceptance checks (October 13, 2026)

On macOS, the RPC and CLI test listeners also received local `HEAD` health probes. The fixtures now ignore those probes; `stored_status_survives_repeated_rpc_response_disconnects` and the launcher release-distribution tests pass. The live Regtest cases `disconnected_payment_preparation_keeps_the_original_transaction`, `background_recovers_prepare_interrupted_before_store_recording`, `restart_recovers_claim_before_preparation_without_payment_post`, `address_faucet_background_recovery_after_restart_pays_each_destination_once`, `insufficient_send_stays_failed_after_restart_and_later_funding`, and `expired_unseen_send_replaces_once_under_the_original_key` passed individually with `--locked --profile dev-runtime --ignored --exact`. The disconnected case pauses a real wallet network call beyond the dispatch budget while duplicate admission and fingerprint conflict are checked; it then verifies both journals and confirms the original after restart. The changed restart cases observe completion by GET without another payment POST. The expiry case withholds the original broadcast, mines past its expiry, verifies the replacement against the canonical chain and SDK destination balance, then checks terminal replay under the original key.

The full selected live suite was then run serially with `cargo test --locked --profile dev-runtime -p ths-server --test activity_recovery -- --ignored --skip large_reward_history_keeps_treasury_faucet_responsive --test-threads=1`: 16 passed, 0 failed. The reward-history stress test was excluded.

`cargo fmt --all -- --check` and full-workspace Clippy passed. A local `cargo test --workspace` run reached 220 passing server tests and two pre-existing macOS HTTP/2 mock failures (`wallet_broadcast_accepts_the_lightwalletd_queued_response` and `broadcast_times_out_after_lightwalletd_accepts_the_request`); the new RPC test passed in that run. The existing Linux CI run for the PR head passed. Run the follow-up commit through Linux CI before merge.

The rebuilt app and launcher were also run together in a disposable `pr235-acceptance` instance. A CLI Send confirmed through the new API. With lightwalletd paused, a second CLI Send returned pending with key `24ac9ccb-cc7c-4656-9619-b5f8341c5302`; after unpausing, the same command returned confirmed activity `3e6ddb47-1f35-4981-a57a-5950f16ced03`. The exact-key GET returned one activity, and the SDK account 4 balance was 1,000,000 zatoshi. In Safari, a pending Send to account 3 remained visible after reload and in a second tab during the same outage, then changed to confirmed after recovery without another Send. The instance and its volumes were removed by normal launcher shutdown. The launcher release-distribution tests passed when run serially; the earlier parallel local run had two test-fixture timing/probe failures.

## Scope

The nine production/documentation plan tasks are implemented. The follow-up covers the specified acceptance paths through real HTTP/Regtest flows and focused unit seams. Boundary inclusion, zero expiry, unavailable presence, stale journal repair, and fairness under slow lookups remain covered by unit seams rather than separate live Docker scenarios. The 10,000-block reward-history stress case was not run. Submission remains outside wallet transactions, account 6 stays hidden, SDK balances remain authoritative, host services remain Regtest/loopback, and cleanup stays instance-scoped.
