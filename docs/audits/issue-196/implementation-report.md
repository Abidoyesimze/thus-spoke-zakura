# fix(payments)!: implementation and validation report

This branch extends main at `769795f242e2d527875443f83029c06c6c347138` (#219). It implements immediate durable acceptance, safe original-transaction recovery, bounded/fair progression, durable execution errors, and payment API compatibility documentation. It reuses the merged stores, wallet journal, queued-submission handling, and compatible client identity flows.

## Compatibility

Send, account faucet, and internal/external address faucet POSTs return the existing operation body with `202 Accepted` for pending operations and `200 OK` for terminal replays. `Location` contains the exact encoded-key status URL. Scripts must follow that URL with GET and distinguish pending from `confirmed`/`failed`; POST success no longer means payment completion. See the [payment API documentation](../../../README.md#payment-api-compatibility).

## Implementation and evidence map

The tables distinguish implementation evidence from integrated acceptance. Tests named below are present in the linked source and passed in the recorded local runs. They do not imply all original issue conditions have been exercised through real HTTP clients.

Source owners: [API and unit regressions](../../../crates/ths-server/src/api.rs), [wallet and journal regressions](../../../crates/ths-server/src/wallet.rs), [guarded store regressions](../../../crates/ths-server/src/db.rs), [live recovery scenarios](../../../crates/ths-server/tests/activity_recovery.rs), and [startup/readiness regressions](../../../crates/ths-server/src/main.rs).

| #196 criterion | Concrete evidence | Remaining acceptance limits |
| --- | --- | --- |
| Immediate POST acceptance; disconnect preserves execution | `all_payment_posts_admit_without_execution_while_payments_are_busy`, `payment_replays_use_stored_status_and_exact_encoded_location`, `expired_attempt_observes_preparation_and_keeps_guards_until_recording_finishes` | Deterministic real-wallet preparation paused during an actual payment HTTP disconnect was not added/executed. |
| Restart after claim without another payment POST | `payment_confirmation_loop_recovers_reopened_records_without_wallet_sync` | Dedicated real-Regtest restart before preparation remains unproven by this seam. |
| Journal-backed repair after interrupted server persistence | `expired_wallet_journals_restore_both_server_rows_without_network`, `server_store_repairs_preparation_from_the_wallet_journal`; live `retry_recovers_prepare_interrupted_before_store_recording` | Live scenario preserves the existing recovery assertions; it is not the complete new disconnect-before-preparation oracle. |
| Lost response, queued submission, unavailable lookup, mining failure | `retry_submits_the_same_prepared_transaction_after_a_lost_response`, `a_lookup_failure_does_not_broadcast_or_change_status`, queued internal/external unit cases; both queued lost-response/restart live cases and `broadcast_recovers_after_auto_mine_failure` | The mining-failure live regression still mines directly through the node. Its pass proves background confirmation of the original, not wholly autonomous mining recovery. |
| Concurrent duplicates, deadline expiry, fingerprint conflicts | `payment_admission_replays_enforce_fingerprints_and_cross_store_identity`, `dispatch_deadline_bounds_waiting_for_payment_serialization`, `expired_attempt_observes_preparation_and_keeps_guards_until_recording_finishes`; live `concurrent_identical_sends_have_one_chain_effect` | The complete combined real-preparation pause/duplicate/deadline scenario remains unrun. |
| Durable terminal insufficient funds; later funding/new key | `insufficient_send_is_terminal_for_same_key_but_a_new_key_can_retry`, `insufficient_send_policy_waits_for_wallet_journal_and_checks_server_transaction`, `insufficient_send_keeps_the_uncertain_original_lookup_error_pending` | Dedicated funded real-wallet failure/restart/later-funding scenario remains unrun. |
| Proven expiry, boundary inclusion, zero expiry, unknown presence, stale repair | `saved_original_inclusion_wins_over_expiry_only_on_the_selected_chain`, `a_transaction_seen_at_expiry_is_not_replaced`, `scanned_expiry_does_not_authorize_replacing_a_journal`, `replacement_authorization_rechecks_the_journal_inside_the_worker`, `matching_replacement_authorization_keeps_the_journal_after_proposal_failure`; guarded store tests | Live successful expiry replacement with canonical payout and SDK balance oracles remains unrun. |
| External progress, fair traversal, available sync guard | `slow_internal_lookup_does_not_starve_external_faucet_dispatch`, `slow_journal_height_lookup_preserves_recovery_and_external_dispatch_across_passes`, `attempt_bounds_treasury_and_wallet_sync_serialization_waits`, `automatic_mining_stages_share_the_remaining_attempt_budget`; live external/internal-address scenarios | These are scoped coverage, not every slow-stage/fairness and outage combination. |
| Integrated compatible CLI/dashboard and parent conditions | Existing launcher identity/resumption regressions passed; unchanged callers source-audited; web assets rebuilt in Docker | Actual CLI interruption/resumption and browser restart/two-tab flows against the switched server were not run. |

| #176 parent condition | Evidence and boundary |
| --- | --- |
| Real preparation disconnect/restart; repair both stores | Atomic journaling and worker guards preserved; journal repair has seam/live coverage above. Full deterministic HTTP disconnect oracle remains outstanding. |
| Claim restart, lost/queued broadcast, lookup/mining recovery | Reopened-record seam plus selected live recovery cases; GET observation replaces synchronous POST completion. Direct-node-mining cases do not prove autonomous mining. |
| Expiry identity, one canonical payout, SDK balances | Unit/seam expiry policy and live duplicate/roundtrip canonical-chain and balance assertions; dedicated live expiry replacement remains outstanding. |
| Terminal insufficient funds through restart/later funding | Guarded policy and retained-key seam coverage; real funded wallet scenario remains outstanding. |
| Duplicates and dispatch deadline without overlapping construction | Busy-admission/worker-deadline/fingerprint seams plus live duplicate payout case; combined real pause/deadline case remains outstanding. |
| CLI interruption and dashboard restart/two-tab identity | Existing client source/regressions retained; production client flows remain outstanding. |
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

Passing cases: `address_faucet_retry_after_server_restart_pays_each_destination_once`, `concurrent_identical_sends_have_one_chain_effect`, `external_address_faucet_behavior_is_unchanged`, `internal_address_faucet_recovers_after_auto_mine_failure`, `internal_address_faucets_record_confirmed_activity`, `prepare_rolls_back_constructed_transaction_when_journal_write_fails`, `queued_external_faucet_retry_survives_lost_response_and_restart`, `queued_send_retry_survives_lost_response_and_restart`, `retry_recovers_prepare_interrupted_before_store_recording`, `same_account_cross_pool_round_trip_is_replay_safe`, `startup_funding_retry_after_auto_mine_failure_has_one_payout`, and `broadcast_recovers_after_auto_mine_failure`.

Live cases exercised the current Cargo-built host server with `zakuracore/zakura:1.6.0` and `ths-recovery-lightwalletd:local`, loopback proxies, actual wallet stores, and fixture-scoped resources. They did not exercise the rebuilt app container. The 10,000-block reward-history performance case was intentionally not run. No performance claim is made. No standalone web lint/test commands or browser flow ran; web source is unchanged, and Docker rebuilt its assets. CI results are not included in this local evidence.

## Scope

The nine production/documentation plan tasks are implemented. This report records the observed evidence and explicitly outstanding integrated scenarios; it does not establish full acceptance or authorize closure of #196/#176. Submission remains outside wallet transactions, account 6 stays hidden, SDK balances remain authoritative, host services remain Regtest/loopback, and cleanup stays instance-scoped.
