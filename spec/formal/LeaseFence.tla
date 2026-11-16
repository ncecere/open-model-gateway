------------------------------ MODULE LeaseFence ------------------------------
(***************************************************************************)
(* Two replicas compete for one lease row; a holder runs writes. Checks     *)
(* "a fenced (lost-lease) runner never writes" for the designs in the code: *)
(*   Mode "InTx"        work_leases (0029, leases.rs): omg_lease_fence is    *)
(*                      the first statement of the write transaction and    *)
(*                      holds the lease row FOR SHARE until commit, so a    *)
(*                      takeover waits.                                     *)
(*   Mode "InTxNoShare" mutation: fence check inside the transaction but    *)
(*                      without the FOR SHARE lock.                         *)
(*   Mode "Local"       mutation: the holder checks its local term, then     *)
(*                      writes with no database check.                      *)
(*   Mode "RunnerFlag"  the gateway-run batch runner (jobs/runner.rs,       *)
(*                      jobs/store.rs claim_gateway/renew_lease): async_jobs.runner_id +      *)
(*                      runner_lease_until, a local `lost` flag set by the  *)
(*                      renewal task, and NO runner id or epoch on any      *)
(*                      write.                                              *)
(* TrustMargin models assumption A2 (leases.rs MARGIN): a holder stops      *)
(* trusting its term before the database expires it. The fence must not     *)
(* need it; the local-check designs do.                                     *)
(* Experiment for issue #594; see spec/formal/README.md.                    *)
(***************************************************************************)
EXTENDS Integers, FiniteSets

CONSTANTS Replicas, Mode, TrustMargin, MaxEpoch, MaxCrashes

Nil == -1                            \* no term / no holder (integer, so comparisons type-check)
NoHolder == "none"
HoldsShareLock(r) == Mode = "InTx"

VARIABLES
  lease,    \* [holder, epoch, live]: the lease row (work_leases / async_jobs.runner_*)
  term,     \* term[r]: the epoch r believes it holds, or Nil
  pc,       \* pc[r]: idle | checked | fenced
  ck,       \* ck[r]: the epoch captured at the local check (the fencing token)
  shared,   \* replicas holding the lease row FOR SHARE
  crashes,
  stale,    \* history: some write committed by a replica that was no longer the current term
  reorder,  \* history: some write committed with a token older than an earlier committed write
  lastW     \* highest token committed so far

vars == <<lease, term, pc, ck, shared, crashes, stale, reorder, lastW>>

Init ==
  /\ lease = [holder |-> NoHolder, epoch |-> 0, live |-> FALSE]
  /\ term = [r \in Replicas |-> Nil]
  /\ pc = [r \in Replicas |-> "idle"]
  /\ ck = [r \in Replicas |-> 0]
  /\ shared = {}
  /\ crashes = 0
  /\ stale = FALSE /\ reorder = FALSE /\ lastW = 0

\* omg_lease_acquire (0029 `omg_lease_acquire`): takes a free or expired lease or renews the
\* caller's own; a new term always gets a larger epoch. The UPDATE waits for a
\* FOR SHARE holder (Mode InTx).
Acquire(r) ==
  /\ \/ lease.holder = r
     \/ lease.holder = NoHolder
     \/ ~lease.live
  /\ (HoldsShareLock(r) => shared = {})
  /\ LET renewing == lease.holder = r /\ lease.live
         e == IF renewing THEN lease.epoch ELSE lease.epoch + 1
     IN /\ e <= MaxEpoch
        /\ lease' = [holder |-> r, epoch |-> e, live |-> TRUE]
        /\ term' = [term EXCEPT ![r] = e]
  /\ UNCHANGED <<pc, ck, shared, crashes, stale, reorder, lastW>>

\* The renewal task of a replica that is told another term is current drops
\* its belief (Leases::renew_once `Ok(None) => *slot = None`; Runner::run's renewal task sets `lost`).
Observe(r) ==
  /\ term[r] # Nil /\ lease.live /\ lease.holder # r
  /\ term' = [term EXCEPT ![r] = Nil]
  /\ UNCHANGED <<lease, pc, ck, shared, crashes, stale, reorder, lastW>>

\* Local term runs out before the database expiry (Leases::renew_once `trusted_until`, MARGIN).
TrustLapse(r) ==
  /\ term[r] # Nil
  /\ term' = [term EXCEPT ![r] = Nil]
  /\ UNCHANGED <<lease, pc, ck, shared, crashes, stale, reorder, lastW>>

\* The database clock passes expires_at. Assumption A2 when TrustMargin.
Expire ==
  /\ lease.live
  /\ (IF TrustMargin THEN term[lease.holder] = Nil ELSE TRUE)
  /\ lease' = [lease EXCEPT !.live = FALSE]
  /\ UNCHANGED <<term, pc, ck, shared, crashes, stale, reorder, lastW>>

\* A batch runner's renewal (store::renew_lease) renews its own row even after
\* the term lapsed, as long as nobody else claimed it.
RenewOwn(r) ==
  /\ Mode = "RunnerFlag" /\ lease.holder = r /\ ~lease.live
  /\ lease' = [lease EXCEPT !.live = TRUE]
  /\ UNCHANGED <<term, pc, ck, shared, crashes, stale, reorder, lastW>>

\* Job tick: leases.held() (Leases::held) then work(fence).
Check(r) ==
  /\ pc[r] = "idle" /\ term[r] # Nil
  /\ pc' = [pc EXCEPT ![r] = "checked"]
  /\ ck' = [ck EXCEPT ![r] = term[r]]
  /\ UNCHANGED <<lease, term, shared, crashes, stale, reorder, lastW>>

\* omg_lease_fence (0029 `omg_lease_fence`, `PERFORM 1 ... FOR SHARE`): current holder, same epoch, unexpired.
Fence(r) ==
  /\ Mode \in {"InTx", "InTxNoShare"} /\ pc[r] = "checked"
  /\ IF lease.holder = r /\ lease.epoch = ck[r] /\ lease.live
        THEN /\ pc' = [pc EXCEPT ![r] = "fenced"]
             /\ shared' = IF HoldsShareLock(r) THEN shared \cup {r} ELSE shared
        ELSE /\ pc' = [pc EXCEPT ![r] = "idle"]        \* SQLSTATE 55006, nothing committed
             /\ UNCHANGED shared
  /\ UNCHANGED <<lease, term, ck, crashes, stale, reorder, lastW>>

Record(r) ==
  /\ stale'   = (stale \/ lease.holder # r \/ lease.epoch # ck[r])
  /\ reorder' = (reorder \/ ck[r] < lastW)
  /\ lastW'   = IF ck[r] > lastW THEN ck[r] ELSE lastW

Commit(r) ==
  /\ \/ (Mode \in {"InTx", "InTxNoShare"} /\ pc[r] = "fenced")
     \/ (Mode \in {"Local", "RunnerFlag"} /\ pc[r] = "checked")
  /\ Record(r)
  /\ pc' = [pc EXCEPT ![r] = "idle"]
  /\ shared' = shared \ {r}
  /\ UNCHANGED <<lease, term, ck, crashes>>

Crash(r) ==
  /\ crashes < MaxCrashes
  /\ crashes' = crashes + 1
  /\ term' = [term EXCEPT ![r] = Nil]
  /\ pc' = [pc EXCEPT ![r] = "idle"]
  /\ shared' = shared \ {r}
  /\ UNCHANGED <<lease, ck, stale, reorder, lastW>>

Next ==
  \/ Expire
  \/ \E r \in Replicas :
       Acquire(r) \/ Observe(r) \/ TrustLapse(r) \/ RenewOwn(r) \/ Check(r)
       \/ Fence(r) \/ Commit(r) \/ Crash(r)

Spec == Init /\ [][Next]_vars

TypeOK ==
  /\ lease.epoch \in 0..MaxEpoch
  /\ \A r \in Replicas : pc[r] \in {"idle", "checked", "fenced"}

\* I5. "A fenced (lost-lease) runner never writes results": no committed
\* write was made under a term that was no longer the current one.
NoStaleCommit == ~stale
\* Committed tokens never go backwards: a takeover's writes are not
\* overtaken by the old holder's.
NoReorder == ~reorder
EpochMonotone == [][lease'.epoch >= lease.epoch]_vars

==============================================================================
