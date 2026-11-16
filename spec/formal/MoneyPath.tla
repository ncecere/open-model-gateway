------------------------------ MODULE MoneyPath ------------------------------
(***************************************************************************)
(* Model of the money path of one client request: admission, the upstream   *)
(* call, settlement, cancellation, expired-lease reconciliation, operator    *)
(* evidence, failover, replica crash and message loss. Experiment for       *)
(* issue #594; see spec/formal/README.md for scope, code references and     *)
(* results. Models what the code at the commit named there does, not what   *)
(* the docs wish.                                                           *)
(*                                                                          *)
(* One client request (served by replica Serving) has up to |Attempts|      *)
(* attempts. Every attempt has its own execution + reservation + ledger     *)
(* hold, written by ONE statement (admit_unobserved, the WITH e AS (INSERT *)
(* inference_executions) ... INSERT monetary_ledger statement), so Admit is *)
(* a single atomic step. Settlement is one transaction (finish_unobserved,  *)
(* the WITH e AS (UPDATE inference_executions ...) statement), also one     *)
(* atomic step; a settlement that fails, times out or is dropped MAY have   *)
(* committed (commit-ack loss) and is modelled both ways. Line numbers are  *)
(* deliberately not cited here: anchors are function names and SQL          *)
(* fragments (see README.md). The upstream is an environment: it may never receive the *)
(* request, or bill it and lose the answer ("the upstream may have done the *)
(* work when the gateway does not know").                                   *)
(*                                                                          *)
(* The mutation switch Mut selects a deliberate bug (see check.sh); "none"  *)
(* is the model of the real code.                                           *)
(***************************************************************************)
EXTENDS Integers, FiniteSets

CONSTANTS
  Attempts,    \* attempt slots of the request, {1,2} (`(1..=3).contains(&plan.max_attempts)` in Engine::execute_protocol)
  Replicas,    \* gateway replicas
  Serving,     \* the replica serving the client request (an element of Replicas)
  HOLD,        \* reserved ceiling of one attempt, micro-USD abstracted to a number
  ACTUAL,      \* what the provider really bills a processed attempt (ACTUAL < HOLD)
  MaxCrashes,  \* bound on replica crashes in one behaviour
  FreePrice,   \* the pinned price is entirely free or not applicable (all_free_or_not_applicable)
  Mut          \* mutation switch; "none" = the code as read

ASSUME ACTUAL < HOLD /\ Serving \in Replicas /\ FreePrice \in BOOLEAN

NoAmt == -1                          \* SQL NULL for amounts
Last  == CHOOSE m \in Attempts : \A x \in Attempts : x <= m
LiveThread == {"admitted", "dispatched", "settling"}

VARIABLES
  \* --- processes (volatile; a replica crash loses them) ---
  up,            \* up[r]: replica r is running
  crashes,       \* crashes so far
  streamed,      \* the request is a streaming request (fixed per behaviour)
  pc,            \* pc[a]: the attempt's request thread
  sret,          \* sret[a]: a response stream was returned to the client (`stream_returned = true` in track_stream)
  out,           \* out[a]: the thread's outcome once it settles
  fo,            \* fo[a]: the thread settled and may fail over (`failing_over` in Engine::attempt)
  cancelPending, \* cancelPending[a]: detached cancellation settlement queued (`impl Drop for ExecutionGuard`)
  inbox,         \* inbox[a]: the upstream's answer as received by the gateway
  gotUsage,      \* gotUsage[a]: the gateway received a complete usage report
  failedOver,    \* failedOver[a]: attempt a+1 was started after a
  \* --- upstream (environment) ---
  upst,          \* upst[a]: none | received | billed | rejected
  \* --- PostgreSQL (durable) ---
  res,           \* governance_reservations row per attempt
  exec,          \* inference_executions.state per attempt
  led,           \* monetary_ledger kinds per attempt, UNIQUE(execution_id,kind)
  nset           \* history: how many times a reservation entered `settled`

vars == <<up, crashes, streamed, pc, sret, out, fo, cancelPending, inbox, gotUsage,
          failedOver, upst, res, exec, led, nset>>

NoRes == [st |-> "none", held |-> 0, actual |-> NoAmt, expired |-> FALSE]

(* What the provider really charged for attempt a, or -1 while that is still *)
(* undetermined (the request may yet be sent, or is being processed).        *)
Truth(a) ==
  IF upst[a] = "billed" THEN ACTUAL
  ELSE IF upst[a] = "rejected" THEN 0
  ELSE IF upst[a] = "received" THEN -1
  ELSE IF pc[a] \in {"idle", "admitted"} THEN -1
  ELSE 0                              \* never delivered, and never will be

(* What a reservation contributes to budget_totals (0015): the hold while it *)
(* is pending or unknown, the actual once settled.                           *)
Contrib(r) ==
  IF r.st \in {"pending", "unknown"} THEN r.held
  ELSE IF r.st = "settled" THEN r.actual ELSE 0

------------------------------------------------------------------------------
(* Settlement transaction: governance/settlement.rs finish_unobserved.       *)

(* Layers a mutation can remove (README section 3): "r_*" are reconcile's,   *)
(* "s_*" settlement's. Each real layer is a separate defence in the code.    *)
DroppedLayers ==
  CASE Mut = "reconcile_no_pred"            -> {"r_pred"}
    [] Mut = "reconcile_no_exec"            -> {"r_exec"}
    [] Mut = "reconcile_no_check"           -> {"r_check"}
    [] Mut = "reconcile_no_pred_exec"       -> {"r_pred", "r_exec"}
    [] Mut = "reconcile_no_pred_exec_check" -> {"r_pred", "r_exec", "r_check"}
    [] Mut = "settle_no_pred"               -> {"s_pred"}
    [] Mut = "settle_no_exec"               -> {"s_exec"}
    [] Mut = "settle_no_uniq"               -> {"s_uniq"}
    [] Mut = "settle_no_pred_exec"          -> {"s_pred", "s_exec"}
    [] Mut = "settle_no_pred_exec_uniq"     -> {"s_pred", "s_exec", "s_uniq"}
    [] OTHER                                -> {}
Dropped(l) == l \in DroppedLayers

KnownCost(a, o)  == o = "succeeded"        \* `record.outcome == Succeeded && input/output present` in finish_unobserved
\* free_rejection: only UpstreamRejected/ContextLength (outcome "rejected"; a 429 Busy
\* is "busy"), no stream returned, no usage, AND a pinned price that is entirely
\* free or not applicable. For a priced model a rejection ends `unknown`.
FreeReject(a, o) == FreePrice /\ o = "rejected" /\ ~sret[a]
SettlesNow(a, o) ==
  \/ KnownCost(a, o)
  \/ FreeReject(a, o)
  \/ (Mut = "unknown_as_zero" /\ o \in {"timeout", "unavail", "busy"})
KindOf(a, o) == IF SettlesNow(a, o) THEN "settlement" ELSE "unknown"

(* Guards of the real transaction: the row is still `pending` (read under   *)
(* FOR UPDATE, `if r.state != "pending"`), the execution is still `started` *)
(* (`WHERE id=$1 ... AND state='started'` plus the (1,1,1) row-count check), *)
(* and the ledger has no row of this kind yet (monetary_ledger_execution_    *)
(* kind_key UNIQUE, 0030). A failed guard rolls back or returns              *)
(* Replay/Conflict: no write.                                                *)
Applies(a, o) ==
  /\ (Dropped("s_pred") \/ res[a].st = "pending")
  /\ (Dropped("s_exec") \/ exec[a] = "started")
  /\ (Dropped("s_uniq") \/ KindOf(a, o) \notin led[a])

SettleResult(a, o) ==
  [st      |-> IF SettlesNow(a, o) THEN "settled" ELSE "unknown",
   held    |-> IF Mut = "release_on_cancel" /\ o = "cancelled" THEN 0 ELSE res[a].held,
   actual  |-> IF SettlesNow(a, o) THEN (IF KnownCost(a, o) THEN ACTUAL ELSE 0) ELSE NoAmt,
   expired |-> res[a].expired]
ExecAfter(o) == IF o = "succeeded" THEN "succeeded" ELSE IF o = "cancelled" THEN "cancelled" ELSE "failed"

Commit(a, o) ==
  IF Applies(a, o)
  THEN /\ res'  = [res  EXCEPT ![a] = SettleResult(a, o)]
       /\ exec' = [exec EXCEPT ![a] = ExecAfter(o)]
       /\ led'  = [led  EXCEPT ![a] = @ \cup {KindOf(a, o)}]
       /\ nset' = [nset EXCEPT ![a] = @ + (IF SettlesNow(a, o) THEN 1 ELSE 0)]
  ELSE UNCHANGED <<res, exec, led, nset>>

------------------------------------------------------------------------------
(* Request thread (inference/attempt.rs, mod.rs)                             *)

Admit(a) ==
  /\ pc[a] = "idle" /\ up[Serving] /\ (IF a = 1 THEN TRUE ELSE failedOver[a - 1])
  /\ pc' = [pc EXCEPT ![a] = "admitted"]
  /\ IF Mut = "reuse_reservation" /\ a > 1
        THEN UNCHANGED <<res, exec, led>>     \* bug: failover reuses the previous reservation
        ELSE /\ res'  = [res  EXCEPT ![a] = [st |-> "pending", held |-> HOLD, actual |-> NoAmt, expired |-> FALSE]]
             /\ exec' = [exec EXCEPT ![a] = "started"]
             /\ led'  = [led  EXCEPT ![a] = {"hold"}]
  /\ UNCHANGED <<up, crashes, streamed, sret, out, fo, cancelPending, inbox, gotUsage, failedOver, upst, nset>>

\* Dispatch: the request message reaches the upstream, or is lost on the wire.
Dispatch(a) ==
  /\ pc[a] = "admitted" /\ up[Serving]
  /\ pc' = [pc EXCEPT ![a] = "dispatched"]
  /\ \/ upst' = [upst EXCEPT ![a] = "received"]
     \/ UNCHANGED upst
  /\ UNCHANGED <<up, crashes, streamed, sret, out, fo, cancelPending, inbox, gotUsage, failedOver, res, exec, led, nset>>

\* Upstream decides: it bills the work, or rejects the request before processing it.
Process(a) ==
  /\ upst[a] = "received"
  /\ \/ upst' = [upst EXCEPT ![a] = "billed"]
     \/ upst' = [upst EXCEPT ![a] = "rejected"]
  /\ UNCHANGED <<up, crashes, streamed, pc, sret, out, fo, cancelPending, inbox, gotUsage, failedOver, res, exec, led, nset>>

\* The answer reaches the gateway (it may also be lost: then it never does).
Deliver(a) ==
  /\ pc[a] = "dispatched" /\ up[Serving] /\ inbox[a] = "empty" /\ upst[a] \in {"billed", "rejected"}
  /\ inbox' = [inbox EXCEPT ![a] = IF upst[a] = "billed" THEN "usage" ELSE "reject"]
  /\ sret'  = [sret EXCEPT ![a] = IF streamed /\ upst[a] = "billed" THEN TRUE ELSE @]
  /\ UNCHANGED <<up, crashes, streamed, pc, out, fo, cancelPending, gotUsage, failedOver, upst, res, exec, led, nset>>

Receive(a) ==
  /\ pc[a] = "dispatched" /\ up[Serving] /\ inbox[a] # "empty"
  /\ pc'  = [pc EXCEPT ![a] = "settling"]
  /\ out' \in {[out EXCEPT ![a] = o] : o \in (IF inbox[a] = "usage" THEN {"succeeded"} ELSE {"rejected", "busy"})}
  /\ gotUsage' = [gotUsage EXCEPT ![a] = (inbox[a] = "usage") \/ @]
  /\ UNCHANGED <<up, crashes, streamed, sret, fo, cancelPending, inbox, failedOver, upst, res, exec, led, nset>>

\* The request deadline passes, or the transport fails: outcome Failed, no usage.
Timeout(a) ==
  /\ pc[a] = "dispatched" /\ up[Serving]
  /\ pc'  = [pc EXCEPT ![a] = "settling"]
  /\ out' = [out EXCEPT ![a] = "timeout"]
  /\ UNCHANGED <<up, crashes, streamed, sret, fo, cancelPending, inbox, gotUsage, failedOver, upst, res, exec, led, nset>>
ConnError(a) ==
  /\ pc[a] = "dispatched" /\ up[Serving]
  /\ pc'  = [pc EXCEPT ![a] = "settling"]
  /\ out' = [out EXCEPT ![a] = "unavail"]
  /\ UNCHANGED <<up, crashes, streamed, sret, fo, cancelPending, inbox, gotUsage, failedOver, upst, res, exec, led, nset>>

\* Explicit settlement by the thread (ExecutionGuard::finish, SETTLEMENT_DEADLINE 3 s). The
\* thread sets `finished` first, so a failed settlement is not retried.
SettleExplicit(a) ==
  /\ pc[a] = "settling" /\ up[Serving]
  /\ Commit(a, out[a])
  /\ pc' = [pc EXCEPT ![a] = "done"]
  /\ fo' = [fo EXCEPT ![a] = Applies(a, out[a])]
  /\ UNCHANGED <<up, crashes, streamed, sret, out, cancelPending, inbox, gotUsage, failedOver, upst>>
\* The caller sees a failure (error or the 3 s deadline), but the write may or
\* may not have committed (commit-ack loss); both are modelled.
SettleExplicitFail(a) ==
  /\ pc[a] = "settling" /\ up[Serving]
  /\ pc' = [pc EXCEPT ![a] = "done"]
  /\ \/ UNCHANGED <<res, exec, led, nset>>
     \/ Commit(a, out[a])
  /\ UNCHANGED <<up, crashes, streamed, sret, out, fo, cancelPending, inbox, gotUsage, failedOver, upst>>

\* Client drops the future (impl Drop for ExecutionGuard). Before `finished`: a detached
\* cancellation settlement is queued. While the explicit settlement is in
\* flight: it is dropped (rolled back) and Drop does nothing.
ClientCancel(a) ==
  /\ pc[a] \in {"admitted", "dispatched"} /\ up[Serving]
  /\ pc' = [pc EXCEPT ![a] = "abandoned"]
  /\ cancelPending' = [cancelPending EXCEPT ![a] = TRUE]
  /\ UNCHANGED <<up, crashes, streamed, sret, out, fo, inbox, gotUsage, failedOver, upst, res, exec, led, nset>>
\* Dropped while the settlement write is in flight: it may or may not have landed.
ClientCancelSettling(a) ==
  /\ pc[a] = "settling" /\ up[Serving]
  /\ pc' = [pc EXCEPT ![a] = "abandoned"]
  /\ \/ UNCHANGED <<res, exec, led, nset>>
     \/ Commit(a, out[a])
  /\ UNCHANGED <<up, crashes, streamed, sret, out, fo, cancelPending, inbox, gotUsage, failedOver, upst>>
CancelSettle(a) ==
  /\ cancelPending[a] /\ up[Serving]
  /\ Commit(a, "cancelled")
  /\ cancelPending' = [cancelPending EXCEPT ![a] = FALSE]
  /\ UNCHANGED <<up, crashes, streamed, pc, sret, out, fo, inbox, gotUsage, failedOver, upst>>
CancelSettleFail(a) ==
  /\ cancelPending[a] /\ up[Serving]
  /\ cancelPending' = [cancelPending EXCEPT ![a] = FALSE]
  /\ \/ UNCHANGED <<res, exec, led, nset>>      \* commit-ack loss: it may have landed
     \/ Commit(a, "cancelled")
  /\ UNCHANGED <<up, crashes, streamed, pc, sret, out, fo, inbox, gotUsage, failedOver, upst>>

\* Failover (Engine::attempt `failing_over`, routing::may_failover): only after
\* the previous attempt's settlement returned Ok, for Busy (a 429: "busy") or,
\* under the allow_ambiguous_failover policy, a transport failure ("unavail");
\* a 4xx rejection or a timeout never fails over. In the code the decision lives
\* only in the `Err` arm, so "never after a stream" is structural; here it is
\* this guard (NoFailoverAfterStream checks the model's own guard). The previous
\* attempt ended `unknown` with its hold kept, so failover leaves two live holds.
Failover(a) ==
  /\ a < Last /\ pc[a] = "done" /\ fo[a] /\ out[a] \in {"unavail", "busy"} /\ ~failedOver[a] /\ up[Serving]
  /\ (~sret[a] \/ Mut = "failover_after_stream")
  /\ failedOver' = [failedOver EXCEPT ![a] = TRUE]
  /\ UNCHANGED <<up, crashes, streamed, pc, sret, out, fo, cancelPending, inbox, gotUsage, upst, res, exec, led, nset>>

------------------------------------------------------------------------------
(* Time, reconciliation and operator evidence                                *)

\* The reservation's lease passes (database clock). Assumption A1: the lease
\* outlives the thread's whole deadline, so a live thread never sees it.
ExpireLease(a) ==
  /\ res[a].st # "none" /\ ~res[a].expired
  /\ (Mut = "early_expiry" \/ pc[a] \notin LiveThread)
  /\ res' = [res EXCEPT ![a].expired = TRUE]
  /\ UNCHANGED <<up, crashes, streamed, pc, sret, out, fo, cancelPending, inbox, gotUsage, failedOver, upst, exec, led, nset>>

\* reconcile_batch, run by every replica (not leased; SKIP LOCKED). Layers that
\* keep a settled row from being flipped to `unknown`:
\*   r_pred   `WHERE state='pending'` in the SELECT and in the UPDATE
\*   r_exec   `changed != ids.len()` after `UPDATE inference_executions ... AND state='started'`
\*   r_check  CHECK(state='settled' AND actual IS NOT NULL OR state<>'settled' AND actual IS NULL)
\*            (0001_enterprise.sql), because the UPDATE does not clear `actual`
\* The ledger UNIQUE on kind 'unknown' is always on (it only matters for rows
\* already reconciled).
ReconcileOK(a) ==
  IF Mut = "reconciler_off" THEN FALSE            \* liveness mutation: nobody reconciles
  ELSE /\ (IF Dropped("r_pred") THEN res[a].st # "none" ELSE res[a].st = "pending")
       /\ res[a].expired
       /\ (Dropped("r_exec") \/ exec[a] = "started")
       /\ (Dropped("r_check") \/ res[a].actual = NoAmt)
       /\ "unknown" \notin led[a]
Reconcile(a, r) ==
  /\ up[r] /\ ReconcileOK(a)
  /\ res'  = [res EXCEPT ![a].st = "unknown",
                         ![a].held = IF Mut = "reconcile_releases_hold" THEN 0 ELSE @]
  /\ exec' = [exec EXCEPT ![a] = IF @ = "started" THEN "cancelled" ELSE @]
  /\ led'  = [led EXCEPT ![a] = @ \cup {"unknown"}]
  /\ UNCHANGED <<up, crashes, streamed, pc, sret, out, fo, cancelPending, inbox, gotUsage, failedOver, upst, nset>>

\* resolve_usage: an operator settles an `unknown`,
\* terminal reservation with evidence of the provider's real charge.
\* Assumption A3: evidence reflects the final bill (upstream no longer in flight).
Evidence(a) ==
  /\ res[a].st = "unknown" /\ exec[a] \notin {"none", "started"}
  /\ "reconciliation" \notin led[a]
  /\ (Truth(a) >= 0 \/ Mut = "evidence_early")
  /\ res'  = [res EXCEPT ![a].st = "settled", ![a].actual = IF Truth(a) >= 0 THEN Truth(a) ELSE 0]
  /\ led'  = [led EXCEPT ![a] = @ \cup {"reconciliation"}]
  /\ nset' = [nset EXCEPT ![a] = @ + 1]
  /\ UNCHANGED <<up, crashes, streamed, pc, sret, out, fo, cancelPending, inbox, gotUsage, failedOver, upst, exec>>

------------------------------------------------------------------------------
(* Crash and restart                                                         *)

Crash(r) ==
  /\ up[r] /\ crashes < MaxCrashes
  /\ up' = [up EXCEPT ![r] = FALSE]
  /\ crashes' = crashes + 1
  /\ IF r = Serving
        THEN /\ pc' = [a \in Attempts |-> IF pc[a] \in LiveThread THEN "dead" ELSE pc[a]]
             /\ cancelPending' = [a \in Attempts |-> FALSE]
             /\ inbox' = [a \in Attempts |-> "empty"]
             /\ fo' = [a \in Attempts |-> FALSE]
        ELSE UNCHANGED <<pc, cancelPending, inbox, fo>>
  /\ UNCHANGED <<streamed, sret, out, gotUsage, failedOver, upst, res, exec, led, nset>>
Restart(r) ==
  /\ ~up[r]
  /\ up' = [up EXCEPT ![r] = TRUE]
  /\ UNCHANGED <<crashes, streamed, pc, sret, out, fo, cancelPending, inbox, gotUsage, failedOver, upst, res, exec, led, nset>>

------------------------------------------------------------------------------
Init ==
  /\ up = [r \in Replicas |-> TRUE]
  /\ crashes = 0
  /\ streamed \in BOOLEAN
  /\ pc = [a \in Attempts |-> "idle"]
  /\ sret = [a \in Attempts |-> FALSE]
  /\ out = [a \in Attempts |-> "none"]
  /\ fo = [a \in Attempts |-> FALSE]
  /\ cancelPending = [a \in Attempts |-> FALSE]
  /\ inbox = [a \in Attempts |-> "empty"]
  /\ gotUsage = [a \in Attempts |-> FALSE]
  /\ failedOver = [a \in Attempts |-> FALSE]
  /\ upst = [a \in Attempts |-> "none"]
  /\ res = [a \in Attempts |-> NoRes]
  /\ exec = [a \in Attempts |-> "none"]
  /\ led = [a \in Attempts |-> {}]
  /\ nset = [a \in Attempts |-> 0]

Next ==
  \/ \E a \in Attempts :
       \/ Admit(a) \/ Dispatch(a) \/ Process(a) \/ Deliver(a) \/ Receive(a)
       \/ Timeout(a) \/ ConnError(a) \/ SettleExplicit(a) \/ SettleExplicitFail(a)
       \/ ClientCancel(a) \/ ClientCancelSettling(a) \/ CancelSettle(a) \/ CancelSettleFail(a) \/ Failover(a)
       \/ ExpireLease(a) \/ Evidence(a)
       \/ \E r \in Replicas : Reconcile(a, r)
  \/ \E r \in Replicas : Crash(r) \/ Restart(r)

Spec == Init /\ [][Next]_vars

\* Safety only has no fairness; the liveness check adds it.
Fair ==
  /\ \A a \in Attempts :
       /\ WF_vars(Dispatch(a)) /\ WF_vars(Timeout(a))
       /\ WF_vars(SettleExplicit(a) \/ SettleExplicitFail(a))
       /\ WF_vars(CancelSettle(a) \/ CancelSettleFail(a))
       /\ WF_vars(ExpireLease(a))
       /\ WF_vars(\E r \in Replicas : Reconcile(a, r))
  /\ \A r \in Replicas : WF_vars(Restart(r))
LiveSpec == Spec /\ Fair

------------------------------------------------------------------------------
(* Invariants. Each comment names the AGENTS.md sentence and the code that   *)
(* is supposed to guarantee it.                                              *)

TypeOK ==
  /\ \A a \in Attempts :
       /\ res[a].st \in {"none", "pending", "unknown", "settled"}
       /\ res[a].held \in {0, HOLD}
       /\ res[a].actual \in {NoAmt, 0, ACTUAL}
       /\ exec[a] \in {"none", "started", "succeeded", "failed", "cancelled"}
       /\ led[a] \subseteq {"hold", "settlement", "unknown", "reconciliation"}
       /\ pc[a] \in {"idle", "admitted", "dispatched", "settling", "done", "dead", "abandoned"}
       /\ upst[a] \in {"none", "received", "billed", "rejected"}

\* I1. "Every upstream attempt needs its own durable reservation."
\* admit_unobserved commits execution+reservation+hold before call_upstream
\* (admit_unobserved, then call_upstream in Engine::attempt); a failover is a new
\* Admit with a new execution id (`Uuid::now_v7()` for attempt > 0).
OwnReservation ==
  \A a \in Attempts :
    (upst[a] # "none" \/ pc[a] \in {"dispatched", "settling", "done"}) => res[a].st # "none"

\* Rows stay coherent: pending <=> execution started (reconcile_batch's execution row-count check relies on it).
ExecConsistent ==
  \A a \in Attempts :
    /\ (res[a].st = "pending") => exec[a] = "started"
    /\ (res[a].st \in {"unknown", "settled"}) => exec[a] \in {"succeeded", "failed", "cancelled"}

\* I2a. "Unknown cost keeps its hold": an unknown reservation keeps the whole
\* admitted hold (reconcile_batch only flips state; the settlement UPDATE never
\* lowers held_microusd).
UnknownRetainsHold == \A a \in Attempts : res[a].st = "unknown" => res[a].held >= HOLD

\* I4. "Treat unknown usage as zero" never: a settled amount equals what the
\* provider really charged (finish_unobserved `let actual = ...`: only complete usage, or (FreePrice) a
\* free pre-processing rejection, settles; everything else is `unknown`).
SettledMatchesTruth ==
  \A a \in Attempts : res[a].st = "settled" => (Truth(a) >= 0 /\ res[a].actual = Truth(a))

\* I3. Exactly one settlement or reconciliation per reservation, and the
\* ledger agrees (UNIQUE(execution_id,kind,admitted_at), 0030:613; events must
\* not be added together as charges, governance.md "Settlement and holds").
SettleOnce == \A a \in Attempts : nset[a] <= 1
LedgerConsistent ==
  \A a \in Attempts :
    /\ Cardinality(led[a] \cap {"settlement", "reconciliation"}) <= 1
    /\ (res[a].st = "settled") => Cardinality(led[a] \cap {"settlement", "reconciliation"}) = 1
    /\ (res[a].st # "none") => "hold" \in led[a]

\* "Failover ... must never occur after a stream is returned."
NoFailoverAfterStream == \A a \in Attempts : failedOver[a] => ~sret[a]

\* Diagnostic (NOT a safety property of the code; expected to FAIL): an attempt
\* whose complete usage report reached the gateway ends up `settled`.
KnownUsageSettled == \A a \in Attempts : gotUsage[a] => res[a].st # "unknown"

Inv == TypeOK /\ OwnReservation /\ ExecConsistent /\ UnknownRetainsHold
       /\ SettledMatchesTruth /\ SettleOnce /\ LedgerConsistent /\ NoFailoverAfterStream

(* Action properties. *)

\* I3. A settled reservation is final: nothing un-settles or re-settles it
\* (every writer's `WHERE state='pending'`/`'unknown'`).
SettledTerminal ==
  [][\A a \in Attempts :
       res[a].st = "settled"
         => (res'[a].st = "settled" /\ res'[a].held = res[a].held /\ res'[a].actual = res[a].actual)]_vars

\* I2. "A hold is never released before settlement or reconciliation": what an
\* attempt contributes to the totals only goes down when the reservation
\* becomes settled with the provider's real charge, and a retained hold never
\* shrinks (governance.md "Cancellation, reconciliation and retention").
HoldNeverReleased ==
  [][\A a \in Attempts :
       /\ Contrib(res'[a]) < Contrib(res[a])
            => (res'[a].st = "settled" /\ res'[a].actual = Truth(a)')
       /\ (res[a].st \in {"pending", "unknown"} /\ res'[a].st \in {"pending", "unknown"})
            => res'[a].held >= res[a].held]_vars

\* Liveness: every pending reservation leaves `pending` (settled or unknown).
Resolves == \A a \in Attempts : (res[a].st = "pending") ~> (res[a].st # "pending")

(* Reachability witnesses. Each is the negation of a scenario that must be   *)
(* reachable; TLC is EXPECTED to report a violation, which proves the model  *)
(* really exercises the scenario (an invariant that never fires could just   *)
(* be unreachable).                                                          *)
W_Failover      == ~failedOver[1]
W_StreamSettled == ~(sret[1] /\ res[1].st = "settled")
W_BilledUnknown == ~(upst[1] = "billed" /\ res[1].st = "unknown")   \* answer lost, cost unknown
W_Evidence      == ~(nset[1] = 1 /\ "reconciliation" \in led[1])
W_CancelUnknown == ~(exec[1] = "cancelled" /\ res[1].st = "unknown" /\ ~res[1].expired)
W_ReconcilerWon == ~(pc[1] = "dead" /\ res[1].st = "unknown")           \* only the reconciler can have moved it
W_TwoHolds      == ~(res[1].st = "unknown" /\ res[2].st = "pending")  \* failover: both holds live
W_RejectedFree  == ~(res[1].st = "settled" /\ res[1].actual = 0 /\ upst[1] = "rejected")  \* needs FreePrice
W_RejectedUnknown == ~(res[1].st = "unknown" /\ upst[1] = "rejected" /\ out[1] = "rejected")   \* priced model
W_BusyFailover  == ~(failedOver[1] /\ out[1] = "busy")
W_AckLost       == ~(pc[1] = "done" /\ ~fo[1] /\ res[1].st = "settled")   \* write landed, caller saw failure
W_CrashPending  == ~(crashes = 1 /\ pc[1] = "dead" /\ res[1].st = "pending")

==============================================================================
