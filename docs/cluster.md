# Lumen clustering

The environment, its clusters, and the machinery that keeps a cluster honest:
membership, quorum, and fencing. Three stages in: the model, the topology
engine, and the read path came first; the workflows second — joining an
environment with a one-time token, and building (or destroying) a cluster
transactionally, per node, per step, unwound completely on failure. This
release completes fencing: the create writes one `fence_ipmilan` device per
member into the CIB, every direction can be live-tested from the console —
guarded, and recorded either way — and the break-glass confirm-peer-dead
exists for the one state that has no automatic resolution. What still lands
after is listed at the end.

```
lumen-storage/
├── lumen-cluster/     the clustering domain (Rust library crate)
└── lumen-drbd/        replicated volumes on top of it — see docs/storage.md
lumen-controlplane/
├── src/api/cluster.rs thin HTTP handlers over lumen-cluster
├── src/api/peer.rs    one control plane answering another
└── src/peers.rs       one control plane calling another (TLS client)
lumen-webui/
└── app/(console)/infrastructure/{clusters,nodes}
```

## Component split

A fifth domain, laid out exactly as the other four:

```
model.rs        what a cluster is: name, members, preferred node, BMCs
environment.rs  the membership record, the CA, join tokens
store.rs        the environment's state on disk: record, identity, tokens
topology.rs     one renderer, two regimes: corosync.conf, fencing, DRBD policy
networks.rs     typed cluster networks — Core, Management, External
join.rs         the workflows: the peer channel, preflight, create + unwind
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
writes are the privileged half, and every one of them is a transient unit:
`install` for the configuration, `systemctl` for the stack, `pcs` for the
CIB.

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
offered only while a peer is unfenced-unreachable, and only after typing the
peer node's name and acknowledging — in the words of the dialog — that a
wrongly-confirmed peer means both sides write the same volumes. Underneath
it is `pcs stonith confirm --force`: Pacemaker recovers as if fencing
succeeded, on the operator's word. The break-glass is an operator action,
not a fencing mechanism — it is the one escape hatch, and the console keeps
it prominent enough to find at 3 a.m.: a red **Confirm dead** button appears
on the node's own row the moment the node is unfenced-unreachable, and
nowhere else, because offering it anywhere else would be offering a way to
corrupt data with one request.

Two things follow from having exactly one fence path. There is **no
supported configuration with fencing disabled** — no API field, no UI
control, and the property renderer always emits `stonith-enabled=true`. And
**fence-device health is cluster-level news**: every device carries a
60-second monitor — the continuous BMC connectivity check — whose failure
degrades the cluster's own health pill rather than hiding in a panel, and a
device that has never been live-tested pins a warning that does not go away —
an untested fence path is one that fails during the outage that needed it.

### The BMC password goes into the CIB over a pipe, and nowhere else

`pcs stonith create` would take the password as an argument, and an argument
lands in the journal and `/proc` — the rule since docs/system.md is that a
password is never an argument. So the fence devices are not created with pcs
at all: the device is rendered as CIB XML, password inside, and piped to
`cibadmin --create --scope resources --xml-pipe` as the standard input of a
transient unit — the same content-over-the-pipe shape as the corosync
authkey. After that the password exists in exactly one place, Pacemaker's
CIB, which is root-only and is where the fence agent has to read it from
anyway. It is deliberately **not** in the membership record — the record is
gossiped, and a gossiped secret is a distributed one — which is also why the
wizard never shows a password back and why changing a BMC password means
recreating the device, not editing a stored copy.

The same reasoning removes the CIB on teardown: `remove_cluster_config`
deletes `/var/lib/pacemaker/cib` along with the corosync configuration,
because a stale CIB would resurrect the old cluster's fence devices — old
passwords included — into the next cluster built on the node.

A placement constraint rides along with each device: `fence-<node>` is
banned from `<node>` itself, because a node hosting its own executioner is a
race the cluster loses.

### A fence test is a real power cycle, and both answers are recorded

The per-direction fence test does the only thing that proves a fence path:
`pcs stonith fence <node>`, for real — the target powers off through its BMC
and boots back up. Anything less (a BMC ping, a status query) tests the
monitor, which the cluster already runs every minute; the thing that fails
during an outage is the power operation itself, so that is what the test
exercises.

Because it is real, it is guarded: an acknowledgement field
(`i_understand_this_power_cycles_the_node`), refused on a cluster that is
not fully healthy — fencing a struggling cluster is an outage, not a test —
and refused from the target's own console, because a node running the test
that powers itself off takes the answer down with it. The outcome is
recorded **either way**: a failed test is not an error in the request, it is
exactly the news the untested warning exists to force out before an outage
finds it first, and it keeps the cluster degraded until the path is fixed
and retested.

The record of tests lives on the membership record (`ClusterRecord::
fence_tests`), not in Pacemaker — `crm_mon` can say whether a device's
monitor passes, never whether the device was ever proven — and it gossips
with the record, so a test run from one console clears the warning on every
console.

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
data: the record carries definitions (nodes, clusters, replicated-volume
placements), while the data itself lives in its cluster's zvols and in
libvirt, not here. Consensus machinery would be a great deal of surface for
a document that a version counter already keeps agreed.

### The token pins the issuer before anything secret moves

Joining is one pasted string. The token carries where to call, a one-time
secret, and the SHA-256 of the certificate the issuer serves — so the
joining node's TLS client accepts exactly that certificate and nothing else,
*before* the token or anything it unlocks crosses the wire. Without the pin,
a machine-in-the-middle holding the connection would be handed the
environment whole: the CA key, the session secret, the membership. With it,
the trust decision is made by the operator who carried the token from one
console to the other — the same operator the whole join already trusts.

The same ordering is why minting the first token bootstraps the environment
*and* swaps the listener onto the environment certificate in the same
request: the token pins the certificate the issuer will present, so that
certificate has to be serving before the token leaves the node.

Tokens are one-time and live fifteen minutes. They are stored hashed, so
reading the token file is not the same as holding a token, and a token that
reached a **failed** join is spent too — fail-closed, because "try the same
token again" and "replay the token I captured" are the same operation.

### The grant carries the CA key, deliberately

Every environment node can mint a token and sign a join — that is what
"minted on any environment node" means — so every node holds the CA key, and
a join hands it over along with the CA, the node's own certificate and key,
the session secret, and the membership record. A certificate-signing request
would keep the newcomer's private key at home and look more proper doing it;
it would also protect one key on a channel that must already be trusted with
all of the others. The pin above is what makes the channel worthy of that,
and the CSR machinery would add parsing and a second round trip to protect
nothing.

### The peer channel: CA-verified TLS and secret-signed tickets, not client certificates

After the join, peers call each other over TLS verified against the
environment CA, and every call carries a **peer ticket** — a one-minute JWT
signed with the environment-shared session secret, `kind: peer`. The server
is proven by its certificate, the client by its ticket: mutual
authentication, using material both sides already hold.

The textbook answer is mTLS with client certificates, and it was declined
on purpose. `axum-server` never surfaces the client's certificate to a
handler — reaching it means reimplementing the accept loop — and an
integration test driving the router directly has no TLS in it at all, so
peer authentication would be untestable exactly where everything else is
tested. The ticket rides in the request, is testable in the same
`tower::oneshot` harness as every session check, and its secret is
distributed precisely as far as the CA key it would be defending against.
One claim keeps the two ticket populations apart, and
`a_peer_ticket_is_not_a_session_and_a_session_is_not_a_peer` pins both
directions.

### pcs configures; it does not install

`pcs cluster setup` was the expected tool and is deliberately not used. It
requires pcsd running on every member and `pcs host auth` against the
`hacluster` account — a daemon, an account, and a password added to the
trust surface so that pcs can write a corosync.conf this crate already
renders and property-tests. Instead the workflow pushes the rendered conf
and a fresh authkey to each member over the peer channel, each member writes
them as a transient unit, and the stack is started with `systemctl`. pcs
remains the tool for what it is good at — CIB-level operations, which are
local and need no pcsd — so `pcs property set` and the VIP's
`pcs resource create` run on the coordinator exactly as `zpool create` runs:
handed to systemd.

Two write-path details worth their sentences. The configuration travels to
`/etc/corosync` as the standard input of `install -m 0600 /dev/stdin` —
content over the pipe, mode and target as typed arguments, no shell, and
nothing secret in argv, which is the rule since docs/system.md. And the
authkey is 256 alphanumeric characters rather than raw bytes: ~1500 bits of
entropy against corosync's need, and text survives every JSON body and pipe
between the coordinator and `/etc/corosync/authkey` without an encoding
step or a base64 decoder process.

### The record is written last

A create runs preflight → generate → prepare each member → start each
member → wait for the cluster to actually form → set properties → **then**
write the membership record. Any failure before the last step tears down
exactly the members that were touched — stack stopped and disabled,
configuration removed, the Core address released — and the record never knew
the cluster existed. That is what makes "a half-created cluster is not a
representable state" an invariant rather than an aspiration, and
`a_failed_create_unwinds_completely` holds it: every node ends unassigned,
and the error names the node and step that failed.

The wizard shows the same truth: the plan is laid out before anything runs —
every step pending, per node — and failure appends visible unwind steps
rather than replacing the history with an apology.

### Joining signs everyone out, and that is the join working

The environment shares one web-session secret, distributed at join and
swapped into the running control plane the moment the grant lands. Every
outstanding session on the joining node — including the operator's own — was
signed with the old secret and dies with it. The console says so before the
operator clicks, and the login page that follows is the success state: the
next sign-in works on every node of the environment, which is the entire
point.

### A node with no cluster stack is standalone, not broken

The clustering backend — and the replicated-storage backend built on it —
is constructed in `main.rs` without a probe or an unavailable fallback. A
fresh appliance has no corosync, no membership record, and no environment —
and that is its ordinary state, not a failure to report. The backend answers "no environment", the service
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
| POST | `/api/environment/tokens` | Mint a one-time join token; the first mint bootstraps the environment |
| POST | `/api/environment/join` | Join with a pasted token; every session on this node is re-signed |
| POST | `/api/environment/preflight` | Judge the named nodes for cluster membership, links included |
| POST | `/api/environment/clusters` | Start a create — `202`, then poll |
| GET | `/api/environment/clusters/pending` | The create in flight (or last finished): per node, per step |
| DELETE | `/api/environment/clusters/:name` | Destroy — every member torn down, `i_understand_this_may_lose_data` required |
| DELETE | `/api/environment/nodes/:name` | Remove an unassigned node from the environment |
| POST | `/api/environment/clusters/:name/fence/:node/test` | Guarded live fence test — the node really power-cycles; `i_understand_this_power_cycles_the_node` required |
| POST | `/api/environment/clusters/:name/nodes/:node/confirm-dead` | Break-glass — only for an unfenced-unreachable node; `i_have_verified_the_node_is_powered_off` required |

A node that never joined an environment answers `GET /api/environment` with
no `environment` object and itself as the one entry in `unassigned` — the
console renders one shape either way.

The peer surface — `/api/peer/join`, `/api/peer/membership`,
`/api/peer/preflight`, `/api/peer/cluster/{prepare,start,teardown}` — is one
control plane answering another: peer-ticket authenticated, except join,
whose one-time token is the authentication. Nothing under `/api/peer` is for
a browser, and no browser session opens any of it.

---

## Web UI

**Infrastructure → Clusters** is the cluster list — one card per cluster with
the health pill, regime, node count, quorum state, fencing summary, and the
pinned untested-fencing warning. The plural is the point: this console
administers every cluster in the environment, from any node. **Create
Cluster** is the wizard, as a tabbed dialog like every other creator in the
console — every tab reachable, invalid ones marked: members with per-node
preflight results and their reasons inline; networks, with per-member NIC
pickers fed by what each node's preflight actually reported, proposed Core
addresses, adopted Management addressing, and the optional VIP; the fencing
seats with their BMC passwords (masked, never shown back); a review of
everything about to be generated; and then the create itself, live — per
node, per step, fence devices included, because a wizard that closes on
submit turns a five-minute workflow into a spinner. Destroy is the
typed-name confirmation the console uses everywhere destruction is meant.

**Infrastructure → Nodes** is every node the console can see: the current
node's card first, then each cluster's members grouped under the cluster's
name — state, rings, fencing, address, version — then the unassigned nodes as
their own group, labelled standalone hypervisors rather than leftovers.
**Add Node** shows the token flow in both directions — mint here, or paste
here — because the operator is standing at one console or the other, and the
page cannot know which. Removing an unassigned node is an inline confirm,
not a modal: the node can simply join again. Each member row carries the
fencing actions: the live fence test behind its own dialog — what will
happen, the acknowledgement, then the answer, pass or fail — and, only while
a node is unfenced-unreachable, the break-glass **Confirm dead**.

Both pages poll at five seconds and render straight from `/api/environment`;
the health pill is derived in the service, so the console and the API can
never disagree about what a card says.

---

## Development

```sh
make test    # installer + all six domain crates + control plane
make lint    # shellcheck, rpmlint, fmt/clippy for eight manifests
```

`lumen-cluster` needs no system libraries: state is read through the cluster
stack's own command-line tools, privileged writes are handed to systemd, and
there is nothing to link. Its tests never form a cluster and never open a
socket — the whole workflow engine runs against `MockPeers` (the in-memory
peer channel) and the mock backend, which is what lets
`a_failed_create_unwinds_completely` and
`a_token_admits_a_node_once_and_only_once` run under `cargo test` on a
laptop. The parsers are tested against captured output of the formats EL10
ships: corosync 3.1, pacemaker 2.1, chrony 4.6.
`MockBackend::environment()` plus `environment_membership()` is the
acceptance scenario — a two-node cluster, a three-node cluster, and one
unassigned node — and the control plane's `tests/cluster_flow.rs` drives the
real router over it: the token bootstrap, the peer-join grant, both
directions of ticket separation, a polled create, a refused-then-acked
destroy, the fence test with its recorded answer, and the break-glass in the
one state it is offered.

The one thing the in-memory tests cannot cover is two real control planes
completing the handshake over real TLS — the fingerprint pin and the
certificate chain. That is the manual test: two nodes, mint on one, paste on
the other, and `openssl s_client` against both afterwards shows the same CA.

## Out of scope for this stage

Replicated volumes have landed — `lumen-drbd`, documented in
docs/storage.md, riding this crate's membership record and topology policy
— and so have machines on replicated disks, live migration, and the
domain-definition replication HA will restart from. Still in the order it
lands: External-network realization (bridges
on every member) and the typed-networks page; the HA manager; adding a node
to an existing cluster and the 2→3 scale-out, and with them removing a
member from a live cluster (a node holding volume replicas cannot leave —
the record now knows enough to refuse); the environment-wide console
federation — aggregated reads with per-node freshness, proxied writes — for
which the peer channel built here is the transport; and gossip beyond the
once-a-minute record exchange. A removed node also keeps its stale
environment state until it re-joins somewhere; a "leave and reset" for the
node itself rides the federation stage. One fencing consequence of the
missing federation is worth naming: a fence test runs from a member of the
cluster it tests, so testing every direction of a two-node cluster means
signing into each node once — the proxied-writes stage dissolves that.
