# Spike S38: a model of disaster recovery

**Question.** Phase 8 adds a node at a second site that keeps a copy of the pair's replicated state, shipped asynchronously (about 5 minutes behind at most, the pair never waiting for it), and that an operator activates by hand when site 1 is lost (decided 2026-11-10). What must activation do so that the singleton never runs at two sites, a site 1 that is alive and cut off included? And is an acknowledged write lost only if it came after the DR node's last applied entry?

**Answer.**

**The ROADMAP's proposed activation is not safe.** A site-1 primary renews its lease through its partner's beats as well as at the witness (`failover/node.rs`, `Primary::step`). So a DR witness that renews nothing and waits out the lease leaves a pair that hears itself running the singleton beside the DR node.
- **`DrF_Proposal`:** with no fault at all, both sites run 69 states in, after an activation that waited out the lease as the witness saw it.
- **`DrF_Proposal_Cut`:** the same with site 1 alive and cut off from site 2.
- **The data path:** site 1 goes on committing after the activation, so what D committed no longer agrees with what clients were told (`DrR_Proposal_Data`).

**Two remedies are safe, each with a cost.**
- **A site lease.** With a DR node configured, a primary runs singletons, and so commits (S20's R5 and `CommitWhileRunning`), only while it holds a second lease that only the DR witness renews. Activation stops renewing and waits it out, so a site 1 that is alive and cut off stops by itself.
  - **Costs:**
    - a site-2 or WAN outage longer than the site lease stops site 1's singletons and its writes;
    - an operator's `failover confirm` at site 1 cannot run a singleton while site 2 is unreachable.
  - **The witness's record:** its record of when it last renewed a site lease is in memory, so after a restart it must count as just renewed. Forgetting it lets the DR node activate under a live site lease (`DrF_NoWitnessReset`).
- **Operator fencing.** Activation waits for the operator to confirm that site 1 is down or fenced, as `failover confirm` does today. The pair stays independent of site 2, and safety rests on the operator being right (VERIFICATION.md, A9). A DR node beside a witness elsewhere has only this remedy.

**Writes.**
- **D applies committed entries only.** If it took one before commit, a failover at site 1 could replace that entry, and D's copy would then contradict an acknowledged write before its own end (`DrR_NoCommittedOnly`: A writes, ships, crashes; B commits another write at the same index).
- **What is lost:** with committed entries only, sent by any node (the standby or an alone primary), an activation keeps exactly D's copy. The writes lost are exactly those after D's last applied entry (`NoLostWriteDr`, in every design config).

**Failback.**
- **The site-1 node:** a node of site 1 rejoins as a standby of the DR node by taking D's committed log. It drops what it had after D's point, its commit index included: S20's rules only ever raise a commit index, so DR4's catch-up must lower it. The model reseeds abstractly; S20 checks the catch-up mechanics.
- **The hand-back:** once both nodes have caught up, D hands back through a new epoch at the witness.

**Refusing grants while fencing** is not needed for safety (`DrF_NoFenceRefusal`, 30 minutes of random traces, none unsafe): a grant also renews a site lease, which only postpones the activation. It is needed for the activation to happen at all, and the design keeps it.

**Unchanged when off.** With no DR node configured, each model has exactly its parent's states: `DrF_Off1` 288,754,513, as `L2_Crashes_Fixed1`; `DrR_Off1` 82,040,501, as S20's `Eager1`.

## Model

**Moved:** the models, their configs and the scripts are in `docs/tla` since ROADMAP DR4 (2026-11-19): `DrFailover.tla`, `DrReplication.tla`, `DrF_*.cfg`, `DrR_*.cfg`, `dr-mkcfg.sh` (was `mkcfg.sh`) and `dr-run.sh` (was `run.sh`, its variables `DR_*` for `S38_*`). The conformance of the code DR4 built is in `docs/tla/VERIFICATION.md`, "Disaster recovery".

Two models, each a copy of its parent with the DR node behind switches. With the switches off, each has its parent's states exactly (the `Off` configs below).

- **`DrFailover.tla`** is `docs/tla/FailoverL2.tla`, with time.
  - **The DR node is the witness W**, one machine at site 2, as ROADMAP Phase 8 proposed.
  - **Two new faults, one fault each:**
    - `SiteCut`: every WAN direction at once, lost or held; site 1 is alive and cut off.
    - `SiteDown`: both site-1 nodes crash at once.
  - **The operator's `DrFence`:** the witness renews nothing and refuses every request.
  - **`DrActivate`:** once the witness's wait is over, it takes a new epoch, names itself promoted, and runs the singleton. A renewal from site 1 is then answered `Fenced`.
  - **The site lease** (`SiteLease`):
    - a primary also needs a lease that only the witness renews, from the renewal's send time, of `SiteLeaseLen`;
    - the witness waits `SiteLeaseLen` after it last renewed either node's;
    - a witness restart counts as a renewal, since it does not know when it last renewed.
  - **Without the site lease,** the witness waits out the pair's lease as it knows it (`Hold` since the primary's last renewal at the witness). With `OperatorRight`, the operator activates only while both site-1 nodes are down.
  - **The property:** `AtMostOneRunningSites`, at most one of A, B and W runs the singleton.
- **`DrReplication.tla`** is `spikes/s20/PairReplication.tla`, with atomic steps and the lease abstracted as there.
  - **The replica D** is at the witness's machine.
  - **Shipping:** a live pair node that reaches site 2 sends D the next entry of its committed prefix. When that entry is compacted, it sends the prefix itself. D applies only committed entries, in order. Which node ships is left open: any may, the standby or an alone primary.
  - **Activation:**
    - D leads from its copy at a new epoch;
    - what clients were told becomes D's copy;
    - site-1 nodes are stale until reseeded.
  - **Failback**, abstract (S20 checks catch-up itself):
    - a stale node that reaches D takes D's committed log as a standby;
    - once both have caught up, D hands back, and the witness promotes one of them at a new epoch.
  - **The site lease**, as above, abstracted: it runs out only while its holder cannot reach a witness that is not fencing.
  - **The properties:**
    - `AtMostOneRunningSites`;
    - `DrPrefix`: D holds only acknowledged writes, and `DrNoConflict`, never one that contradicts an acknowledged write;
    - `NoLostWriteDr`: what clients were told only grows, except at an activation, which keeps exactly D's copy;
    - `StaleInert`: a stale node neither runs nor serves a read;
    - S20's `CommittedAgree`, `LeaderComplete`, `ReadLinearizable` and `CommittedStable`, over the nodes that are not stale.

## Results

Checked on 2026-11-17 and 18 with TLC 2026.02.28 (OpenJDK 21) on the 16-core development machine, idle apart from the runs. "Site faults" means the site cut and the loss of both site-1 nodes, one fault each; "all faults" adds a crash, a lost link (LAN, serial, WAN) and a witness crash. Every config has one activation at any moment, and DrReplication's has the failback.

| Config | Faults | Distinct states | Result |
| --- | --- | --- | --- |
| `DrF_SiteLease_Site1` | one site fault | 628,912,597 | pass (1 h 34 min, 12 workers, depth 335) |
| `DrF_Operator_Site1` | one site fault | 278,140,883 | pass (42 min, 12 workers, depth 335) |
| `DrF_SiteLease_Site_holds1` | one site fault, frames held through a cut | over 10,428,745,308 at depth 131, 299 million still queued | no violation; stopped by hand after 34 h (12 workers), the queue still growing. Its last checkpoint can resume it (`-recover`) |
| `DrF_SiteLease1`, `DrF_Operator1` | all faults, one | random: about 588,000 traces, 310 million states each | no violation (30 min each, 4 workers) |
| `DrF_SiteLease2`, `DrF_Operator2` | all faults, two | random: 559,000 and 552,000 traces, about 310 million states each | no violation (30 min each) |
| `DrF_Off1` | as `L2_Crashes_Fixed1`, no DR node | 288,754,513 | pass, the parent's count exactly (1 h 4 min, 4 workers, depth 340) |
| `DrF_Proposal` | none | 1,106,522 (stopped) | `AtMostOneRunningSites` violated: the proposal as written |
| `DrF_Proposal_Cut` | one site cut | 29,991,155 (stopped) | `AtMostOneRunningSites` violated: site 1 alive and cut off |
| `DrF_NoWitnessReset_W`, `DrF_NoWitnessReset` | a witness crash; all faults | 479,627 (stopped); random, 4 min | `AtMostOneRunningSites` violated, as required |
| `DrF_NoFenceRefusal` | all faults, one | random: 586,000 traces, 309 million states | no violation: not a safety rule (above) |
| `DrF_Vac_Run`, `DrF_Vac_OperatorRun`, `DrF_Vac_SiteFence`, `DrF_Vac_Leased` | one site fault | 78 to 32,794,379 (stopped) | each violated, as required: the DR node runs under either remedy, the site lease is what stops a site-1 primary, a node runs on a lease |
| `DrR_SiteLease_Site1` | one site fault | 126,549,511 | pass (1 h 27 min, 4 workers, depth 51) |
| `DrR_Operator_Site1` | one site fault | 81,711,997 | pass (58 min, 4 workers, depth 48) |
| `DrR_SiteLease1`, `DrR_Operator1` | all faults, one | random: 1,693,000 and 316,000 traces, about 130 million states each | no violation (30 min each) |
| `DrR_SiteLease2`, `DrR_Operator2` | all faults, two | random: 972,000 and 382,000 traces, about 132 million states each | no violation (30 min each) |
| `DrR_Off1` | as S20's `Eager1`, no DR node | 82,040,501 | pass, the parent's count exactly (54 min, 4 workers, depth 49) |
| `DrR_Proposal`, `DrR_Proposal_Data` | none | 48 and 313 (stopped) | `AtMostOneRunningSites`, then `DCommittedAgree`, violated |
| `DrR_NoCommittedOnly` | a crash | 3,831 (stopped) | `DrNoConflict` violated, as required |
| `DrR_Vac_Ship`, `_Loss`, `_OperatorLoss`, `_DrWrite`, `_Failback` | one site fault | 232 to 269,100 (stopped) | each violated, as required: D applies entries, an activation loses writes (either remedy), D commits its own, a failback hands the singleton back to site 1 |

**Not covered.**
- Frames held through a site cut, exhaustively: `DrF_SiteLease_Site_holds1` was stopped short (above). Random traces with held frames (`DrF_SiteLease1`, `DrF_SiteLease2`) found nothing either.
- A DR node apart from the witness (it has the operator's remedy only, above).
- More than one activation.
- Clock drift between sites: as in FailoverL2, clocks do not drift. DR4 gives the site lease a margin for drift, as `margin_ms` does for the pair's lease.
- The lag and the RPO target, which are not safety properties.

## How to run

```sh
# Requires Java 11+ and tla2tools.jar at ~/tla/tla2tools.jar.
cd docs/tla
bash dr-mkcfg.sh                                  # writes every config
./dr-run.sh DrF_Proposal DrF_Vac_Run              # into ~/tla-runs/dr-<date>
DR_WORKERS=12 ./dr-run.sh DrF_SiteLease_Site1     # exhaustive, with checkpoints
DR_SIMULATE=30 ./dr-run.sh DrF_SiteLease2         # random traces for 30 minutes
```

The runs above were made from `spikes/s38` with the old names, into `~/tla-runs/s38-<date>`.

`DrF_*` configs check `DrFailover.tla`, and `DrR_*` configs check `DrReplication.tla`. `-deadlock` is passed because both models stop once their bounds are reached.
