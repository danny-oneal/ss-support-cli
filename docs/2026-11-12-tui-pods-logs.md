# TUI Pods and Logs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** From the operation list, `p` shows the pods of a namespace, and enter on a pod tails its log.

**Architecture:** Two screens on the existing shell. The pod list is one background load (`Cmd::Task`). The log is a stream: a new `Cmd::Stream` hands the shell a future that opens the port's line receiver; the shell forwards lines to the asking screen in batches, one batch at a time (the next batch is read only after the screen has taken the previous one), and drops the receiver when the screen leaves the stack, which stops kubectl. The log screen keeps a bounded buffer.

**Tech Stack:** as M1 (ratatui 0.30, tokio). No new dependencies.

**Spec:** `docs/2026-11-12-finish-roadmap.md` milestone M2; `docs/2026-11-07-rust-port-master-plan.md` Phase 6 (`k8spods`, `logstail`). Builds on `docs/2026-11-12-tui-core.md`.

## Decisions

The user's instruction of 2026-11-12 is to implement the roadmap without further questions, so the milestone's open decisions are settled here by the assistant. Redirect any of them in review.

- **Namespace:** the pod list opens on the context's default namespace. `n` opens a one-line prompt to type another; an empty answer means the default again. The namespace is kept only for as long as the screen is open. No config field is added.
- **Log keys:** `f` follow the tail on/off · `↑` `↓` `pgup` `pgdn` scroll (scrolling up stops following) · `g` top · `G` bottom and follow · `←` `→` scroll sideways · `/` filter lines · `esc` back.
- **Follow means the view, not kubectl:** the stream is always opened with `follow = true`; `f` only decides whether the view sticks to the newest line. Leaving the screen ends the stream.
- **Buffer:** the newest 5,000 lines are kept; older ones are dropped and counted (`… 1,234 earlier lines dropped`).
- **Refresh:** the pod list reloads on `r` only. Nothing polls the cluster.
- **Colour codes in logs are removed**, not shown as text: ANSI escape sequences are stripped before a line is stored.
- **Release:** 0.1.0-alpha.6, stacked on the M1 branch.

## Global Constraints

Those of `docs/2026-11-12-tui-core.md`, plus:
- The Kubernetes port stays read-only; this plan adds no method to it.
- No bare `&str`/`String` widget; text from the cluster goes through `tui::text` before it is drawn.
- A stream never outlives its screen, and a screen never holds more than its buffer limit.

## Review Focus

1. A chatty pod cannot grow memory without bound: lines are pulled from the port only as fast as the screen takes them, and the buffer has a hard cap. Pinned in Tasks 1 and 3.
2. Leaving the log screen (esc, Ctrl-C, quit, an interrupt) stops the stream, so no kubectl process is left behind. Pinned in Tasks 1 and 4.
3. Escape sequences in log lines and pod names cannot drive the terminal, and colour codes do not litter the text. Pinned in Tasks 2 and 3.
4. Kubernetes not configured, no context, a namespace that does not exist, a pod that vanished: each shows an error and leaves the screen usable. Pinned in Tasks 2 and 3.
5. A log that ends (pod deleted, kubectl exits) says so instead of looking frozen. Pinned in Task 3.

---

### Task 1: Streams in the shell

**Files:** Modify `tui/screen.rs`, `tui/shell.rs`, every `match` on `Msg` in `tui/screens/*.rs`, `tui/bridge.rs` tests, `tui/testing.rs`.

**Interfaces (produced):**
```rust
// screen.rs
pub type LineStream = BoxFuture<Result<mpsc::Receiver<String>, String>>;
pub enum Cmd { /* … */ Stream(u64, LineStream) }
pub enum Msg { /* … */ Lines(u64, Vec<String>), StreamEnded(u64, Option<String>) }
pub enum Loaded { /* … */ Pods(Vec<Pod>) }
```
Rules: the shell opens the stream on a task. A failure to open is `Msg::StreamEnded(id, Some(error))`. Lines are delivered as `Msg::Lines(id, batch)` with at most 256 lines per batch; after sending a batch the forwarder waits until the shell has handed it to the screen before reading more. The channel closing is `Msg::StreamEnded(id, None)`. When the asking screen leaves the stack, or the shell closes, the forwarder is aborted and the receiver dropped.

- [ ] **Step 1: Failing tests** in `shell.rs`:
  - `stream_lines_reach_the_screen_that_asked_in_order`
  - `a_stream_that_cannot_open_ends_with_its_error`
  - `a_closed_stream_ends_without_an_error`
  - `popping_the_screen_drops_the_stream` (the sender's `closed()` resolves)
  - `closing_the_shell_drops_every_stream`
  - `a_fast_stream_is_read_one_batch_at_a_time` (a sender that has queued 10,000 lines on a channel of capacity 64 has had at most 256 + 64 taken while the shell has delivered nothing)
  - `lines_for_a_screen_that_has_gone_are_dropped`
- [ ] **Step 2: Run** `cargo test -p ss-support --lib tui::shell`. Expected: FAIL.
- [ ] **Step 3: Implement.** Each stack entry owns its streams (`AbortHandle` plus an `Arc<Semaphore>` used as the one-batch credit; the shell adds a permit after delivering a `Msg::Lines`).
- [ ] **Step 4: Run** `cargo test -p ss-support --lib tui::`. Expected: PASS. **Step 5: Commit** `feat(tui): line streams routed through the shell`.

### Task 2: Pod list

**Files:** Create `tui/screens/k8spods.rs`; modify `tui/text.rs` (ANSI stripping), `ss-core/src/ops/s3/mod.rs` (`age` becomes `pub`).

**Interfaces:**
- Consumes: `Deps.kubectl: Option<Arc<dyn KubectlPort>>`, `KubectlPort::list_json(namespace, "pods")`, `ss_core::svc::kubectl::{parse_pods, Pod}`, `ops::s3::age(then, now)`, `components::prompt::Prompt`.
- Produces: `pub struct K8sPods`, `K8sPods::new(ctx: &Ctx) -> Self`; `pub fn text::strip_ansi(text: &str) -> String` (removes CSI `ESC [ … final` and OSC `ESC ] … BEL|ST` sequences; `clean` is applied by callers afterwards). The enter key, which opens a pod's log, and its test are added in Task 3, where the log screen exists.

- [ ] **Step 1: Failing tests:**
  - `strip_ansi_removes_colour_and_cursor_sequences_and_keeps_the_text`
  - `strip_ansi_leaves_an_unterminated_sequence_harmless`
  - `the_pod_list_opens_on_the_default_namespace` (title `Pods — default namespace`)
  - `pods_are_listed_with_ready_status_restarts_and_age` (snapshot)
  - `n_asks_for_a_namespace_and_lists_it` (title `Pods — orders`)
  - `an_empty_namespace_answer_returns_to_the_default`
  - `r_reloads_and_keeps_the_selected_pod`
  - `a_namespace_without_pods_says_no_pods`
  - `kubernetes_not_configured_shows_the_error_and_esc_leaves`
  - `a_failed_reload_keeps_the_listing_and_reports_the_error`
  - `a_stale_listing_is_ignored`
  - `pod_names_with_control_characters_are_listed_safely`
  - `the_selection_stays_in_view`
  - `a_tiny_area_draws_without_panicking`
- [ ] **Step 2: Run.** Expected: FAIL. **Step 3: Implement.** **Step 4: Run.** Expected: PASS. **Step 5: Commit** `feat(tui): pod list`.

### Task 3: Log tail

**Files:** Create `tui/screens/logstail.rs`; modify `tui/screens/k8spods.rs` (enter).

**Interfaces:**
- Consumes: `KubectlPort::logs(namespace, pod, true)`, `Cmd::Stream`, `Msg::Lines`, `Msg::StreamEnded`, `text::{strip_ansi, clean, slice_columns}`.
- Produces: `pub struct LogsTail`, `LogsTail::new(namespace: &str, pod: &str, ctx: &Ctx) -> Self`; `pub const MAX_LINES: usize = 5_000`.

- [ ] **Step 1: Failing tests:**
  - `lines_appear_as_they_arrive_and_the_view_follows_the_newest`
  - `scrolling_up_stops_following_and_new_lines_do_not_move_the_view`
  - `f_and_shift_g_return_to_the_newest_line`
  - `g_jumps_to_the_first_line_kept`
  - `the_buffer_keeps_the_newest_lines_and_counts_the_dropped` (5,010 lines in: 5,000 kept, the screen says `10 earlier lines dropped`)
  - `the_view_holds_its_place_when_old_lines_are_dropped` (not following: the same line stays on the top row, or the oldest kept line once it is gone)
  - `colour_codes_are_removed_and_other_escapes_are_inert`
  - `slash_filters_lines_and_shows_how_many_match`
  - `a_long_line_scrolls_sideways`
  - `a_stream_that_ends_says_the_log_ended`
  - `a_stream_that_fails_shows_the_error_and_esc_leaves`
  - `lines_of_another_stream_are_ignored`
  - `enter_on_a_pod_opens_its_log` (in `k8spods.rs`)
  - `a_tiny_area_draws_without_panicking`
- [ ] **Step 2: Run.** Expected: FAIL. **Step 3: Implement** on a `VecDeque<String>`. **Step 4: Run.** Expected: PASS. **Step 5: Commit** `feat(tui): pod log tail`.

### Task 4: Picker key, end-to-end tests, docs, release

**Files:** Modify `tui/screens/opspicker.rs` (`p`), `crates/ss-support/tests/tui.rs`, `README.md`, `CHANGELOG.md`, `CLAUDE.md`, `docs/2026-11-12-finish-roadmap.md`, `Cargo.toml` (version), `crates/ss-support/Cargo.toml` (ss-core version pin).

- [ ] **Step 1: Failing tests:** in `opspicker.rs`, `p_opens_the_pod_list` (replaces `p_does_nothing`). In `tests/tui.rs` with `FakeKubectl` through `Services::with_kubectl`:
  - `the_pod_list_opens_a_pod_log_and_esc_returns`
  - `leaving_the_log_stops_the_stream` (a port whose stream sender reports `closed()` after esc)
  - `kubernetes_not_configured_is_shown_and_the_tui_keeps_running`
  - `an_interrupt_while_tailing_exits_130`
- [ ] **Step 2: Run** `cargo test -p ss-support --test tui`. Expected: FAIL. **Step 3: Implement** the `p` key.
- [ ] **Step 4:** README key table (Operations row gains `p` pods; new rows `Pods` and `Logs`), CHANGELOG, CLAUDE.md (streams), roadmap status.
- [ ] **Step 5:** `make check`, `make msrv`. Walk the real binary on a pseudo-terminal: `p` shows the Kubernetes error on an unconfigured home and esc returns.
- [ ] **Step 6:** Version `0.1.0-alpha.6`; CHANGELOG section dated. **Commit** `feat(tui): pods and logs (0.1.0-alpha.6)`.

---

## Self-Review

- **Spec coverage:** roadmap M2 deliverable (T2, T3, T4), bounded buffer and inert escapes (T3), stream stops with the screen (T1, T4), no `kube_context` error (T2), read-only port (constraint). Open decisions are settled in Decisions.
- **Type consistency:** `Cmd::Stream`/`Msg::Lines`/`Msg::StreamEnded` are defined in T1 and consumed in T3; `Loaded::Pods` is defined in T1 and consumed in T2; `LogsTail::new` is produced in T3 and consumed by T2's screen in the same task.
- **Review Focus:** 1 → T1, T3; 2 → T1, T4; 3 → T2, T3; 4 → T2, T3; 5 → T3.
