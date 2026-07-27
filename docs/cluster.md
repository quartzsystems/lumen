# Lumen clustering

The environment, its clusters, and the machinery that keeps a cluster honest:
membership, quorum, and fencing. This stage is the foundation — the model,
the topology engine, and the read path. The workflows that build and change
clusters land in the stages after it and are listed at the end.

```
lumen-storage/
└── lumen-cluster/     the clustering domain (Rust library crate)
lumen-controlplane/
└── src/api/cluster.rs thin HTTP handlers over lumen-cluster
lumen-webui/
└── app/(console)/infrastructure/{clusters,nodes}
```

## Component split

A fifth domain, laid out exactly as the other four:

```
model.rs        what a cluster is: name, members, preferred node, BMCs
environment.rs  the membership record and its reconciliation rule
topology.rs     one renderer, two regimes: corosync.conf, fencing, DRBD policy
networks.rs     typed cluster networks — Core, Management, External
state.rs        what corosync and Pacemaker actually report
validate.rs     pure rules over a definition and the environment
backend/        the supported command line (cli/), plus mock/ and unavailable/
service.rs      the one entry point the control plane calls
```

### The dependency direction

```
lumen-sys  <-  lumen-zfs      <-  lumen-virt
lumen-sys  <-  lumen-cluster
lumen-net  <-  lumen-cluster
lumen-net  <----------------------^
```

Clustering depends on the system domain for one thing — running privileged
cluster commands (`pcs`, fence agents) outside the control plane's sandbox,
through `lumen_sys::exec`, exactly as `zpool create` does — and on the
networking domain because a cluster's networks are validated against what each
node actually has and will be realized through NetworkManager. The direction
is deliberate: a cluster is built on networks, while a network has no reason
to know a cluster exists.

---

## Decisions

### Two tiers, two mechanisms

A Lumen **environment** is one administrative trust domain: every node joins
it, one sign-in works everywhere, and any node's console shows all of it. It
is Lumen's own construct — membership records, a CA, a shared session secret —
held by the control planes with **no corosync anywhere**. A **cluster** is
where corosync and Pacemaker exist: an independent 2–5 node quorum, fencing,
and replication domain inside the environment, one corosync instance per
cluster, spanning only that cluster's nodes.

The split is what keeps each tier simple. Everything that must be *correct
under partition* — membership, quorum, fencing, replicated data — is
cluster-scoped, where corosync already solves it. Everything that must merely
be *convenient* — one console, one login — is environment-scoped, where a
gossiped record and a shared secret are enough. The environment is
administration only, never a data path: no volume, no machine, and no
heartbeat ever crosses a cluster boundary.

A node belongs to exactly one environment and at most one cluster. An
environment node in no cluster is a **valid standalone hypervisor** — today's
appliance, unchanged — and appears in the console as unassigned rather than
as something incomplete.

"Environment" is the working name for the top tier; nothing in the repo had a
term for it (the codebase reserved only "cluster", which keeps its meaning),
and "realm" was already taken by the credential verifiers.

### corosync and Pacemaker own what must be correct; Lumen owns what the operator sees

Membership, quorum, and fencing have to be right the day a partition happens,
and they already are — in corosync and Pacemaker, which have spent twenty
years being wrong so they no longer are. Lumen does not reimplement any of
it. What Lumen owns is the presentation and the policy above it: state is
read from `crm_mon --output-as=xml` (Pacemaker's one machine-readable status
format — the text form is for eyes and changes between releases),
`corosync-quorumtool`, and `corosync-cfgtool -s`, parsed into this crate's
own model and served through the console.

The other half of the division: **Pacemaker does not manage virtual
machines.** libvirt stays the source of truth for machines, exactly as
docs/compute.md establishes, and Lumen's own HA manager (a later stage) will
restart a dead node's machines — only after Pacemaker confirms the node was
fenced. Handing VM lifecycles to Pacemaker would mean two owners for one
machine and a resource agent second-guessing the domain document.

Reads are unprivileged — the status tools answer over sockets the sandbox
does not cover — so nothing in the read path touches `lumen_sys::exec`. The
privileged verbs will, when the workflows land.

### One topology engine, two regimes

Everything that differs between a two-node cluster and a three-to-five node
cluster is decided in `topology.rs` and nowhere else. The rest of the crate —
and `lumen-drbd` after it — asks the engine what to write rather than
carrying its own `if n == 2`.

| | two nodes | three to five |
| --- | --- | --- |
| corosync quorum | `two_node: 1`, `wait_for_all: 1` | plain majority |
| fence race | asymmetric `pcmk_delay_base` | none — quorum decides |
| Pacemaker | (defaults) | `no-quorum-policy=stop` |
| DRBD | `fencing resource-and-stonith` + handlers | volume quorum, suspend on loss |

Two nodes are a real regime, not a degenerate three: there is no majority to
have, so `two_node` keeps the survivor quorate after fencing succeeds,
`wait_for_all` keeps a cold-booting node from claiming quorum before it has
ever seen its peer, and the fence-race delay decides who survives a partition
where both sides are alive and shooting. At three and above all of that
disappears, because majority quorum answers the same questions better.

`topology::tests::the_invariants_hold_for_every_supported_size` walks every N
in 2..=5 — exhaustively, because the domain is four values — and holds all
four rows, plus: every node has exactly one fence device, every device
targets a member, and STONITH can never be off. The 2→3 scale-out drops out
of this design for free: regenerate from the grown definition and the
two-node mechanisms simply stop being emitted
(`growing_from_two_to_three_drops_the_two_node_mechanisms`).

### The fence delay protects its target

`pcmk_delay_base` on a fence device delays actions *through* that device —
that is, delays killing that device's **target**. So the preferred node of a
two-node cluster carries the delay on **its own** device: the peer must wait
ten seconds to kill it, it waits zero to kill the peer, and it wins the race.
Getting this backwards silently inverts the preference and nothing fails
until the wrong node survives a partition. That is why the assignment is
rendered in one place and pinned by
`the_fence_delay_sits_on_the_preferred_nodes_own_device`.

### IPMI is the only fencing mechanism — deliberately

Every piece of Lumen hardware has a BMC, so fencing is one `fence_ipmilan`
device per node and nothing else: no sbd, no watchdog, no second level, at
any node count. One fence path is one thing to configure, one thing to test,
and one thing to reason about during an incident — a fencing topology with
fallback levels is exactly the kind of rarely-exercised machinery that fails
the first time it is needed.

**The consequence is accepted, and it is this:** at any N, a partition that
coincides with the BMC path being unreachable has no automatic resolution.
Nothing can prove the peer is dead, so nothing pretends to. DRBD's fencing
policy suspends I/O on the affected volumes — integrity over availability,
always — until either the BMC path recovers and fencing completes on its own,
or an operator performs the break-glass **confirm peer is dead** operation:
available only while a peer is unfenced-unreachable, and only after typing
the peer node's name and acknowledging that confirming a live peer dead
destroys data. The break-glass is an operator action, not a fencing
mechanism — it is the one escape hatch, and the console keeps it prominent
enough to find at 3 a.m. rather than buried in a menu.

Two things follow from having exactly one fence path, and both are already in
this stage's read model. There is **no supported configuration with fencing
disabled** — no API field, no UI control, and the property renderer always
emits `stonith-enabled=true`. And **fence-device health is cluster-level
news**: the BMC connectivity monitor failing degrades the cluster's own
health pill rather than hiding in a panel, and a device that has never been
live-tested pins a warning that does not go away — an untested fence path is
one that fails during the outage that needed it.

### The membership record is last-writer-wins, and that is enough

The environment's membership record is a small document — nodes, addresses,
cluster assignments — replicated by gossip between control planes. It
reconciles by a version counter: higher version wins whole; a tie is broken
by comparing the serialized records, which is arbitrary but computes the same
answer on every node, and that is the property that matters
(`a_tie_resolves_the_same_way_from_both_sides`). Records from different
environments are never merged — adopting a stranger's membership over gossip
would be a takeover, not a sync.

This is deliberately not Raft and not a CRDT. The record changes when an
operator runs a workflow — a few times a year, not a few times a second —
and the cost of losing a concurrent write is re-running a workflow, never
data: volumes and machines live in their cluster and in libvirt, not here.
Consensus machinery would be a great deal of surface for a document that a
version counter already keeps agreed.

### A node with no cluster stack is standalone, not broken

The clustering backend is the one domain in `main.rs` constructed without a
probe or an unavailable fallback. A fresh appliance has no corosync, no
membership record, and no environment — and that is its ordinary state, not
a failure to report. The backend answers "no environment", the service
answers with the node itself as the one unassigned node, and the console
renders the same shape it will render for a six-node environment. A
*corrupt* membership record, by contrast, is an error that names the file —
quietly falling back to standalone would make the environment appear to
vanish, which is far worse than an error message.

### An unreachable cluster is presented, not dropped

The environment view lists every cluster the membership record names. One
whose state cannot be read appears with health `unknown`, the reason it
could not be asked, and its members listed from the record with nothing
claimed about them — rather than disappearing from the answer, which an
operator would read as "the cluster is gone" at exactly the moment it is
merely unreachable. Same rule as everywhere else in the console: say what is
known, and say that the rest is not known.

---

## API

All routes require a valid session and use the existing error envelope
`{ "error": … }`. The shapes extend the repo's "grouped by node" convention
one level upward: grouped by cluster, then by node.

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET | `/api/environment` | The whole environment: every cluster with its nodes, quorum, and fencing, plus the unassigned nodes |
| GET | `/api/environment/clusters/:name` | One cluster, the same view the environment answer carries |

A node that never joined an environment answers `GET /api/environment` with
no `environment` object and itself as the one entry in `unassigned` — the
console renders one shape either way.

---

## Web UI

**Infrastructure → Clusters** is the cluster list — one card per cluster with
the health pill, regime, node count, quorum state, fencing summary, and the
pinned untested-fencing warning. The plural is the point: this console
administers every cluster in the environment, from any node. A node with no
environment gets an explanation of what an environment is, not an empty
table. Create Cluster is present and disabled, carrying the reason — the
wizard lands with a later stage, and a control that says why it is grey beats
one that is silently absent.

**Infrastructure → Nodes** is every node the console can see: the current
node's card first, then each cluster's members grouped under the cluster's
name — state, rings, fencing, address, version — then the unassigned nodes as
their own group, labelled standalone hypervisors rather than leftovers. Add
Node is disabled the same way Create Cluster is.

Both pages poll at five seconds and render straight from `/api/environment`;
the health pill is derived in the service, so the console and the API can
never disagree about what a card says.

---

## Development

```sh
make test    # installer + all five domain crates + control plane
make lint    # shellcheck, rpmlint, fmt/clippy for seven manifests
```

`lumen-cluster` needs no system libraries: state is read through the cluster
stack's own command-line tools, and there is nothing to link. Its tests never
form a cluster — the mock backend simulates a full environment (two clusters,
partitions, fence failures, an unreachable cluster) in memory, and the
parsers are tested against captured output of the formats EL10 ships:
corosync 3.1 and pacemaker 2.1. `MockBackend::environment()` is the
acceptance scenario — a two-node cluster, a three-node cluster, and one
unassigned node — and the control plane's `tests/cluster_flow.rs` drives the
real router over it.

## Out of scope for this stage

Everything that changes a cluster, in the order it lands: environment join
(the one-time token, the CA, the shared session secret, the mTLS peer
channel and gossip); the cluster-create workflow with per-node preflight and
full unwind on failure; typed-network realization through lumen-net; fence
devices, per-direction fence tests, continuous BMC monitoring, and the
break-glass confirm; replicated volumes (`lumen-drbd`); the HA manager;
node add/remove and the 2→3 scale-out; the Management VIP; and the
environment-wide console federation (aggregated reads, proxied writes, the
environment-shared session secret). The read model in this stage was shaped
so each of those arrives as new verbs over the same views rather than as a
reshaping of them.
