---------------------------- MODULE ProxyOnce ------------------------------
(***************************************************************************)
(* The proxyOnce claim protocol of one imposter, for one request signature *)
(* (crates/rift-mock-core/src/imposter/core/proxy.rs, handle_proxy_request;*)
(* recording/proxy_store.rs, LocalProxyStore; core/mod.rs, mutate_stubs).  *)
(*                                                                         *)
(* Every action is one step between two points where another task can      *)
(* run.  The engine serves on tokio's multi-threaded runtime, so a step    *)
(* boundary is any point where a lock is released or a value was read      *)
(* from an atomic, not only an `.await`: two requests' synchronous code    *)
(* really does overlap.  A step under one lock (`responses`+`pending` in   *)
(* try_claim_as and record, `stubs_write` in mutate_stubs) is one action.  *)
(* README.md has the action table with the lines each one models.          *)
(*                                                                         *)
(* A signature has one `responses` entry and one `pending` entry, so both  *)
(* are single values here.  Every recorded stub matches the request, and   *)
(* the proxy stub matches it too; the first stub in the list answers.      *)
(*                                                                         *)
(* Ghost flags record a property violation at the step that commits it, so *)
(* an invariant is one flag and TLC's trace ends on the guilty step.       *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Requests,       \* requests with this signature; each runs handle_proxy_request once
    Editors,        \* operators' stub edits, one edit each (core/mod.rs:840, lifecycle.rs)
    EditKinds,      \* the kinds of edit they may make, see Edits
    Clearers,       \* DELETE /imposters/:port/savedProxyResponses (recording.rs:185)
    Restarters,     \* restart_runtime_state, PUT /imposters on a running imposter (manager.rs:3070)
    GeneratesStub,  \* predicateGenerators, addWaitBehavior or addDecorateBehavior is set (proxy.rs:1326)
    SettleCanFail,  \* the store may fail `record`/`complete`; LocalProxyStore never does (proxy_store.rs:193)
    Preloaded       \* TRUE: starts recorded, with its carrier in front of the proxy

ASSUME /\ Requests # {}
       /\ IsFiniteSet(Editors) /\ IsFiniteSet(Clearers) /\ IsFiniteSet(Restarters)
       /\ EditKinds \subseteq {"move", "delete", "replace", "deleteProxy", "replaceAll"}
       /\ GeneratesStub \in BOOLEAN /\ SettleCanFail \in BOOLEAN /\ Preloaded \in BOOLEAN

NoRec == 0      \* no entry in `responses`; recordings are named by the token that made them
NoToken == 0    \* no entry in `pending`; a request holding no claim
NoSlot == 0     \* no answering stub

Proxy(s) == [kind |-> "proxy", slot |-> s, claim |-> FALSE]
Rec(c)   == [kind |-> "rec", slot |-> NoSlot, claim |-> c]

VARIABLES
    responses,      \* NoRec or a token: `PortState.responses` (proxy_store.rs:290)
    pending,        \* NoToken or a token: `PortState.pending` (proxy_store.rs:298)
    nextToken,      \* `next_token` (proxy_store.rs:313)
    stubs,          \* sequence of Proxy/Rec: the current `StubSnapshot`; `claim` is `recorded_claim`
    nextSlot,       \* NEXT_STUB_SLOT, as far as proxy stubs need it (core/mod.rs:293)
    req,            \* [Requests -> record], see InitReq
    edit,           \* [Editors -> [pc, saw]]
    clear,          \* [Clearers -> [pc, saw]]
    restart,        \* [Restarters -> {"idle", "done"}]
    ghost           \* property flags and counters, see Invariants

vars == <<responses, pending, nextToken, stubs, nextSlot, req, edit, clear, restart, ghost>>

(***************************************************************************)
(* Stub-list helpers.                                                      *)
(***************************************************************************)
Indices(s) == DOMAIN s
RemoveAt(s, i) == SubSeq(s, 1, i - 1) \o SubSeq(s, i + 1, Len(s))
InsertAt(s, i, e) == SubSeq(s, 1, i - 1) \o <<e>> \o SubSeq(s, i, Len(s))
Set(s, i, e) == [s EXCEPT ![i] = e]

\* The first index of `s` whose element satisfies `P`, or 0: Rust's `position`.
FirstWhere(s, P(_)) ==
    IF \E i \in Indices(s) : P(s[i])
    THEN CHOOSE i \in Indices(s) : P(s[i]) /\ \A j \in Indices(s) : P(s[j]) => i <= j
    ELSE 0

IsProxy(e) == e.kind = "proxy"
IsRec(e) == e.kind = "rec"
IsCarrier(e) == e.claim

FirstCarrier(s) == FirstWhere(s, IsCarrier)
FirstProxy(s) == FirstWhere(s, IsProxy)
HasCarrier(s) == FirstCarrier(s) # 0

\* The carriers ahead of the first proxy stub: those a request can still be answered by.  A carrier
\* behind it is the shadowed one #1705 keeps: it stays in the list and is never reached.
FrontCarriers(s) ==
    {i \in Indices(s) : IsCarrier(s[i]) /\ (FirstProxy(s) = 0 \/ i < FirstProxy(s))}

SlotIndex(s, slot) == FirstWhere(s, LAMBDA e : IsProxy(e) /\ e.slot = slot)

\* recording_is_shadowed (proxy.rs:1077-1095): `answering` is in the list, and the first carrier
\* is behind it.  No carrier, or the answering stub gone, is not shadowed.
Shadowed(s, slot) ==
    /\ slot # NoSlot
    /\ SlotIndex(s, slot) # 0
    /\ FirstCarrier(s) > SlotIndex(s, slot)

\* The anchor of insert_or_append_proxy_stub (proxy.rs:1109-1111): the answering stub, else the
\* first proxy stub, else the end of the list (proxy.rs:1149).
InsertIndex(s, slot) ==
    IF SlotIndex(s, slot) # 0 THEN SlotIndex(s, slot)
    ELSE IF FirstProxy(s) # 0 THEN FirstProxy(s)
    ELSE Len(s) + 1

(***************************************************************************)
(* Requests.                                                               *)
(*                                                                         *)
(* pc:        where the request is.                                        *)
(* answering: the proxy stub's slot it matched (handler.rs:1510).          *)
(* token:     the claim its HeldClaim holds, NoToken when none.            *)
(* lost:      `lost_race` (proxy.rs:1401): no claim, so no local stub.     *)
(* again:     it has already claimed again after an empty lookup (#1778).  *)
(* saw:       the recording its AlreadyRecorded was about.                 *)
(* shadowed:  ghost.  Every stub list since its Match shadowed the         *)
(*            recording, so no edit or racing request gives it a reason to *)
(*            replay.                                                      *)
(* landed:    ghost.  Its settle wrote the recording.  The engine cannot   *)
(*            tell: HeldClaim::settle returns nothing (proxy.rs:104).      *)
(***************************************************************************)
InitReq == [pc |-> "idle", answering |-> NoSlot, token |-> NoToken, lost |-> FALSE, again |-> FALSE,
            saw |-> NoRec, shadowed |-> FALSE, landed |-> FALSE]

\* A stub-list change is seen by the ghost of every request still on its way.
Reshadow(r, s) == [x \in DOMAIN r |-> [r[x] EXCEPT !.shadowed = @ /\ Shadowed(s, r[x].answering)]]

NoGhost == [hits |-> 0, recorded |-> 0, forgotWhileClaimed |-> FALSE, forgotOther |-> FALSE,
            replayedShadowed |-> FALSE, unclaimedForward |-> FALSE, unclaimedInsert |-> FALSE,
            carrierNotRecorded |-> FALSE]

Init ==
    /\ responses = IF Preloaded THEN 1 ELSE NoRec
    /\ pending = NoToken
    /\ nextToken = IF Preloaded THEN 2 ELSE 1
    /\ stubs = IF GeneratesStub /\ Preloaded THEN <<Rec(TRUE), Proxy(1)>> ELSE <<Proxy(1)>>
    /\ nextSlot = 2
    /\ req = [r \in Requests |-> InitReq]
    /\ edit = [e \in Editors |-> [pc |-> "idle", saw |-> NoRec]]
    /\ clear = [c \in Clearers |-> [pc |-> "idle", saw |-> NoRec]]
    /\ restart = [u \in Restarters |-> "idle"]
    /\ ghost = IF Preloaded THEN [NoGhost EXCEPT !.recorded = 1] ELSE NoGhost

\* forget_recorded (proxy_store.rs:481-486): the entry goes, whichever recording it now is, and
\* a pending claim is left alone.  `saw` is what the caller decided about.
ForgetGhost(saw) ==
    [ghost EXCEPT !.forgotWhileClaimed = @ \/ pending # NoToken,
                  !.forgotOther = @ \/ (responses # NoRec /\ responses # saw),
                  !.recorded = 0]

\* The snapshot load (core/mod.rs:789), and the first stub of the list answers.  A recorded stub
\* answers from the snapshot and never reaches the store; so does an imposter with no stubs, as a
\* no-match.  The proxy stub goes on to handle_proxy_request (handler.rs:1502).
Match(r) ==
    /\ req[r].pc = "idle"
    /\ IF Len(stubs) > 0 /\ IsProxy(stubs[1])
       THEN req' = [req EXCEPT ![r].pc = "claim", ![r].answering = stubs[1].slot,
                               ![r].shadowed = Shadowed(stubs, stubs[1].slot)]
       ELSE req' = [req EXCEPT ![r].pc = "done"]
    /\ UNCHANGED <<responses, pending, nextToken, stubs, nextSlot, edit, clear, restart, ghost>>

\* try_claim_as for proxyOnce (proxy_store.rs:375-391): the responses read guard is held across
\* the pending lock, so the two reads and the insert are one step (the #171/#118 fix).
\* `onRecorded` is where an AlreadyRecorded goes: the shadow check on the first claim (proxy.rs:1414,
\* only for a request that generates a stub), the lookup on the second (proxy.rs:1423, 1444).
TryClaim(r, onRecorded) ==
    /\ IF responses # NoRec
       THEN /\ req' = [req EXCEPT ![r].pc = onRecorded,
                                  ![r].saw = IF req[r].pc = "claim" THEN responses ELSE @]
            /\ UNCHANGED <<pending, nextToken>>
       ELSE IF pending # NoToken
       THEN /\ req' = [req EXCEPT ![r].pc = "forward", ![r].lost = TRUE]
            /\ UNCHANGED <<pending, nextToken>>
       ELSE /\ pending' = nextToken
            /\ nextToken' = nextToken + 1
            /\ req' = [req EXCEPT ![r].pc = "forward", ![r].token = nextToken]
    /\ UNCHANGED <<responses, stubs, nextSlot, edit, clear, restart, ghost>>

Claim(r) ==
    /\ req[r].pc = "claim"
    /\ TryClaim(r, IF GeneratesStub THEN "shadow" ELSE "lookup")

\* recording_is_shadowed (proxy.rs:1077), its own snapshot load (proxy.rs:1085).
ShadowCheck(r) ==
    /\ req[r].pc = "shadow"
    /\ req' = [req EXCEPT ![r].pc = IF Shadowed(stubs, req[r].answering) THEN "forget" ELSE "lookup"]
    /\ UNCHANGED <<responses, pending, nextToken, stubs, nextSlot, edit, clear, restart, ghost>>

\* forget_recorded on the request path (proxy.rs:1420).
ShadowForget(r) ==
    /\ req[r].pc = "forget"
    /\ responses' = NoRec
    /\ ghost' = ForgetGhost(req[r].saw)
    /\ req' = [req EXCEPT ![r].pc = "reclaim"]
    /\ UNCHANGED <<pending, nextToken, stubs, nextSlot, edit, clear, restart>>

\* The second try_claim_as (proxy.rs:1423); its outcome runs through the match at proxy.rs:1443.
Reclaim(r) ==
    /\ req[r].pc = "reclaim"
    /\ TryClaim(r, "lookup")

\* `lookup` (proxy.rs:1446; proxy_store.rs:466).  A recording replays and the request is done.
\* None is the "AlreadyRecorded but nothing to replay" arm: the request claims once more, and a
\* second miss forwards with `lost_race` set, so no stub (#1778).
Lookup(r) ==
    /\ req[r].pc = "lookup"
    /\ IF responses # NoRec
       THEN /\ req' = [req EXCEPT ![r].pc = "done"]
            /\ ghost' = [ghost EXCEPT !.replayedShadowed = @ \/ (req[r].shadowed /\ responses = req[r].saw)]
       ELSE IF req[r].again
       THEN /\ req' = [req EXCEPT ![r].pc = "forward", ![r].lost = TRUE]
            /\ UNCHANGED ghost
       ELSE /\ req' = [req EXCEPT ![r].pc = "reclaim", ![r].again = TRUE]
            /\ UNCHANGED ghost
    /\ UNCHANGED <<responses, pending, nextToken, stubs, nextSlot, edit, clear, restart>>

\* The upstream call (proxy.rs:1527-1530), which is awaited.
Forward(r) ==
    /\ req[r].pc = "forward"
    /\ req' = [req EXCEPT ![r].pc = "settle"]
    /\ ghost' = [ghost EXCEPT !.hits = @ + 1,
                              !.unclaimedForward = @ \/ (req[r].token = NoToken /\ ~req[r].lost)]
    /\ UNCHANGED <<responses, pending, nextToken, stubs, nextSlot, edit, clear, restart>>

\* After the settle: a stub is inserted unless a concurrent request holds the claim (proxy.rs:1676).
AfterSettle(r) == IF GeneratesStub /\ ~req[r].lost THEN "insert" ELSE "done"

\* HeldClaim::settle (proxy.rs:104), to `complete` or `record` (proxy_store.rs:413-464), under the
\* responses write lock and the pending lock.  A token that no longer names the pending claim
\* changes nothing and still answers Ok (proxy_store.rs:449-452).  A request without a claim has
\* nothing to settle; it still takes this step, on its way out of the upstream await.
Settle(r) ==
    /\ req[r].pc = "settle"
    /\ LET lands == req[r].token # NoToken /\ pending = req[r].token
       IN /\ req' = [req EXCEPT ![r].pc = AfterSettle(r), ![r].landed = lands]
          /\ IF lands
             THEN /\ responses' = IF responses = NoRec THEN req[r].token ELSE responses
                  /\ pending' = NoToken
                  /\ ghost' = [ghost EXCEPT !.recorded = @ + 1]
             ELSE UNCHANGED <<responses, pending, ghost>>
    /\ UNCHANGED <<nextToken, stubs, nextSlot, edit, clear, restart>>

\* The store failed the settle: the claim is released (proxy.rs:123-129), and the caller cannot
\* tell, so a stub is still inserted (proxy.rs:1661, 1688).  The failing call changes nothing, so
\* the release is the only commit and this is one step.
SettleFail(r) ==
    /\ SettleCanFail
    /\ req[r].pc = "settle"
    /\ req[r].token # NoToken
    /\ req' = [req EXCEPT ![r].pc = AfterSettle(r)]
    /\ pending' = IF pending = req[r].token THEN NoToken ELSE pending
    /\ UNCHANGED <<responses, nextToken, stubs, nextSlot, edit, clear, restart, ghost>>

\* insert_or_append_proxy_stub for proxyOnce (proxy.rs:1148-1156), under `stubs_write`.  The stub
\* carries the claim when the request held one (`settled`, proxy.rs:1688).  Its
\* forget_orphaned_claims finds nothing: the insert removes no carrier.
Insert(r) ==
    /\ req[r].pc = "insert"
    /\ LET s == InsertAt(stubs, InsertIndex(stubs, req[r].answering), Rec(req[r].token # NoToken))
       IN /\ stubs' = s
          /\ req' = Reshadow([req EXCEPT ![r].pc = "done"], s)
    /\ ghost' = [ghost EXCEPT !.unclaimedInsert = @ \/ req[r].token = NoToken,
                              !.carrierNotRecorded = @ \/ (req[r].token # NoToken /\ ~req[r].landed)]
    /\ UNCHANGED <<responses, pending, nextToken, nextSlot, edit, clear, restart>>

\* The request's future is dropped, or the forward or a behavior failed and it returned Err: either
\* way HeldClaim::drop releases the claim, token-checked (proxy.rs:133-139; proxy_store.rs:400-411).
\* Only at the awaits: nothing between the settle and the insert can drop the future.
DropRequest(r) ==
    /\ req[r].pc \in {"forward", "settle"}
    /\ pending' = IF req[r].token # NoToken /\ pending = req[r].token THEN NoToken ELSE pending
    /\ req' = [req EXCEPT ![r].pc = "done"]
    /\ UNCHANGED <<responses, nextToken, stubs, nextSlot, edit, clear, restart, ghost>>

(***************************************************************************)
(* Stub edits (mutate_stubs_from, core/mod.rs:840-869; reconcile, life-    *)
(* cycle.rs:49): the swap under `stubs_write`, then forget_orphaned_claims *)
(* outside it, from the before and after lists of the swap.                *)
(***************************************************************************)
\* The stub lists an operator can make out of `s`: a recorded stub moved behind the proxy stub
\* (move_stub), a recorded stub deleted (delete_stub), a recording replaced with new content (its
\* claim goes, core/mod.rs:407), the proxy stub deleted, or every stub replaced with a proxy stub
\* of a new slot (replace_stubs; every state is new, core/mod.rs:1004).
Edits(s) ==
    (IF "move" \in EditKinds
     THEN {RemoveAt(s, i) \o <<s[i]>> : i \in {j \in Indices(s) : IsRec(s[j]) /\ j < FirstProxy(s)}}
     ELSE {})
    \cup
    (IF "delete" \in EditKinds THEN {RemoveAt(s, i) : i \in {j \in Indices(s) : IsRec(s[j])}} ELSE {})
    \cup
    (IF "replace" \in EditKinds
     THEN {Set(s, i, Rec(FALSE)) : i \in {j \in Indices(s) : IsCarrier(s[j])}}
     ELSE {})
    \cup
    (IF "deleteProxy" \in EditKinds THEN {RemoveAt(s, i) : i \in {j \in Indices(s) : IsProxy(s[j])}} ELSE {})
    \cup
    (IF "replaceAll" \in EditKinds THEN {<<Proxy(nextSlot)>>} ELSE {})

\* forget_orphaned_claims (core/mod.rs:912-924): a claim some state in `before` held and none in
\* `after` does.  `kept` is computed from the swap's own `after`, not from the list at the forget.
Orphans(before, after) == HasCarrier(before) /\ ~HasCarrier(after)

EditSwap(e) ==
    /\ edit[e].pc = "idle"
    /\ \E s \in Edits(stubs) :
        /\ stubs' = s
        /\ nextSlot' = IF s = <<Proxy(nextSlot)>> THEN nextSlot + 1 ELSE nextSlot
        /\ edit' = [edit EXCEPT ![e] = [pc |-> IF Orphans(stubs, s) THEN "forget" ELSE "done",
                                        saw |-> responses]]
        /\ req' = Reshadow(req, s)
    /\ UNCHANGED <<responses, pending, nextToken, clear, restart, ghost>>

\* forget_orphaned_claims' store call (core/mod.rs:929), after the swap and outside the lock.
EditForget(e) ==
    /\ edit[e].pc = "forget"
    /\ responses' = NoRec
    /\ ghost' = ForgetGhost(edit[e].saw)
    /\ edit' = [edit EXCEPT ![e].pc = "done"]
    /\ UNCHANGED <<pending, nextToken, stubs, nextSlot, req, clear, restart>>

(***************************************************************************)
(* The two ways the store is cleared.  `clear` (proxy_store.rs:488-496)    *)
(* takes every entry and every in-flight claim, under all three locks.     *)
(***************************************************************************)
ClearGhost == [ghost EXCEPT !.recorded = 0]

\* Imposter::clear_proxy_responses, step one (recording.rs:186).
ClearStore(c) ==
    /\ clear[c].pc = "idle"
    /\ responses' = NoRec
    /\ pending' = NoToken
    /\ ghost' = ClearGhost
    /\ clear' = [clear EXCEPT ![c].pc = "swap"]
    /\ UNCHANGED <<nextToken, stubs, nextSlot, req, edit, restart>>

\* Step two (recording.rs:195-215): every recorded stub goes, and overwrite_all gives every stub
\* left a new slot (core/mod.rs:993-1005), so the proxy stub's slot changes.
ClearSwap(c) ==
    /\ clear[c].pc = "swap"
    /\ LET s == IF FirstProxy(stubs) # 0 THEN <<Proxy(nextSlot)>> ELSE <<>>
       IN /\ stubs' = s
          /\ nextSlot' = nextSlot + 1
          /\ clear' = [clear EXCEPT ![c] = [pc |-> IF Orphans(stubs, s) THEN "forget" ELSE "done",
                                            saw |-> responses]]
          /\ req' = Reshadow(req, s)
    /\ UNCHANGED <<responses, pending, nextToken, edit, restart, ghost>>

\* Step three: mutate_stubs' forget_orphaned_claims (core/mod.rs:866).
ClearForget(c) ==
    /\ clear[c].pc = "forget"
    /\ responses' = NoRec
    /\ ghost' = ForgetGhost(clear[c].saw)
    /\ clear' = [clear EXCEPT ![c].pc = "done"]
    /\ UNCHANGED <<pending, nextToken, stubs, nextSlot, req, edit, restart>>

\* restart_runtime_state (manager.rs:3070-3088): restart_cycles keeps every state's claim
\* (lifecycle.rs:249), and the store is cleared with the stubs left in place.
RestartClear(u) ==
    /\ restart[u] = "idle"
    /\ responses' = NoRec
    /\ pending' = NoToken
    /\ ghost' = ClearGhost
    /\ restart' = [restart EXCEPT ![u] = "done"]
    /\ UNCHANGED <<nextToken, stubs, nextSlot, req, edit, clear>>

(***************************************************************************)
(* The next-state relation.                                                *)
(***************************************************************************)
Next ==
    \/ \E r \in Requests :
        \/ Match(r) \/ Claim(r) \/ ShadowCheck(r) \/ ShadowForget(r) \/ Reclaim(r) \/ Lookup(r)
        \/ Forward(r) \/ Settle(r) \/ SettleFail(r) \/ Insert(r) \/ DropRequest(r)
    \/ \E e \in Editors : EditSwap(e) \/ EditForget(e)
    \/ \E c \in Clearers : ClearStore(c) \/ ClearSwap(c) \/ ClearForget(c)
    \/ \E u \in Restarters : RestartClear(u)

Spec == Init /\ [][Next]_vars

\* Requests and edits are interchangeable; tokens stay as minted, since their order is real.
Symmetry == Permutations(Requests) \cup Permutations(Editors)

(***************************************************************************)
(* Invariants.                                                             *)
(***************************************************************************)
Pcs == {"idle", "claim", "shadow", "forget", "reclaim", "lookup", "forward", "settle", "insert", "done"}

TypeInv ==
    /\ responses \in 0..nextToken
    /\ pending \in 0..nextToken
    /\ \A r \in Requests : req[r].pc \in Pcs
    /\ \A i \in Indices(stubs) : stubs[i].kind \in {"proxy", "rec"} /\ stubs[i].claim \in BOOLEAN

\* At most one recording ahead of the proxy: the winner's.  A second means the protocol recorded
\* the identity twice.
AtMostOneCarrier == Cardinality(FrontCarriers(stubs)) <= 1

\* #1193: a pending claim has a live holder, so a later identical request is not forwarded as
\* InFlight and never recorded for the life of the imposter.
NoWedgedClaim ==
    pending # NoToken =>
        \E r \in Requests : req[r].token = pending /\ req[r].pc \in {"forward", "settle"}

\* The windows in which a recording legitimately has no carrier: the winner settled and has not
\* inserted (proxy.rs:1659, 1683), or an edit or clear swapped and has not forgotten yet.
RecordingWindow ==
    \/ \E r \in Requests : req[r].pc = "insert" /\ req[r].token # NoToken
    \/ \E e \in Editors : edit[e].pc = "forget"
    \/ \E c \in Clearers : clear[c].pc = "forget"

\* #1386: the recording lives as long as the stub it was inserted as.  A store entry with no
\* carrier would keep replaying a recording the operator deleted.
ClaimLifetimeIsStubLifetime ==
    (GeneratesStub /\ responses # NoRec /\ ~HasCarrier(stubs)) => RecordingWindow

\* The other direction: a carrier the requests are answered by has its recording.  A carrier with
\* none lets a request that matched the proxy stub before it landed claim again.  Only a
\* savedProxyResponses clear between its two steps, and a restart, which keeps the stubs on purpose,
\* make one without a race.
CarrierHasRecording ==
    (FrontCarriers(stubs) # {} /\ responses = NoRec) =>
        \/ \E c \in Clearers : clear[c].pc = "swap"
        \/ \E u \in Restarters : restart[u] = "done"

\* #1705: a request that reached the proxy stub with the recording behind it the whole way was not
\* answered by that recording, so it does not replay it.  A newer recording a racing request made
\* for the same miss is another matter: that one replays, as the concurrent first request does.
ShadowedRecordingForwards == ~ghost.replayedShadowed

\* At most one recording lands between two removals of the entry: `record` holding the claim is
\* the only way in, and try_claim_as gives one claim at a time (#171/#118).
RecordedOnce == ghost.recorded <= 1

\* A concurrent loser forwards but never records: inserting its stub too would stack a duplicate
\* per loser (proxy.rs:1399).
LoserNeverRecords == \A r \in Requests : req[r].lost => req[r].pc # "insert"

\* The store's two maps exclude each other for one signature: try_claim_as only claims with no
\* entry, and record clears the claim with the write.
ClaimExcludesRecording == pending # NoToken => responses = NoRec

\* The swap-to-forget window.  forget_recorded is keyed by signature only, so a forget decided
\* about one recording removes whichever is there when it runs.
ForgetRemovesWhatItSaw == ~ghost.forgotOther

\* Only a request that lost the race forwards with no claim (#1778).
ForwardUnrecordedOnlyWhenLost == ~ghost.unclaimedForward

\* Only a request holding a claim inserts a stub: a stub without a claim is not forgotten with its
\* recording and stacks in front of the proxy like a loser's.
OnlyClaimHolderInserts == ~ghost.unclaimedInsert

\* A stub carries a claim only if its request's recording landed.  The store drops a stale token's
\* recording and answers Ok, and fails a settle without the caller seeing it; either way the stub
\* goes in carrying a claim, so a request from before a clear stacks up next to the current one.
CarrierMeansRecorded == ~ghost.carrierNotRecorded

(***************************************************************************)
(* Witnesses: the races the properties depend on stay reachable.           *)
(***************************************************************************)
NeverTwoForwards == ghost.hits <= 1
NeverForgetWhileClaimed == ~ghost.forgotWhileClaimed

=============================================================================
