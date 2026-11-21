------------------------------ MODULE DrFailover ------------------------------
(*
 * Layer 2: failover of a singleton between an HA pair (A, B) with a witness
 * (W), as `bulkhead_host::failover` implements it, with time.
 *
 * Layer 1 (FailoverL1.tla, from spike S16) has atomic steps and abstracts
 * the lease as "it runs out only while its holder cannot renew". Here time
 * is discrete ticks, and every time the code keeps is an age or a countdown,
 * capped where larger values change nothing, so the state space is finite:
 *
 *   - Every link is a FIFO queue per direction. LAN and WAN can be up, held
 *     (frames kept until healed, as QUIC keeps unacknowledged data) or lost
 *     (frames dropped); the serial line is up or lost. On an up link a frame
 *     arrives within Delay ticks. A fault takes one direction or both.
 *   - Each tick a node may beat once and step once, in either order, or
 *     skip either (a slow node). A beat carries the node's status (role,
 *     promoting, epoch) and, from a confirming standby, an echo: the age of
 *     the newest beat it has seen from its partner.
 *   - A node keeps the last status per path and its age; a path is live
 *     while that age is at most Timeout.
 *   - The primary renews through its partner's confirming beats (by echo
 *     with EchoLease, else by arrival) and at the witness (by the renewal's
 *     send time). Its singleton runs while its lease countdown is positive.
 *   - The witness answers each request at once (Granted, NotYet with a wait,
 *     Refused) and keeps no pending request. Its epoch and last promotion
 *     survive its crashes; its renewal record too, unless ForgetRenewals: then
 *     it restarts counting the node it promoted as just renewed (the code).
 *   - A standby asks when it hears its partner on no path; it stops asking
 *     if it hears a primary again with no request in flight, or one past
 *     its retry time, whose answer was lost (cancel); it
 *     steps down after hearing its partner Down for a lease (stand-down).
 *   - Nodes crash and restart Down; their epoch survives only with
 *     PersistEpoch. Replies can be lost with their link.
 *   - A reply carries its request's age, so a lease (from a renewal or a
 *     grant) runs from when the request it answers was sent.
 *   - With SerialLost, there is no serial line: it is lost from the start
 *     and never heals, and its loss is not counted among the faults.
 *   - With MaxSwitchovers > 0, an operator may hand over on purpose (a
 *     planned switchover): a primary whose partner it hears as
 *     a confirming standby stops its singleton at once, steps down and beats
 *     Down and yielding. A standby that hears a yield asks the witness at
 *     once, rather than after hearing nothing for Timeout, and does not
 *     stand down for it; the witness grants by its usual rule. A yielding
 *     node rejoins the new primary, or, a lease later than a down node
 *     would, resumes.
 *   - With OperatorConfirms, an operator may confirm that a standby's
 *     partner is gone while the standby hears it on no path and cannot
 *     reach the witness. The operator is right: the partner is down. Once
 *     the partner's lease is waited out the standby promotes itself without
 *     a lease (an override), and runs until its partner confirms it as
 *     standby; the confirmation lapses if it hears its partner first.
 *
 * Rounds. Each tick runs in phases, which keeps the interleavings to those
 * that matter: faults, then delivery of every frame due (in a fixed link
 * order), then one node's turn and the other's (either node first; in a
 * turn the node may beat, step, both in either order, or neither), then
 * the tick. A frame sent in a round arrives in the next (Delay = 1) unless
 * its link is held or lost.
 *
 * Sessions are not modelled: every delivered frame is acted on, which is
 * the worst case for stale beats. Clocks do not drift.
 *
 * The property: the singleton never runs on both nodes at once.
 *
 * Spike S38 adds a disaster-recovery node: the witness W, at site 2, which
 * an operator may activate by hand when site 1 is lost. Everything above is
 * FailoverL2.tla unchanged when Activations = 0 and SiteLease = FALSE.
 *
 *   - SiteCut, one fault: every WAN direction lost or held at once, site 1
 *     alive and cut off from site 2. SiteDown, one fault: both site-1 nodes
 *     crash at once (a power cut at site 1).
 *   - DrFence (the operator): the witness stops renewing anything and
 *     refuses every request, a resume included. DrActivate: once its wait
 *     is over, it takes a new epoch, names itself promoted, and runs the
 *     singleton; a renewal from site 1 is then answered Fenced.
 *   - The wait. A site-1 primary renews through its partner's beats as
 *     well as at the witness, so a witness that stops answering does not
 *     stop a pair that hears itself. With SiteLease, a primary also needs a
 *     site lease, renewed only at the witness (by the renewal's send time,
 *     SiteLeaseLen - Margin), and the witness activates once SiteLeaseLen
 *     has passed since it last renewed either node's; a witness restart
 *     counts as a renewal (it forgot when it last renewed). Without it,
 *     the witness waits out the lease as it knows it (Hold since the
 *     primary's last renewal), and with OperatorRight the operator
 *     activates only while both site-1 nodes are down.
 *
 * The DR property: the singleton never runs at two sites at once
 * (AtMostOneRunningSites).
 *
 * ROADMAP DR4b adds, with DrApart, a DR node D apart from the witness (W is
 * then the witness only, at site 2 or elsewhere, and never runs the
 * singleton). W stays the one authority: its kept epoch and promoted node
 * decide who runs. D only asks and follows:
 *
 *   - D and W talk over a link of their own (DWan), lost or held as one
 *     D fault (MaxDwFaults, counted apart from MaxFaults, so that a site
 *     fault and a D fault can meet in one run); a crash of D or of W is a
 *     D fault too.
 *   - Each tick D sends W its intent, if nothing of its is in flight:
 *     "state" (none, leading), "fence" (asking, after the operator's
 *     activation) or "handBack" to a pair node (frozen, after the
 *     operator's failback). W acts on it and answers with whom it has
 *     promoted, after acting: fence starts fencing (DrFence's rule) unless
 *     W fences or has promoted D already; handBack promotes the pair node
 *     at a new epoch if W has promoted D. DrActivate then promotes D.
 *   - D leads once an answer says W promoted it (a lost grant recovers by
 *     itself), and stops once one says W promoted another; frozen, it runs
 *     nothing and stays frozen until an answer says W promoted another.
 *   - W's fencing is in memory only: a restart ends it, and D's next fence
 *     starts it again. A restarted D keeps nothing and leads again only on
 *     W's word.
 *   - Rule-off switches: DUnfreezes (a frozen D gives up waiting and leads
 *     again) and DTrustsDisk (a restarted D leads from its own record that
 *     it was active).
 *)
EXTENDS Integers, Sequences, FiniteSets

CONSTANTS A, B, W
CONSTANTS Lease, Margin, Timeout, Delay, MaxEpoch, QueueLen, MaxFaults
\* Design switches: TRUE is the current code, FALSE the code before the fix.
CONSTANTS EchoLease, ConfirmOnSend, PersistEpoch
\* Whether the witness's renewal record is lost when it crashes (FALSE before
\* the record was dropped from what it keeps on disk).
CONSTANT ForgetRenewals
\* Which faults the environment may inject.
CONSTANTS LanFaults, SerialFaults, WanFaults, Crashes, WitnessCrashes
\* Whether links can hold frames (else only lose them).
CONSTANT Holds
\* Whether an operator may confirm a standby's partner gone (R7 at layer 2).
CONSTANT OperatorConfirms
\* A deployment without a serial line: lost from the start, never healed,
\* and not counted as a fault.
CONSTANT SerialLost
\* How many planned switchovers an operator may ask for (not faults).
CONSTANT MaxSwitchovers
\* L1: a yielding primary releases its lease at the witness, which may then
\* grant its partner at once (FALSE: the code before L1, which waits out the
\* lease). ReleaseClears: every grant clears the witness's record of a
\* release (FALSE only to show that it must).
CONSTANTS FastHandover, ReleaseClears
\* S38: operator activations of the DR node (not faults); a site lease and its
\* length; whether the operator activates only with site 1 down; whether a
\* site is lost (cut off from site 2, or both its nodes down) as one fault.
CONSTANTS Activations, SiteLease, SiteLeaseLen, OperatorRight, SiteFaults
\* Design switches for the DR node: while fencing, refuse every request
\* (FALSE: the usual rules); a witness restart counts as a site-lease
\* renewal (FALSE: it forgets, as if it never renewed).
CONSTANTS FenceRefusesAll, WitnessResetsSite
\* DR4b: a DR node D apart from the witness; its faults (link, crash, the
\* witness's crash), bounded apart; operator failbacks (not faults); D's
\* crashes; rule-off switches (FALSE is the design).
CONSTANT D
CONSTANTS DrApart, MaxDwFaults, HandBacks, DCrashes, DUnfreezes, DTrustsDisk

Max(x, y) == IF x >= y THEN x ELSE y
Min(x, y) == IF x <= y THEN x ELSE y

Pair == {A, B}
Partner(n) == IF n = A THEN B ELSE A
Paths == {"lan", "ser"}
Hold == Lease + Margin
\* Ages saturate here: beyond it, no comparison changes.
Cap == Max(IF Timeout > Hold THEN Timeout + 1 ELSE Hold + 1,
           IF SiteLease THEN SiteLeaseLen + 1 ELSE 0)
None == [role |-> "None", promoting |-> FALSE, epoch |-> 0, yields |-> FALSE]

Older(a) == Min(a + 1, Cap)

Lan(f, t) == <<"lan", f, t>>
Ser(f, t) == <<"ser", f, t>>
Wan(f, t) == <<"wan", f, t>>
DWan(f, t) == <<"dwan", f, t>>
DChannels == IF DrApart THEN {DWan(D, W), DWan(W, D)} ELSE {}
Channels == {Lan(A, B), Lan(B, A), Ser(A, B), Ser(B, A)}
            \cup {Wan(n, W) : n \in Pair} \cup {Wan(W, n) : n \in Pair}
            \cup DChannels

VARIABLES
    role, epoch, alive, lease,
    asking,       \* a standby asking to be promoted (its beats then confirm nothing)
    asked,        \* a request was sent in this episode (a grant would be taken)
    inflight,     \* a request's answer is due
    retry,        \* ticks until it may ask again
    confAge,      \* ticks since its beats last confirmed the partner's lease
    downAge,      \* ticks the partner has said Down, or -1
    view,         \* [path -> [st, age]]: the partner's last status per path
    seenAge,      \* age of the newest beat seen from the partner, or -1
    opConfirmed,  \* an operator confirmed the partner gone, not yet acted on
    overridden,   \* a primary running without a lease by that confirmation
    yielding,     \* stepped down for a planned switchover, until it rejoins
    switchovers,  \* planned switchovers asked for so far
    sinceSwitch,  \* ticks since the last switchover, or -1 (for NoFastHandover)
    beaten, stepped,
    wEpoch, wPromoted, wRenewedAge, wAlive,
    wReleased,    \* the node it last promoted released its lease at wEpoch (L1)
    site,         \* S38: a node's site-lease countdown (with SiteLease)
    wSiteAge,     \* S38: ticks since the witness last renewed a node's site lease
    wDr,          \* S38: "none", "fencing" or "active" (on disk)
    activations,  \* S38: activations asked for so far
    dAlive,       \* DR4b: D is up
    dState,       \* DR4b: "none", "asking", "leading", "frozen"
    dTo,          \* DR4b: the pair node D hands back to, while frozen
    dDisk,        \* DR4b: D's own record that it was active (DTrustsDisk only)
    dwFaults,     \* DR4b: D faults so far
    failbacks,    \* DR4b: failbacks asked for so far
    chan, q, faults,
    phase,        \* "env", "deliver", "first", "second", "tick"
    first         \* the node whose turn comes first this round

nodeVars == <<role, epoch, alive, lease, asking, asked, inflight, retry, confAge,
              downAge, view, seenAge, opConfirmed, overridden, yielding, beaten, stepped, site>>
witnessVars == <<wEpoch, wPromoted, wRenewedAge, wAlive, wReleased, wSiteAge, wDr>>
dVars == <<dAlive, dState, dTo, dDisk, dwFaults, failbacks>>
sw == <<switchovers, sinceSwitch, activations, dVars>>
vars == <<nodeVars, witnessVars, chan, q, faults, sw, phase, first>>

Running(n) == alive[n] /\ role[n] = "Primary" /\ (lease[n] > 0 \/ overridden[n])
              /\ (SiteLease => site[n] > 0)
\* The DR node runs the singleton once activated, while it is up: the
\* witness itself, or with DrApart D, once it has heard so.
RunningW == ~DrApart /\ wAlive /\ wDr = "active"
RunningD == DrApart /\ dAlive /\ dState = "leading"
Confirming(n) == role[n] = "Standby" /\ ~asking[n]

Live(n, p) == view[n][p].age <= Timeout
AnyLive(n) == \E p \in Paths : Live(n, p)
LiveSt(n, lanOnly) == {view[n][p].st : p \in {x \in Paths : Live(n, x) /\ (~lanOnly \/ x = "lan")}}
PrimaryEpochs(n, lanOnly) == {s.epoch : s \in {t \in LiveSt(n, lanOnly) : t.role = "Primary"}}
PartnerDown(n) == \E s \in LiveSt(n, FALSE) : s.role = "Down"

Push(qq, c, m) ==
    IF chan[c] = "lost" \/ Len(qq[c]) >= QueueLen
    THEN qq
    ELSE [qq EXCEPT ![c] = Append(@, [m EXCEPT !.due = Delay])]

Init ==
    /\ role = [n \in Pair |-> "Down"]
    /\ epoch = [n \in Pair |-> 0]
    /\ alive = [n \in Pair |-> TRUE]
    /\ lease = [n \in Pair |-> 0]
    /\ asking = [n \in Pair |-> FALSE]
    /\ asked = [n \in Pair |-> FALSE]
    /\ inflight = [n \in Pair |-> FALSE]
    /\ retry = [n \in Pair |-> 0]
    /\ confAge = [n \in Pair |-> 0]
    /\ downAge = [n \in Pair |-> -1]
    /\ view = [n \in Pair |-> [p \in Paths |-> [st |-> None, age |-> 0]]]
    /\ seenAge = [n \in Pair |-> -1]
    /\ opConfirmed = [n \in Pair |-> FALSE]
    /\ overridden = [n \in Pair |-> FALSE]
    /\ yielding = [n \in Pair |-> FALSE]
    /\ switchovers = 0
    /\ sinceSwitch = -1
    /\ beaten = [n \in Pair |-> FALSE]
    /\ stepped = [n \in Pair |-> FALSE]
    /\ wEpoch = 1
    /\ wPromoted = A
    /\ wRenewedAge = [n \in Pair |-> Cap]
    /\ wAlive = TRUE
    /\ wReleased = FALSE
    /\ chan = [c \in Channels |-> IF SerialLost /\ c[1] = "ser" THEN "lost" ELSE "up"]
    /\ q = [c \in Channels |-> <<>>]
    /\ faults = 0
    /\ phase = "env"
    /\ first = A
    /\ site = [n \in Pair |-> 0]
    /\ wSiteAge = [n \in Pair |-> Cap]
    /\ wDr = "none"
    /\ activations = 0
    /\ dAlive = TRUE
    /\ dState = "none"
    /\ dTo = A
    /\ dDisk = FALSE
    /\ dwFaults = 0
    /\ failbacks = 0

\* DR4b: what D asks W for, by its state.
DIntent == CASE dState = "asking" -> "fence"
             [] dState = "frozen" -> "handBack"
             [] OTHER -> "state"

\* ------------------------------------------------------------------ time

\* A tick: once every frame due on an up link has arrived, everything ages.
Tick ==
    /\ phase = "tick"
    /\ phase' = "env"
    /\ UNCHANGED first
    /\ LET aged == [c \in Channels |->
                     [i \in 1..Len(q[c]) |->
                        [q[c][i] EXCEPT !.age = Older(@),
                                        !.due = IF chan[c] = "up" THEN @ - 1 ELSE @]]]
       IN q' = IF DrApart /\ dAlive /\ q[DWan(D, W)] = <<>>
               THEN Push(aged, DWan(D, W), [k |-> DIntent, to |-> dTo, age |-> 0, due |-> 0])
               ELSE aged
    /\ lease' = [n \in Pair |-> Max(lease[n] - 1, 0)]
    /\ retry' = [n \in Pair |-> Max(retry[n] - 1, 0)]
    /\ confAge' = [n \in Pair |-> Older(confAge[n])]
    /\ downAge' = [n \in Pair |-> IF downAge[n] < 0 THEN -1 ELSE Older(downAge[n])]
    /\ seenAge' = [n \in Pair |-> IF seenAge[n] < 0 THEN -1 ELSE Older(seenAge[n])]
    /\ view' = [n \in Pair |-> [p \in Paths |-> [view[n][p] EXCEPT !.age = Older(@)]]]
    /\ wRenewedAge' = [n \in Pair |-> Older(wRenewedAge[n])]
    /\ sinceSwitch' = IF sinceSwitch < 0 THEN -1 ELSE Older(sinceSwitch)
    /\ beaten' = [n \in Pair |-> FALSE]
    /\ stepped' = [n \in Pair |-> FALSE]
    /\ site' = [n \in Pair |-> Max(site[n] - 1, 0)]
    /\ wSiteAge' = IF SiteLease THEN [n \in Pair |-> Older(wSiteAge[n])] ELSE wSiteAge
    /\ UNCHANGED <<role, epoch, alive, asking, asked, inflight, opConfirmed, overridden,
                   yielding, wEpoch, wPromoted, wAlive, wReleased, chan, faults, switchovers,
                   wDr, activations, dVars>>

\* ------------------------------------------------------------------ faults

Faulty(c) == \/ c[1] = "lan" /\ LanFaults
             \/ c[1] = "ser" /\ SerialFaults
             \/ c[1] = "wan" /\ WanFaults
Rev(c) == <<c[1], c[3], c[2]>>
\* A fault takes one direction of a link, or both (one fault either way).
Dirs(c, both) == IF both THEN {c, Rev(c)} ELSE {c}

Hold1(c, both) ==
    /\ Holds /\ faults < MaxFaults /\ Faulty(c) /\ c[1] # "ser"
    /\ \A d \in Dirs(c, both) : chan[d] = "up"
    /\ chan' = [d \in Channels |-> IF d \in Dirs(c, both) THEN "held" ELSE chan[d]]
    /\ faults' = faults + 1
    /\ UNCHANGED <<nodeVars, witnessVars, q, sw>>

Lose(c, both) ==
    /\ faults < MaxFaults /\ Faulty(c)
    /\ \E d \in Dirs(c, both) : chan[d] # "lost"
    /\ chan' = [d \in Channels |-> IF d \in Dirs(c, both) THEN "lost" ELSE chan[d]]
    /\ q' = [d \in Channels |-> IF d \in Dirs(c, both) THEN <<>> ELSE q[d]]
    /\ faults' = faults + 1
    /\ UNCHANGED <<nodeVars, witnessVars, sw>>

\* Held frames move on, due within Delay of the heal.
Heal(c) ==
    /\ ~(SerialLost /\ c[1] = "ser")
    /\ chan[c] # "up"
    /\ chan' = [chan EXCEPT ![c] = "up"]
    /\ q' = [q EXCEPT ![c] = [i \in 1..Len(@) |-> [@[i] EXCEPT !.due = Delay]]]
    /\ UNCHANGED <<nodeVars, witnessVars, faults, sw>>

Crash(n) ==
    /\ Crashes /\ faults < MaxFaults /\ alive[n]
    /\ alive' = [alive EXCEPT ![n] = FALSE]
    /\ role' = [role EXCEPT ![n] = "Down"]
    /\ lease' = [lease EXCEPT ![n] = 0]
    /\ opConfirmed' = [opConfirmed EXCEPT ![n] = FALSE]
    /\ overridden' = [overridden EXCEPT ![n] = FALSE]
    /\ yielding' = [yielding EXCEPT ![n] = FALSE]
    /\ site' = [site EXCEPT ![n] = 0]
    /\ faults' = faults + 1
    /\ UNCHANGED <<epoch, asking, asked, inflight, retry, confAge, downAge, view,
                   seenAge, beaten, stepped, witnessVars, chan, q, sw>>

\* A restart knows nothing but, with PersistEpoch, its epoch; its start
\* counts as its last confirmation of the partner's lease.
Restart(n) ==
    /\ ~alive[n]
    /\ alive' = [alive EXCEPT ![n] = TRUE]
    /\ epoch' = [epoch EXCEPT ![n] = IF PersistEpoch THEN @ ELSE 0]
    /\ asking' = [asking EXCEPT ![n] = FALSE]
    /\ asked' = [asked EXCEPT ![n] = FALSE]
    /\ inflight' = [inflight EXCEPT ![n] = FALSE]
    /\ retry' = [retry EXCEPT ![n] = 0]
    /\ confAge' = [confAge EXCEPT ![n] = 0]
    /\ downAge' = [downAge EXCEPT ![n] = -1]
    /\ view' = [view EXCEPT ![n] = [p \in Paths |-> [st |-> None, age |-> 0]]]
    /\ seenAge' = [seenAge EXCEPT ![n] = -1]
    /\ UNCHANGED <<role, lease, opConfirmed, overridden, yielding, beaten, stepped, site,
                   witnessVars, chan, q, faults, sw>>

WitnessCrash ==
    /\ WitnessCrashes /\ wAlive
    /\ IF DrApart THEN dwFaults < MaxDwFaults ELSE faults < MaxFaults
    /\ wAlive' = FALSE
    /\ faults' = IF DrApart THEN faults ELSE faults + 1
    /\ dwFaults' = IF DrApart THEN dwFaults + 1 ELSE dwFaults
    /\ UNCHANGED <<nodeVars, wEpoch, wPromoted, wRenewedAge, wReleased, wSiteAge, wDr, chan, q,
                   switchovers, sinceSwitch, activations, dAlive, dState, dTo, dDisk, failbacks>>

\* With ForgetRenewals the record is lost: the node last promoted counts as
\* renewed at the restart, never earlier than its true last renewal; the
\* other has no record. A release is kept in memory only: forgotten, the
\* witness waits out the lease again.
WitnessRestart ==
    /\ ~wAlive
    /\ wAlive' = TRUE
    /\ wReleased' = FALSE
    /\ wRenewedAge' = IF ForgetRenewals
                       THEN [n \in Pair |-> IF n = wPromoted THEN 0 ELSE Cap]
                       ELSE wRenewedAge
    \* S38: when it last renewed a site lease is in memory only.
    /\ wSiteAge' = IF ~SiteLease THEN wSiteAge
                   ELSE IF WitnessResetsSite THEN [n \in Pair |-> 0]
                   ELSE [n \in Pair |-> Cap]
    \* DR4b: fencing is in memory only (with the witness as DR node too,
    \* but there the operator asks again, which the model leaves out).
    /\ wDr' = IF DrApart /\ wDr = "fencing" THEN "none" ELSE wDr
    /\ UNCHANGED <<nodeVars, wEpoch, wPromoted, chan, q, faults, sw>>

\* ------------------------------------------------------------------ beats

Status(n) == [role |-> role[n], promoting |-> asking[n], epoch |-> epoch[n],
              yields |-> yielding[n]]

\* On LAN and serial; a primary also renews at the witness.
Beat(n) ==
    /\ alive[n] /\ ~beaten[n]
    /\ LET b == [k |-> "beat", st |-> Status(n),
                 echo |-> IF Confirming(n) THEN seenAge[n] ELSE -1, age |-> 0, due |-> 0]
           r == [k |-> "renew", epoch |-> epoch[n], age |-> 0, due |-> 0]
           q1 == Push(Push(q, Lan(n, Partner(n)), b), Ser(n, Partner(n)), b)
       IN q' = IF role[n] = "Primary" THEN Push(q1, Wan(n, W), r) ELSE q1
    /\ beaten' = [beaten EXCEPT ![n] = TRUE]
    /\ confAge' = IF ConfirmOnSend /\ Confirming(n) THEN [confAge EXCEPT ![n] = 0] ELSE confAge
    /\ UNCHANGED <<role, epoch, alive, lease, asking, asked, inflight, retry, downAge,
                   view, seenAge, opConfirmed, overridden, yielding, stepped, site, witnessVars,
                   chan, faults, sw>>

\* ------------------------------------------------------------------ steps

StepDown(n, e) ==
    /\ role' = [role EXCEPT ![n] = "Down"]
    /\ epoch' = [epoch EXCEPT ![n] = Max(@, e)]
    /\ lease' = [lease EXCEPT ![n] = 0]
    /\ asking' = [asking EXCEPT ![n] = FALSE]
    /\ asked' = [asked EXCEPT ![n] = FALSE]
    /\ inflight' = [inflight EXCEPT ![n] = FALSE]
    /\ confAge' = [confAge EXCEPT ![n] = 0]
    /\ downAge' = [downAge EXCEPT ![n] = -1]
    /\ opConfirmed' = [opConfirmed EXCEPT ![n] = FALSE]
    /\ overridden' = [overridden EXCEPT ![n] = FALSE]

\* Send a request; with ConfirmOnSend, a standby that was confirming until
\* this step counts now as its last confirmation.
Ask(n, resume) ==
    LET ago == IF ConfirmOnSend /\ Confirming(n) THEN 0 ELSE confAge[n]
    IN /\ asked' = [asked EXCEPT ![n] = TRUE]
       /\ inflight' = [inflight EXCEPT ![n] = TRUE]
       /\ retry' = [retry EXCEPT ![n] = Lease]
       /\ confAge' = [confAge EXCEPT ![n] = ago]
       /\ q' = Push(q, Wan(n, W), [k |-> "request", resume |-> resume, epoch |-> epoch[n],
                                   ago |-> ago, age |-> 0, due |-> 0])

Done(n) == stepped' = [stepped EXCEPT ![n] = TRUE]

\* `Fenced` by the partner, a primary at a higher epoch.
StepPrimary(n) ==
    /\ role[n] = "Primary"
    /\ \/ \E e \in PrimaryEpochs(n, FALSE) :
             /\ e > epoch[n]
             /\ StepDown(n, e)
             /\ UNCHANGED <<retry, q, yielding>>
       \/ /\ ~\E e \in PrimaryEpochs(n, FALSE) : e > epoch[n]
          /\ UNCHANGED <<role, epoch, lease, asking, asked, inflight, retry, confAge,
                         downAge, opConfirmed, overridden, yielding, q>>

TopEpoch(n) == Max(epoch[n], IF PrimaryEpochs(n, FALSE) = {} THEN 0
                             ELSE CHOOSE x \in PrimaryEpochs(n, FALSE) :
                                      \A y \in PrimaryEpochs(n, FALSE) : x >= y)

\* The partner is heard: maybe cancel; watch for it saying Down.
StandbyHears(n) ==
    LET e == TopEpoch(n)
        \* A request past its retry time counts as answered (its answer was
        \* lost); a grant after the cancel finds asked FALSE and is ignored.
        cancel == asking[n] /\ (~inflight[n] \/ retry[n] = 0) /\ PrimaryEpochs(n, FALSE) # {}
        newAsking == asking[n] /\ ~cancel
        \* The code before ConfirmOnSend refreshed the confirmation on every
        \* step that hears the partner.
        conf == IF ~ConfirmOnSend /\ ~newAsking THEN 0 ELSE confAge[n]
        down == IF ~PartnerDown(n) THEN -1 ELSE IF downAge[n] < 0 THEN 0 ELSE downAge[n]
        \* The partner yields (a planned switchover) and no path says it is
        \* primary: ask at once, as on silence, and do not stand down.
        yieldHeard == PrimaryEpochs(n, FALSE) = {}
                      /\ \E s \in LiveSt(n, FALSE) : s.role = "Down" /\ s.yields
    IN /\ AnyLive(n)
       /\ \/ /\ yieldHeard
             /\ epoch' = [epoch EXCEPT ![n] = e]
             /\ asking' = [asking EXCEPT ![n] = TRUE]
             /\ downAge' = [downAge EXCEPT ![n] = -1]
             /\ opConfirmed' = [opConfirmed EXCEPT ![n] = FALSE]
             /\ \/ /\ retry[n] = 0
                   /\ Ask(n, FALSE)
                \/ /\ retry[n] > 0
                   /\ UNCHANGED <<asked, inflight, retry, confAge, q>>
             /\ UNCHANGED <<role, lease, overridden, yielding>>
          \/ /\ ~yieldHeard /\ down > Hold
             /\ StepDown(n, e)
             /\ UNCHANGED <<retry, q, yielding>>
          \/ /\ ~yieldHeard /\ down <= Hold
             /\ epoch' = [epoch EXCEPT ![n] = e]
             /\ asking' = [asking EXCEPT ![n] = newAsking]
             /\ asked' = [asked EXCEPT ![n] = IF cancel THEN FALSE ELSE @]
             /\ retry' = [retry EXCEPT ![n] = IF cancel THEN 0 ELSE @]
             /\ inflight' = [inflight EXCEPT ![n] = IF cancel THEN FALSE ELSE @]
             /\ confAge' = [confAge EXCEPT ![n] = conf]
             /\ downAge' = [downAge EXCEPT ![n] = down]
             \* An operator's confirmation is about this silence only.
             /\ opConfirmed' = [opConfirmed EXCEPT ![n] = FALSE]
             /\ UNCHANGED <<role, lease, overridden, yielding, q>>

\* Silent on every path: `Ask`, or with an operator's confirmation and the
\* partner's lease waited out, `Override`.
StandbySilent(n) ==
    /\ ~AnyLive(n)
    /\ \/ /\ opConfirmed[n] /\ confAge[n] >= Hold
          /\ role' = [role EXCEPT ![n] = "Primary"]
          /\ epoch' = [epoch EXCEPT ![n] = TopEpoch(n) + 1]
          /\ overridden' = [overridden EXCEPT ![n] = TRUE]
          /\ opConfirmed' = [opConfirmed EXCEPT ![n] = FALSE]
          /\ asking' = [asking EXCEPT ![n] = FALSE]
          /\ asked' = [asked EXCEPT ![n] = FALSE]
          /\ inflight' = [inflight EXCEPT ![n] = FALSE]
          /\ retry' = [retry EXCEPT ![n] = 0]
          /\ UNCHANGED <<lease, confAge, downAge, yielding, q>>
       \/ /\ ~(opConfirmed[n] /\ confAge[n] >= Hold)
          /\ epoch' = [epoch EXCEPT ![n] = TopEpoch(n)]
          /\ asking' = [asking EXCEPT ![n] = TRUE]
          /\ \/ /\ retry[n] = 0
                /\ Ask(n, FALSE)
             \/ /\ retry[n] > 0
                /\ UNCHANGED <<asked, inflight, retry, confAge, q>>
          /\ UNCHANGED <<role, lease, downAge, opConfirmed, overridden, yielding>>

StepStandby(n) ==
    /\ role[n] = "Standby"
    /\ StandbyHears(n) \/ StandbySilent(n)

RejoinOk(n) == \E e \in PrimaryEpochs(n, TRUE) : e >= epoch[n]

StepDownNode(n) ==
    /\ role[n] = "Down"
    /\ \/ \* `Rejoin` a primary heard on the LAN.
          \E e \in PrimaryEpochs(n, TRUE) :
             /\ e >= epoch[n]
             /\ role' = [role EXCEPT ![n] = "Standby"]
             /\ epoch' = [epoch EXCEPT ![n] = e]
             /\ asked' = [asked EXCEPT ![n] = FALSE]
             /\ retry' = [retry EXCEPT ![n] = 0]
             /\ confAge' = [confAge EXCEPT ![n] = 0]
             /\ yielding' = [yielding EXCEPT ![n] = FALSE]
             /\ UNCHANGED <<lease, asking, inflight, downAge, opConfirmed, overridden, q>>
       \/ \* `Resume`, once its own confirmations are out of date.
          /\ ~RejoinOk(n) /\ confAge[n] >= Hold /\ retry[n] = 0
          /\ Ask(n, TRUE)
          /\ UNCHANGED <<role, epoch, lease, asking, downAge, opConfirmed, overridden, yielding>>
       \/ /\ ~RejoinOk(n) /\ ~(confAge[n] >= Hold /\ retry[n] = 0)
          /\ UNCHANGED <<role, epoch, lease, asking, asked, inflight, retry, confAge,
                         downAge, opConfirmed, overridden, yielding, q>>

Step(n) ==
    /\ alive[n] /\ ~stepped[n]
    /\ \/ StepPrimary(n) \/ StepStandby(n) \/ StepDownNode(n)
    /\ Done(n)
    /\ UNCHANGED <<alive, view, seenAge, beaten, site, witnessVars, chan, faults, sw>>

\* ------------------------------------------------------------------ delivery

Renew(n, age) == lease' = [lease EXCEPT ![n] = Max(@, Lease - Margin - age)]
\* S38: a site lease, from the witness only, from the request's send time.
SiteRenew(n, age) == site' = IF SiteLease THEN [site EXCEPT ![n] = Max(@, SiteLeaseLen - Margin - age)]
                             ELSE site

RecvBeat(c, m) ==
    LET n == c[3]
        confirms == role[n] = "Primary" /\ m.st.role = "Standby" /\ ~m.st.promoting
                    /\ m.st.epoch <= epoch[n]
        age == IF EchoLease THEN m.echo + m.age ELSE 0
    IN /\ view' = [view EXCEPT ![n][c[1]] = [st |-> m.st, age |-> 0]]
       /\ seenAge' = [seenAge EXCEPT ![n] = IF @ < 0 THEN m.age ELSE Min(@, m.age)]
       /\ \/ /\ confirms /\ (~EchoLease \/ m.echo >= 0)
             /\ Renew(n, age)
             \* Confirmed as primary by its partner: an override ends.
             /\ overridden' = [overridden EXCEPT ![n] = FALSE]
          \/ /\ ~(confirms /\ (~EchoLease \/ m.echo >= 0))
             /\ UNCHANGED <<lease, overridden>>
       /\ q' = [q EXCEPT ![c] = Tail(@)]
       /\ UNCHANGED <<role, epoch, asking, asked, inflight, retry, confAge, downAge,
                      opConfirmed, yielding, beaten, stepped, site, witnessVars>>

Reply(qq, n, m) == Push(qq, Wan(W, n), m)

\* S38: the witness renewed n's site lease now (with SiteLease).
SiteRenewed(n) == wSiteAge' = IF SiteLease THEN [wSiteAge EXCEPT ![n] = 0] ELSE wSiteAge

RecvAtWitnessKind(c, m, n, p, qq, goneIn, ok, newEpoch) ==
       \/ /\ m.k = "release"
          /\ q' = qq
          /\ UNCHANGED <<wEpoch, wPromoted, wRenewedAge, wSiteAge>>
       \/ /\ m.k = "renew" /\ m.epoch = wEpoch /\ n = wPromoted /\ wDr = "none"
          /\ wRenewedAge' = [wRenewedAge EXCEPT ![n] = 0]
          /\ SiteRenewed(n)
          /\ q' = Reply(qq, n, [k |-> "leaseOk", age |-> m.age, due |-> 0])
          /\ UNCHANGED <<wEpoch, wPromoted>>
       \* S38: fencing, the witness renews nothing (and so answers nothing).
       \/ /\ m.k = "renew" /\ m.epoch = wEpoch /\ n = wPromoted /\ wDr # "none"
          /\ q' = qq
          /\ UNCHANGED <<wEpoch, wPromoted, wRenewedAge, wSiteAge>>
       \/ /\ m.k = "renew" /\ ~(m.epoch = wEpoch /\ n = wPromoted)
          /\ q' = Reply(qq, n, [k |-> "fenced", epoch |-> wEpoch, age |-> 0, due |-> 0])
          /\ UNCHANGED <<wEpoch, wPromoted, wRenewedAge, wSiteAge>>
       \/ /\ m.k = "request" /\ ~ok
          /\ q' = Reply(qq, n, [k |-> "refused", epoch |-> wEpoch, age |-> 0, due |-> 0])
          /\ UNCHANGED <<wEpoch, wPromoted, wRenewedAge, wSiteAge>>
       \/ /\ m.k = "request" /\ ok /\ goneIn > 0
          /\ q' = Reply(qq, n, [k |-> "notYet", wait |-> goneIn, age |-> 0, due |-> 0])
          /\ UNCHANGED <<wEpoch, wPromoted, wRenewedAge, wSiteAge>>
       \/ /\ m.k = "request" /\ ok /\ goneIn <= 0
          /\ wEpoch' = newEpoch
          /\ wPromoted' = IF m.resume THEN wPromoted ELSE n
          /\ wRenewedAge' = [wRenewedAge EXCEPT ![n] = 0, ![p] = Cap]
          /\ SiteRenewed(n)
          /\ q' = Reply(qq, n, [k |-> "granted", epoch |-> newEpoch, age |-> m.age, due |-> 0])

RecvAtWitness(c, m) ==
    LET n == c[2]
        p == Partner(n)
        qq == [q EXCEPT ![c] = Tail(@)]
        \* Released (L1): the node last promoted stepped down at wEpoch and
        \* said so, so neither its renewals nor the requester's
        \* confirmations of its lease need waiting out.
        released == FastHandover /\ wReleased /\ ~m.resume /\ p = wPromoted
        goneIn == IF released THEN 0
                  ELSE Max(Hold - (m.ago + m.age), Hold - wRenewedAge[p])
        \* S38: while fencing or active, with FenceRefusesAll, nothing is granted.
        ok == /\ ~(FenceRefusesAll /\ wDr # "none")
              /\ IF m.resume THEN n = wPromoted ELSE m.epoch + 1 > wEpoch
        newEpoch == IF m.resume THEN wEpoch ELSE m.epoch + 1
        grants == m.k = "request" /\ ok /\ goneIn <= 0
    IN /\ wReleased' = CASE m.k = "release" /\ FastHandover /\ n = wPromoted
                                /\ m.epoch = wEpoch -> TRUE
                          [] grants /\ ReleaseClears -> FALSE
                          [] OTHER -> wReleased
       /\ RecvAtWitnessKind(c, m, n, p, qq, goneIn, ok, newEpoch)

Keep(n) == UNCHANGED <<role, epoch, lease, asking, asked, inflight, retry, confAge, downAge,
                       opConfirmed, overridden, yielding>>

RecvReply(c, m) ==
    LET n == c[3]
        leaseOk == m.k = "leaseOk" /\ role[n] = "Primary"
        fenced == m.k = "fenced" /\ role[n] = "Primary" /\ m.epoch > epoch[n]
        granted == m.k = "granted" /\ role[n] # "Primary" /\ asked[n]
        refusedS == m.k = "refused" /\ role[n] = "Standby"
        refusedD == m.k = "refused" /\ role[n] = "Down"
    IN /\ q' = [q EXCEPT ![c] = Tail(@)]
       /\ \/ /\ leaseOk
             /\ Renew(n, m.age)
             /\ SiteRenew(n, m.age)
             /\ UNCHANGED <<role, epoch, asking, asked, inflight, retry, confAge, downAge,
                            opConfirmed, overridden, yielding>>
          \/ /\ fenced
             /\ StepDown(n, m.epoch)
             /\ UNCHANGED <<retry, yielding, site>>
          \/ /\ granted
             /\ role' = [role EXCEPT ![n] = "Primary"]
             /\ epoch' = [epoch EXCEPT ![n] = m.epoch]
             /\ lease' = [lease EXCEPT ![n] = Max(Lease - Margin - m.age, 0)]
             /\ asking' = [asking EXCEPT ![n] = FALSE]
             /\ asked' = [asked EXCEPT ![n] = FALSE]
             /\ inflight' = [inflight EXCEPT ![n] = FALSE]
             /\ retry' = [retry EXCEPT ![n] = 0]
             /\ opConfirmed' = [opConfirmed EXCEPT ![n] = FALSE]
             /\ overridden' = [overridden EXCEPT ![n] = FALSE]
             /\ yielding' = [yielding EXCEPT ![n] = FALSE]
             /\ SiteRenew(n, m.age)
             /\ UNCHANGED <<confAge, downAge>>
          \/ /\ m.k = "notYet"
             /\ inflight' = [inflight EXCEPT ![n] = FALSE]
             /\ retry' = [retry EXCEPT ![n] = m.wait]
             /\ UNCHANGED <<role, epoch, lease, asking, asked, confAge, downAge, opConfirmed,
                            overridden, yielding, site>>
          \/ /\ refusedS
             /\ StepDown(n, m.epoch)
             /\ retry' = [retry EXCEPT ![n] = Lease]
             /\ UNCHANGED <<yielding, site>>
          \/ /\ refusedD
             /\ epoch' = [epoch EXCEPT ![n] = Max(@, m.epoch)]
             /\ asked' = [asked EXCEPT ![n] = FALSE]
             /\ inflight' = [inflight EXCEPT ![n] = FALSE]
             /\ retry' = [retry EXCEPT ![n] = Lease]
             /\ UNCHANGED <<role, lease, asking, confAge, downAge, opConfirmed, overridden,
                            yielding, site>>
          \/ \* Anything else is no news to this standing.
             /\ ~leaseOk /\ ~fenced /\ ~granted /\ m.k # "notYet" /\ ~refusedS /\ ~refusedD
             /\ Keep(n)
             /\ UNCHANGED site

\* DR4b: W acts on D's intent, then answers with whom it has promoted.
RecvFromD(c, m) ==
    LET fence == m.k = "fence" /\ wDr = "none" /\ wPromoted # D
        back == m.k = "handBack" /\ wPromoted = D /\ wDr = "active"
        promoted == IF back THEN m.to ELSE wPromoted
        qq == [q EXCEPT ![c] = Tail(@)]
    IN /\ q' = Push(qq, DWan(W, D), [k |-> "wState", promoted |-> promoted, age |-> 0, due |-> 0])
       /\ wDr' = CASE fence -> "fencing" [] back -> "none" [] OTHER -> wDr
       /\ wEpoch' = IF back THEN wEpoch + 1 ELSE wEpoch
       /\ wPromoted' = promoted
       \* As Witness::hand_back's grant, applied: the node handed back to
       \* counts as just renewed, its partner not at all.
       /\ wRenewedAge' = IF back THEN [n \in Pair |-> IF n = m.to THEN 0 ELSE Cap]
                          ELSE wRenewedAge
       /\ UNCHANGED <<wReleased, wSiteAge>>

\* DR4b: D follows whom W has promoted.
RecvAtD(m) ==
    LET mine == m.promoted = D
    IN /\ dState' = CASE dState \in {"none", "asking"} /\ mine -> "leading"
                       [] dState \in {"leading", "frozen"} /\ ~mine -> "none"
                       [] OTHER -> dState
       /\ dDisk' = IF DTrustsDisk THEN (CASE dState \in {"none", "asking"} /\ mine -> TRUE
                                          [] dState \in {"leading", "frozen"} /\ ~mine -> FALSE
                                          [] OTHER -> dDisk)
                   ELSE dDisk

\* Deliver the frame at the head of an up link; one to a dead receiver is lost.
Deliver(c) ==
    /\ chan[c] = "up" /\ q[c] # <<>>
    /\ LET m == Head(q[c])
           to == c[3]
       IN \/ /\ to = W /\ wAlive /\ c[1] # "dwan"
             /\ RecvAtWitness(c, m)
             /\ UNCHANGED <<nodeVars, wAlive, wDr, sw>>
          \/ /\ to = W /\ wAlive /\ c[1] = "dwan"
             /\ RecvFromD(c, m)
             /\ UNCHANGED <<nodeVars, wAlive, sw>>
          \/ /\ to = D /\ dAlive
             /\ RecvAtD(m)
             /\ q' = [q EXCEPT ![c] = Tail(@)]
             /\ UNCHANGED <<nodeVars, witnessVars, switchovers, sinceSwitch, activations, dAlive,
                            dTo, dwFaults, failbacks>>
          \/ /\ (to = W /\ ~wAlive) \/ (to = D /\ ~dAlive) \/ (to \in Pair /\ ~alive[to])
             /\ q' = [q EXCEPT ![c] = Tail(@)]
             /\ UNCHANGED <<nodeVars, witnessVars, sw>>
          \/ /\ to \in Pair /\ alive[to] /\ m.k = "beat"
             /\ RecvBeat(c, m)
             /\ UNCHANGED <<alive, sw>>
          \/ /\ to \in Pair /\ alive[to] /\ m.k # "beat"
             /\ RecvReply(c, m)
             /\ UNCHANGED <<alive, view, seenAge, beaten, stepped, witnessVars, sw>>
    /\ UNCHANGED <<chan, faults>>

\* ------------------------------------------------------------------ spec

\* Links in the order frames are delivered within a round.
Order == <<Lan(A, B), Lan(B, A), Ser(A, B), Ser(B, A),
           Wan(A, W), Wan(B, W), Wan(W, A), Wan(W, B)>>
         \o (IF DrApart THEN <<DWan(D, W), DWan(W, D)>> ELSE <<>>)
Deliverable(c) == chan[c] = "up" /\ q[c] # <<>> /\ Head(q[c]).due = 0
NextDeliverable(c) ==
    /\ Deliverable(c)
    /\ \A i \in 1..Len(Order) : Order[i] = c => \A j \in 1..(i - 1) : ~Deliverable(Order[j])

\* The witness cannot be reached from n: down, or its WAN not up either way.
WitnessUnreachable(n) == ~wAlive \/ chan[Wan(n, W)] # "up" \/ chan[Wan(W, n)] # "up"

\* An operator, asked by a standby that hears its partner on no path and
\* cannot reach the witness, confirms the partner gone. The operator is
\* right: the partner is down. Not a fault.
OperatorConfirm(n) ==
    /\ OperatorConfirms
    /\ alive[n] /\ role[n] = "Standby" /\ ~opConfirmed[n]
    /\ ~AnyLive(n) /\ WitnessUnreachable(n)
    /\ ~alive[Partner(n)]
    /\ opConfirmed' = [opConfirmed EXCEPT ![n] = TRUE]
    /\ UNCHANGED <<role, epoch, alive, lease, asking, asked, inflight, retry, confAge,
                   downAge, view, seenAge, overridden, yielding, beaten, stepped, site, witnessVars,
                   chan, q, faults, sw>>

\* A planned switchover: an operator asks a primary, with a
\* lease of its own and a partner it hears as a confirming standby, to hand
\* over. It stops its singleton and steps down at once, yielding. Not a
\* fault.
Switchover(n) ==
    /\ switchovers < MaxSwitchovers
    /\ alive[n] /\ role[n] = "Primary" /\ ~overridden[n]
    /\ \E s \in LiveSt(n, FALSE) : s.role = "Standby" /\ ~s.promoting
    /\ StepDown(n, epoch[n])
    /\ yielding' = [yielding EXCEPT ![n] = TRUE]
    \* It may resume only after a lease more than a down node waits, so the
    \* standby's request reaches the witness first; if the standby cannot
    \* take over, it resumes then.
    /\ retry' = [retry EXCEPT ![n] = Lease + Hold]
    \* L1: and tells the witness it has released its lease at its epoch;
    \* lost or dropped, the witness waits the lease out as before.
    /\ q' = IF FastHandover
            THEN Push(q, Wan(n, W), [k |-> "release", epoch |-> epoch[n], age |-> 0, due |-> 0])
            ELSE q
    /\ switchovers' = switchovers + 1
    /\ sinceSwitch' = 0
    /\ UNCHANGED <<alive, view, seenAge, beaten, stepped, site, witnessVars, chan, faults,
                   activations, dVars>>

\* ------------------------------------------------------------------ S38

WanChannels == {c \in Channels : c[1] = "wan"}

\* Site 1 alive and cut off from site 2: every WAN direction at once, one fault.
SiteCut(held) ==
    /\ SiteFaults /\ faults < MaxFaults /\ (held => Holds)
    /\ \A c \in WanChannels : chan[c] = "up"
    /\ chan' = [c \in Channels |-> IF c \in WanChannels THEN (IF held THEN "held" ELSE "lost")
                                  ELSE chan[c]]
    /\ q' = IF held THEN q ELSE [c \in Channels |-> IF c \in WanChannels THEN <<>> ELSE q[c]]
    /\ faults' = faults + 1
    /\ UNCHANGED <<nodeVars, witnessVars, sw>>

\* Site 1 lost: both its nodes crash at once, one fault.
SiteDown ==
    /\ SiteFaults /\ faults < MaxFaults /\ alive[A] /\ alive[B]
    /\ alive' = [n \in Pair |-> FALSE]
    /\ role' = [n \in Pair |-> "Down"]
    /\ lease' = [n \in Pair |-> 0]
    /\ opConfirmed' = [n \in Pair |-> FALSE]
    /\ overridden' = [n \in Pair |-> FALSE]
    /\ yielding' = [n \in Pair |-> FALSE]
    /\ site' = [n \in Pair |-> 0]
    /\ faults' = faults + 1
    /\ UNCHANGED <<epoch, asking, asked, inflight, retry, confAge, downAge, view,
                   seenAge, beaten, stepped, witnessVars, chan, q, sw>>

\* The operator starts an activation at the DR node: it renews nothing more.
DrFence ==
    /\ ~DrApart
    /\ activations < Activations /\ wAlive /\ wDr = "none"
    /\ wDr' = "fencing"
    /\ activations' = activations + 1
    /\ UNCHANGED <<nodeVars, wEpoch, wPromoted, wRenewedAge, wAlive, wReleased, wSiteAge, chan, q,
                   faults, switchovers, sinceSwitch, dVars>>

\* Its wait over, the DR node takes a new epoch and runs the singleton.
DrWaited ==
    IF SiteLease THEN \A n \in Pair : wSiteAge[n] >= SiteLeaseLen
    ELSE /\ wRenewedAge[wPromoted] >= Hold
         /\ OperatorRight => (~alive[A] /\ ~alive[B])

DrActivate ==
    /\ wAlive /\ wDr = "fencing" /\ DrWaited
    /\ wDr' = "active"
    /\ wEpoch' = wEpoch + 1
    /\ wPromoted' = IF DrApart THEN D ELSE W
    /\ UNCHANGED <<nodeVars, wRenewedAge, wAlive, wReleased, wSiteAge, chan, q, faults, sw>>

\* DR4b. The operator asks D to activate: from now on D asks W to fence.
DOperatorActivate ==
    /\ DrApart /\ activations < Activations /\ dAlive /\ dState = "none"
    /\ dState' = "asking"
    /\ activations' = activations + 1
    /\ UNCHANGED <<nodeVars, witnessVars, chan, q, faults, switchovers, sinceSwitch, dAlive, dTo,
                   dDisk, dwFaults, failbacks>>

\* The operator asks D to hand back to pair node n: it stops running and
\* asks W, until W says it promoted another.
DOperatorFailback(n) ==
    /\ DrApart /\ failbacks < HandBacks /\ dAlive /\ dState = "leading"
    /\ dState' = "frozen"
    /\ dTo' = n
    /\ failbacks' = failbacks + 1
    /\ UNCHANGED <<nodeVars, witnessVars, chan, q, faults, switchovers, sinceSwitch, activations,
                   dAlive, dDisk, dwFaults>>

\* Rule-off: a frozen D gives up waiting for W and leads again.
DUnfreeze ==
    /\ DrApart /\ DUnfreezes /\ dAlive /\ dState = "frozen"
    /\ dState' = "leading"
    /\ UNCHANGED <<nodeVars, witnessVars, chan, q, faults, switchovers, sinceSwitch, activations,
                   dAlive, dTo, dDisk, dwFaults, failbacks>>

DCrash ==
    /\ DrApart /\ DCrashes /\ dAlive /\ dwFaults < MaxDwFaults
    /\ dAlive' = FALSE
    /\ dwFaults' = dwFaults + 1
    /\ UNCHANGED <<nodeVars, witnessVars, chan, q, faults, switchovers, sinceSwitch, activations,
                   dState, dTo, dDisk, failbacks>>

\* A restarted D keeps nothing: it follows W again (with DTrustsDisk, it
\* leads at once if it last knew itself active).
DRestart ==
    /\ DrApart /\ ~dAlive
    /\ dAlive' = TRUE
    /\ dState' = IF DTrustsDisk /\ dDisk THEN "leading" ELSE "none"
    /\ UNCHANGED <<nodeVars, witnessVars, chan, q, faults, switchovers, sinceSwitch, activations,
                   dTo, dDisk, dwFaults, failbacks>>

\* A D fault on its link to W: one direction or both, lost or held.
DwLose(c, both) ==
    /\ DrApart /\ dwFaults < MaxDwFaults /\ c \in DChannels
    /\ \E d \in Dirs(c, both) : chan[d] # "lost"
    /\ chan' = [d \in Channels |-> IF d \in Dirs(c, both) THEN "lost" ELSE chan[d]]
    /\ q' = [d \in Channels |-> IF d \in Dirs(c, both) THEN <<>> ELSE q[d]]
    /\ dwFaults' = dwFaults + 1
    /\ UNCHANGED <<nodeVars, witnessVars, faults, switchovers, sinceSwitch, activations, dAlive,
                   dState, dTo, dDisk, failbacks>>

DwHold(c, both) ==
    /\ DrApart /\ Holds /\ dwFaults < MaxDwFaults /\ c \in DChannels
    /\ \A d \in Dirs(c, both) : chan[d] = "up"
    /\ chan' = [d \in Channels |-> IF d \in Dirs(c, both) THEN "held" ELSE chan[d]]
    /\ dwFaults' = dwFaults + 1
    /\ UNCHANGED <<nodeVars, witnessVars, q, faults, switchovers, sinceSwitch, activations, dAlive,
                   dState, dTo, dDisk, failbacks>>

Env ==
    \/ \E held \in BOOLEAN : SiteCut(held)
    \/ SiteDown
    \/ DrFence \/ DrActivate
    \/ DOperatorActivate \/ \E n \in Pair : DOperatorFailback(n)
    \/ DUnfreeze \/ DCrash \/ DRestart
    \/ \E c \in DChannels : \E both \in BOOLEAN : DwLose(c, both) \/ DwHold(c, both)
    \/ \E n \in Pair : Switchover(n)
    \/ \E n \in Pair : OperatorConfirm(n)
    \/ \E c \in Channels : \E both \in BOOLEAN : Hold1(c, both) \/ Lose(c, both)
    \/ \E c \in Channels : Heal(c)
    \/ \E n \in Pair : Crash(n) \/ Restart(n)
    \/ WitnessCrash \/ WitnessRestart

Turn(n) == (phase = "first" /\ n = first) \/ (phase = "second" /\ n # first)

Next ==
    \/ phase = "env" /\ Env /\ UNCHANGED <<phase, first>>
    \/ phase = "env" /\ phase' = "deliver"
       /\ UNCHANGED <<nodeVars, witnessVars, chan, q, faults, sw, first>>
    \/ phase = "deliver" /\ \E c \in Channels : NextDeliverable(c) /\ Deliver(c)
       /\ UNCHANGED <<phase, first>>
    \/ /\ phase = "deliver" /\ ~\E c \in Channels : Deliverable(c)
       /\ \E f \in Pair : first' = f
       /\ phase' = "first"
       /\ UNCHANGED <<nodeVars, witnessVars, chan, q, faults, sw>>
    \/ \E n \in Pair : Turn(n) /\ (Beat(n) \/ Step(n)) /\ UNCHANGED <<phase, first, sw>>
    \/ /\ phase \in {"first", "second"}
       /\ phase' = IF phase = "first" THEN "second" ELSE "tick"
       /\ UNCHANGED <<nodeVars, witnessVars, chan, q, faults, sw, first>>
    \/ Tick

Spec == Init /\ [][Next]_vars

\* ------------------------------------------------------------------ properties

AtMostOneRunning == ~(Running(A) /\ Running(B))

AtMostOnePrimaryPerEpoch ==
    ~(alive[A] /\ alive[B] /\ role[A] = "Primary" /\ role[B] = "Primary"
      /\ epoch[A] = epoch[B])

EpochBound == wEpoch <= MaxEpoch /\ \A n \in Pair : epoch[n] <= MaxEpoch

\* S38: the singleton runs at one site at most: on A, on B or on the DR node.
AtMostOneRunningSites ==
    Cardinality({n \in Pair : Running(n)}) + (IF RunningW THEN 1 ELSE 0)
        + (IF RunningD THEN 1 ELSE 0) <= 1

\* Vacuity checks: not properties, but invariants a config must violate, to
\* show the model reaches what the properties are about. NoLeasedRunner:
\* some node runs the singleton on a lease (L2_Vacuity); with Lease = 3,
\* Margin = 1 and Delay = 1 none ever did, since every lease comes from a
\* frame at least two ticks old. NoHandover: a switchover hands the
\* singleton to the other node (L2_Switchover_Reach).
NoLeasedRunner == ~\E n \in Pair : alive[n] /\ role[n] = "Primary" /\ lease[n] > 0
NoHandover == ~(switchovers > 0 /\ \E n \in Pair : Running(n) /\ epoch[n] = 2)
\* NoFastHandover: the other node runs the singleton sooner after a
\* switchover than the old lease and margin (Hold) could have run out
\* (L2_Switchover_Fast_Reach, with FastHandover).
NoFastHandover == ~(sinceSwitch >= 0 /\ sinceSwitch < Hold
                    /\ \E n \in Pair : Running(n) /\ epoch[n] = 2)
\* S38. NoDrRun: the DR node runs the singleton (an activation completes).
\* NoSiteFence: a site-1 primary that still holds its pair lease has lost
\* its site lease while an activation is under way, so the site lease is
\* what stops it.
NoDrRun == ~RunningW
NoSiteFence == ~\E n \in Pair : alive[n] /\ role[n] = "Primary" /\ lease[n] > 0 /\ site[n] = 0
                              /\ wDr = "fencing"
\* DR4b. NoApartRun: D runs the singleton. NoApartHandBack: after a
\* failback a pair node runs it again.
NoApartRun == ~RunningD
NoApartHandBack == ~(failbacks > 0 /\ \E n \in Pair : Running(n))

=============================================================================
