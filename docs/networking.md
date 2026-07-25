# Lumen networking

Bridges, bonds, VLAN interfaces, and per-adapter settings, configured through
NetworkManager over the system bus, with a management bridge (`br0`) carrying
the management address.

```
lumen-networking/
├── nicnames/          lumen-nicnames: deterministic nic0..nicN naming
├── lumen-net/         the networking domain (Rust library crate)
└── system/            appliance integration files
lumen-controlplane/
└── src/api/network.rs thin HTTP handlers over lumen-net
lumen-webui/
├── components/console/DataTable.tsx   search/filter/resize/hide columns
└── app/(console)/networking/{overview,interfaces}
```

## Component split

`lumen-networking/lumen-net` owns networking. `lumen-controlplane` owns HTTP.

```
model.rs      desired state — what the operator wants
state.rs      observed state — what the box actually has
validate.rs   pure rules over the two
plan.rs       the two -> an ordered list of backend operations
backend/      NetworkManager over D-Bus (nm/), plus mock/ and unavailable/
store.rs      committed, staged, and in-flight state on disk
service.rs    stage -> validate -> checkpoint -> apply -> confirm
```

The control plane's `src/api/network.rs` deserializes, calls one
`NetworkService` method, and serializes. There is no netlink, no D-Bus, and no
validation logic above `lumen-net`.

`lumen-net` is a **path dependency**, not a workspace member:

```toml
lumen-net = { path = "../lumen-networking/lumen-net" }
```

`lumen-installer/app` and `lumen-controlplane` are independent manifests today
and the Makefile drives them with separate `--manifest-path`/`--target-dir`
pairs. A path dependency needs no workspace, and introducing one at the repo
root would change how every existing build target resolves. `make lint`,
`make test`, and a `networking` job in CI cover the new crate on its own.

## Backend: NetworkManager over D-Bus

NetworkManager already owns the management connection — the installer writes
`/etc/NetworkManager/system-connections/management.nmconnection` — so stage 1
keeps it as the configuration engine and drives it over the system bus with
[zbus](https://crates.io/crates/zbus).

This is load-bearing, not incidental. `lumen-controlplane.service` runs with
`ProtectSystem=strict` (so `/etc` is read-only to it) and
`ProtectKernelTunables=yes` (so `/proc/sys` is read-only). NetworkManager
performs the privileged filesystem and sysctl work **in its own process**, so
none of that hardening has to be weakened to configure a bridge.

zbus is pure Rust. Crates that run bindgen/libclang at build time are excluded
deliberately, for the same reason the PAM layer is a ~150-line in-tree FFI —
libclang stays out of the appliance and CI toolchains.

`nmcli` is never shelled out to and its output is never parsed.

### D-Bus surface used

| Object | Interface | Members |
| ------ | --------- | ------- |
| `/org/freedesktop/NetworkManager` | `org.freedesktop.NetworkManager` | `GetAllDevices`, `ActivateConnection`, `DeactivateConnection`, `CheckpointCreate`, `CheckpointRollback`, `CheckpointDestroy`, `CheckpointAdjustRollbackTimeout`, `Checkpoints`, `Version` |
| `…/Settings` | `…NetworkManager.Settings` | `ListConnections`, `AddConnection`, `GetConnectionByUuid`, `ReloadConnections` |
| per-connection | `…Settings.Connection` | `GetSettings`, `Update`, `Delete` |
| per-device | `…NetworkManager.Device` | `Interface`, `DeviceType`, `State`, `Managed`, `Mtu`, `ActiveConnection`, `Ip4Config` |
| per-device | `…NetworkManager.Device.Wired` | `PermHwAddress`, `HwAddress`, `Speed`, `Carrier` |
| per-address-set | `…NetworkManager.IP4Config` | `AddressData`, `Gateway` |
| per-activation | `…NetworkManager.Connection.Active` | `Connection`, `Uuid`, `Id` |

Connection settings are `a{sa{sv}}`. Exactly one module knows that —
`backend/nm/settings.rs` — which builds and reads them through a small typed
helper rather than scattering `HashMap<String, HashMap<String, Value>>`
through the code.

---

## Decisions

### `controller=` / `port-type=`, not `master=` / `slave-type=`

**Chosen: `controller=` / `port-type=`, one spelling everywhere** — in the
D-Bus settings the control plane writes and in the keyfiles the installer
writes. On read, either spelling is accepted (`settings::get_controller`), so
a profile written by an older appliance or by hand still parses.

The new names landed in NetworkManager **1.46**. AlmaLinux 10.2 — the release
pinned in `iso/upstream.env` — is far past that.

**Version observed:** not confirmed from a running EL10 host in this
environment (no NetworkManager and no reachable AlmaLinux mirror from the
build container). Because a guess here would produce a bridge with no ports
and no error — an older daemon ignores unknown properties silently —
`NmBackend::connect()` reads `org.freedesktop.NetworkManager.Version` at
startup and **refuses to start** below 1.46 with a message naming the version
it found. The manual test script below opens with `nmcli --version` so the
real number gets recorded the first time this runs on hardware.

### `ProtectSystem=strict` needed no relaxation

**No `ReadWritePaths=` was added.** The unit in
`lumen-controlplane/systemd/lumen-controlplane.service` is unchanged.

`ProtectSystem=strict` makes `/usr`, `/boot`, `/efi`, and `/etc` read-only; it
does not touch `/run`, so `/run/dbus/system_bus_socket` stays reachable.
`ProtectKernelTunables=yes` makes `/proc/sys` read-only, and the only thing
`lumen-net` wants from there is to *read* `/proc/sys/kernel/hostname` for the
node name. Everything privileged — writing keyfiles under `/etc`, setting
sysctls, moving addresses — happens inside NetworkManager's process.

Verified rather than assumed, by reproducing both directives in a mount
namespace and connecting to a real system bus from inside it:

```console
$ unshare -m --propagation private sh -c '
    mount --bind /usr /usr && mount -o remount,bind,ro /usr
    mount --bind /etc /etc && mount -o remount,bind,ro /etc
    mount --bind /proc/sys /proc/sys && mount -o remount,bind,ro /proc/sys
    ./hardening-check'
/etc read-only:        true
/proc/sys read-only:   true
hostname readable:     true
system bus connect:    true
system bus call:       true
```

(`hardening-check` is a throwaway binary that asserts the namespace really is
read-only, then opens `zbus::Connection::system()` and calls
`org.freedesktop.DBus.ListNames`.)

### Checkpoint flags: `0x02 | 0x04`

```
DELETE_NEW_CONNECTIONS  (0x02)
DISCONNECT_NEW_DEVICES  (0x04)
```

`DELETE_NEW_CONNECTIONS` because a rollback that leaves behind the profiles the
apply created is not a rollback: those profiles autoconnect, and the node comes
back in the broken state that was just undone. `DISCONNECT_NEW_DEVICES` is the
same argument for the devices those profiles brought into existence — a
half-built bridge should not survive its own revert.

Deliberately **not** set:

- `DESTROY_ALL` (0x01) would tear down every other checkpoint on the box.
- `ALLOW_OVERLAPPING` (0x08) would let a second apply start while one is
  outstanding. That is exactly the case the service already refuses with a
  409; letting NetworkManager accept it would hide the conflict rather than
  surface it.

The device list passed to `CheckpointCreate` is **empty**, which means every
device. A management-network change has the whole box as its blast radius.

### `NetworkConfig`: duplicated, not shared

The installer's `NetworkConfig` and `lumen-net`'s `IpConfig` are separate types
with an identical wire format (`{"mode":"dhcp"}` /
`{"mode":"static","cidr":…,"gateway":…,"dns":[…]}`).

Sharing the type would mean the installer depending on `lumen-net`, and even
with `default-features = false` the optional dependency still lands in
`lumen-installer/app/Cargo.lock` — so `cargo fetch --locked` for a GTK4
installer build would start pulling a D-Bus stack it never links. That is the
condition the brief said not to cross.

Instead the format is pinned from both sides by a matched pair of tests that
name each other:

- `lumen-net`: `model::tests::ip_config_matches_installer_network_config`
- installer: `config::tests::network_config_wire_format_matches_lumen_net`

`IpConfig` additionally has a `Disabled` arm, which the installer has no use
for — a port carries no addressing of its own.

### Library crate now, `lumen-netd` only if forced

`lumen-net` compiles into the `lumen-controlplane` binary and ships in the
existing x86_64 `lumen-controlplane` RPM. The `lumen-networking` RPM stays
**noarch** and keeps its system-integration role.

A separate `lumen-netd` with a unix socket would buy process isolation, and it
is the obvious refactor to reach for. It is not worth it yet, because the
thing isolation usually protects against — a crash mid-apply stranding the box
— is already handled *better* than a separate daemon would handle it:
NetworkManager holds the checkpoint out of process, so the node self-heals even
if the control plane dies between `CheckpointCreate` and `confirm`. A second
daemon would add an IPC surface, a unit, and an arch-specific package without
changing that.

**The trigger that would force the split:** networking needing privileges the
control plane must not have. Today it needs none — NetworkManager does the
privileged work. The moment a stage needs to run `ip`, load kernel modules,
write sysctls, or open raw sockets directly (Open vSwitch, VXLAN, WireGuard,
and BGP/FRR are all candidates), that work belongs in a small privileged
executor, and `lumen-netd` is that executor. At that point `lumen-networking`
becomes x86_64 and this crate moves behind the socket. Nothing in the current
structure blocks that: the `NetworkBackend` trait is already the seam.

### Bridge naming: `br0`, alongside `nic0..nicN`

`lumen-nicnames` pins physical adapters to `nic0..nicN` by PCI order. Bridges
take the parallel `br0..brN`, allocated as the first free index
(`service::next_bridge_name`), and bonds `bond0..bondN`. The two namespaces
never collide because one covers hardware and the other covers links Lumen
creates, and an operator reading `br0` / `nic0` can tell instantly which is
which.

The management bridge is `br0` on a fresh install because it is allocated
first. It is not special-cased: what makes a link the management link is
`management.link` in the desired state, not its name.

### Addressing lives on the link, not on `management`

`Nic`, `Bond`, `Bridge`, and `Vlan` each carry their own `ip: IpConfig`
(defaulting to `Disabled`) and their own `comment`. `ManagementRef` is now
just a pointer: it names the link the console is reached on, which is the link
the appliance must not be allowed to strand, and nothing more.

An earlier shape put the single address on `ManagementRef.ip`. That made a
storage or migration network unrepresentable, and it made the console's
interface dialogs lie — a bridge's address is a property of the bridge, not of
the appliance. Two invariants keep the safety the old shape got for free, both
enforced in `validate`:

- `port_has_address` — a link enslaved to a bridge or bond may not hold an
  address. The kernel drops it; refusing beats accepting a setting the box
  ignores.
- `management_not_addressed` — the management link must have *some*
  addressing. Clearing it is the same failure as unplugging the cable and
  otherwise produces no error anywhere.

A document written by an older appliance still has `management.ip`.
`ManagementRef::legacy_ip` reads it and `NetworkDesiredState::migrate` folds
it onto the link; `Store` runs that on every read, so nothing above the store
knows the older shape existed, and it is never written back out. Without the
migration an in-place upgrade would fail `deny_unknown_fields`, re-seed from
the box, and silently lose every setting NetworkManager does not report back.

`IpConfig::default()` is `Disabled` rather than `Dhcp` for the same reason: an
unconfigured NIC now carries one of these, and it must not quietly start
asking for a lease.

### Component classes added to `globals.css`

Added to the same component layer, in the file's existing commented-section
style. All ported from Quartz Command's equivalents, as the file's own header
comment anticipates:

| Section | Classes | Ported from |
| ------- | ------- | ----------- |
| Data table | `.qz-table-wrap`, `.qz-table`, `.qz-table-fixed`, `.qz-table-title`, `.qz-resizer`, `.qz-th-label`, `.qz-table-empty`, `.qz-mono`, `.qz-dim` | Quartz Command inventory tables |
| Table toolbar | `.qz-toolbar`, `.qz-search`, `.qz-search-input`, `.qz-filter`, `.qz-filter-label`, `.qz-filter-select`, `.qz-columns-menu`, `.qz-rowcount`, `.qz-clear-filters` | Quartz Command inventory tables |
| Buttons | `.btn`, `.btn-primary`, `.btn-danger`, `.btn-ghost`, `.btn-sm` | Quartz Command action bars |
| Form controls | `.field`, `.field-label`, `.input`, `.select`, `.input-invalid`, `.field-error`, `.field-hint`, `.checkbox-row`, `.port-list` | Quartz Command settings forms |
| Dialog | `.dialog-scrim`, `.dialog`, `.dialog-title`, `.dialog-subtitle`, `.dialog-actions` | Quartz Command modals |
| Menu | `.menu`, `.menu-item` | Quartz Command split buttons |
| Callout | `.callout`, `.callout-warn`, `.callout-crit` | Quartz Command inline notices |
| Pending / checkpoint | `.pending-bar`, `.checkpoint-bar`, `.checkpoint-bar-fill`, `.checkpoint-bar-body`, `.checkpoint-countdown` | new — nothing in Quartz Command counts down to a self-revert |

No CSS-in-JS and no component library was added. Icons are `lucide-react`,
already a dependency.

### `DataTable`: one table component, ported from Quartz Command

`components/console/DataTable.tsx` is the shape Quartz Command's inventory
tables established — a search box and per-column drop-down filters over the
top, a **Columns** menu to hide what does not matter today, columns the
operator drags to resize, and a row count. Every console table that follows
gets it for free; the Interfaces page is the first caller.

Two things are worth knowing about it:

- **`table-layout: fixed`.** A dragged width is meaningless under `auto`,
  where the browser reflows every column to fit its content. Cells clip with
  an ellipsis rather than wrap, so row height stays constant down a long list.
- **Layout is remembered per table id** in `localStorage`, under
  `<id>.widths` and `<id>.hidden`. A resize writes once on mouse-up, not once
  per mousemove. A browser with no storage is not an error — the table just
  starts from its defaults each time.

The node's hostname is a full-width row inside the table, above the column
headers, rather than a heading above it: with clustering there will be several
of these stacked up, and the name belongs to the rows, not to the page.

Ports are listed flat, against their controller in the Ports/Slaves column,
rather than indented under it. An operator scanning the Name column wants a
plain list of everything the box has.

### `no-auto-default=*`

`lumen-networking/system/NetworkManager/00-lumen.conf` ships into
`/usr/lib/NetworkManager/conf.d`. Without it, NetworkManager invents a "Wired
connection N" profile for every adapter that has none and brings it up with
automatic addressing. That fights the model directly: an adapter nobody has
configured is meant to have no profile, and the console reports it as
unconfigured. An auto-created profile would appear as a configuration the
operator never made, hold an address nobody asked for, and come back every
boot after the console removed it.

It lives in the vendor directory rather than `/etc`, so a local override is a
higher-sorting file the operator adds and this one never becomes a modified
config file.

A `br_netfilter` sysctl drop-in is **not** shipped yet. It only matters once
guests are attached to a bridge and their traffic would otherwise traverse the
host firewall; libvirt integration is out of scope for this stage, and the
module is not loaded by default, so a drop-in now would only produce boot-time
warnings about a key that does not exist.

---

## Staged apply with auto-revert

Networking is the one place where a bad commit costs a truck roll, so nothing
is applied the moment it is requested.

```
   stage ──► validate ──► apply ──► confirm      change is permanent
                            │
                            ├────► rollback      reverted now
                            │
                            └────► (silence)     NetworkManager reverts it
```

1. Changes are **staged** into a pending target, persisted as JSON in the
   control plane's state dir (`LUMEN_CP_STATE_DIR`, which `StateDirectory=`
   already makes writable). No database.
2. `POST /api/network/apply` validates, calls `CheckpointCreate([], secs,
   flags)`, pushes the planned operations, and answers with the checkpoint id
   and an **absolute** confirm deadline.
3. `POST /api/network/confirm` destroys the checkpoint. Permanent; the pending
   set clears.
4. **No confirm before the deadline** and NetworkManager rolls the whole thing
   back on its own — including reactivating the previous management
   connection. It does this in its own process, so a control plane that
   crashed mid-apply changes nothing.
5. `POST /api/network/rollback` reverts immediately. The staged target is left
   staged, because the operator usually wants to fix it and retry rather than
   retype it.
6. `POST /api/network/apply/extend` calls
   `CheckpointAdjustRollbackTimeout` for a slow operator.

The window defaults to **60 seconds**, configurable with
`LUMEN_CP_NET_CONFIRM_SECS`.

A second apply while a checkpoint is outstanding is a **409**. On daemon
start, `NetworkService::reconcile()` adopts a checkpoint it has a record for
(NetworkManager was watching it the whole time the daemon was down, and the
operator can still confirm it) and **rolls back** any checkpoint it has no
record for — an orphan from a run that died mid-apply. Undoing a change nobody
confirmed is the safe direction, and leaving it would block every future apply.

### What actually gets activated

Profiles are written declaratively: every link in the desired state gets an
`AddConnection` or an `UpdateConnection`. Activation is not. Only links that
are new, down, or **observably** different are activated, because rewriting a
NetworkManager profile does not disturb the running device while reactivating
it does. That is what keeps an unrelated change from blipping the management
link.

Settings NetworkManager does not report per device (STP, `miimon`, hash
policy, link autonegotiation) are always written, so they do reach the box —
they just do not on their own force a reactivation. They take effect the next
time the link they belong to is activated, and none of them is worth dropping
the operator's session for.

A physical adapter with nothing asked of it — no controller, no address — is
given a profile but is **not** forced up. On the first apply of a freshly
seeded configuration that would otherwise bring up every adapter in the
machine at once.

## Validation

`validate(desired, observed, ack) -> Vec<ValidationError>`; pure, and it
returns every problem it finds rather than the first. Each error carries a
machine-readable `code` (the console pins it to a field, tests assert on it)
and a human sentence the console renders verbatim.

| Code | Rejected because |
| ---- | ---------------- |
| `multiple_controllers` | a link is a port of more than one bridge/bond |
| `controller_cycle` | bridge → bond → bridge, or a link enslaved to itself |
| `empty_management_controller` | the management address is on a controller with no ports |
| `management_is_a_port` | the management link is itself a port, so it cannot hold an address |
| `duplicate_vlan` | two VLANs share a (parent, id) |
| `vlan_id_out_of_range` | outside 1–4094 |
| `bridge_mtu_exceeds_port` | a bridge MTU above its smallest port's — Linux clamps silently, we refuse |
| `unknown_reference` | a port/parent/management link nothing defines |
| `duplicate_name` | two links with one name |
| `invalid_name` | not a usable interface name |
| `primary_not_a_port` | a bond `primary` that is not one of its ports |
| `port_has_address` | a link that is a port of a bridge/bond has an address of its own — the kernel drops it |
| `management_not_addressed` | the management link has no addressing at all, so applying would leave the console unreachable |
| `management_disconnect` | the change moves the management address off a reachable link without `i_understand_this_may_disconnect_me` |

`management_is_a_port` is not in the original list but is the trap worth
naming: building a bridge over the management adapter and forgetting to move
the address is the mistake an operator actually makes, NetworkManager accepts
it without complaint, and the result is a node with the address on neither
link. `POST /api/network/management-bridge` is the route that does it
correctly.

Staging rejects everything except `management_disconnect`, which is collected
at apply time rather than while editing. So the pending set is always
applicable except for that one acknowledgement.

## Management bridge

The management address must live on a bridge so the first virtual machine does
not require moving it.

### New installs come up bridged

`lumen-installer` writes **two** keyfiles instead of one:

- `management.nmconnection` — `type=bridge`, `interface-name=br0`, the
  `[ipv4]` block exactly as before, `[ipv6] method=disabled`,
  `bridge.stp=false`, `bridge.forward-delay=0`, and `bridge.mac-address`
  pinned to the chosen adapter's hardware address.
- `management-port.nmconnection` — `type=ethernet`,
  `interface-name=<nicN>`, `controller=br0`, `port-type=bridge`.

**The MAC pin matters.** A Linux bridge otherwise inherits the lowest MAC among
its ports, so adding a second adapter later silently changes the management MAC
and breaks DHCP reservations and any switch-side port security — at boot, with
no error. `InstallConfig` gained `nic_mac`, read from
`/sys/class/net/<nic>/address` (the same value `lumen-nicnames` pins names to)
and plumbed through from the NIC the operator picked.

The installer's four-question UI is unchanged. The bridge is fixed appliance
policy, not an operator decision.

### Converting an already-installed node

`POST /api/network/management-bridge` converts a flat `nicN` management
connection into `br0` + port. It:

- returns **idempotent success** (`"converted": false`) if a bridge already
  carries the address;
- runs entirely inside a checkpoint with the standard confirm window, so a
  mistake self-heals;
- preserves the exact IP configuration, including DHCP-versus-static and the
  resolvers (observed state carries the configured method, so seeding from a
  running box does not turn a lease into a static address);
- pins `bridge.mac-address` to the adapter's permanent MAC so the address
  survives DHCP;
- is a single atomic apply — the node is never left with the address on
  neither link;
- refuses to run over an existing staged set, so the conversion is exactly
  what gets applied.

---

## API

All routes require a valid session and use the existing error envelope
`{ "error": "…" }`. Rejected configurations add an `errors` array alongside it,
each entry carrying `code`, `link`, `field`, and `message`.

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET | `/api/network/interfaces` | Observed state, grouped by node |
| GET | `/api/network/interfaces/:name` | One link on the local node, detailed |
| GET | `/api/network/config` | Current desired state |
| GET | `/api/network/pending` | Staged delta + validation results + checkpoint |
| POST | `/api/network/bridges` · `/bonds` · `/vlans` | Stage a create |
| PATCH | `/api/network/bridges/:name` · `/bonds/:name` · `/vlans/:name` · `/nics/:name` | Stage an update |
| DELETE | `/api/network/bridges/:name` · `/bonds/:name` · `/vlans/:name` | Stage a delete |
| DELETE | `/api/network/pending` | Discard all staged changes |
| POST | `/api/network/apply` | Validate, checkpoint, apply |
| POST | `/api/network/confirm` | Destroy the checkpoint, make permanent |
| POST | `/api/network/rollback` | Roll back now |
| POST | `/api/network/apply/extend` | Extend the confirm window |
| POST | `/api/network/management-bridge` | Convert `nicN` → `br0` |

### Node grouping

`GET /api/network/interfaces` answers

```json
{ "nodes": [ { "node": "lumen", "interfaces": [ … ] } ] }
```

— a one-element list today. Mutating endpoints accept an optional `node`
field which defaults to the local node and currently rejects anything else
with a clear "not in a cluster" error. The response shape and the request
field are in place from day one so neither changes when clustering lands, and
so the console can render its per-node layout now.

Each interface object carries everything a table row needs without a second
round trip: `name`, `altname`, `kind`, `admin_up`, `oper_state`, `carrier`,
`perm_mac`, `mac`, `speed_mbps`, `duplex`, `mtu`, `addresses` (CIDR),
`gateway`, `dns`, `ip`, `controller`, `ports`, `bond_mode`, `vlan_id`,
`parent`, `vlan_aware`, `comment`, `management`, `deletable`,
`delete_blocked_reason`, `change`, and `present`. Rows arrive ordered so a
controller is immediately followed by its ports.

`addresses` and `ip` answer different questions and the console shows the
second: `addresses` is what the box has on the link right now, `ip` is what
the configuration asks for. A staged-but-unapplied address exists only in
`ip`; a DHCP link with a lease and a static link with the same address are
indistinguishable in `addresses`.

Loopback is not among the rows. It is on every Linux box, is not
configurable, and is never what an operator opened the page to look at — so
`views()` drops it, and `ObservedState::addressed()` ignores it too (127.0.0.1
must never make an unaddressed appliance look addressed).

`altname` is the name the kernel gave a physical adapter before udev renamed
it — `enp3s0`, `eno1` — which is what ties a row in the table to a label on
the chassis. An alternative name is not exposed in sysfs and reading the
netlink property list is a whole dependency for one string per NIC, so
`lumen-nicnames` records it as `AlternativeName=` in the same
`70-lumen-nic<N>.link` file that does the renaming, and the backend reads that
line back. The tool that renames the adapter is the one that knows what the
name used to be.

The install path is where that record nearly gets lost. `lumen-nicnames` runs
twice: once in the live environment with `--apply`, which renames the running
adapters and records what they were called, and once against the install target
with `--root`, which writes the link files the installed system boots with. By
the time the second run happens `/sys` already says `nic0`, so the kernel's
original name survives only in the *live* root's link file — and the target
root, being brand new, has nothing to carry it forward. So the second run reads
the live root too, for recorded alternative names only; index pins still come
solely from the target, which is the system being built. Without that the
Alternative Name column is empty on every freshly installed node, which is
precisely where it is most wanted.

---

## Walkthrough

Everything below works against a running appliance with nothing but `curl` and
`jq`. `-k` is there because the appliance's certificate is self-signed on first
boot.

```sh
HOST=https://192.168.10.5:8443
JAR=$(mktemp)

# 1. Sign in. The session is an httpOnly cookie, so keep a cookie jar.
curl -sk -c "$JAR" -X POST "$HOST/api/auth/login" \
     -H 'Content-Type: application/json' \
     -d '{"username":"root","password":"…","realm":"lumen"}' | jq

# 2. What is on the box.
curl -sk -b "$JAR" "$HOST/api/network/interfaces" |
  jq '.nodes[] | {node, interfaces: [.interfaces[] | {name, kind, oper_state, addresses, controller, ports}]}'

# 3. The committed configuration, and anything staged.
curl -sk -b "$JAR" "$HOST/api/network/config"  | jq
curl -sk -b "$JAR" "$HOST/api/network/pending" | jq

# 4. Stage a bridge over a spare adapter.
curl -sk -b "$JAR" -X POST "$HOST/api/network/bridges" \
     -H 'Content-Type: application/json' \
     -d '{"name":"br1","ports":["nic1"],"stp":false}' | jq '.changes'

#    A rejected change answers 400 with the codes the console pins to fields:
curl -sk -b "$JAR" -X POST "$HOST/api/network/vlans" \
     -H 'Content-Type: application/json' \
     -d '{"name":"vlan9999","parent":"nic1","vlan_id":9999}' | jq '.errors'
# [ { "code": "vlan_id_out_of_range", "link": "vlan9999", "field": "vlan_id", … } ]

# 5. Apply. Note the absolute deadline in the answer — that is what to count
#    down to, not a duration.
curl -sk -b "$JAR" -X POST "$HOST/api/network/apply" \
     -H 'Content-Type: application/json' -d '{}' | jq
# { "checkpoint": { "id": "/org/freedesktop/NetworkManager/Checkpoint/1",
#                   "confirm_deadline": 1785000060, "seconds_remaining": 60,
#                   "rollback_secs": 60 },
#   "operations": [ "add br1 (bridge)", "update nic1 (ethernet)", … ] }

# 6. Still reachable? Then keep it.
curl -sk -b "$JAR" -X POST "$HOST/api/network/confirm" -d '{}' | jq

#    Need longer to check something first:
curl -sk -b "$JAR" -X POST "$HOST/api/network/apply/extend" \
     -H 'Content-Type: application/json' -d '{"seconds":120}' | jq

#    Or undo it now rather than waiting the window out:
curl -sk -b "$JAR" -X POST "$HOST/api/network/rollback" -d '{}' | jq

#    Or do nothing at all: after the deadline the node has already reverted,
#    and the checkpoint reads back as gone.
curl -sk -b "$JAR" "$HOST/api/network/pending" | jq '.checkpoint'
# null

# 7. Move the management address onto a bridge (idempotent).
curl -sk -b "$JAR" -X POST "$HOST/api/network/management-bridge" -d '{}' | jq
curl -sk -b "$JAR" -X POST "$HOST/api/network/confirm" -d '{}' | jq

# 8. Stage a removal, then think better of it.
curl -sk -b "$JAR" -X DELETE "$HOST/api/network/bridges/br1" | jq '.changes'
curl -sk -b "$JAR" -X DELETE "$HOST/api/network/pending"     | jq '.changes'
```

---

## Manual test script

Automated tests run against the in-memory backend and cover stage → validate →
apply → confirm and stage → apply → expire. They cannot cover a cable, a
kernel bridge, or a session that actually drops. **The whole design rests on
the claim that a bad apply self-heals — verify it on real hardware before
trusting it.**

Run these on a two-adapter appliance you can physically reach.

### 0. Record the daemon version

```sh
nmcli --version    # expect 1.46 or newer; the backend refuses to start below it
```

Write the number into this document's `controller=`/`port-type=` section the
first time.

### 1. Create a bridge and apply it

1. Console → **Networking → Interfaces** → **Create → Linux Bridge**.
2. Name `br1`, tick the spare adapter, stage it. The row appears with a
   **created** badge and the pending bar shows the count.
3. **Apply configuration**. Read the dialog: it should state the window length
   the API reported, not a hardcoded number.
4. Confirm within the window.
5. Check the box agrees: `ip -br link show br1` and `bridge link show`.

### 2. Let the window expire — the important one

1. Stage a change that will **break your own path to the node**. The honest
   version is to move the management address to an adapter with no cable, or
   to set the management link's MTU to something the switch will not pass.
2. Apply, acknowledge the disconnect warning, and then **do nothing**.
3. The console should show *"lost contact with the node — it reverts
   automatically in N s unless you confirm"* and keep counting down locally.
4. Watch the node's console (or ping it). Within the window it must come back
   on the previous configuration by itself.
5. When the browser reaches it again, the countdown must clear and the table
   must show the *old* configuration — the server is the authority, not
   anything cached in the page.
6. `GET /api/network/config` must not contain the change. It was never
   committed.

Do this one **before** trusting any of the rest.

### 3. Kill the control plane mid-window

1. Stage and apply any change.
2. `systemctl stop lumen-controlplane` before confirming.
3. Do nothing. NetworkManager must still revert on schedule — the checkpoint
   is in its process, not ours.
4. `systemctl start lumen-controlplane`, then
   `GET /api/network/pending`: the checkpoint reads as gone and the staged set
   is still staged.
5. Repeat, but restart the control plane *within* the window and confirm
   through the console. `reconcile()` should have adopted the checkpoint —
   look for "adopting a network change that is still waiting to be confirmed"
   in the journal.

### 4. Convert the management adapter to `br0`

Only meaningful on a node installed before this change, or one where `br0` was
removed.

1. The banner should be on the Interfaces page. Press **Create management
   bridge**.
2. Before confirming, check from another host that the address still answers,
   and check the MAC:
   ```sh
   ip -br addr show br0        # same address as nic0 had
   cat /sys/class/net/br0/address    # equals /sys/class/net/nic0/address
   ```
3. Confirm. Reboot. The address and the MAC must both survive — a DHCP node
   must come back with the same lease.
4. Press the button again: it must succeed with `"converted": false` and take
   no checkpoint.

### 5. Pull a cable on a bond member

1. Create a bond over both adapters (`active-backup`, `miimon` 100), apply,
   confirm.
2. Physically unplug one member.
3. Within a poll interval (~5 s) the Interfaces table must show that member
   with **no carrier** while the bond stays **activated**.
4. `cat /proc/net/bonding/bond0` should agree about which port is active.
5. Plug it back in; the row must recover on its own.

### 6. Reload mid-window

1. Apply a change, and before confirming, hard-reload the browser.
2. The countdown must come back — rehydrated from
   `GET /api/network/pending`, not from `localStorage`.
3. Navigate between **Overview** and **Interfaces**. The countdown must stay
   on screen the whole time.

---

## Development

```sh
make test    # installer + lumen-net + control plane; no NetworkManager needed
make lint    # shellcheck, rpmlint, fmt/clippy for all three crates
```

Every test in `lumen-net` and every control-plane API test runs against
`backend::mock::MockBackend`, an in-memory model of NetworkManager. CI never
touches the runner's networking. The mock deliberately does **not** model
NetworkManager's rollback timer — a test that waits sixty seconds for a timer
is a test nobody runs — so tests drive expiry explicitly with
`MockBackend::expire_checkpoints()`.

The mock is compiled unconditionally and exported rather than sitting behind
`#[cfg(test)]`: a `cfg(test)` item is invisible to another crate's integration
tests, and `lumen-controlplane/tests/network_flow.rs` needs it. It is injected
exactly the way `tests/auth_flow.rs` injects its mock realm.

Against a real box, run the control plane as root — the system bus policy for
NetworkManager's configuration methods requires it, and the appliance's unit
already does:

```sh
cd lumen-controlplane && sudo LUMEN_CP_NO_TLS=1 cargo run
```

If NetworkManager is unreachable at startup the daemon still comes up: the
backend is swapped for `UnavailableBackend`, every networking call answers
with the reason, and the console shows it. An operator whose networking is
broken needs the console more than usual, so "the console will not start" is
the worse failure.

## Out of scope for this stage

Open vSwitch, VXLAN, BGP/FRR, WireGuard, OpenVPN, firewall rules, DHCP/IPAM,
cluster-wide state or drift detection, and libvirt integration. No database.
No privileged executor — the point of the NetworkManager-over-D-Bus design is
that this stage does not need one.

In the console, only **Networking → Overview** and **Networking → Interfaces**
are implemented. Networks, Fabrics, Routing, Tunnels, Firewall, and
Diagnostics remain stubs.
