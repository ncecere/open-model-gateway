# Formal model of the money path (experiment, #594)

A time-boxed experiment: a small TLA+ model of admission, settlement, cancellation, expired-lease
reconciliation, operator evidence, failover, replica crashes, message loss and lease fencing,
checked with TLC and compared with the code. Nothing here is wired into CI, `package.json` or
the release gates. It models the code at commit `1ec1bb23a6` (main after #633); line numbers
below are from that commit.

```sh
spec/formal/check.sh            # everything (about 3 minutes); or: safety wide live lease diag mutations assumptions witnesses
TLA_WORK=/path spec/formal/check.sh safety   # where the jar and TLC state live (default ~/.cache/omg-formal)
```

Needs `docker` and `curl`. TLC runs in `eclipse-temurin@sha256:18bb4a91...` (a JRE) with
`--network none`, `-m 3g`, `-XX:MaxRAMPercentage=70` and a read-only copy of the specs.
`tla2tools.jar` v1.7.4 (the latest stable release; v1.8.0 is a pre-release) is fetched once
from the official GitHub release and checked against SHA-256
`936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`; the release page
publishes SHA-1 `bee4a54f3ee3d4afc347c3240ec2d9e93b075104` for it, which also matches (the
release has no SHA-256). The jar is never committed.

Files: `MoneyPath.tla` (+ `MoneyPath.cfg`, `MoneyPathWide.cfg`, `MoneyPathLive.cfg`,
`MoneyPathDiag.cfg`), `LeaseFence.tla` (+ `LeaseFence.cfg`), `check.sh`. Mutations, assumption
tests and witnesses are the same spec with a `Mut` switch or a different invariant; `check.sh`
generates their configs.

## 1. The state machine, as the code does it

One execution row (`inference_executions.state`: `started`, then `succeeded`/`failed`/`cancelled`)
and one reservation row (`governance_reservations.state`: `pending`, `unknown`, `settled`) exist per
upstream attempt, with append-only ledger rows `hold`, `settlement`, `unknown`, `reconciliation`
(`UNIQUE(execution_id,kind,admitted_at)`, `0030_partition_history.sql:613`). What a reservation
contributes to `budget_totals` (0015, trigger-maintained): its hold while `pending` or `unknown`,
its actual cost once `settled`.

| Transition | Writer | Guard that makes it safe |
|---|---|---|
| (none) -> `pending`, execution `started`, ledger `hold` | `admit_unobserved`, one statement (`governance.rs:462-467`); batch/line/video variants `governance/batch.rs:332-346, 506-520` | Shared catalog and authority locks, then the totals/counter rows (`governance.rs:332, 422`); a failover is a new admission with a new execution id (`inference/attempt.rs:90-94`) |
| `pending` -> `settled` (usage complete, or a free pre-processing rejection) / -> `unknown` (anything else, incl. cancelled and timeout) | `governance::finish`, one statement (`governance/settlement.rs:621-629`) | Reservation row `FOR UPDATE` (`:37, 566`), read `state='pending'` under it (`:587`), `inference_executions ... state='started'` in the statement and `(1,1,1)` row-count check (`:621, 630`), ledger UNIQUE |
| `pending` -> `unknown` (lease expired), execution -> `cancelled` | `reconcile_expired`, every replica every 5 s (`governance/reconcile.rs:28-75`, `maintenance.rs:87`) | `SELECT ... state='pending' AND lease_expires_at<=now ... FOR UPDATE SKIP LOCKED` (`:32`), `UPDATE ... state='pending'` (`:47`), claimed==updated (`:52`), executions `state='started'` row count (`:56-59`); holds are not touched |
| `unknown` -> `settled` (operator evidence) | `resolve_usage` (`reconcile.rs:119-258`) | Admin authority re-checked in the tx; reservation `FOR UPDATE` (scoped) or the installation row (global); read `state='unknown'` and execution terminal (`:230`); the UPDATE itself (`:244`) has no state predicate |
| `settled` -> anything | nobody | Every writer above refuses a non-`pending`/non-`unknown` row; the database does not (section 4.2, finding 4) |
| `held` lowered | only settlement/evidence, and the batch envelope transfer `governance/batch.rs:521-528` (moves hold into a line's own reservation) | |
| lease extended | `extend_lease` (`governance/jobs.rs:13`) | `state='pending'`, `greatest(...)` |

The request thread (`inference/attempt.rs`): admit (`:97`), `call_upstream` under the request
deadline (`:136`), then settle the outcome inline (`:149, 167, 513, 528`). `ExecutionGuard::finish`
sets `finished` before it awaits the 3 s settlement (`inference/mod.rs:449`); `Drop` queues a
detached cancellation settlement only if `finished` is false (`:503-546`). Failover happens only
after the previous attempt's settlement returned `Ok`, only for `Busy` or (with
`allow_ambiguous_failover`) `UpstreamUnavailable`/`Overloaded`, and never once a stream was
returned (`attempt.rs:152-158`, `routing.rs:392-402`, `attempt.rs:466`). A failed attempt with
unknown usage settles `unknown` and keeps its hold; so the failover leaves two live holds.

Two different "leases" exist and the issue's wording covers both:

* Reservation lease (`governance_reservations.lease_expires_at`): not a fencing token. It only says
  when the reconciler may take a `pending` row. `lease = request_timeout + 5 s` (`attempt.rs:44-47`)
  outlives the request deadline (`inference/mod.rs:175`).
* Work leases (`work_leases`, 0029, `leases.rs`): singleton background jobs. Each job transaction
  starts with `omg_lease_fence` (epoch = fencing token) and holds the row `FOR SHARE` until commit.
  The money path itself uses none: reconciliation is a `SKIP LOCKED` queue on every replica.
* A third, unfenced one: the gateway-run batch runner lease (`async_jobs.runner_id` /
  `runner_lease_until`, `jobs/store.rs:422-441`). See finding 1.

## 2. What is modelled, and what is not

`MoneyPath.tla`: one client request with up to 2 attempts (3 in the wide run), 2 replicas (one
serves, both reconcile), a streaming flag, an environment upstream that may never receive the
request or may bill and lose the answer, client cancellation at any point (also during the
settlement write), failed settlement writes (the 3 s deadline), replica crash/restart, lease
expiry, the reconciler, operator evidence and failover. Admission, settlement, reconcile and
evidence are one atomic step each (each is one transaction in the code, `Applies` models its
guards). Amounts are two numbers, `HOLD = 2` and `ACTUAL = 1`; "truth" is what the provider
really billed.

`LeaseFence.tla`: two replicas, one lease row with epoch, local belief, check-then-write,
`omg_lease_fence` with and without `FOR SHARE`, expiry, takeover, crash. Four designs: `InTx`
(0029 as built), `InTxNoShare`, `Local`, `RunnerFlag` (the batch runner as coded).

Deliberately left out: budgets, limits, rate/in-flight counters and the trigger-maintained totals
(the model computes spend from the rows; `budget verify` is the check for the triggers); pricing,
partial usage, held-floor raising (`greatest(held,floor)`), unbounded/unpriced cost; more than one
request, so no cross-request interference; realtime windows (`governance/realtime.rs`), the batch
envelope and its line transfers, native and video jobs and their lease extension; authorization,
revocation and catalog locks; the DB locking mechanics (assumed to give each transaction a
consistent read-modify-write, see assumptions); external side effects of leased jobs (audit export,
webhooks, file deletion).

## 3. Invariants and results

All runs below gave the expected result in `check.sh all` (about 3 minutes, 4 cores, 3 GB).

| # | Invariant (AGENTS.md sentence) | TLC name | Code that is meant to guarantee it |
|---|---|---|---|
| 1 | "Every actual upstream attempt needs its own durable reservation" | `OwnReservation` | `governance.rs:462-473` before `call_upstream`; new execution id per attempt (`attempt.rs:90-94`) |
| 2 | "Never delete unknown reservations"; a hold is never released before settlement or reconciliation | `UnknownRetainsHold`, `HoldNeverReleased` (action) | reconcile flips state only (`reconcile.rs:47`); settlement raises or keeps `held` (`settlement.rs:622`); docs/governance.md "Cancellation, reconciliation and retention" |
| 3 | Settlement exactly once; no un-settling | `SettleOnce`, `LedgerConsistent`, `SettledTerminal` (action) | row lock + `state` reads/predicates above; ledger UNIQUE; `finished` flag in `ExecutionGuard` |
| 4 | "Never treat unknown usage as zero" | `SettledMatchesTruth` | `settlement.rs:600-606` (only complete usage or a free rejection settles), `free_rejection` `:431-456` |
| 5 | A fenced runner never writes | `NoStaleCommit`, `NoReorder` (`LeaseFence`) | `omg_lease_fence` + `FOR SHARE` (`0029:73-81`, `leases.rs:106`); **not** present for the batch runner |
| + | "Failover ... never after a stream is returned" | `NoFailoverAfterStream` | `attempt.rs:152-158`, `routing.rs:392` |
| + | Rows coherent (pending <=> execution started) | `ExecConsistent` | `reconcile.rs:56-59` aborts when it is not |
| + | A pending reservation always leaves `pending` (settled or unknown), under fairness | `Resolves` (liveness) | reconciler on every replica, restart |
| + | Dropped future still settles or stays unknown | covered by 2, 4 and `Resolves`: the cancel path ends `unknown` (never zero) or stays `pending` until the reconciler | `mod.rs:503-546` |

| Config | Result | Distinct states | Depth | Time |
|---|---|---|---|---|
| `MoneyPath.cfg` (2 attempts, 2 replicas, 1 crash) | no error | 46,000 (155,180 generated) | 21 | 4 s |
| `MoneyPathWide.cfg` (3 attempts, 2 crashes) | no error | 2,103,632 (10,571,556 generated) | 32 | about 90 s |
| `MoneyPathLive.cfg` (+ `Resolves` under weak fairness) | no error | 46,000 | 21 | 11 s |
| `LeaseFence.cfg` (`InTx`, no local-clock assumption) | no error | 4,758 | 16 | 2 s |
| same with `TrustMargin = TRUE` | no error | 1,690 | 17 | 2 s |
| `MoneyPathDiag.cfg` (`KnownUsageSettled`) | **violated**, expected | about 1,100 | 9 | 2 s |

### Mutations: each deliberate bug in the model produces a counterexample

Attempt numbers in brackets. `Crash`/`ExpireLease` are environment steps.

| Mutation (`Mut`) | Invariant that fires | Shortest trace found |
|---|---|---|
| `reuse_reservation`: failover reuses attempt 1's reservation | `OwnReservation` | Admit[1], Dispatch[1], ConnError[1] (answer lost), SettleExplicit[1] (`unknown`), Failover, Admit[2] (no row written), Dispatch[2]: the upstream receives attempt 2 with no reservation |
| `release_on_cancel`: cancellation settlement zeroes the hold | `UnknownRetainsHold`, and `HoldNeverReleased` alone | Admit, ClientCancel, CancelSettle: `unknown` with `held = 0`, spend dropped without evidence |
| `reconcile_releases_hold`: reconciler also frees the hold | `UnknownRetainsHold`, and `HoldNeverReleased` alone | Admit, ClientCancel, ExpireLease, Reconcile |
| `reconcile_no_state_guard`: reconciler without `state='pending'` (and without the execution row-count check) | `SettledTerminal` | Admit, Crash, ExpireLease, Reconcile (`unknown`), Evidence (`settled`), Reconcile again: `settled` becomes `unknown`, a settled lease row is still "expired" |
| `settle_no_state_check`: settlement without the row-state/ledger guards | `SettledTerminal` | Admit, ClientCancel (detached settle queued), ExpireLease, Reconcile, Evidence (`settled`), CancelSettle: the late detached settle un-settles it |
| `unknown_as_zero`: failed attempt without usage settles at zero | `SettledMatchesTruth` | Admit, Dispatch, Timeout, SettleExplicit: settled 0 while the upstream may still bill |
| `failover_after_stream` | `NoFailoverAfterStream` | Admit, Dispatch, Process (billed), Deliver (stream returned), ConnError, SettleExplicit, Failover |
| `LeaseFence` `InTxNoShare`: fence check in the tx, no `FOR SHARE` | `NoStaleCommit` | Acquire[r1], Check[r1], TrustLapse[r1], Fence[r1] (passes), Expire, Acquire[r2] (takeover not blocked), Commit[r1] |
| `LeaseFence` `Local`: check the local term, write without a database check | `NoStaleCommit` | Acquire[r1], Check[r1], TrustLapse[r1], Expire, Acquire[r2], Commit[r1] |
| liveness: `reconciler_off` | `Resolves` | Admit, ClientCancel, ExpireLease, Crash, Restart: the reservation stays `pending` |

Assumption tests: `early_expiry` (a lease may expire while the request thread is alive, A1
off) still passes, so A1 is not needed for safety; `evidence_early` (the operator settles before
the upstream is final, A3 off) fails `SettledMatchesTruth`.

Reachability witnesses (`W_*`, each expected to be violated, so the scenario is reachable):
failover, a settled stream, a billed attempt left `unknown` after a lost answer, operator
evidence, cancel ending `unknown`, the reconciler moving a dead request's reservation, two live
holds after failover, a free rejection settling at zero, a crash leaving `pending`.

## 4. Where the code relies on something the model had to assume

1. **Read-then-write under one lock is atomic.** Settlement, `resolve_usage` (`reconcile.rs:244`)
   and realtime (`realtime.rs:619`) write the reservation with no `state` predicate; the guard is
   the earlier read under `FOR UPDATE` in the same transaction (scoped mode) or the installation
   row (global mode), plus, for settlement, `executions ... state='started'` in the statement and
   the `(1,1,1)` check. True today; it is the reason the `*_no_state_*` mutations are real
   exposures, not artefacts.
2. **The database does not enforce monotonic reservation state.** The runtime role may `UPDATE`
   `state` and `held_microusd` (`deploy/staging/runtime-grants.sql:159-161`); the only structural
   guards are the `state='settled' <=> actual present` CHECK (`0001:142`) and the ledger UNIQUE.
   There is no trigger forbidding `settled`/`unknown` going backwards or a hold shrinking.
   Nine statements in five files update the table (`governance/{batch,reconcile,settlement,realtime,jobs}.rs`);
   each is guarded in code. The two mutations show what one missing guard would do.
3. **A1: the lease outlives the request thread.** The model passes without it, so it only
   decides whether a slow attempt ends `settled` or `unknown`, not whether money is lost.
   A process stalled between admission and dispatch can still send the request after the
   reconciler took the row (`timeout_at` polls its inner future first); the result is a billed,
   `unknown` attempt with its hold retained.
4. **A3: evidence is final.** `resolve_usage` requires a terminal execution (`:230`) but cannot
   know the upstream stopped; the model fails when evidence arrives while the upstream is still
   billing. It is an operator-input assumption.
5. **Message loss is permanent and the upstream is not cancelled by the gateway.** Dropping a
   stream cannot guarantee zero charges (docs/governance.md); the model lets the upstream bill
   after any gateway event.
6. **Local term belief (A2, `leases.rs:32`).** `InTx` does not need it (passes with it off);
   `Local` and the batch runner do and still fail.

## 5. Findings

None of these is a way to lose or double-count money. The model found no violation of
invariants 1 to 4 in the code as read.

1. **The batch runner's lease is not a fencing token: confirmed by reading; impact low-to-moderate,
   no money impact.** Invariant 5 holds for `work_leases` (the model confirms the fence needs
   both the in-transaction check and `FOR SHARE`, and not the local clock) but not for the
   gateway-run batch runner. TLC, mode `RunnerFlag`, five steps: runner S claims the batch
   (`store.rs:428`), its lease lapses (renewal every 15 s, lease 60 s, `runner.rs:70-72`), runner
   N claims it (`store.rs:428`), S writes. In the code: (a) S stalls 60 s or more (SIGSTOP, VM
   freeze, starvation) or its renewals fail (`runner.rs:232` ignores renewal errors and keeps
   going; unlike `leases.rs:240` there is no local trust deadline); (b) N's tick claims the batch and
   `resume` calls `interrupt_running`, marking S's in-flight lines `interrupted` (`runner.rs:314`,
   `store.rs:454-461`); (c) S wakes; before its renewal task gets `None` and sets `lost`
   (`runner.rs:228-231`) a dispatch pass reads `should_stop` as false (`dispatch.rs:34, 41`) and
   writes: `claim_line`, `admit_line`, `progress` (additive, no line identity, `store.rs:386`),
   `insert_segment`, `finish_line` (`WHERE state='running'`, so the result of a line that was
   billed and settled is dropped, `store.rs:496`), and finalization (`runner.rs:297` is a local
   flag read, then `finish_batch`, `close_batch`, segment deletion). No runner id or epoch is
   on any of these statements. Money stays safe because each line has its own reservation
   (`admit_line` locks the envelope `FOR UPDATE` and requires it pending, `governance/batch.rs:422-428`),
   a line runs once (`INSERT ... ON CONFLICT DO NOTHING`, `store.rs:475`), and `close_batch` is
   `state='pending'`-guarded and idempotent. Effects: lost results for billed lines, transient
   wrong progress counters, two runners finalizing one batch. Exposure needs a stall of 60 s or
   more plus a window of a round trip. Suggested follow-up issue: carry `runner_id` (and a bumped
   epoch) in every runner write, or fence them with `omg_lease_fence`-style `FOR SHARE`.
2. **A cancelled or failed settlement write turns a fully reported success into `unknown`:
   confirmed by reading, by design for the timeout case, low severity.** `finish` marks
   `finished = true` before the write (`inference/mod.rs:449`), so a client disconnect while the
   inline settlement is awaiting (non-stream `attempt.rs:167`, stream `:513`) drops the write and
   `Drop` does nothing (`:505`); a failed or timed-out write also leaves `pending`
   (test `inference/tests.rs:851` asserts no retry). The reconciler then marks it `unknown`; the
   hold is kept (money safe, spend overstated) until an operator resolves it with evidence. The
   model's `MoneyPathDiag` shows it in 8 steps (Receive usage, ClientCancel while `settling`,
   ExpireLease, Reconcile). Not a safety violation; it is a cost-accuracy gap. A detached
   settlement for the cancel-during-settle case would close it; I did not find a test for that
   exact case.
3. **Batch and video envelopes are admitted with a 10-minute lease and extended in a second
   transaction: confirmed by reading, low.** `admit_batch` uses `ADMISSION_LEASE_SECONDS = 600`
   (`jobs/batch.rs:21, 264`), then `insert_batch`, then `hold_until` -> `extend_lease`
   (`jobs/batch.rs:315`, `jobs/mod.rs:373`, `governance/jobs.rs:13`). If the process dies or the
   extension fails (it only logs, `jobs/mod.rs:379`) the reconciler marks the envelope `unknown`
   after 10 minutes while the job row stays queued; `admit_line` then refuses every line
   (`governance/batch.rs:427`), `close_batch` returns false. Money is safe (hold kept as unknown); the
   batch is a zombie until an operator acts. Outside the model (async jobs not modelled); found by
   the comparison.
4. **Hardening suggestion, not a bug:** a `BEFORE UPDATE` trigger on `governance_reservations`
   refusing `settled -> *`, `unknown -> pending`, and `held_microusd` decreasing unless the new
   state is `settled` would make invariants 2 and 3 a database guarantee and turn the two
   `*_no_state_*` mutations from "code review catches it" into "the database refuses it". It
   must allow the envelope transfer (`governance/batch.rs:522`) and realtime growth.

## 6. Limits of the experiment

* Time-boxed (about 2.5 hours of work). The model is hand-written from reading the code; nothing
  checks it against the code. A mismatch would show only on review. The citations above are the
  only drift guard.
* Finite: one request, at most 3 attempts, 2 replicas, at most 2 crashes. No symmetry reduction.
  Counterexamples are minimal for BFS but state counts of violating runs vary between runs
  (parallel workers).
* Amounts and pricing are abstract; the model cannot find arithmetic, valuation or rounding bugs.
* Liveness is checked only under the stated weak-fairness assumptions.
* Realtime, batch envelope arithmetic, native jobs, budgets, counters, authorization and the
  SQL lock mechanics are out of scope; the lock semantics are taken from the code and from
  PostgreSQL's documented behaviour, not verified here.
* No Java on the machine: TLC ran only in the pinned container.

## 7. Recommendation

**Keep as documentation and as a design-review tool; do not add it to CI now.**

* Value: it states invariants 1 to 5 precisely, ties each to a guard with a line reference, shows
  with 10 mutations that the guards are what matters (in particular that the `WHERE state=`
  predicates and `FOR SHARE` are load-bearing, and that A1/A2 are not for the fence), and makes
  finding 1 (no fence on the batch runner) a three-line argument instead of a reading exercise.
  It found no money bug in the current code. The real race tests (`governance/tests.rs`,
  `leases/tests.rs`, the locks suite) already cover the implemented behaviour; this adds a
  checked statement of the design, and is most useful *before* changing settlement, reconciliation,
  cancellation or lease code (for example the detached cancel-during-settle fix of finding 2,
  or a fence for the batch runner).
* Cost to maintain: two small modules (about 400 and 150 lines). Every change to the transitions
  in section 1 needs a matching spec edit and a re-run (3 minutes). The weak point is drift;
  a cheap mitigation is a test that checks every `file:line` anchor above still contains the quoted
  token.
* To put it in CI: a job that runs `check.sh` as is (image and jar already pinned by digest and
  SHA-256; the image is about 200 MB, the jar 2.2 MB, run time about 3 minutes, 3 GB). It must
  satisfy the supply-chain rules (image by digest, jar by checksum, read-only token, no network
  at run time) and the `all` run's result lines are the output to assert on. Reconsider it if the
  batch runner gets a fence, if realtime or async-job accounting is modelled, or if the money path
  is changed often enough that a regression could slip past the integration tests.
