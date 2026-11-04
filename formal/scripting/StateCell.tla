---- MODULE StateCell ----
\* The Mountebank-style script state of one imposter: its cell, the pool's route to one worker, the
\* live state object its runs share there, and the swap a runtime restart makes.  What each action
\* stands for is cited as file:line against crates/rift-mock-core/src (js_engine = scripting/js_engine.rs).
\* README.md says what the model leaves out and what that hides.
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS
    Runs,            \* the script runs
    r1, r2,          \* two of them; with Sequels, r2 runs after r1 for the same request
    Workers,         \* MB pool workers (js_engine:1014)
    Accesses,        \* ScriptFileAccess values; a worker has one realm per access (js_engine:424)
    NoWorker, NoRun,
    MaxJobs,         \* jobs and timers a run leaves queued when its hook function returns
    MaxRestarts,     \* restart_runtime_state calls (imposter/manager.rs:3083)
    MaxAccessChanges,\* set_script_file_access calls (imposter/core/mod.rs:765)
    Sequels,         \* r2 is a later hook of r1's request (imposter/handler.rs:1589, core/matching.rs:148)
    GetterWrites,    \* hook.finish runs script before the save: a getter on the answer (js_engine:679)
    SaveFails,       \* js_to_json refuses the state: a cycle, past the depth cap (js_engine:2941, #1635)
    Panics           \* a job panics: the worker rebuilds and its runs go with it (js_engine:1094-1105)

Cells == 1..(MaxRestarts + 1)
Keys == Workers \X Accesses \X Cells
Kinds == {"hook", "predicate"}
Tokens == Runs \X (0..(1 + MaxJobs * Cardinality(Runs) + 1))

VARIABLES
    current,        \* the cell ImposterCore::script_state holds (imposter/core/mod.rs:593)
    access,         \* the access ImposterCore::script_files holds
    accessChanges,
    cell,           \* cell[c]: the writes the cell holds (ScriptStateCell)
    owner,          \* owner[c]: the worker MbPool::routes names for key c (js_engine:965)
    holding,        \* the runs holding a Claim (js_engine:932)
    run,            \* each run's progress
    live,           \* live[<<w, a, c>>]: the writes in realm a's live state object on worker w
    present,        \* the <<w, a, c>> with an entry in MbRealm::live_state (js_engine:504)
    busy,           \* busy[w]: the run whose MbRealm::end has the worker's thread now
    panics,
    \* Ghost state: what the engine does not keep but the properties speak of.
    written,        \* written[c]: every write a run scoped to c has executed
    owed,           \* owed[r]: r's writes when it answered
    savedToOld,     \* a run saved after a restart had moved the imposter off its cell
    heirFailed      \* runs failed by a job another run left them

vars == <<current, access, accessChanges, cell, owner, holding, run, live, present, busy, panics,
          written, owed, savedToOld, heirFailed>>

Stages == {"idle", "scoped", "queued", "deferred", "flight", "save", "finish", "release", "done"}
InFlight == {"flight", "save", "finish", "release"}

Init ==
    /\ current = 1
    /\ access \in Accesses
    /\ accessChanges = 0
    /\ cell = [c \in Cells |-> {}]
    /\ owner = [c \in Cells |-> NoWorker]
    /\ holding = {}
    /\ run = [r \in Runs |-> [pc |-> "idle", cell |-> 1, files |-> access, worker |-> NoWorker,
                              kind |-> "hook", own |-> 0, inherited |-> 0, gone |-> FALSE,
                              end |-> "none", seq |-> 0, answered |-> FALSE]]
    /\ live = [k \in Keys |-> {}]
    /\ present = {}
    /\ busy = [w \in Workers |-> NoRun]
    /\ panics = 0
    /\ written = [c \in Cells |-> {}]
    /\ owed = [r \in Runs |-> {}]
    /\ savedToOld = FALSE
    /\ heirFailed = {}

Tok(r) == <<r, run[r].seq>>
KeyOf(r) == <<run[r].worker, run[r].files, run[r].cell>>
ClaimsOn(c) == {r \in holding : run[r].cell = c}
Free(r) == busy[run[r].worker] = NoRun

\* MbRealm::save_state's in_flight check, and the realm's self.runs: the other runs of r's imposter
\* in r's realm (js_engine:553, :686).  A run being ended was removed from self.runs (js_engine:660).
Kin(r) == {o \in Runs \ {r} : run[o].pc = "flight" /\ KeyOf(o) = KeyOf(r)}

\* MbJsThread::split_from (js_engine:416): the imposter has a live state in another realm here.
SplitFrom(r) == \E a \in Accesses \ {run[r].files} : <<run[r].worker, a, run[r].cell>> \in present

\* Drop a Claim: MbPool::release (js_engine:997).  The route goes with the last claim on the key.
Released(r) ==
    /\ holding' = holding \ {r}
    /\ owner' = IF ClaimsOn(run[r].cell) = {r}
                THEN [owner EXCEPT ![run[r].cell] = NoWorker]
                ELSE owner

\* ---- The caller: a request on the serving path. ----

\* ImposterCore::script_scope and script_file_access: an atomic load of each.  A request loads the
\* scope once, before its first match, and every hook of it runs on that cell (#1784); the file access
\* is loaded per hook, as a stub write has to reach a request about to run a script (#1645).  With
\* Sequels, r2 is a later hook of r1's request: it runs once r1 answered.  A run's kind is set here: a predicate inject answers as soon as its function
\* returned, and its caller blocks on a sync channel, so it can never be abandoned.
Capture(r) ==
    /\ run[r].pc = "idle"
    /\ Sequels /\ r = r2 => run[r1].answered
    /\ \E kind \in Kinds :
         run' = [run EXCEPT ![r].pc = "scoped", ![r].cell = IF Sequels /\ r = r2 THEN run[r1].cell ELSE current,
                            ![r].files = access,
                            ![r].kind = kind]
    /\ UNCHANGED <<current, access, accessChanges, cell, owner, holding, live, present, busy, panics,
                   written, owed, savedToOld, heirFailed>>

\* submit_mb_job (js_engine:1119) and MbPool::claim (js_engine:965), under the routes lock: the key's
\* worker while it has a claim, else any worker (the code picks the least loaded idle one).  The send
\* to the worker's queue follows outside the lock; the model takes it with the claim, and lets the
\* worker start its queued runs in any order, which is more orders than a FIFO queue allows.
Submit(r) ==
    /\ run[r].pc = "scoped"
    /\ LET c == run[r].cell IN
       \E w \in IF owner[c] = NoWorker THEN Workers ELSE {owner[c]} :
         /\ holding' = holding \cup {r}
         /\ owner' = [owner EXCEPT ![c] = w]
         /\ run' = [run EXCEPT ![r].pc = "queued", ![r].worker = w]
    /\ UNCHANGED <<current, access, accessChanges, cell, live, present, busy, panics,
                   written, owed, savedToOld, heirFailed>>

\* The caller stops waiting: its tokio timeout, or the request's cancellation, which drops the
\* oneshot's receiver (js_engine:2570, Reply::abandoned :791).  A predicate's Reply::Sync cannot.
CallerLeaves(r) ==
    /\ run[r].kind = "hook"
    /\ run[r].pc \in {"queued", "deferred", "flight"}
    /\ ~run[r].gone
    /\ run' = [run EXCEPT ![r].gone = TRUE]
    /\ UNCHANGED <<current, access, accessChanges, cell, owner, holding, live, present, busy, panics,
                   written, owed, savedToOld, heirFailed>>

\* ---- A pool worker: one thread, so at most one of a worker's steps at a time, and its End is
\* ---- never interleaved with another of its steps (busy). ----

\* start_inject / start_decorate: nobody waits any more, so the script never runs (js_engine:2709,
\* :4157), checked again when a deferred start is retried.  Dropping the claim drops the job.
StartAbandoned(r) ==
    /\ run[r].pc \in {"queued", "deferred"}
    /\ run[r].gone
    /\ Free(r)
    /\ Released(r)
    /\ run' = [run EXCEPT ![r].pc = "done"]
    /\ UNCHANGED <<current, access, accessChanges, cell, live, present, busy, panics,
                   written, owed, savedToOld, heirFailed>>

\* The imposter has a run in flight in another realm: its script access changed meanwhile.  This
\* start waits on thread.deferred, keeping its claim, tried again after every step (js_engine:2712,
\* :3077, :4160, :449).
Defer(r) ==
    /\ run[r].pc = "queued"
    /\ ~run[r].gone
    /\ Free(r)
    /\ SplitFrom(r)
    /\ run' = [run EXCEPT ![r].pc = "deferred"]
    /\ UNCHANGED <<current, access, accessChanges, cell, owner, holding, live, present, busy, panics,
                   written, owed, savedToOld, heirFailed>>

\* start_*: take the live state (MbRealm::live_state, js_engine:528: the live one, else a new one built
\* from the cell), run the hook function, whose synchronous section nothing interleaves with, and
\* admit the run (js_engine:561), which ends it at once when it has its answer.  The function always
\* writes here: a run that writes nothing adds no behaviour the properties can see.
Start(r) ==
    /\ run[r].pc \in {"queued", "deferred"}
    /\ ~run[r].gone
    /\ Free(r)
    /\ ~SplitFrom(r)
    /\ LET k == KeyOf(r)
           c == run[r].cell
           base == IF k \in present THEN live[k] ELSE cell[c]
       IN /\ present' = present \cup {k}
          /\ live' = [live EXCEPT ![k] = base \cup {Tok(r)}]
          /\ written' = [written EXCEPT ![c] = @ \cup {Tok(r)}]
    /\ \/ \* A predicate answers now, and its jobs end with it (PredicateHook, js_engine:3197).
          /\ run[r].kind = "predicate"
          /\ run' = [run EXCEPT ![r].pc = "save", ![r].end = "predicate", ![r].seq = @ + 1]
          /\ busy' = [busy EXCEPT ![run[r].worker] = r]
       \/ \* The hook has its answer now, or threw; what it queued is the end's to deal with.
          /\ run[r].kind = "hook"
          /\ \E jobs \in 0..MaxJobs, how \in {"finished", "threw"} :
               run' = [run EXCEPT ![r].pc = "save", ![r].end = how, ![r].own = jobs, ![r].seq = @ + 1]
          /\ busy' = [busy EXCEPT ![run[r].worker] = r]
       \/ \* It waits on a promise job or a timer, holding no worker.
          /\ run[r].kind = "hook"
          /\ \E jobs \in 0..MaxJobs :
               run' = [run EXCEPT ![r].pc = "flight", ![r].own = jobs, ![r].seq = @ + 1]
          /\ UNCHANGED busy
    /\ UNCHANGED <<current, access, accessChanges, cell, owner, holding, panics, owed, savedToOld,
                   heirFailed>>

\* MbRealm::step, one job (js_engine:579): it runs as the run it is tagged with, writes to the live
\* state, and may throw, which ends that run (js_engine:596).
JobWrites(r, c) ==
    /\ live' = [live EXCEPT ![KeyOf(r)] = @ \cup {Tok(r)}]
    /\ written' = [written EXCEPT ![c] = @ \cup {Tok(r)}]

OwnJob(r) ==
    /\ run[r].pc = "flight"
    /\ run[r].own > 0
    /\ Free(r)
    /\ JobWrites(r, run[r].cell)
    /\ \/ /\ run' = [run EXCEPT ![r].own = @ - 1, ![r].seq = @ + 1]
          /\ UNCHANGED busy
       \/ /\ run' = [run EXCEPT ![r].own = @ - 1, ![r].seq = @ + 1, ![r].pc = "save", ![r].end = "threw"]
          /\ busy' = [busy EXCEPT ![run[r].worker] = r]
    /\ UNCHANGED <<current, access, accessChanges, cell, owner, holding, present, panics, owed,
                   savedToOld, heirFailed>>

\* A job another run left this one (MbJobExecutor::retag): it runs as this run, but a throw is logged
\* and dropped, as the job is not this run's (#1786).
InheritedJob(r) ==
    /\ run[r].pc = "flight"
    /\ run[r].inherited > 0
    /\ Free(r)
    /\ JobWrites(r, run[r].cell)
    /\ run' = [run EXCEPT ![r].inherited = @ - 1, ![r].seq = @ + 1]
    /\ UNCHANGED <<busy, heirFailed>>
    /\ UNCHANGED <<current, access, accessChanges, cell, owner, holding, present, panics, owed,
                   savedToOld>>

\* How a run in flight ends (MbRealm::ending, js_engine:612).  Time is abstract: its answer may arrive
\* from any job, its own or a kin's through a promise they share, and its deadline may pass at any
\* moment.  MB_ABANDON_POLL (js_engine:403) bounds when a worker with a run in flight next looks; it
\* looks at any moment here, and it does look (next_wake, js_engine:462), so a run is never stranded.
EndNow(r, how) ==
    /\ run[r].pc = "flight"
    /\ Free(r)
    /\ run' = [run EXCEPT ![r].pc = "save", ![r].end = how]
    /\ busy' = [busy EXCEPT ![run[r].worker] = r]
    /\ UNCHANGED <<current, access, accessChanges, cell, owner, holding, live, present, panics,
                   written, owed, savedToOld, heirFailed>>

Finish(r) == EndNow(r, "finished")
Deadline(r) == EndNow(r, "deadline")
Abandoned(r) == run[r].gone /\ EndNow(r, "abandoned")
\* No job of its own, and no kin's that could still settle what it waits for (js_engine:625-632).
Idle(r) ==
    /\ run[r].own + run[r].inherited = 0
    /\ \A o \in Kin(r) : run[o].own + run[o].inherited = 0
    /\ EndNow(r, "idle")

\* MbRealm::end, first step (js_engine:674): save the live state to the run's own cell, and drop it
\* when no run of the imposter is left in the realm (MbRealm::save_state, js_engine:541).  The cell
\* lock is taken for this save only.  A save js_to_json refuses keeps the cell and logs (js_engine:2941).
EndSave(r) ==
    /\ run[r].pc = "save"
    /\ LET k == KeyOf(r)
           c == run[r].cell
           last == Kin(r) = {}
           writes == GetterWrites /\ run[r].end \in {"finished", "idle", "threw"}
           after == live[k] \cup (IF writes THEN {Tok(r)} ELSE {})
       IN /\ IF k \in present
             THEN /\ \/ /\ cell' = [cell EXCEPT ![c] = after]
                        /\ savedToOld' = (savedToOld \/ c # current)
                     \/ /\ SaveFails
                        /\ UNCHANGED <<cell, savedToOld>>
                  /\ present' = IF last THEN present \ {k} ELSE present
                  /\ live' = IF last THEN [live EXCEPT ![k] = {}] ELSE [live EXCEPT ![k] = after]
             ELSE UNCHANGED <<cell, savedToOld, present, live>>
          /\ written' = IF writes THEN [written EXCEPT ![c] = @ \cup {Tok(r)}] ELSE written
          /\ run' = [run EXCEPT ![r].pc = "finish", ![r].seq = IF writes THEN @ + 1 ELSE @]
    /\ UNCHANGED <<current, access, accessChanges, owner, holding, busy, panics, owed, heirFailed>>

\* MbRealm::end, after the save: the answer goes out.  An abandoned run drops its hook: no answer.
FinishAnswered(r) ==
    /\ run[r].pc = "finish"
    /\ LET mine == {t \in Tokens : t[1] = r /\ t[2] < run[r].seq}
       IN /\ owed' = IF run[r].end = "abandoned" THEN owed ELSE [owed EXCEPT ![r] = mine]
          /\ run' = [run EXCEPT ![r].pc = "release", ![r].answered = (run[r].end # "abandoned")]
    /\ UNCHANGED <<current, access, accessChanges, cell, owner, holding, present, busy, panics,
                   live, written, savedToOld, heirFailed>>

\* MbRealm::end, last step (js_engine:686-697): what the run left passes to a kin when it finished or
\* went idle and the hook bequeaths, a predicate never does, else it is dropped; then the claim goes.
\* An inherited job's expiry (retag's deadline) is left out: a job dropped at its expiry is the
\* same state as one this run was never left.
EndRelease(r) ==
    /\ run[r].pc = "release"
    /\ LET left == run[r].own + run[r].inherited
           bequeaths == run[r].end \in {"finished", "idle"} /\ left > 0
           heirs == IF bequeaths THEN Kin(r) ELSE {}
           ended == [run EXCEPT ![r].pc = "done", ![r].own = 0, ![r].inherited = 0]
       IN IF heirs = {}
          THEN run' = ended
          ELSE \E heir \in heirs :
                 run' = [ended EXCEPT ![heir].inherited = @ + left]
    /\ Released(r)
    /\ busy' = [busy EXCEPT ![run[r].worker] = NoRun]
    /\ UNCHANGED <<current, access, accessChanges, cell, live, present, panics, written, owed,
                   savedToOld, heirFailed>>

\* A job panicked (js_engine:1094-1105): catch_unwind, and thread = MbJsThread::new().  Its realms and
\* every run in flight or deferred there are dropped with their unsaved writes; their claims go with
\* them, and their callers see the job fail.  Runs still in the queue are the new thread's.
Panic(w) ==
    /\ Panics
    /\ panics = 0
    /\ busy[w] = NoRun
    /\ LET lost == {r \in Runs : run[r].worker = w /\ run[r].pc \in {"flight", "deferred"}}
           stays == holding \ lost
       IN /\ holding' = stays
          /\ owner' = [c \in Cells |-> IF \E r \in stays : run[r].cell = c THEN owner[c] ELSE NoWorker]
          /\ run' = [r \in Runs |-> IF r \in lost
                                    THEN [run[r] EXCEPT !.pc = "done", !.own = 0, !.inherited = 0]
                                    ELSE run[r]]
    /\ present' = {k \in present : k[1] # w}
    /\ live' = [k \in Keys |-> IF k[1] = w THEN {} ELSE live[k]]
    /\ panics' = 1
    /\ UNCHANGED <<current, access, accessChanges, cell, busy, written, owed, savedToOld, heirFailed>>

\* ---- The admin side. ----

\* restart_runtime_state (imposter/manager.rs:3083): a fresh cell, not a cleared one.  A delete and
\* a re-create on the port is the same step here: the new instance has a new cell.
Restart ==
    /\ current < MaxRestarts + 1
    /\ current' = current + 1
    /\ UNCHANGED <<access, accessChanges, cell, owner, holding, run, live, present, busy, panics,
                   written, owed, savedToOld, heirFailed>>

\* set_script_file_access (imposter/core/mod.rs:765).
SetAccess ==
    /\ accessChanges < MaxAccessChanges
    /\ \E a \in Accesses \ {access} :
         /\ access' = a
         /\ accessChanges' = accessChanges + 1
    /\ UNCHANGED <<current, cell, owner, holding, run, live, present, busy, panics,
                   written, owed, savedToOld, heirFailed>>

\* Every run ended, or is a later hook of a request whose first one got no answer and so never comes.
Terminated ==
    /\ \A r \in Runs :
         \/ run[r].pc = "done"
         \/ Sequels /\ r = r2 /\ run[r].pc = "idle" /\ run[r1].pc = "done" /\ ~run[r1].answered
    /\ UNCHANGED vars

Next ==
    \/ \E r \in Runs :
         \/ Capture(r) \/ Submit(r) \/ CallerLeaves(r)
         \/ StartAbandoned(r) \/ Defer(r) \/ Start(r)
         \/ OwnJob(r) \/ InheritedJob(r)
         \/ Finish(r) \/ Deadline(r) \/ Abandoned(r) \/ Idle(r)
         \/ EndSave(r) \/ FinishAnswered(r) \/ EndRelease(r)
    \/ \E w \in Workers : Panic(w)
    \/ Restart
    \/ SetAccess
    \/ Terminated

Spec == Init /\ [][Next]_vars

\* Every worker step is taken eventually: the MB_ABANDON_POLL wake-up is what makes this true of a
\* worker with a run in flight and nothing to do.
LiveSpec ==
    /\ Spec
    /\ \A r \in Runs :
         /\ WF_vars(Capture(r) \/ Submit(r))
         /\ WF_vars(StartAbandoned(r) \/ Defer(r) \/ Start(r))
         /\ WF_vars(Finish(r) \/ Deadline(r))
         /\ WF_vars(EndSave(r) \/ FinishAnswered(r) \/ EndRelease(r))

\* ---- Properties. ----

TypeInv ==
    /\ current \in Cells
    /\ access \in Accesses
    /\ cell \in [Cells -> SUBSET Tokens]
    /\ owner \in [Cells -> Workers \cup {NoWorker}]
    /\ holding \subseteq Runs
    /\ present \subseteq Keys
    /\ live \in [Keys -> SUBSET Tokens]
    /\ busy \in [Workers -> Runs \cup {NoRun}]
    /\ written \in [Cells -> SUBSET Tokens]
    /\ \A r \in Runs :
         /\ run[r].pc \in Stages
         /\ Tok(r) \in Tokens
         /\ run[r].kind \in Kinds

\* A claim is held from the submit until the run's end let it go, and the route names its worker.
ClaimsAgree ==
    \A r \in holding :
        /\ run[r].pc \in {"queued", "deferred"} \cup InFlight
        /\ owner[run[r].cell] = run[r].worker

\* All runs of an imposter's cell share one live state object, wherever they are: two would each
\* save over the other's writes.  js_engine:504, :416, :965.
OneLiveStatePerKey ==
    \A c \in Cells : Cardinality({k \in present : k[3] = c}) <= 1

\* The control: every run of a key queued or in flight is on one worker (MbPool::claim).
NeverTwoWorkersOneKey ==
    \A c \in Cells : Cardinality({run[r].worker : r \in ClaimsOn(c)}) <= 1

\* A caller holding its answer finds its run's writes in the cell (#1678).
CellCurrentBeforeAnswer ==
    \A r \in Runs : run[r].answered => owed[r] \subseteq cell[run[r].cell]

\* Every write a run executed is in the live state while there is one, and in the cell otherwise.
\* The union only matters to a model with two live states, which OneLiveStatePerKey refuses.
NoLostUpdate ==
    \A c \in Cells :
        LET held == {k \in present : k[3] = c}
        IN written[c] \subseteq (IF held = {} THEN cell[c] ELSE UNION {live[k] : k \in held})

\* A cell holds writes of runs scoped to it only, whatever a restart did meanwhile (#1726).
OldRunSavesToOldCell ==
    \A c \in Cells : \A t \in cell[c] : run[t[1]].cell = c

\* No worker runs a job of a run that has ended, nor is a worker held by one (js_engine:696).
EndedRunsLeaveNothing ==
    \A r \in Runs :
        /\ run[r].pc = "done" => run[r].own + run[r].inherited = 0 /\ r \notin holding
        /\ \A w \in Workers : busy[w] = r => run[r].pc \in {"save", "finish", "release"}

\* ---- What the model found, all fixed. ----

\* The hooks of one request run on one state, as one Mountebank imposter object's do.
RequestKeepsOneCell ==
    Sequels /\ run[r2].pc \notin {"idle"} => run[r1].cell = run[r2].cell

\* A job a finished run left behind writes to the state like any, but only its own run may fail by it.
HeirFailsOnlyByItsOwnJob == heirFailed = {}

\* ---- Witnesses (witness.py): each must be violated, so what it denies stays reachable. ----

\* A run in flight across a restart saves to the cell the imposter no longer has.
NeverSaveToOldCell == ~savedToOld

\* Runs of two cells of one imposter are in flight on two workers: the keys are the cells.
NeverTwoWorkersOneImposter == Cardinality({run[r].worker : r \in holding}) <= 1

\* A start waits for its imposter's runs in another realm.
NeverDeferred == \A r \in Runs : run[r].pc # "deferred"

\* A run answers while another of its imposter is in flight, and leaves it its jobs.
NeverInherits == \A r \in Runs : run[r].inherited = 0

\* The caller left a run in flight, which is then dropped without an answer.
NeverAbandoned == \A r \in Runs : ~(run[r].gone /\ run[r].end = "abandoned")

\* ---- Liveness (Live.cfg). ----

\* Every run submitted ends and lets go of its claim.
RunsEnd == \A r \in Runs : (run[r].pc = "queued") ~> (run[r].pc = "done")

WorkerSymmetry == Permutations(Workers)
FullSymmetry == Permutations(Workers) \cup Permutations(Runs) \cup Permutations(Accesses)
====
