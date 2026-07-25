# Lumen compute & storage

The first virtual machine that boots: domain lifecycle through libvirt, disks
on zvols under `rpool`, a three-column console, and the Virtual Machines pages.

```
lumen-compute/
├── lumen-virt/        the virtualization domain (Rust library crate)
└── system/            appliance integration files
lumen-storage/
├── lumen-zfs/         the storage domain (Rust library crate)
└── system/            appliance integration files
lumen-controlplane/
├── src/api/vms.rs     thin HTTP handlers over lumen-virt
└── src/api/storage.rs thin HTTP handlers over lumen-zfs
lumen-webui/
├── lib/SecondaryNavContext.tsx   the console's second nav column
└── app/(console)/{virtual-machines,storage}
```

## Component split

Two new domains, laid out exactly as `lumen-networking/lumen-net`:

```
model.rs        what a machine is / what a pool is
state.rs        what the hypervisor and the box actually report
domain_xml.rs   the domain document, both directions, round-trip tested
validate.rs     pure rules over a machine and the node it would run on
backend/        libvirt (or the zfs command line), plus mock/ and unavailable/
service.rs      the one entry point the control plane calls
```

`src/api/vms.rs` and `src/api/storage.rs` deserialize, call one service method,
and serialize. There is no libvirt, no ZFS, and no XML above that line.

Both crates are **path dependencies**, not workspace members, for the reason
[docs/networking.md](networking.md) gives: the manifests are independent and
the Makefile drives each with its own `--manifest-path`/`--target-dir` pair.

### The dependency direction

`lumen-virt` depends on `lumen-zfs` **and** `lumen-net`. A machine needs a
volume to boot from and a bridge to attach to; neither storage nor networking
has any reason to know a machine exists. `AppState` constructs them in that
order, and `VirtService::new` takes the other two services as parameters — the
same shape `NetworkService` already had for its backend, so the control plane's
tests inject in-memory implementations all the way down.

One consequence is worth naming: when networking or storage is *unreadable*,
`lumen-virt` skips that check rather than refusing every machine.
`HostFacts::bridges_known` and `pools_known` carry that distinction, and
`validate::an_unreadable_subsystem_skips_its_check_rather_than_failing_everything`
pins it. Refusing to define a machine because storage is down would turn one
broken thing into two.

`lumen-storage` was created now even though it is read-only, because moving
code between components later is worse than a small crate today. Part 3 makes
it writable.

---

## Decisions

### The `virt` crate, not an in-tree FFI

**Chosen: the published `virt` crate.** It does not run bindgen.

The repo keeps bindgen and libclang out of the appliance and CI toolchains —
that is why the PAM layer is a ~150-line in-tree FFI. The same question had to
be asked here, and the answer came out the other way.

`virt` 0.4.3 depends on `virt-sys` 0.3.1, whose **only** build dependency is
`pkg-config`. `bindgen` is an *optional* build dependency behind the
`bindgen_regenerate` feature, which regenerates the checked-in bindings and is
not enabled. The default build copies `bindgen/bindings.rs` out of the
published crate and runs `pkg-config` to find the library.

Verified in the EL10 CI container, not assumed:

```console
$ dnf -y install rust cargo libvirt-devel pkgconf-pkg-config
$ rpm -q libvirt-devel
libvirt-devel-11.10.0-12.el10_2.alma.1.x86_64

$ rpm -q clang-libs
package clang-libs is not installed
$ find / -name 'libclang.so*'          # nothing

$ cargo tree -e normal,build
virtprobe v0.1.0
├── virt v0.4.3
│   ├── libc v0.2.189
│   ├── uuid v1.24.0
│   └── virt-sys v0.3.1
│       └── libc v0.2.189
│       [build-dependencies]
│       └── pkg-config v0.3.33      <- and nothing else

$ cargo run --bin probe
virGetVersion rc=0 libvirt=11010000
$ ldd target/debug/probe | grep virt
	libvirt.so.0 => /lib64/libvirt.so.0
```

Linking needs `libvirt-devel` in the CI container and the RPM build root;
running the tests does not need a hypervisor. **The `compute` CI job asserts
this decision rather than trusting it**: it greps `cargo tree -e normal,build`
for `bindgen`/`clang-sys` and fails if either appears, so a future version bump
that starts generating bindings breaks in CI rather than on an appliance.

The appliance itself needs `libvirt-daemon-kvm`, `qemu-kvm`, and `edk2-ovmf`;
`lumen-compute.spec` requires all three.

### `ProtectSystem=strict` needed no relaxation

**No `ReadWritePaths=` was added.** `lumen-controlplane.service` is unchanged.

Two different arguments, for two different subsystems.

**Machine operations** go to `libvirtd`/`virtqemud` — a privileged daemon with
its own API, reached over a socket under `/run`. That is exactly the
arrangement networking has with NetworkManager over the system bus, and it has
exactly the same consequence: the privileged work happens in another process,
so none of the hardening has to move. `ProtectSystem=strict` leaves `/run`
writable.

**Volume operations** reach the kernel through `/dev/zfs`, which
`ProtectSystem=strict` does not cover — it makes `/usr`, `/boot`, `/efi`, and
`/etc` read-only and says nothing about `/dev`, and `PrivateDevices=` is not
set on this unit.

Reproduced rather than assumed, by rebuilding the unit's sandbox in a mount
namespace on an EL10 container (the same technique docs/networking.md used):

```console
$ unshare -m --propagation private bash /s/hardening-check.sh
/etc                                   READ-ONLY
/etc/zfs  (zpool.cache lives here)     READ-ONLY
/usr                                   READ-ONLY
/proc/sys                              READ-ONLY
/run                                   writable
/var/lib (StateDirectory)              writable
/dev present in the sandbox            yes
/dev/zfs visible                       yes
/run/libvirt reachable                 yes
```

The middle line is the important one, and it cuts both ways:

- `/dev/zfs` is reachable, so `zfs create -V` works from inside the sandbox.
  Dataset and volume operations are ioctls on that device; they do not write
  `/etc/zfs/zpool.cache`.
- `/etc/zfs` **is** read-only, so any operation that *does* write
  `zpool.cache` would fail. That is precisely `zpool create`, `import`, and
  `export`.

Which is why pool operations are out of scope and `lumen-execd` will exist.
The console's Storage page says so in those words rather than offering a
button that cannot work.

**Still to confirm on hardware:** creating a zvol from the running service on a
real node with real pools. This environment has no ZFS kernel module (the
container's host is not Linux), so the filesystem half of the claim is
reproduced above and the ioctl half is not. Step 2 of the manual script below
is that confirmation, and it is the first thing to run.

### EL10's libvirt has no ZFS storage backend

**Checked, and it does not** — so volume creation goes through `lumen-zfs`
rather than `virStorageVolCreateXML`.

Checked at the artifact level rather than by running `virsh
pool-capabilities`, which would need a daemon:

```console
$ dnf -y install libvirt-daemon-driver-storage
$ ls /usr/lib64/libvirt/storage-backend/
libvirt_storage_backend_disk.so     libvirt_storage_backend_mpath.so
libvirt_storage_backend_fs.so       libvirt_storage_backend_rbd.so
libvirt_storage_backend_iscsi.so    libvirt_storage_backend_scsi.so
libvirt_storage_backend_logical.so

$ rpm -qa 'libvirt-daemon-driver-storage*' | xargs -n1 rpm -ql | grep -i zfs
(nothing)
```

Expected — Red Hat ships no ZFS, so the driver is not compiled in — but worth
confirming, because delegating volume creation to libvirtd would have been
strictly better if it had been available.

### The command line, not `libzfs`

`lumen-zfs`'s real backend shells out to `zfs` and `zpool` with **typed argv
arrays**, never an interpolated shell string. `libzfs` has no stable ABI,
changes shape between releases, and binding it would put a code-generation step
into a toolchain that deliberately has none.

There is no string to interpolate into and therefore nothing to escape: every
invocation is an array handed to `execve` through `tokio::process::Command`.
Columns are asked for by name (`-H -p -o name,size,alloc,…`), so a release that
adds one does not shift the fields out from under the parser. This is the same
discipline `lumen_net::plan::Op` enforces on the networking side — what can be
asked for is an enumeration, not a sentence.

Operator-supplied strings reach a command only after passing
`model::valid_pool_name` or `model::is_lumen_volume`, both of which reject
anything containing a separator, whitespace, or a leading `-`.

### The storage namespace: `<pool>/lumen/vm-<vmid>-disk-<n>`

Every volume Lumen creates lives under `<pool>/lumen/`. **Nothing outside that
prefix is destroyable by anything in this crate.**

`is_lumen_volume` accepts exactly `<pool>/lumen/<leaf>` — one pool component,
the prefix, one leaf, no traversal components, no `@` (a snapshot), no leading
`-`. It is checked in `StorageService::destroy_volume` *and* again in the
backend, because a backend that trusts its caller is one refactor away from not
being safe. `model::only_a_lumen_volume_is_destroyable` is the table that pins
it, and it lists the refusals as well as the acceptances.

The name is numeric and boring on purpose: every disk is findable from the
machine it belongs to without consulting anything else, and a machine that has
to be cleaned up by hand can be cleaned up by prefix.

### The domain document is the database

There is no database. libvirt already stores every machine's definition
durably, hands it back verbatim, and preserves elements it does not understand
— so Lumen's own per-machine data rides along inside it.

```xml
<metadata>
  <lumen:vm xmlns:lumen="https://www.quartzsystems.net/xmlns/lumen/1.0">
    <lumen:vmid>101</lumen:vmid>
    <lumen:description>Public web server</lumen:description>
    <lumen:tags>production,web</lumen:tags>
  </lumen:vm>
</metadata>
```

The namespace URI is versioned so a later shape can be told from this one by
anything reading the document, including a human with `virsh dumpxml`. The
prefix is *not* depended on — libvirt is free to rewrite it, so the parser
matches on local names.

This mirrors networking, where NetworkManager's keyfiles hold the committed
state and only the staged delta is persisted. Machines have no staged delta at
all, so they persist nothing.

Two things deliberately live outside the document:

- **`start_on_boot`** is libvirt's autostart flag, not an element. It is read
  and written on its own, and folded into `VmConfig` by the service.
- **The start time** an uptime is measured from is metadata set on the
  *running* domain only (`VIR_DOMAIN_AFFECT_LIVE`). libvirt holds it, so it
  survives a control-plane restart, and it disappears by itself when the
  machine stops — there is no stale value to clean up. A machine somebody
  started with `virsh` simply has no uptime, which is honest.

### VMIDs start at 100 and fill the lowest free slot

Numeric, starting at 100, matching the muscle memory of anyone who has run a
hypervisor before. `VirtService::next_vmid` scans the machines that exist and
takes the lowest free identifier, so removing one from the middle frees its
number — which is what an operator expects and what keeps the numbers small
enough to say out loud.

Hardware addresses are derived from the VMID rather than randomised:
`52:54:00:<vmid hi>:<vmid lo>:<index>`. The `52:54:00` prefix is locally
administered (the low bit of `0x52` is set), so it is in the range reserved for
addresses nobody bought. Deriving it means the same machine gets the same
address every time it is defined — a DHCP reservation keeps working after the
machine is rebuilt.

### Boot order: per-device only

libvirt **rejects** mixing `<boot order='N'/>` on devices with
`<os><boot dev='…'/></os>`, so one of the two had to win. Per-device won: it
can express "this disk, then that adapter" rather than "some disk, then some
adapter".

`VmConfig::normalized()` is what turns the model's device-class order into
those numbers, and `render` normalizes before writing — so rendering is a pure
function of the configuration and two equal configurations always render
identically. `parse(render(c)) == c.normalized()` is the round-trip property,
asserted for the sample machine and for fifteen named variations of it.

A boot entry for a device class the machine does not have is dropped by
`normalized()`, because it is not a setting — it is a line with nothing behind
it, and the document would not carry it either.

### Which changes apply live, and which wait for a restart

The distinction is **libvirt's own answer**, not a table maintained here.

There is exactly one way a change is persisted: `define` with the whole
document. Nothing else writes the stored configuration. The `*_live` calls are
purely additive on top, carrying `VIR_DOMAIN_AFFECT_LIVE` and nothing else —
which is also what stops a device being added to the configuration twice.

| Change | How it reaches a running machine |
| ------ | -------------------------------- |
| memory | `virDomainSetMemoryFlags(LIVE)` is attempted. libvirt refuses above the maximum the machine booted with, and that refusal becomes "waiting for a restart", carrying its own message. |
| processors | `virDomainSetVcpusFlags(LIVE)` — same. |
| attach/detach a disk | `virDomainAttachDeviceFlags(LIVE)` / `DetachDeviceFlags(LIVE)` is attempted; the hypervisor's refusal is reported as-is. |
| attach/detach an adapter | The same pair. |
| CPU model, layout, machine type, firmware, boot order, guest agent | **Always a restart.** These are only written by a whole-document `define`, and a `define` never touches the running machine. That is the API surface, not a guess about it. |
| name | Refused outright while running: libvirt allows `virDomainRename` only on a stopped domain, so "wait for a restart" would be a lie. |
| description, tags, start on boot | Immediate. None of them is visible to the guest. |

`PATCH /api/vms/{vmid}` answers with `applied_live` and `pending_reboot`, both
lists of sentences, and `VmView.pending_reboot` reports the *observed* gap —
what the machine is running versus what it is stored as — rather than a
prediction. The console shows both verbatim.

### No staged apply, and why

Networking stages every change and applies it inside a NetworkManager
checkpoint that reverts itself if nobody confirms. That machinery exists
because a bad network commit costs a drive to the rack: the operator loses the
console at the same moment they lose the thing they need the console to fix.

Machines are not like that.

- Starting one is **immediate and observable** — the state changes in the same
  request, and the console shows it.
- It is **reversible by the obvious action**: stop it again.
- A machine that fails to start leaves the node exactly as it was; the failure
  is contained in one guest.
- Above all, **it cannot cut the operator off**. Nothing a machine does takes
  the management address away.

Copying the checkpoint engine here would add a second staging system, a second
set of confirm/rollback endpoints, and a second way for the console to be out
of step with the node — in exchange for undoing something a single click
already undoes.

What machines *do* have and networking does not is the live-versus-restart
split above, and that is surfaced rather than invented.

### Validation

`validate(config, planned_disks, facts) -> Vec<ValidationError>`; pure, and it
returns every problem it finds rather than the first. Each error carries a
machine-readable `code` (the console pins it to a field, tests assert on it)
and a human sentence the console renders verbatim.

| Code | Rejected because |
| ---- | ---------------- |
| `duplicate_vmid` | another machine already has this identifier |
| `duplicate_name` | another machine already has this name, case-insensitively |
| `invalid_name` | not a usable machine name |
| `invalid_tag` | a tag with a comma, a space, or nothing in it |
| `vmid_out_of_range` | outside 100–65535 |
| `invalid_vcpus` | zero processors |
| `vcpus_exceed_host` | more processors than the node has threads |
| `invalid_memory` | below 128 MiB, where a guest does not finish starting |
| `memory_exceeds_host` | more than the node has, minus a 2 GiB reserve for the node itself |
| `topology_mismatch` | sockets × cores × threads ≠ the processor count — libvirt refuses this outright |
| `unknown_bridge` | an adapter names a bridge the node does not have |
| `unknown_pool` | a disk names a pool the node does not have |
| `disk_exceeds_pool` | disks that do not fit, measured **together** per pool |
| `invalid_vlan_tag` | outside 1–4094 |
| `duplicate_device` | two disks on one target, or two adapters with one address |
| `unacknowledged_destructive_operation` | see below |

`unknown_bridge` is the one worth naming: a machine pointed at a bridge that is
not there defines cleanly and then has no network at all, with nothing anywhere
saying why. The message names the bridges the node *does* have, not just the
one it does not.

### Acknowledgements

Three operations refuse to proceed without
`i_understand_this_may_lose_data`, because each of them takes a running guest
down without warning it or removes data that cannot be recovered:

- `POST /stop` — the equivalent of pulling the power. (`/shutdown` is the ACPI
  request and needs nothing.)
- `POST /reset` — the reset button.
- `DELETE /vms/{vmid}` with `purge_disks`, **or** on a running machine.
- `DELETE /vms/{vmid}/disks/{id}` with `purge_disks`.

`purge_disks` is **off by default**. Removing a machine and destroying its data
are two decisions, and the response says which volumes were removed and which
were kept, so an operator who did not ask for the data to go is told where it
still is.

One guard is worth spelling out: detaching a disk with `purge_disks` from a
running machine that could **not** take the live detach is refused. The disk
leaves the stored configuration, the volume is left in place, and the error
says to restart the machine and try again — destroying a volume a guest still
has open is the one mistake with no undo.

### Component classes added to `globals.css`

Same commented-section style as the rest of the file.

| Section | Classes | Why |
| ------- | ------- | --- |
| Context navigation | `.context-nav`, `.context-nav-header`, `.context-nav-title`, `.context-nav-sub`, `.context-nav-items`, `.context-nav-item` | new — nothing had a second nav column |
| Sidebar machines | `.sidebar-vm-list`, `.sidebar-vm`, `.sidebar-vm-id`, `.sidebar-vm-name`, `.sidebar-vm-node`, `.sidebar-vm-filter` | new — nav entries that are runtime data |
| State dot | `.state-dot` and tones | new — a state where a badge is too wide |
| Meter | `.qz-meter`, `.qz-meter-fill`, and the warn/crit tones | new — a proportion read without dividing |
| Facts | `.qz-facts` | new — the label/value grid every detail panel uses |

No CSS-in-JS and no component library was added. Icons are `lucide-react`,
already a dependency.

---

## API

All routes require a valid session and use the existing error envelope
`{ "error": … }`. A rejected machine adds an `errors` array alongside it, each
entry carrying `code`, `vm`, `field`, and `message`.

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET | `/api/vms` | All machines, grouped by node |
| GET | `/api/vms/:vmid` | One machine, full detail |
| POST | `/api/vms` | Define the machine and the volumes its disks live on |
| PATCH | `/api/vms/:vmid` | Update; reports what applied live and what waits |
| DELETE | `/api/vms/:vmid` | Undefine; `purge_disks` off by default |
| POST | `/api/vms/:vmid/start` | Start it |
| POST | `/api/vms/:vmid/shutdown` | Ask the guest to stop |
| POST | `/api/vms/:vmid/stop` | Stop it now — needs the acknowledgement |
| POST | `/api/vms/:vmid/reboot` | Ask the guest to restart |
| POST | `/api/vms/:vmid/reset` | Restart it now — needs the acknowledgement |
| POST | `/api/vms/:vmid/disks` | Create a volume and attach it |
| DELETE | `/api/vms/:vmid/disks/:id` | Detach; destroys the volume only if asked |
| POST | `/api/vms/:vmid/nics` | Attach an adapter |
| DELETE | `/api/vms/:vmid/nics/:id` | Detach it (`:id` is the hardware address) |
| GET | `/api/storage/pools` | Pools, grouped by node |
| GET | `/api/storage/pools/:pool/volumes` | Datasets and volumes under a pool |

There is deliberately **no** endpoint that creates, imports, or destroys a
pool, and `vm_flow::there_is_no_way_to_create_or_destroy_a_pool` asserts that
none appears by accident.

### Node grouping

`GET /api/vms` answers

```json
{ "nodes": [ { "node": "lumen", "vms": [ … ] } ] }
```

— a one-element list today, exactly as `/api/network/interfaces` does.
Mutating endpoints accept an optional `node` field which defaults to the local
node and currently rejects anything else with a clear "not in a cluster" error.
The check lives in `src/api/request.rs`, shared by all three domains.

Each machine object carries everything a table row *and* the detail page need
without a second round trip, including `actions` — per-verb `allowed`,
`reason`, and `requires_acknowledgement` — so a disabled control explains
itself rather than being silently grey. That is the same contract
`delete_blocked_reason` established for networking.

---

## Web UI

### The third column

`ConsoleShell` becomes three columns when — and only when — a page has put
something in the secondary-nav slot. The slot is a **generic** `ReactNode`
context (`lib/SecondaryNavContext.tsx`) that knows nothing about machines;
Storage will use the same column in part 3, and the shell should only learn
about a third column once. Every page that ignores the slot keeps the
two-column layout it already had, and the column is emptied on unmount so
navigating away never leaves a stale sidebar.

`CheckpointBar` is untouched.

### Machines are runtime data, not navigation

`lib/nav.ts` calls itself the single source of truth for navigation, and
navigation is static. Machines are not — they appear, disappear, and change
state while the operator is looking at them — so they are **composed into** the
sidebar beneath the Virtual Machines item rather than added to `NAV`.

The list is read once for the whole console (`lib/VmContext.tsx`) and shared by
three consumers: the sidebar, the command palette, and the page itself.
Polling once and sharing the answer is what keeps that from being three
requests every five seconds. The list scrolls on its own, and gains a filter
box past fifteen entries.

The command palette takes machines as a **dynamic source**, matching on name,
identifier, state, and tags. Jumping to a machine by name is the single most
useful thing ⌘K does on a hypervisor.

One subtlety: the sidebar does **not** read the query string. `useSearchParams`
in the console layout would put the whole layout behind a Suspense boundary at
export time for the sake of one highlighted row, so the page publishes the
selected identifier through the same context instead.

### Routing

The static export permits no dynamic segments, so state lives in query
parameters: `/virtual-machines?vm=101&section=overview`. `useSearchParams()`
requires a `<Suspense>` boundary or the export fails at prerender with an
opaque error — `VirtualMachinesPage` is the boundary and `VirtualMachines` is
the component behind it.

Console, Snapshots, Backups, and Tasks render as stubs in the existing
`StubPage` voice.

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

# 2. What the node has to put a machine on.
curl -sk -b "$JAR" "$HOST/api/storage/pools" |
  jq '.nodes[].pools[] | {name, health, free, used_percent}'

# 3. Define a machine: 2 processors, 4 GiB, a 32 GiB disk on rpool, on br0.
curl -sk -b "$JAR" -X POST "$HOST/api/vms" \
     -H 'Content-Type: application/json' \
     -d '{"name":"web01","vcpus":2,"memory_mib":4096,
          "disks":[{"pool":"rpool","size_gib":32}],
          "nics":[{"bridge":"br0"}]}' | jq '{vmid, name, state, disks, nics}'
# The volume it created:
#   /dev/zvol/rpool/lumen/vm-100-disk-0

#    A rejected machine answers 400 with the codes the console pins to fields:
curl -sk -b "$JAR" -X POST "$HOST/api/vms" \
     -H 'Content-Type: application/json' \
     -d '{"name":"web02","nics":[{"bridge":"br9"}]}' | jq '.errors'
# [ { "code": "unknown_bridge", "field": "bridge", … } ]

# 4. Start it, and watch it come up.
curl -sk -b "$JAR" -X POST "$HOST/api/vms/100/start" -d '{}' |
  jq '{state, current_vcpus, current_memory_mib, uptime_secs}'

# 5. Grow it while it runs, and see what actually reached the guest.
curl -sk -b "$JAR" -X PATCH "$HOST/api/vms/100" \
     -H 'Content-Type: application/json' -d '{"memory_mib":8192}' |
  jq '{applied_live, pending_reboot}'
# { "applied_live": ["memory set to 8192 MiB"], "pending_reboot": [] }

#    Something with no live path at all:
curl -sk -b "$JAR" -X PATCH "$HOST/api/vms/100" \
     -H 'Content-Type: application/json' -d '{"firmware":"bios"}' |
  jq '.pending_reboot'
# [ "firmware (takes effect when the machine restarts)" ]

# 6. Another disk, and take it away again with its volume.
curl -sk -b "$JAR" -X POST "$HOST/api/vms/100/disks" \
     -H 'Content-Type: application/json' \
     -d '{"pool":"rpool","size_gib":16,"bus":"virtio-scsi"}' | jq '.vm.disks'
curl -sk -b "$JAR" -X DELETE "$HOST/api/vms/100/disks/sda" \
     -H 'Content-Type: application/json' \
     -d '{"purge_disks":true,"i_understand_this_may_lose_data":true}' | jq '.vm.disks'

# 7. Ask it to stop; then, if it will not, take the power away.
curl -sk -b "$JAR" -X POST "$HOST/api/vms/100/shutdown" -d '{}' | jq '.state'
curl -sk -b "$JAR" -X POST "$HOST/api/vms/100/stop" \
     -H 'Content-Type: application/json' \
     -d '{"i_understand_this_may_lose_data":true}' | jq '.state'

#    Without the acknowledgement it is a 400, not a stopped machine:
curl -sk -b "$JAR" -X POST "$HOST/api/vms/100/stop" -d '{}' | jq '.errors[0].code'
# "unacknowledged_destructive_operation"

# 8. Remove it. The disks stay unless you say otherwise.
curl -sk -b "$JAR" -X DELETE "$HOST/api/vms/100" -d '{}' |
  jq '{removed_volumes, kept_volumes}'
# { "removed_volumes": [], "kept_volumes": ["rpool/lumen/vm-100-disk-0"] }
```

The machine is a plain domain the whole time — `virsh list --all`,
`virsh dumpxml web01`, and `zfs list -t volume` all show exactly what the API
just described.

---

## Manual test script

Automated tests run against the in-memory backends and cover create → start →
shutdown → delete, the acknowledgement paths, and the live-versus-restart
split. They cannot cover a guest that installs, a bridge that carries traffic,
or a kernel that has ZFS in it. **Run these on real hardware before trusting
any of it.**

### 0. Record what the node actually has

```sh
virsh version                                   # hypervisor and library
virsh pool-capabilities | grep -i zfs || echo "no ZFS storage backend (expected)"
zpool list -H -p -o name,size,alloc,free,frag,dedup,health,readonly
systemctl is-enabled virtqemud.socket zfs-zed.service
```

### 1. Create a zvol from the running service — the one still unconfirmed

This is the claim the design rests on and the only one the reproduction above
could not finish. Do it first.

1. Console → **Virtual Machines → Create**, one 8 GiB disk on `rpool`.
2. On the node: `zfs list -t volume` must show
   `rpool/lumen/vm-100-disk-0`, and `ls -l /dev/zvol/rpool/lumen/` must show
   the device node.
3. `journalctl -u lumen-controlplane` must contain no permission error.
4. **Then confirm the other half of the claim**, which is what keeps pool
   operations out of scope:
   ```sh
   ls -l --time-style=full-iso /etc/zfs/zpool.cache   # note the timestamp
   # create and destroy another disk from the console
   ls -l --time-style=full-iso /etc/zfs/zpool.cache   # must be unchanged
   ```
   If that file's timestamp moves, a volume operation is writing it and this
   design is wrong — say so in this document before going further.

Write the result into this document's hardening section either way.

### 2. Install a guest from an ISO onto a zvol

The real acceptance test, and the first end-to-end proof that the `br0`
decision from the networking stage was right.

1. Put an installer ISO on the node.
2. Create a machine with a 32 GiB disk and one adapter on `br0`.
3. Attach the ISO by hand for now — optical devices are not in this stage:
   ```sh
   virsh attach-disk web01 /path/to.iso sdz --type cdrom --mode readonly --config
   virsh dumpxml web01 | grep -A3 cdrom
   ```
4. Start it and install through **Console** — or, until part 2, through
   `virt-viewer --connect qemu:///system web01`.
5. The guest must get an address over `br0` from the same network the node is
   on. Check from the guest, and check the lease from the network's side:
   the hardware address must be `52:54:00:00:64:00` for machine 100.
6. `virsh domiflist web01` must show the adapter on `br0`.

### 3. Reboot the host

1. Turn **Start on boot** on for the machine, from **Options**.
2. `virsh dominfo web01 | grep Autostart` must read `enable` — the flag is
   libvirt's, not something Lumen keeps.
3. `reboot`.
4. The machine must be running when the node comes back, and the console must
   show an uptime measured from the node's boot rather than from the moment
   the control plane started. Restart `lumen-controlplane` and check the
   uptime does **not** reset — that is the live-metadata mechanism working.

### 4. Delete, and check what went with it

1. Delete the machine with **Also destroy its disks** left **off**.
2. `zfs list -t volume` must still show `rpool/lumen/vm-100-disk-0`, and the
   toast must have said where it is.
3. Recreate a machine, then delete it with the switch **on** and the
   acknowledgement ticked. The volume must be gone.
4. Try to delete a *running* machine without the acknowledgement: it must be
   refused, and the machine must still be running afterwards.

### 5. The live-versus-restart split, on a real guest

1. With the machine running, raise its memory from the **Hardware** page.
   Inside the guest, `free -m` should show the new figure — the balloon can
   grow up to the boot maximum.
2. Raise it well past the boot maximum. The console must report it as waiting
   for a restart, quoting libvirt's own message, and `free -m` inside the guest
   must be unchanged.
3. Restart the machine. The new figure must take effect, and the warning must
   clear on its own.
4. Change the firmware while running: it must report a restart every time,
   without the console having guessed.

### 6. Pull a disk out from under a running guest

1. Attach a second disk to a running machine. It should appear in the guest
   without a restart (`lsblk`).
2. Detach it with **Also destroy the volume** ticked. If the live detach
   succeeded the volume must go; if it did not, the operation must be refused
   with the volume left in place, and the message must say to restart first.
   **Never** let a volume disappear while a guest has it open.

### 7. A machine somebody else defined

1. `virsh define` a domain that carries no Lumen metadata.
2. It must **not** appear in the console — it is not Lumen's to manage.
3. Creating a Lumen machine with that same name must be refused with
   `duplicate_name`: the node still has the name, whoever made it.

---

## Development

```sh
make test    # installer + all three domain crates + control plane
make lint    # shellcheck, rpmlint, fmt/clippy for five manifests
```

Building needs `libvirt-devel` (EL) / `libvirt-dev` (Debian) to link against.
**Running** the tests needs no hypervisor and no pools: every test in
`lumen-virt`, `lumen-zfs`, and the control plane runs against
`backend::mock::MockBackend`, and CI never touches the runner's libvirtd or its
storage. Both mocks are compiled unconditionally and exported rather than
sitting behind `#[cfg(test)]`, for the reason `lumen-net`'s does: a
`cfg(test)` item is invisible to another crate's integration tests.

Against a real box, run the control plane as root — libvirt's socket and
`/dev/zfs` both want it, and the appliance's unit already does:

```sh
cd lumen-controlplane && sudo LUMEN_CP_NO_TLS=1 cargo run
```

If the hypervisor or the storage tooling is unreachable at startup the daemon
still comes up: the backend is swapped for its `Unavailable` variant, every
call answers with the reason, and the console shows it. An operator whose
hypervisor is down needs the console more than usual.

## Out of scope for this stage

The console viewer (part 2 — the VNC device is already defined on every
machine, so that part is purely additive). `zpool create`, `import`, `destroy`,
and `lumen-execd` (part 3). Snapshots, backups, migration, templates, cloning,
clustering, high availability, resource pools, and per-machine permissions.

In the console, only **Virtual Machines** (Overview, Hardware, Options) and
**Storage** are implemented. Console, Snapshots, Backups, and Tasks are stubs.
