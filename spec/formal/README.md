# Formal model of the money path (experiment, #594)

A time-boxed experiment: a small TLA+ model of admission, settlement, cancellation, expired-lease
reconciliation, operator evidence, failover, replica crashes, message loss and lease fencing.
TLC checks the **model**; the comparison between model and code was done **by reading** and
nothing mechanical ties the two together. Nothing here is wired into CI, `package.json` or the
release gates. It describes the code at commit `1ec1bb23a6` (main after #633).

Citations use symbol anchors (a function name or a quoted SQL fragment); a trailing `~N` is an
approximate line at that commit, only a hint.

```sh
spec/formal/check.sh            # everything (about 9 minutes); or: safety wide live lease diag mutations assumptions witnesses
TLA_WORK=/path spec/formal/check.sh safety   # where the jar and TLC state live (default ~/.cache/omg-formal)
```

Needs `docker`, `curl` and `timeout`. TLC runs in `eclipse-temurin@sha256:18bb4a91...` (a JRE) with
`--network none`, `-m 3g`, `--pids-limit`, `--read-only`, `--cap-drop ALL`,
`no-new-privileges`, a wall-clock `timeout` (`TLC_TIMEOUT`, default 20 minutes per run) and a
read-only copy of the specs; only `$TLA_WORK/state` is writable. `TLA_WORK` must be an absolute
path outside the repository without `:`. `tla2tools.jar` v1.7.4 (the latest stable release;
v1.8.0 is a pre-release) is downloaded once through `.github/scripts/fetch-verified.sh` with
SHA-256 `936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`. That SHA-256 is
**trust on first use**: the release publishes only a SHA-1, `bee4a54f3ee3d4afc347c3240ec2d9e93b075104`,
which matches. The jar is never committed.

Files: `MoneyPath.tla` (+ `MoneyPath.cfg` priced, `MoneyPathFree.cfg` free price,
`MoneyPathWide.cfg`, `MoneyPathLive.cfg`, `MoneyPathDiag.cfg`), `LeaseFence.tla`
(+ `LeaseFence.cfg`), `check.sh`. Mutations, assumption tests and witnesses are the same spec with
a `Mut` switch or a different invariant; `check.sh` generates their configs.

## 1. The state machine, as the code does it

One execution row (`inference_executions.state`: `started`, then `succeeded`/`failed`/`cancelled`)
and one reservation row (`governance_reservations.state`: `pending`, `unknown`, `settled`) exist per
upstream attempt (1:1 through the foreign key), with append-only ledger rows `hold`, `settlement`,
`unknown`, `reconciliation` (`monetary_ledger_execution_kind_key`, `UNIQUE(execution_id,kind,admitted_at)`,
0030). What a reservation contributes to `budget_totals` (0015, trigger-maintained): its hold while
`pending` or `unknown`, its actual cost once `settled`.

| Transition | Writer | Guard that makes it safe |
|---|---|---|
| (none) -> `pending`, execution `started`, ledger `hold` | `admit_unobserved`, the `WITH e AS (INSERT INTO inference_executions ...)` statement (`governance.rs` ~462); batch/line variants in `governance/batch.rs` | Shared catalog and authority locks, then the totals/counter rows (`authorize`, `lock_rows`); a failover is a new admission with a new execution id (`Engine::attempt`, `Uuid::now_v7()` for attempt > 0) |
| `pending` -> `settled` (usage complete; or a free pre-processing rejection on a free price) / -> `unknown` (anything else, incl. cancelled, timeout, a rejection on a priced model, a 429) | `governance::finish` -> `finish_unobserved`, one statement (`settlement.rs` ~621) | Reservation row `FOR UPDATE` (`reservation(.., lock)`), read `r.state != "pending"` under it (~587); in the statement itself `UPDATE inference_executions ... AND state='started'` and the `(1,1,1)` row-count check (~630); ledger UNIQUE |
| `pending` -> `unknown` (lease expired), execution -> `cancelled` | `reconcile_expired` / `reconcile_batch`, every replica every 5 s (`maintenance.rs` `reconcile_expired_leases`) | `SELECT ... WHERE state='pending' AND lease_expires_at<=clock_timestamp() ... FOR UPDATE SKIP LOCKED`; `UPDATE ... SET state='unknown' ... AND state='pending'`; `ids.len() != claimed.len()`; `UPDATE inference_executions ... AND state='started'` row count; the `state/actual` CHECK (0001) because the UPDATE does not clear `actual`; holds are not touched |
| `unknown` -> `settled` (operator evidence) | `resolve_usage_once` (`reconcile.rs` ~244) | Admin authority re-checked in the tx; reservation `FOR UPDATE` (scoped) or the installation row (global); read `r.state != "unknown"` and execution terminal; the UPDATE itself has no state predicate |
| `settled` -> anything | nobody | Every writer above refuses; the database does not (section 4.2, finding 4) |
| `held` changed | settlement raises with `greatest(held, floor)`; the batch envelope transfer (`admit_line`, `SET held_microusd=held_microusd-$2`) moves hold into a line's own reservation; the **realtime** paths (`governance/realtime.rs`: window growth, and finalization `held = retained + actual`) can also change it | realtime is out of scope (section 2); `HoldNeverReleased` is stated for the request path |
| lease extended | `extend_lease` (`governance/jobs.rs`) | `state='pending'`, `greatest(...)` |

The request thread (`inference/attempt.rs`): `admit`, `call_upstream` under the request deadline,
then the outcome is settled inline (`settle_complete`, `track_stream`). `ExecutionGuard::finish`
sets `finished` before it awaits the 3 s settlement (`inference/mod.rs` ~449); `impl Drop for
ExecutionGuard` queues a detached cancellation settlement only if `finished` is false. Failover
is decided only in the `Err(error)` arm of `Engine::attempt` (so structurally never after a
stream was returned), only after the previous attempt's settlement returned `Ok`, and only for
`Busy` or (with `allow_ambiguous_failover`) `UpstreamUnavailable`/`Overloaded`
(`routing::may_failover`). A failed attempt with unknown usage settles `unknown` and keeps its
hold, so a failover leaves two live holds. For a **priced** model a provider rejection (4xx) also
ends `unknown`: `free_rejection` settles it at zero only for `UpstreamRejected`/`ContextLength`
with no usage, no stream returned and a pinned price that is entirely free or not applicable
(`all_free_or_not_applicable`, `billing/v3.rs`).

Three different "leases" exist:

* Reservation lease (`governance_reservations.lease_expires_at`): not a fencing token. It only says
  when the reconciler may take a `pending` row. `lease = request_timeout + 5 s` (`Engine::admit`)
  outlives the request deadline (`execute_protocol`).
* Work leases (`work_leases`, 0029, `leases.rs`): singleton background jobs. Each job transaction
  starts with `Fence::check` -> `omg_lease_fence` (epoch = fencing token); the `FOR SHARE` lives in
  the 0029 SQL (`PERFORM 1 FROM work_leases ... FOR SHARE`) and is held until commit. The money
  path itself uses none: reconciliation is a `SKIP LOCKED` queue on every replica.
* A third: the gateway-run batch runner claim (`async_jobs.runner_id` / `runner_epoch` /
  `runner_lease_until`, `store::claim_gateway`). It was unfenced when this model was written and
  is fenced since 0081 (#638): see finding 1.

## 2. What is modelled, and what is not

`MoneyPath.tla`: one client request with up to 2 attempts (3 in the wide run), 2 replicas (one
serves, any alive one reconciles), a streaming flag, a priced or free pricing world
(`FreePrice`), an environment upstream that may never receive the request, may reject it (a 4xx
or a 429) or may bill and lose the answer, client cancellation at any point, settlement writes
that fail, time out or are dropped mid-write **and may or may not have committed** (commit-ack
loss, modelled both ways), replica crash/restart, lease expiry, the reconciler, operator evidence
and failover. Admission, settlement, reconcile and evidence are one atomic step each (each is one
transaction in the code; `Applies`/`ReconcileOK` model its guards layer by layer). Amounts are two
numbers, `HOLD = 2` and `ACTUAL = 1`; "truth" is what the provider really billed.

`LeaseFence.tla`: two replicas, one lease row with epoch, local belief, check-then-write,
`omg_lease_fence` with and without `FOR SHARE`, expiry, takeover, crash. Four designs: `InTx`
(0029 as built), `InTxNoShare`, `Local`, `RunnerFlag` (the batch runner as it was coded before
0081; the runner now corresponds to `InTx`, see finding 1).

Deliberately left out: budgets, limits, rate/in-flight counters and the trigger-maintained totals
(the model computes spend from the rows; `budget verify` is the check for the triggers); pricing
arithmetic, partial usage, held-floor raising, unbounded/unpriced cost; more than one request, so no
cross-request interference; **realtime** windows and finalization (`governance/realtime.rs`), the
batch envelope and its line transfers, native and video jobs and their lease extension;
authorization, revocation and catalog locks; the database locking mechanics; deadlock retry;
external side effects of leased jobs (audit export, webhooks, file deletion).

## 3. Invariants and results

All runs below gave the expected result in `check.sh all` (about 9 minutes on 4 cores, 3 GB; the
wide runs dominate).

| # | Invariant (AGENTS.md sentence) | TLC name | Code that is meant to guarantee it |
|---|---|---|---|
| 1 | "Every actual upstream attempt needs its own durable reservation" | `OwnReservation` | `admit_unobserved` commits before `call_upstream`; new execution id per attempt |
| 2 | "Never delete unknown reservations"; a hold is never released before settlement or reconciliation (request path) | `UnknownRetainsHold`, `HoldNeverReleased` (action) | `reconcile_batch` only flips state; the settlement UPDATE keeps or raises `held`; docs/governance.md "Cancellation, reconciliation and retention" |
| 3 | Settlement exactly once; no un-settling | `SettleOnce`, `LedgerConsistent`, `SettledTerminal` (action) | row lock + the state reads/predicates in section 1; ledger UNIQUE; `finished` flag in `ExecutionGuard` |
| 4 | "Never treat unknown usage as zero" | `SettledMatchesTruth` | `finish_unobserved` (`let actual = if Succeeded && input/output present ... else if free && !violated ...`), `free_rejection` |
| 5 | A fenced runner never writes | `NoStaleCommit`, `NoReorder` (`LeaseFence`) | `omg_lease_fence` + `FOR SHARE` (0029 SQL, `Fence::check`); **not** present for the batch runner |
| + | "Failover ... never after a stream is returned" | `NoFailoverAfterStream` | checks the model's own guard; in the code failover exists only in the `Err` arm of `Engine::attempt` |
| + | Rows coherent (pending <=> execution started) | `ExecConsistent` | `reconcile_batch` aborts when it is not |
| + | A pending reservation always leaves `pending` (settled or unknown), under fairness | `Resolves` (liveness) | reconciler on every replica, restart |
| + | Dropped future still settles or stays unknown | covered by 2, 4 and `Resolves`: the cancel path ends `unknown` (never zero) or stays `pending` until the reconciler; whether upstream work is actually cancelled is **not** modelled | `impl Drop for ExecutionGuard` |

| Config | Result | Distinct states | Depth | Time |
|---|---|---|---|---|
| `MoneyPath.cfg` (priced; 2 attempts, 2 replicas, 1 crash) | no error | 81,446 (273,440 generated) | 21 | 6 s |
| `MoneyPathFree.cfg` (free price) | no error | 80,026 | 21 | 6 s |
| `MoneyPathWide.cfg` (priced; 3 attempts, 2 crashes) | no error | 4,633,130 (23,010,596 generated) | 32 | about 200 s |
| same, free price | no error | 4,552,502 | 32 | about 195 s |
| `MoneyPathLive.cfg` (+ `Resolves`, weak fairness), priced / free | no error | 81,446 / 80,026 | 21 | 15 s each |
| `LeaseFence.cfg` (`InTx`, no local-clock assumption) | no error | 4,758 | 16 | 3 s |
| same with `TrustMargin = TRUE` | no error | 1,690 | 17 | 2 s |
| `MoneyPathDiag.cfg` (`KnownUsageSettled`) | **violated**, expected | about 1,500 | 9 | 2 s |

### Mutations

Each deliberate bug is a `Mut` value. Where the code has several independent layers, a mutation
removes ONE layer (or a listed set), and the table says which removals TLC still catches and which
another layer masks. Traces are the action sequences TLC found; attempt numbers in brackets.

| Mutation (`Mut`) | Result | Shortest trace found |
|---|---|---|
| `reuse_reservation`: failover reuses attempt 1's reservation | `OwnReservation` | Admit[1], Dispatch[1], ConnError[1] (answer lost), SettleExplicit[1] (`unknown`), Failover, Admit[2] (no row written), Dispatch[2]: the upstream receives attempt 2 with no reservation |
| `release_on_cancel`: cancellation settlement zeroes the hold | `UnknownRetainsHold`; with state invariants off, `HoldNeverReleased` | Admit, ClientCancel, CancelSettle |
| `reconcile_releases_hold`: reconciler also frees the hold | same two | Admit, ClientCancel, ExpireLease, Reconcile |
| `unknown_as_zero`: failed attempt without usage settles at zero | `SettledMatchesTruth` | Admit, Dispatch, Timeout, SettleExplicit: settled 0 while the upstream may still bill |
| `failover_after_stream` | `NoFailoverAfterStream` (the model's own guard) | Admit, Dispatch, Process (billed), Deliver (stream returned), ConnError, SettleExplicit, Failover |
| `reconcile_no_pred` / `_no_exec` / `_no_check` / `_no_pred_exec` (one layer, or the first two, removed from the reconcile flip of a settled row) | **masked**: still passes | the remaining layers (execution `state='started'`, the CHECK, the predicate) refuse |
| `reconcile_no_pred_exec_check` (all three removed) | `SettledTerminal` | Admit, Dispatch, Process, Deliver, Receive, SettleExplicit (`settled`), ExpireLease, Reconcile: `settled` becomes `unknown` |
| `settle_no_pred` / `_no_exec` / `_no_uniq` / `_no_pred_exec` (one layer, or pred+exec, removed from the settlement transaction) | **masked**: still passes | row state, execution state and the ledger UNIQUE each refuse a stale write |
| `settle_no_pred_exec_uniq` (all three removed) | `SettledTerminal` | Admit, ClientCancel (detached settle queued), ExpireLease, Reconcile, Evidence (`settled`), CancelSettle: the late detached settle un-settles it |
| `LeaseFence` `InTxNoShare`: fence check in the tx, no `FOR SHARE` | `NoStaleCommit` | Acquire[r1], Check[r1], TrustLapse[r1], Fence[r1] (passes), Expire, Acquire[r2] (takeover not blocked), Commit[r1] |
| `LeaseFence` `Local`: check the local term, write without a database check | `NoStaleCommit` | Acquire[r1], Check[r1], TrustLapse[r1], Expire, Acquire[r2], Commit[r1] |
| liveness: `reconciler_off` | `Resolves` | Admit, ClientCancel, ExpireLease, Crash, Restart: the reservation stays `pending` |

So the mutations show **defence in depth**, not that every predicate alone is load-bearing: in the
model, no single guard of the reconcile flip or of settlement is needed for the invariants while
the others stand (the reconcile flip needs three independent layers to fail; the late-settle case
three as well). They do show that those layers together are what keeps `settled` final, and what a
change that removed several would expose. The same is why section 5 finding 4 suggests a database
trigger as a further layer.

Assumption tests: `early_expiry` (a lease may expire while the request thread is alive, A1 off)
still passes, so A1 is not needed for safety; `evidence_early` (the operator settles before the
upstream is final, A3 off) fails `SettledMatchesTruth`.

Reachability witnesses (`W_*`, each expected to be violated, so the scenario is reachable):
failover, a settled stream, a billed attempt left `unknown` after a lost answer, operator
evidence, cancel ending `unknown`, the reconciler moving a dead request's reservation, two live
holds after failover, a free rejection settling at zero (free price), a priced rejection ending
`unknown`, a 429 failing over, a settlement that committed while the caller saw a failure, a crash
leaving `pending`.

## 4. Where the model assumes, and where the code relies on something

Assumptions of the model (some are also properties of the code):

1. **Read-then-write under one lock is atomic.** `resolve_usage_once` (`reconcile.rs` ~244) and
   the realtime finalization (`realtime.rs` ~619) write the reservation with no `state` predicate
   in the statement; the guard is the earlier read under `FOR UPDATE` in the same transaction
   (scoped mode) or the installation row (global mode). Settlement has an in-statement guard on the
   execution row (`state='started'`, `(1,1,1)` check), plus its locked read. True today.
2. **The database does not enforce monotonic reservation state.** The runtime role may `UPDATE`
   `state` and `held_microusd` (`deploy/staging/runtime-grants.sql`, `GRANT UPDATE(state,actual_microusd,...)`);
   the only structural guards are the `state/actual` CHECK (0001) and the ledger UNIQUE. No trigger
   forbids `settled`/`unknown` going backwards or a hold shrinking. Nine statements in five files
   update the table (`governance/{batch,reconcile,settlement,realtime,jobs}.rs`); each is guarded in code.
3. **A1: the lease outlives the request thread.** The model passes without it, so it only decides
   whether a slow attempt ends `settled` or `unknown`. A process stalled between admission and
   dispatch can still send the request after the reconciler took the row (tokio's `timeout` polls
   its inner future first); the result is a billed, `unknown` attempt with its hold retained.
4. **A3: evidence is final.** `resolve_usage` requires a terminal execution but cannot know the
   upstream stopped; the model fails when evidence arrives while the upstream is still billing.
5. **Message loss is permanent and the upstream is never cancelled.** The model lets the upstream
   bill after any gateway event; AGENTS.md's "dropping a future must cancel upstream work" is not
   modelled, only its accounting consequence (the attempt ends `unknown`).
6. **Abstract amounts.** `ACTUAL < HOLD` and `held` is `0` or `HOLD`. The code allows `actual >
   held` (complete usage settles above the reservation) and raises holds (`greatest(held, floor)`);
   none of that is checked.
7. **The database clock alone judges lease expiry** (`clock_timestamp()`); there is no replica
   clock in the money path model. The batch/work-lease local trust (A2, `MARGIN`) is modelled only
   in `LeaseFence`; `InTx` does not need it, `Local` and the batch runner do and still fail.
8. **Execution and reservation rows are 1:1** through the foreign key; `ExecConsistent` assumes
   every writer moves both together.
9. **`retry_deadlocks!` re-execution is safe** under the guards: a retried transaction re-reads
   the row state, so a lost deadlock race behaves like a late arrival. Not modelled.
10. **Replicas only contribute `up`.** `SKIP LOCKED` claims and the lock order are database
    semantics, not checked; the model has one reconciler step that any alive replica may take.

## 5. Findings

The model checker found no violation of invariants 1 to 4 **in the model of the code as read**;
the comparison was by reading. None of the code findings below is a way to lose or double-count
money.

1. **The batch runner's lease was not a fencing token (#638): confirmed by reading; low-to-moderate,
   no money impact. Fixed in 0081; the model was not re-run.** Since 0081 the claim bumps
   `async_jobs.runner_epoch` and every runner write checks `(runner_id, runner_epoch)` inside its own
   transaction while holding the job row `FOR SHARE` (or `FOR NO KEY UPDATE`), with the takeover
   claim `SKIP LOCKED`: that is the `InTx` design (check in the write transaction plus the row lock),
   not `RunnerFlag` (a local flag set by the renewal task). `RunnerFlag` keeps modelling the code as
   it was before the fix, which is why the model still reproduces the finding. The text below is the
   finding as made. Invariant 5 holds for `work_leases` (the model confirms the fence needs both
   the in-transaction check and `FOR SHARE`, and not the local clock) but not for the gateway-run
   batch runner. TLC, mode `RunnerFlag`, five steps: runner S claims the batch (`claim_gateway`),
   its lease lapses (renewal every 15 s, lease 60 s), runner N claims it, S writes. In the code: (a)
   S stalls 60 s or more (SIGSTOP, VM freeze, starvation) or its renewals fail (`Runner::run`'s
   renewal task ignores `Err(_)` and keeps going; unlike `Leases::renew_once` there is no local
   trust deadline); (b) N's tick claims the batch and `resume` calls `interrupt_running`, marking
   S's in-flight lines `interrupted`; (c) S wakes; before its renewal task gets `None` and sets
   `lost`, a dispatch pass reads `should_stop` as false and writes. No runner id or epoch is on any
   of these statements. The review added the concrete effects:
   * `write_segments` computes `next_seq` once from `store::segments` and only advances it when
     `store_segment` succeeds; with `PRIMARY KEY(job_id, seq)` on `batch_segments` (0021), the runner
     that loses a `seq` collision never advances and loses every later flush.
   * `progress` is additive with no line identity: `LineRun::finished` (`runner/line.rs` ~93) adds a
     completion for a line that `Runner::resume` (`runner.rs` ~316) already counted as interrupted,
     so `request_completed + request_failed` can exceed the total for a while.
   * `finish_line` only matches `state='running'`, so a line that was billed and settled can be shown
     as failed/unavailable.
   * "Two runners finalize one batch" is only duplicate result files and duplicate `close_batch`
     calls: `finish_batch` is state-guarded (`WHERE state IN('queued','in_progress')`) and
     `close_batch` is `state='pending'`-guarded and idempotent.
   * Money stays safe and no second provider call per line is possible: each line has its own
     reservation (`admit_line` locks the envelope `FOR UPDATE` and requires it pending), and the line
     claim is `INSERT ... ON CONFLICT DO NOTHING` (`store::claim_line`).
   Suggested fix: carry `runner_id` (and a bumped epoch) in every runner write, or fence them like
   0029.
2. **A cancelled or failed settlement write turns a fully reported success into `unknown`:
   confirmed by reading, by design for the timeout case, low severity.** `finish` marks
   `finished = true` before the write, so a client disconnect while the inline settlement is awaited
   (`settle_complete`, `track_stream`) drops the write and `Drop` does nothing; a failed or timed-out
   write also leaves `pending` (test `diagnostic_settlement_timeout_is_not_cancellation_or_a_drop_retry`
   asserts no retry). The reconciler then marks it `unknown`; the hold is kept (spend overstated)
   until an operator resolves it with evidence. `MoneyPathDiag` shows it (Receive usage,
   ClientCancel while `settling`, ExpireLease, Reconcile). With commit-ack loss modelled, a write
   that landed but looked failed is the benign counterpart: the row is already `settled` and the
   invariants hold. A cost-accuracy gap, not a safety violation.
3. **Batch and video envelopes are admitted with a 10-minute lease and extended in a second
   transaction: confirmed by reading, low.** `admit_batch` uses `ADMISSION_LEASE_SECONDS = 600`
   (`jobs/batch.rs`), then `insert_batch`, then `hold_until` -> `extend_lease`. If the process dies
   or the extension fails (it only logs) the reconciler marks the envelope `unknown` after 10
   minutes while the job row stays queued; `admit_line` then refuses every line (`state != "pending"`)
   and `close_batch` returns false. Money is safe; the batch is a zombie until an operator acts.
   Outside the model; found by the comparison.
4. **Hardening suggestion, not a bug:** a `BEFORE UPDATE` trigger on `governance_reservations`
   refusing `settled -> *`, `unknown -> pending` and a shrinking `held_microusd` unless the new state
   is `settled` would make invariants 2 and 3 a database guarantee, one more layer in a design
   whose model needs three independent failures before `settled` can be undone. It must allow the
   envelope transfer and realtime growth.

## 6. Limits of the experiment

* Time-boxed. The model is hand-written from reading the code; nothing checks it against the code.
  A mismatch would show only on review. The anchors above are the only drift guard.
* Finite: one request, at most 3 attempts, 2 replicas, at most 2 crashes. No symmetry reduction.
  State counts of violating runs vary between runs (parallel workers); passing runs are exact.
* Amounts and pricing are abstract (section 4.6); the model cannot find arithmetic, valuation or
  rounding bugs. Liveness holds only under the stated weak fairness.
* Realtime, batch envelope arithmetic, native jobs, budgets, counters, authorization and the SQL
  lock mechanics are out of scope.
* No Java on the machine: TLC ran only in the pinned container.

## 7. Recommendation

**Keep as documentation and as a design-review tool; do not add it to CI now.**

* Value: it states invariants 1 to 5 precisely, ties each to a guard, shows which layers hold
  `settled` final (several independent ones; no single one is needed in the model), shows the
  fence needs both the in-transaction check and `FOR SHARE`, and made finding 1 a short argument.
  It found no money bug in the model of the current code. The real race tests (`governance/tests.rs`,
  `leases/tests.rs`, the locks suite) already cover the implemented behaviour; this adds a checked
  statement of the design and is most useful *before* changing settlement, reconciliation,
  cancellation or lease code.
* Cost to maintain: two small modules (about 450 and 150 lines). Every change to the transitions in
  section 1 needs a matching spec edit and a re-run (9 minutes). The weak point is drift; a cheap
  mitigation is a test that checks every cited anchor still appears in its file.
* To put it in CI, `check.sh` cannot run as is. CI downloads must go through
  `.github/scripts/fetch-verified.sh` (docs/verification.md, "Run-time downloads go through one
  script") with the URL and SHA-256 as literals in the workflow, in a read-only job (`contents:
  read`, no third-party action); the JRE image must be pinned by digest as `check.sh` does and
  pulled in that job; and the jar's SHA-256 would be trust-on-first-use (the release publishes only
  a SHA-1, which matches). Run time about 9 minutes, 3 GB, image about 200 MB. Reconsider it if the
  batch runner gets a fence, if realtime or async-job accounting is modelled, or if the money path
  changes often enough that a regression could slip past the integration tests.
