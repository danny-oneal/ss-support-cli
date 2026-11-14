# Guarded Write Operations Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Two operations that change state, `s3.restore-version` and `ddb.item-edit`, usable from the command line and the TUI, each showing the exact change before it asks, and each refusing to write over state the operator did not see.

**Architecture:** Each port gains exactly one write, shaped so the service does the careful part: S3 copies a version server-side (metadata travels with it), DynamoDB updates named attributes under a condition on their previewed values (other attributes are not touched). The operations are ordinary `Operation`s: reads through `Runner::step`, the change through `Runner::preview` + `Runner::mutate`, so the environment allowlist, confirmation, dry run, and audit entry come from the runner unchanged. The TUI needs no new machinery; it gains one browser key and keeps a run's warnings on the result.

**Tech Stack:** as before (aws-sdk-s3 `CopyObject`, aws-sdk-dynamodb `UpdateItem`, serde_dynamo). No new dependencies.

**Spec:** `docs/2026-11-12-finish-roadmap.md` milestone M3; `docs/2026-11-07-rust-port-master-plan.md` Phase 5 (`restore`, `item_edit`); `docs/DESIGN.md` §8 (safety model).

## Decisions

The user's instruction of 2026-11-12 is to implement the roadmap without further questions, so this milestone's open decisions are settled here by the assistant. Redirect any of them in review.

- **`s3.restore-version` is `Mutate`** (confirmed with `y`): on a bucket that keeps versions the restore adds a version and the previous current one stays in the history. The operation **refuses when the current version has no id** (`null`), because then the bucket is not keeping versions and the copy would overwrite the current content for good.
- **`ddb.item-edit` is `Destructive`** (confirmed by typing the table name): DynamoDB keeps no history, so the previous values are gone once written.
- **S3 write shape:** `S3Port::copy_version(bucket, key, version_id) -> new version id`, a server-side copy of that version onto the same key. Not `put(body)`: a re-upload would drop content type, user metadata, and encryption settings, and would cap the size at what fits in memory.
- **DynamoDB write shape:** `DdbPort::update_item(table, key, &ItemChange)`, which sets and removes named top-level attributes on an existing item, conditional on those attributes still having their previewed values. Not `put_item`: a full replace would overwrite attributes another writer changed in the meantime.
- **How an edit is given:** `--set '{"limit": 250, "tags": ["a"]}'` (a JSON object: attribute → new value) and `--remove legacy,oldFlag` (attribute names). Key attributes cannot be set or removed. Nested attributes are replaced whole.
- **Stale preview:** each write re-checks what the preview showed, inside the mutating step. S3: the current version id must still be the one previewed (a small window remains between that check and the copy; the previous version stays in the history either way). DynamoDB: the condition is part of the write itself.
- **Restoring a delete marker** (which would delete the object) is refused. Restoring an older version of an object whose current entry is a delete marker is allowed (it undeletes).
- **TUI:** `R` on a version in the S3 browser opens the `s3.restore-version` form filled in. A run's warnings (a failed audit write) stay visible on the result screen.
- **Audit:** one entry per run, written when the run ends, as today. A process killed between the write and the entry leaves no entry; this is stated in the README and not changed here.
- **Release:** 0.1.0-alpha.7, stacked on the M2 branch.

## Global Constraints

- `make check` and `make msrv` pass at every commit. Every public item has a doc comment; comments, doc comments, and test names describe behavior only.
- Every write goes through `Runner::mutate`. No operation calls a port's write method outside it.
- The Kubernetes port stays read-only; the test in `svc/kubectl/mod.rs` that forbids write methods stays as it is.
- Library code never reads `std::env`. Anything that reaches output uses `BTreeMap`. JSON read from a service is normalized with `jsonutil` before it is diffed.
- Usage errors (a malformed `--set`, a key attribute in `--remove`) are `RunnerError::InvalidParam` → exit 2, raised before any network call that is not needed to detect them.
- Port contract suites stay runnable against the fakes in unit tests and against MinIO and dynalite in CI (`make contract`, `make contract-ddb`).

## Review Focus

1. **A write lands on state the operator did not see.** Another writer, or a slow confirmation. The S3 re-check and the DynamoDB condition must each be exercised by a test that changes the state between preview and write. Tasks 1–4.
2. **A restore changes more than the content**, or less: content type, user metadata. Task 1 (MinIO test with the raw client).
3. **Keys that are awkward in a copy source**: spaces, `+`, `?`, `#`, `%`, non-ASCII. Task 1.
4. **The no-op and refusal paths write nothing and ask nothing**: restoring the current version, restoring a delete marker, a bucket without versions, an edit that changes nothing, an edit of a key attribute. Tasks 3–4.
5. **A failed DynamoDB condition is told apart from a missing item**, so the operator learns which happened. Task 2.
6. **Outside the allowlist nothing is read or written**, and `--dry-run` reads and previews but does not write. Task 5.
7. **Numbers survive the round trip**: `--set '{"limit": 250}'` stores a number, and a condition on a previewed number does not fail because of how it was formatted. Tasks 2, 4.

---

### Task 1: S3 `copy_version`

**Files:** Modify `crates/ss-core/src/svc/mod.rs` (`SvcError::Changed`), `crates/ss-core/src/svc/s3/{mod,fake,lazy,cached,contract}.rs`, `crates/ss-support/src/aws/s3.rs`.

**Interfaces (produced):**
```rust
// svc/mod.rs
SvcError::Changed { what: String }          // "changed since it was read: {what}"
impl SvcError { pub fn changed(what: impl Into<String>) -> Self; pub fn is_changed(&self) -> bool }

// svc/s3/mod.rs
/// Makes `version_id` of `key` the current version by copying it onto the
/// same key on the server; content, content type and user metadata are
/// copied with it. Returns the id of the version the copy created, or
/// `CURRENT` when the bucket does not keep versions.
async fn copy_version(&self, bucket: &str, key: &str, version_id: &str) -> Result<String, SvcError>;

// svc/s3/contract.rs
pub async fn run_writes(fx: &Fixture<'_>);   // mutates the fixture; call after `run`
```
Errors: `NotFound` when the bucket, key, or version does not exist; `Other` when `version_id` is empty or `CURRENT` (an explicit id is required) or names a delete marker.

- [ ] **Step 1: Failing tests.**
  - `svc/mod.rs`: `changed_errors_say_what_changed_and_are_not_not_found`.
  - `fake.rs`: `copy_version_adds_a_new_current_version_with_the_same_body`; `copy_version_of_a_missing_version_is_not_found`; `copy_version_refuses_a_delete_marker_and_the_current_alias`; `copy_version_over_a_delete_marker_makes_the_object_listed_again`.
  - `contract.rs` `run_writes`: copies `version_ids[0]`, then asserts the returned id is new, `list_versions` has one more entry whose newest id is the returned one and `is_latest`, `get_version(CURRENT)` returns `bodies[0]`, every earlier version is still fetchable, and copying an unknown id is `NotFound`. Test `fake_satisfies_the_write_contract`.
  - `lazy.rs`, `cached.rs`: `copy_version_passes_through` (and for `Cached`: a copy is never served from or written to the cache, and the previously cached explicit versions stay valid).
  - `aws/s3.rs` unit: `copy_source_encodes_everything_but_unreserved_characters_and_slashes` (`cfg/a b+c?d#e%f/日本.json` with version `v 1` → `ss-config/cfg/a%20b%2Bc%3Fd%23e%25f/%E6%97%A5%E6%9C%AC.json?versionId=v%201`).
  - `aws/s3.rs` MinIO (env-gated, in the existing contract test): `run_writes` after `run`; plus `a_copy_keeps_content_type_and_user_metadata` and `a_key_with_awkward_characters_can_be_copied`, both using the raw client to seed and to read back.
- [ ] **Step 2: Run** `cargo test -p ss-core svc::` and `cargo test -p ss-support aws::s3`. Expected: FAIL to compile.
- [ ] **Step 3: Implement.** AWS: `copy_object().bucket(b).key(k).copy_source(encoded)`; the response's `version_id`, or `CURRENT` when absent. `FakeS3`: clones the entry under a generated id `copy-{n}` stamped now.
- [ ] **Step 4: Run** the same commands and `make check`. Expected: PASS. **Step 5: Commit** `feat(s3): copy a version onto its key`.

### Task 2: DynamoDB `update_item`

**Files:** Modify `crates/ss-core/src/svc/ddb/{mod,fake,lazy,contract}.rs`, `crates/ss-support/src/aws/ddb.rs`.

**Interfaces (produced):**
```rust
/// A change to named top-level attributes of one existing item.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ItemChange {
    pub set: Item,                                   // attribute -> new value
    pub remove: Vec<String>,                         // attributes to delete
    pub expect: BTreeMap<String, Option<Value>>,     // attribute -> value it must still have; None = absent
}

/// Applies `change` to the item at `key`, only if the item exists and every
/// attribute in `change.expect` still has the value given.
async fn update_item(&self, table: &str, key: &Key, change: &ItemChange) -> Result<(), SvcError>;

pub async fn run_writes(port: &dyn DdbPort, table: &str);   // contract; mutates the table
```
Errors: `NotFound` when the table or the item does not exist; `Changed` when an expectation does not hold; `Other` for an empty change, a key attribute in `set` or `remove`, or a service failure.

- [ ] **Step 1: Failing tests.**
  - `fake.rs`: `update_sets_and_removes_attributes_and_leaves_the_rest`; `update_of_a_missing_item_is_not_found`; `update_fails_as_changed_when_an_expected_value_differs` (and the item is untouched); `update_fails_as_changed_when_an_attribute_expected_absent_exists`; `expected_numbers_match_whatever_their_spelling` (`1` vs `1.0`); `update_refuses_key_attributes_and_an_empty_change`; `index_entries_follow_an_update`.
  - `contract.rs` `run_writes`: the same behaviors through the port against seeded items, ending with `get_item` showing the expected result. Test `fake_satisfies_the_write_contract`.
  - `lazy.rs`: `update_item_passes_through`.
  - `aws/ddb.rs` unit: `the_update_expression_names_every_attribute_through_a_placeholder` (attributes with spaces, dots, and reserved words such as `name` and `size`); `a_condition_is_built_for_every_expectation`.
  - `aws/ddb.rs` dynalite (env-gated): `run_writes` after the read contract.
- [ ] **Step 2: Run** `cargo test -p ss-core svc::ddb` and `cargo test -p ss-support aws::ddb`. Expected: FAIL to compile.
- [ ] **Step 3: Implement.** AWS: one `UpdateItem` with `SET #s0 = :s0, … REMOVE #r0, …` and `attribute_exists(#k) AND #e0 = :e0 AND attribute_not_exists(#e1) …`. On `ConditionalCheckFailedException`, a consistent `GetItem` decides: no item → `NotFound`, otherwise `Changed`.
- [ ] **Step 4: Run** and `make check`. Expected: PASS. **Step 5: Commit** `feat(ddb): update attributes of an item under a condition`.

### Task 3: `s3.restore-version`

**Files:** Create `crates/ss-core/src/ops/s3/restore.rs`; modify `ops/s3/mod.rs`, `ops/mod.rs` (`builtin`, and its id-list test).

**Interfaces:**
- Consumes: `S3Port::{list_versions, get_version, copy_version}`, `compare_bodies`, `Runner::{step, preview, set_resource, mutate}`, `suggest_buckets`, `suggest_keys`, `suggest_versions`.
- Produces: `pub struct RestoreVersion`; `Meta { id: "s3.restore-version", name: "Restore object version", category: "S3", class: Class::Mutate }`; params `bucket` (required), `key` (required), `version` (required: "the version to make current again").

Behavior, in order: read the history → locate `version` (absent → failure naming the key) → refuse a delete marker → refuse when the current entry's id is empty or `null` → when `version` is already the current version return a summary saying nothing was done (no preview, no confirmation, no write) → fetch the version and the current content and build the preview (summary + the diff from current to the version, or a note that the object is currently deleted) → `set_resource("{bucket}/{key}")` → in `mutate("copy the version")`: read the history again, fail with `SvcError::Changed` if the current id is no longer the one previewed, else `copy_version` → result summary: object, restored version, new current version id, previous current version id.

- [ ] **Step 1: Failing tests** (fake-backed, through `Runner::execute`; `yes(true)` or a scripted confirmer as each needs):
  - `restoring_an_older_version_makes_its_body_current_and_keeps_the_history`
  - `the_preview_is_the_diff_from_the_current_version_to_the_restored_one` (a recording confirmer captures `ConfirmSpec.preview` and `resource`)
  - `restoring_the_current_version_does_nothing_and_asks_nothing`
  - `a_delete_marker_cannot_be_restored`
  - `a_bucket_that_keeps_no_versions_is_refused_before_anything_is_asked`
  - `an_object_that_is_currently_deleted_can_be_restored`
  - `a_version_that_is_not_in_the_history_fails_naming_it`
  - `a_change_after_the_preview_stops_the_restore` (the confirmer writes a new version before answering yes; nothing is copied; the error is `Changed`)
  - `a_declined_restore_copies_nothing`; `a_dry_run_previews_and_copies_nothing`
  - `a_body_that_is_not_json_previews_as_a_line_diff`
  - `ops::tests::builtin_registers_every_operation` gains the id.
- [ ] **Step 2: Run** `cargo test -p ss-core ops::s3::restore`. Expected: FAIL. **Step 3: Implement.** **Step 4: Run** and `make check`. Expected: PASS (the read-only guard tests in `ss-support/tests` are updated in Task 5; if one fails here, move that update forward). **Step 5: Commit** `feat(s3): restore a previous version of an object`.

### Task 4: `ddb.item-edit`

**Files:** Create `crates/ss-core/src/ops/ddb/item_edit.rs`; modify `ops/ddb/mod.rs`, `ops/mod.rs`.

**Interfaces:**
- Consumes: `DdbPort::{describe_table, get_item, update_item}`, `ItemChange`, `parse_key_param`, `suggest_tables`, `diff::json`, `jsonutil::parse`.
- Produces: `pub struct ItemEdit`; `Meta { id: "ddb.item-edit", name: "Edit item", category: "DynamoDB", class: Class::Destructive }`; params `table` (required), `key` (required), `set` (JSON object), `remove` (comma-separated names).

Behavior, in order: parse `set` and `remove` (usage errors: `set` not a JSON object; neither given; a name in both) → describe the table, parse the key → usage error if `set` or `remove` names a key attribute → fetch the item (`NotFound` if absent) → compute the edited item; when it equals the stored one return a summary saying nothing was done → preview (summary: table, key, changes; diff `stored` → `edited`) → `set_resource(table)` → `mutate("update the item")` with `expect` = the stored value (or absence) of every attribute touched → result summary + the same diff labelled `before` → `after`.

- [ ] **Step 1: Failing tests:**
  - `setting_and_removing_attributes_changes_only_those`
  - `the_preview_is_the_diff_from_the_stored_item_to_the_edited_one_and_names_the_table`
  - `an_edit_that_changes_nothing_writes_nothing_and_asks_nothing`
  - `a_key_attribute_cannot_be_set_or_removed` (exit kind `Usage`, before the item is fetched)
  - `set_must_be_a_json_object`; `an_edit_needs_set_or_remove`; `an_attribute_cannot_be_both_set_and_removed`
  - `a_missing_item_is_not_found_and_nothing_is_written`
  - `a_change_after_the_preview_stops_the_edit` (the confirmer changes one touched attribute before answering; the error is `Changed`; the other writer's value stands)
  - `a_change_to_another_attribute_after_the_preview_is_kept` (the edit goes through and the other attribute keeps the other writer's value)
  - `numbers_are_stored_as_numbers`; `a_declined_edit_writes_nothing`; `a_dry_run_previews_and_writes_nothing`
  - `yes_does_not_cover_it` (`Runner::yes(true)` with no confirmer → declined)
  - `ops::tests::builtin_registers_every_operation` gains the id.
- [ ] **Step 2: Run** `cargo test -p ss-core ops::ddb::item_edit`. Expected: FAIL. **Step 3: Implement.** **Step 4: Run** and `make check`. Expected: PASS. **Step 5: Commit** `feat(ddb): edit attributes of an item`.

### Task 5: Command-line end-to-end and guards

**Files:** Modify `crates/ss-support/tests/{ddb_guards,flow_guards,s3,ddb}.rs`; create `crates/ss-support/tests/writes.rs`.

- [ ] **Step 1: Failing tests** in `tests/writes.rs` (in-process `run_with`, `Services::isolated`, fakes, config written per test):
  - `the_two_write_operations_are_the_only_operations_that_are_not_reads` (iterates `ops::builtin()`; replaces the per-family "every operation is a read" assertions, which are narrowed to "every other operation")
  - `outside_the_allowlist_a_write_is_refused_and_nothing_is_read_or_written` (a counting port: zero calls; exit 1; audit outcome `denied`) — one test per operation
  - `restore_with_yes_copies_the_version_and_audits_ok`
  - `restore_asks_on_stderr_and_proceeds_on_y`; `restore_declined_exits_1_and_copies_nothing`
  - `item_edit_needs_the_table_name_typed_and_yes_does_not_replace_it`
  - `item_edit_with_the_table_name_typed_updates_the_item`
  - `dry_run_shows_the_preview_and_changes_nothing` (both operations; exit 0)
  - `a_key_attribute_in_set_exits_2`; `a_set_that_is_not_json_exits_2`
  - `ops_list_shows_ten_operations_besides_the_demos`; `ops_describe_shows_the_write_params`
- [ ] **Step 2: Run** `cargo test -p ss-support --test writes`. Expected: FAIL. **Step 3: Implement** whatever the tests show is missing (expected: nothing beyond test updates). **Step 4: Run** `make check`. Expected: PASS. **Step 5: Commit** `test(cli): write operations end to end`.

### Task 6: TUI

**Files:** Modify `tui/screens/{s3browse,opsform,opsresult}.rs`, `crates/ss-support/tests/tui.rs`.

**Interfaces:** `OpsResult::new(op, input, result, warnings: Vec<String>, ctx)`; the run screen collects `Msg::Warning` texts and hands them over; the result screen shows them above the view until it is left.

- [ ] **Step 1: Failing tests:**
  - `opsresult.rs`: `warnings_from_the_run_stay_on_the_result`; `a_result_without_warnings_shows_none`.
  - `opsform.rs`: `a_warning_from_the_run_is_carried_to_the_result` (replaces the status-line test).
  - `s3browse.rs`: `shift_r_opens_the_restore_form_for_the_selected_version`; `shift_r_on_a_delete_marker_or_the_latest_version_says_why_not`; `shift_r_reports_when_restore_is_not_registered`.
  - `tests/tui.rs`: `a_restore_from_the_browser_is_previewed_confirmed_and_done` (the modal shows the diff; `y` + enter; the fake's current body is the restored one; one audit entry); `an_item_edit_needs_the_table_name_typed` (wrong name stays open; right name writes); `a_restore_that_meets_a_newer_version_reports_the_change`.
- [ ] **Step 2: Run.** Expected: FAIL. **Step 3: Implement.** **Step 4: Run** `make check`. Expected: PASS. **Step 5: Commit** `feat(tui): restore from the browser; warnings stay on the result`.

### Task 7: Documentation and release

**Files:** `README.md`, `CHANGELOG.md`, `CLAUDE.md`, `docs/config.example.yaml`, `docs/2026-11-12-finish-roadmap.md`, `Cargo.toml`, `crates/ss-support/Cargo.toml`.

- [ ] **Step 1:** README: a `## Writes` section (the allowlist, what each operation previews, how each is confirmed, `--dry-run`, `--yes` covering `s3.restore-version` only and only on the command line, what the stale-preview checks do and do not cover, what a restore keeps and what it does not, the audit entry and its limit); correct every "only reads" statement in the S3 and DynamoDB sections; add `R` to the browser's keys; update the exit-code paragraph for the new usage errors.
- [ ] **Step 2:** CHANGELOG `[0.1.0-alpha.7]`; CLAUDE.md (ports are no longer read-only except Kubernetes; where the writes live; the re-check rule); roadmap M3 status.
- [ ] **Step 3:** Version `0.1.0-alpha.7`. `make check`, `make msrv`. Walk the real binary on a pseudo-terminal against the fakes is not possible (the binary has no fake ports); record that the adapters' writes are verified by the contract jobs in CI.
- [ ] **Step 4: Commit** `feat: guarded write operations (0.1.0-alpha.7)`.

---

## Self-Review

- **Spec coverage:** roadmap M3 deliverable (T3, T4), port writes with fakes, decorators, adapters, contract suites (T1, T2), allowlist/dry-run/audit (T5), TUI (T6), README corrections (T7). Every open decision of the roadmap is settled in Decisions.
- **Type consistency:** `SvcError::Changed` is produced in T1, returned by `update_item` in T2, and by the restore re-check in T3. `ItemChange` is produced in T2 and consumed in T4. `copy_version` is produced in T1 and consumed in T3. `OpsResult::new` gains `warnings` in T6 and its two callers change in the same task.
- **Review Focus:** 1 → T1–T4; 2, 3 → T1; 4 → T3, T4; 5 → T2; 6 → T5; 7 → T2, T4.
- **Not verifiable on this machine:** the AWS adapters' `CopyObject` and `UpdateItem` against MinIO and dynalite run only in CI.
