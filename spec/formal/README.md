# Formal model of the money path (experiment, #594)

A time-boxed experiment: a small TLA+ model of admission, settlement, cancellation, expired-lease
reconciliation, operator evidence, failover, replica crashes, message loss and lease fencing.
TLC checks the **model**. The original comparison with the implementation was by inspection at
`1ec1bb23a6`. The transition table and discussion below link current implementation guards by
exact file and line. Use a single-line inline Markdown link for each source citation, with literal text from the cited line as its code label. Run `npm run test:rules`
to verify the citations after moving or changing code. The approximate lines in section 5 refer to the original experiment commit.

```sh
spec/formal/check.sh            # everything (about 9 minutes); or: safety wide live lease diag mutations assumptions witnesses
TLA_WORK=/path spec/formal/check.sh safety   # where the jar and TLC state live (default ~/.cache/omg-formal)
```

Needs `docker`, `curl` and `timeout`. TLC runs in
`eclipse-temurin@sha256:18bb4a91a3dea3f03ee31d3755af34dfb40f904fc77ab56cdc2f092e32fff9c2` with
`--network none`, `-m 3g`, `--pids-limit`, `--read-only`, `--cap-drop ALL`,
`no-new-privileges`, a wall-clock `timeout` (`TLC_TIMEOUT`, default 20 minutes per run) and
read-only mounts for specs and tools. TLC writes its state under `$TLA_WORK/state` and has a
bounded writable `/tmp`. `TLA_WORK` must be an absolute path outside the repository without `:`.
`tla2tools.jar` v1.7.4 is downloaded once through `.github/scripts/fetch-verified.sh` with
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
upstream attempt (reservation foreign key [`REFERENCES inference_executions(workspace_id,api_key_id,deployment_id,id,started_at)`](../../apps/gateway/enterprise_migrations/0030_partition_history.sql#L549)), with append-only ledger rows `hold`, `settlement`,
`unknown`, `reconciliation` (`monetary_ledger_execution_kind_key`, [`UNIQUE(execution_id,kind,admitted_at)`](../../apps/gateway/enterprise_migrations/0030_partition_history.sql#L613),
0030). The current budget totals trigger records the actual cost for settled rows
([`WHEN r.state='settled' THEN coalesce(r.actual_microusd,0)`](../../apps/gateway/enterprise_migrations/0034_history_parent_checks.sql#L162))
and the held amount for pending or unknown rows ([`ELSE coalesce(r.held_microusd,0)`](../../apps/gateway/enterprise_migrations/0034_history_parent_checks.sql#L163)).

| Transition | Writer | Guard that makes it safe |
|---|---|---|
| (none) -> `pending`, execution `started`, ledger `hold` | [`fn admit_unobserved`](../../apps/gateway/src/governance.rs#L282), one statement inserts the execution ([`WITH e AS (INSERT INTO inference_executions`](../../apps/gateway/src/governance.rs#L464)), reservation ([`INSERT INTO governance_reservations`](../../apps/gateway/src/governance.rs#L465)) and ledger hold ([`execution_id,'hold'`](../../apps/gateway/src/governance.rs#L466)); batch and line variants ([`INSERT INTO governance_reservations`](../../apps/gateway/src/governance/batch.rs#L361), [`INSERT INTO governance_reservations`](../../apps/gateway/src/governance/batch.rs#L591)) | Shared catalog and authority locks, then the totals/counter rows ([`authorize(store, &mut tx`](../../apps/gateway/src/governance.rs#L332), [`lock_rows(`](../../apps/gateway/src/governance.rs#L424)); a failover is a new admission with a new execution id (`Engine::attempt`, [`Uuid::now_v7()`](../../apps/gateway/src/inference/attempt.rs#L95) for attempt > 0) |
| `pending` -> `settled` (usage complete; or a free pre-processing rejection on a free price) / -> `unknown` (anything else, incl. cancelled, timeout, a rejection on a priced model, a 429) | `governance::finish` -> [`fn finish_unobserved`](../../apps/gateway/src/governance/settlement.rs#L562) | Reservation row `FOR UPDATE` ([`WHERE r.execution_id=$1 FOR UPDATE OF r`](../../apps/gateway/src/governance/settlement.rs#L38)), read [`r.state != "pending"`](../../apps/gateway/src/governance/settlement.rs#L623); in the statement itself [`AND state='started' RETURNING 1`](../../apps/gateway/src/governance/settlement.rs#L657) and a row-count check [`(executions, reservations, entries) != (1, 1, 1)`](../../apps/gateway/src/governance/settlement.rs#L666); ledger UNIQUE |
| `pending` -> `unknown` (lease expired), execution -> `cancelled` | `reconcile_expired` / [`fn reconcile_batch`](../../apps/gateway/src/governance/reconcile.rs#L29), every replica on the [`tokio::time::interval(Duration::from_secs(5))`](../../apps/gateway/src/maintenance.rs#L78) tick ([`reconcile_expired_leases(&store)`](../../apps/gateway/src/maintenance.rs#L87)) | [`WHERE state='pending' AND lease_expires_at<=clock_timestamp() ORDER BY lease_expires_at,execution_id LIMIT $1 FOR UPDATE SKIP LOCKED`](../../apps/gateway/src/governance/reconcile.rs#L33); [`SET state='unknown' WHERE execution_id=ANY($1) AND state='pending'`](../../apps/gateway/src/governance/reconcile.rs#L48); [`ids.len() != claimed.len()`](../../apps/gateway/src/governance/reconcile.rs#L53); [`WHERE id=ANY($1) AND state='started'`](../../apps/gateway/src/governance/reconcile.rs#L57) row count; [`state<>'settled' AND actual_microusd IS NULL`](../../apps/gateway/enterprise_migrations/0083_batch_envelope_resolution.sql#L10) because the UPDATE does not clear `actual`; holds are not touched |
| `unknown` -> `settled` (operator evidence) | [`fn resolve_usage_once`](../../apps/gateway/src/governance/reconcile.rs#L140) | [`p.role='admin'`](../../apps/gateway/src/governance/reconcile.rs#L183) re-checked in the tx; [`reservation(&mut tx, execution, scoped(store))`](../../apps/gateway/src/governance/reconcile.rs#L177) or the installation row in global mode ([`lock(&mut tx).await?`](../../apps/gateway/src/governance/reconcile.rs#L163)); read [`r.state != "unknown"`](../../apps/gateway/src/governance/reconcile.rs#L230) and require a terminal execution ([`SELECT state<>'started' FROM inference_executions`](../../apps/gateway/src/governance/reconcile.rs#L204)); the UPDATE itself has no state predicate |
| `settled` -> anything | nobody | Every writer above refuses; the database does not (section 4 assumption 2, finding 4) |
| `held` changed | settlement raises with [`greatest(held_microusd,$20)`](../../apps/gateway/src/governance/settlement.rs#L658); the batch envelope transfer (`admit_line`, [`SET held_microusd=held_microusd-$2`](../../apps/gateway/src/governance/batch.rs#L605)) moves hold into a line's own reservation; the **realtime** paths (window growth [`SET held_microusd=held_microusd+$2`](../../apps/gateway/src/governance/realtime.rs#L306) and finalization [`r.checked_add(t.actual)`](../../apps/gateway/src/governance/realtime.rs#L598)) can also change it | realtime is out of scope (section 2); `HoldNeverReleased` is stated for the request path |
| lease extended | [`fn accept_job`](../../apps/gateway/src/governance/jobs.rs#L23) for video (a batch has its final lease from admission) | [`SET lease_expires_at=greatest(lease_expires_at,$2) WHERE execution_id=$1 AND state='pending'`](../../apps/gateway/src/governance/jobs.rs#L47) |

The request thread awaits admission, then runs [`call_upstream(`](../../apps/gateway/src/inference/attempt.rs#L138) under the request deadline.
It settles the outcome ([`settle_complete(`](../../apps/gateway/src/inference/attempt.rs#L169), [`track_stream(`](../../apps/gateway/src/inference/attempt.rs#L172)).
`ExecutionGuard` marks settlement started ([`self.finished = true`](../../apps/gateway/src/inference/mod.rs#L452)) and calls
[`fn settle_to_completion`](../../apps/gateway/src/inference/attempt.rs#L569) with
[`SETTLEMENT_DEADLINE: Duration = Duration::from_secs(3)`](../../apps/gateway/src/inference/mod.rs#L366).
The pending write uses [`HandOff::new(`](../../apps/gateway/src/inference/attempt.rs#L593), whose drop path spawns the unfinished future ([`runtime.spawn(async move`](../../apps/gateway/src/inference/hand_off.rs#L75)).
[`impl Drop for ExecutionGuard`](../../apps/gateway/src/inference/mod.rs#L471) queues a detached cancellation settlement only if `finished` is false. Failover
is decided only in the `Err(error)` arm of `Engine::attempt` (so structurally never after a
stream was returned), only after settlement succeeds ([`let failing_over = settled.is_ok()`](../../apps/gateway/src/inference/attempt.rs#L154)), and only for
`Busy` or (with `allow_ambiguous_failover`) `UpstreamUnavailable`/`Overloaded`
([`crate::routing::may_failover(plan, error)`](../../apps/gateway/src/inference/attempt.rs#L157)). A failed attempt with unknown usage settles `unknown` and keeps its
hold, so a failover leaves two live holds. For a **priced** model a provider rejection (4xx) also
ends `unknown`: `free_rejection` settles it at zero only for `UpstreamRejected`/`ContextLength`
with no usage, no stream returned and a pinned price that is entirely free or not applicable
([`l.all_free_or_not_applicable()`](../../apps/gateway/src/governance/settlement.rs#L456)).

Three different "leases" exist:

* Reservation lease (`governance_reservations.lease_expires_at`): not a fencing token. It only says
  when the reconciler may take a `pending` row. Admission converts [`.request_timeout`](../../apps/gateway/src/inference/attempt.rs#L47)
  to whole seconds and adds five seconds of grace ([`+ 5,`](../../apps/gateway/src/inference/attempt.rs#L50)).
  The request deadline uses [`started + self.limits.request_timeout`](../../apps/gateway/src/inference/mod.rs#L176).
* Work leases (`work_leases`, 0029, `leases.rs`): singleton background jobs. Each job transaction
  starts with `Fence::check` -> [`SELECT omg_lease_fence($1,$2,$3)`](../../apps/gateway/src/leases.rs#L107) (epoch = fencing token); the `FOR SHARE` lives in
  the 0029 SQL ([`w.expires_at>clock_timestamp() FOR SHARE`](../../apps/gateway/enterprise_migrations/0029_work_leases.sql#L72)) and is held until commit. The money
  path itself uses none: reconciliation is a `SKIP LOCKED` queue on every replica.
* A third: the gateway-run batch runner claim (`async_jobs.runner_id` / `runner_epoch` /
  `runner_lease_until`, [`runner_epoch=runner_epoch+1`](../../apps/gateway/src/jobs/store.rs#L474)). It was unfenced when this model was written and
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
| 1 | "Every actual upstream attempt needs its own durable reservation" | `OwnReservation` | `admit_unobserved` commits ([`tx.commit().await`](../../apps/gateway/src/governance.rs#L475)) before `call_upstream`; new execution id per attempt |
| 2 | "Never delete unknown reservations"; a hold is never released before settlement or reconciliation (request path) | `UnknownRetainsHold`, `HoldNeverReleased` (action) | `reconcile_batch` only flips state; the settlement UPDATE keeps or raises `held`; docs/governance.md "Cancellation, reconciliation and retention" |
| 3 | Settlement exactly once; no un-settling | `SettleOnce`, `LedgerConsistent`, `SettledTerminal` (action) | row lock + the state reads/predicates in section 1; ledger UNIQUE; `finished` flag in `ExecutionGuard` |
| 4 | "Never treat unknown usage as zero" | `SettledMatchesTruth` | `finish_unobserved` ([`record.outcome == Outcome::Succeeded && input.is_some() && output.is_some()`](../../apps/gateway/src/governance/settlement.rs#L636)), `free_rejection` |
| 5 | A fenced runner never writes | `NoStaleCommit`, `NoReorder` (`LeaseFence`) | `omg_lease_fence` + `FOR SHARE` (0029 SQL, `Fence::check`); the batch runner had none when this was written; since 0081 it has the claim epoch checked under [`runner_id=$2 AND runner_epoch=$3 FOR KEY SHARE`](../../apps/gateway/src/jobs/fence.rs#L97) / [`runner_id=$2 AND runner_epoch=$3 FOR NO KEY UPDATE`](../../apps/gateway/src/jobs/fence.rs#L100) of the job row (#638) |
| + | "Failover ... never after a stream is returned" | `NoFailoverAfterStream` | checks the model's own guard; in the code [`return AttemptEnd::Failover(telemetry)`](../../apps/gateway/src/inference/attempt.rs#L159) exists only in the error arm |
| + | Rows coherent (pending <=> execution started) | `ExecConsistent` | `reconcile_batch` aborts on [`changed != ids.len() as u64`](../../apps/gateway/src/governance/reconcile.rs#L58) |
| + | A pending reservation always leaves `pending` (settled or unknown), under fairness | `Resolves` (liveness) | reconciler on every replica ([`crate::governance::reconcile_expired(store, 100)`](../../apps/gateway/src/maintenance.rs#L149)), restart |
| + | Dropped future still settles or stays unknown | covered by 2, 4 and `Resolves`: the cancel path ends `unknown` (never zero) or stays `pending` until the reconciler; whether upstream work is actually cancelled is **not** modelled | [`self.record(Outcome::Cancelled, None)`](../../apps/gateway/src/inference/mod.rs#L477) in `ExecutionGuard::drop` |

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

1. **Read-then-write under one lock is atomic.** Operator reconciliation [`SET state='settled',actual_microusd=$2`](../../apps/gateway/src/governance/reconcile.rs#L244) and
   the realtime finalization [`SET state=$2,actual_microusd=$3`](../../apps/gateway/src/governance/realtime.rs#L619) write the reservation with no `state` predicate
   in the statement; the guard is the earlier read under `FOR UPDATE` in the same transaction
   (scoped mode) or the installation row (global mode). Settlement has an in-statement guard on the
   execution row (`state='started'`, `(1,1,1)` check), plus its locked read. True today.
2. **The database does not enforce monotonic reservation state.** The runtime role may `UPDATE`
   `state` and `held_microusd` (`deploy/staging/runtime-grants.sql`, [`GRANT UPDATE(state,actual_microusd,`](../../deploy/staging/runtime-grants.sql#L160));
   the state/actual CHECK and ledger UNIQUE constrain the modelled writes. No general trigger
   enforces forward-only state transitions. The later explicit free-price and terminal batch
   envelope resolution paths (0082/0083) are outside the model.
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
8. **Admission inserts the execution and reservation together** (section 1). The foreign key requires every
   reservation to reference its execution. The [`PRIMARY KEY(execution_id,admitted_at)`](../../apps/gateway/enterprise_migrations/0030_partition_history.sql#L538)
   allows at most one reservation for that pair. `ExecConsistent` assumes every writer moves both together.
9. **Deadlock retry** ([`macro_rules! retry_deadlocks`](../../apps/gateway/src/governance/locks.rs#L284)) **is safe** under the guards: a retried transaction re-reads
   the row state, so a lost deadlock race behaves like a late arrival. Not modelled.
10. **Replicas only contribute `up`.** `SKIP LOCKED` claims and the lock order are database
    semantics, not checked; the model has one reconciler step that any alive replica may take.

## 5. Findings

The model checker found no violation of invariants 1 to 4 **in the model of the code as read**;
the comparison was by reading. None of the code findings below is a way to lose or double-count
money.

1. **The batch runner's lease was not a fencing token (#638): confirmed by reading; low-to-moderate,
   no money impact. Fixed in 0081; the model was not re-run for that fix.** Since 0081 the claim bumps
   `async_jobs.runner_epoch` and every runner write checks `(runner_id, runner_epoch)` inside its own
   transaction while holding the job row `FOR KEY SHARE` (or `FOR NO KEY UPDATE`), with the takeover
   claim `FOR UPDATE SKIP LOCKED`: that is the `InTx` design (check in the write transaction plus the row lock),
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
2. **A failed or timed-out settlement write turns a fully reported success into `unknown`
   (a client disconnect used to do so too, fixed in #639): confirmed by reading, by design for the
   timeout case, low severity. The client-disconnect case is
   fixed (#639): `finish` now hands a write that is still in flight when its request is dropped to a spawned
   task, so a disconnect no longer drops it (database tests drop the request, and the
   stream consumer, while the settlement waits on a row lock held by the test).** As found, `finish`
   marked `finished = true` before the write, so a client disconnect while the inline settlement was
   awaited (`settle_complete`, `track_stream`) dropped the write and `Drop` did nothing. A failed or
   timed-out write still leaves `pending`, by design (test
   [`fn diagnostic_settlement_timeout_is_not_cancellation_or_a_drop_retry`](../../apps/gateway/src/inference/tests.rs#L867) asserts no retry); the rest
   of this item describes that case and the original symptom. The reconciler then marks it
   `unknown`; the hold is kept (spend overstated) until an operator resolves it with evidence.
   `MoneyPathDiag` shows it (Receive usage, ClientCancel while `settling`, ExpireLease, Reconcile);
   the model was not changed or re-run for the fix, and in the code that trace now needs a write
   that fails or times out rather than a disconnect. With commit-ack loss modelled, a write
   that landed but looked failed is the benign counterpart: the row is already `settled` and the
   invariants hold. A cost-accuracy gap, not a safety violation.
3. **Batch and video envelopes are admitted with a 10-minute lease and extended in a second
   transaction: confirmed by reading, low. Fixed in #640; the model was not re-run for that fix.** Since #640 a
   batch's job row is written in its admission transaction ([`insert_batch(&mut tx, job`](../../apps/gateway/src/governance/batch.rs#L374)), with the envelope's final lease (poll
   deadline plus grace), and a video's lease extension and job row are one transaction. A gateway-run
   batch that an earlier release left with an `unknown` envelope now ends `failed` with
   [`Some(Stop::HoldUnavailable) => Some("hold_unavailable")`](../../apps/gateway/src/jobs/runner.rs#L575) instead of every line failing as a provider configuration error. The text
   below is the finding as made. `admit_batch` used `ADMISSION_LEASE_SECONDS = 600`
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

* Time-boxed. The citation test detects moved or changed source text. Review is still needed to
  decide whether the cited guard implements the model transition.
* Finite: one request, at most 3 attempts, 2 replicas, at most 2 crashes. No symmetry reduction.
  State counts of violating runs vary between runs (parallel workers); passing runs are exact.
* Amounts and pricing are abstract (section 4.6); the model cannot find arithmetic, valuation or
  rounding bugs. Liveness holds only under the stated weak fairness.
* Realtime, batch envelope arithmetic, native jobs, budgets, counters, authorization and the SQL
  lock mechanics are out of scope.
* No Java on the machine: TLC ran only in the pinned container.

## 7. CI decision

**Run the citation check in the existing CI rules suite. Keep full TLC runs for design review (#642).**

* Value: it states invariants 1 to 5 precisely, ties each to a guard, shows which layers hold
  `settled` final (several independent ones; no single one is needed in the model), shows the
  fence needs both the in-transaction check and `FOR SHARE`, and made finding 1 a short argument.
  It found no money bug in the model of the current code. The real race tests (`governance/tests.rs`,
  `leases/tests.rs`, the locks suite) already cover the implemented behaviour; this adds a checked
  statement of the design and is most useful *before* changing settlement, reconciliation,
  cancellation or lease code.
* Cost to maintain: two small modules (about 450 and 150 lines). Every change to the transitions in
  section 1 needs a matching spec edit and a re-run. The citation check needs only Node and the checkout.
* TLC explores the same state graph after a Rust-only or SQL-only change. The two wide runs
  alone explored 4,633,130 priced states in 169 seconds and 4,552,502 free-price states in
  210 seconds on the #642 amd64 run. All 44 checks gave their expected result in 8 min 43 s
  including setup. The pinned image occupies 116 MB on that host.
* If the models start changing frequently, a dedicated CI job would need `contents: read`,
  no write credentials, and a digest-pinned image. Setup needs network access to pull that
  image and fetch the versioned jar through `.github/scripts/fetch-verified.sh`, with the URL
  and SHA-256 literals in the workflow. Run TLC itself with `--network none`, read-only inputs
  and the resource limits in `check.sh`.
