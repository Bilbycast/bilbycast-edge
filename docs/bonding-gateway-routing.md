# Gateway-based path selection for bonded outputs

**Status:** IMPLEMENTED (2026-06-09). This began as a proposal; the
"As-built" section below records what actually shipped and where it
deviates from the original design. The proposal text is retained for
rationale.

## As-built (what shipped)

Both binding modes are implemented end-to-end and compile + unit-test
green (the full edge debug binary links):

- **Interface mode** (`engine` / `bonding-transport`): adaptive NIC pin —
  `SO_BINDTODEVICE`, automatic fallback to unprivileged `IP_UNICAST_IF`
  on `EPERM`. Resolved mechanism surfaced on `BondPathLegStats.binding`.
- **Gateway mode** (`engine::bond_routing`): per-leg policy route via
  **pure-Rust netlink** (`rtnetlink` 0.24 + `netlink-packet-route` 0.34 — the
  two are version-locked, see the note in `Cargo.toml`), using the edge process's
  `CAP_NET_ADMIN` (already granted by the default unit and the binary's
  file caps). Config fields `gateway` + `source` on the bonded-output UDP
  path; `engine::output_bonded` programs routes before the socket binds
  and tears them down on every exit; `SlotAllocator` owns the reserved
  table/priority ranges with startup GC.
- **Decision change vs proposal — source-IP rules, not fwmark.** Gateway
  mode keys the policy rule on `from <source>` (reusing the bond socket's
  existing `bind` to the source IP) instead of `SO_MARK` + `fwmark`. This
  means **zero changes to the bonding crate** for gateway mode and one
  fewer reserved range. Addressing is **option-b** (edge ensures the
  `source` address on the NIC via `RTM_NEWADDR`; addresses are left in
  place on teardown). `interface` names the (shared) NIC used as the
  route `dev`; it is required in gateway mode.
- **Manager UI** (`config/bonding.js`): per-path **Path selection**
  dropdown (None / Interface / Gateway) with gateway/source fields and an
  Interface `<datalist>` fed by `window.Bilby.bondInterfaces` (populating
  that global from the node's `network_interfaces` is the one remaining
  wiring task; the field is a graceful free-text until then).

- **Failure and concurrency.** A `program` that fails removes whatever
  part of the leg's rule and route it had installed and frees its slot,
  so a leg retried every 5 s against an unplugged modem no longer uses up
  the node's 256 slots (each failed try used to keep one). A source
  address it added stays on the NIC, as on teardown. `program` and
  `teardown` run whole under the manager's one lock, so two calls for one
  leg never interleave: two racing re-programs leave one slot, and a
  `teardown` made while a `program` of its leg is in flight waits for it
  and returns with the leg gone.
- **Stopping a bonded output.** A `program` made *after* a `teardown`
  installs the leg again, as it should, so the output must make none once
  it stops. It stops its 5 s re-assert task first — cancels the task's own
  token and waits for it, at most 2 s — and only then tears its legs down.
  The task prefers its token to a due tick or a socket-rebuild event and
  checks it before every leg and every `program`. Past the 2 s the task
  is left to finish, not aborted: it can then only be inside an integrity
  check, after which it returns, or inside a `program` that asked for the
  lock before the teardown, which therefore runs after it and removes
  what it installed. Before this the task was never stopped or waited
  for: a re-assert that reached a leg after its teardown re-installed the
  leg's rule, route and slot for an output that had stopped, steering
  that source address through the dead output's gateway until the same
  leg was programmed again, a clean shutdown, or a restart's sweep.
- **Integrity check.** `is_intact`, which the 5 s re-assert uses, checks
  the leg's `from <source>` rule (its priority, its source as a host
  prefix and its table) as well as the route in its table. It used to
  check only the table, so a leg whose rule an `ip rule flush` or a
  network manager had removed was reported intact and rode the main
  default route until something else re-programmed it (a flow restart or
  a socket rebuild).

The netlink calls run against a real kernel in
`tests/bond_routing_netns.rs`, which is `#[ignore]`d because it needs
`CAP_NET_ADMIN` in a throwaway network namespace. CI runs it on every pull
request and push to `main`, as root under `sudo unshare --net`, once per set of
bases (default, moved, the highest, either side of the kernel's VRF rule,
an unparseable value, and four refused ones); its header holds the docker
command for a local run. It covers the startup sweep (which must leave the
kernel's own rules, the VRF one included, alone), `program`, `is_intact`
(emptied table; rule deleted or altered), `teardown`, `teardown_all`,
failed programs, racing calls on one leg, refused bases and the dish
route, in both families. Stopping an output's re-assert task before its
teardown is tested in `engine::output_bonded` against a fake route
manager (`bond_routing::LegRoutes`), since a namespace cannot land an
output's stop inside a re-assert on purpose. Gateway mode has still
**not** run on real hardware with modems and routers; that live
validation is pending.

---

# Design proposal (original) — gateway-based path selection

**Where this proposal and the As-built section above disagree, As-built wins.**
The places where the difference would actively mislead have been folded back in
below (source-IP rules rather than fwmark, relay-leg gateway support, the event
names that actually ship); everything else is retained as rationale.

Open questions at the bottom were resolved as: Q1→b, Q2→defaults kept,
Q3→hard-refuse, Q4→leave addresses, Q5→edge-only (bonding crate untouched).

## 1. Goal & scope

Let an operator bind each leg of a **bonded output** to a specific **router/gateway**
instead of a specific NIC, so the "all cellular routers + Starlink on one dumb
Ethernet switch, one PC NIC on the same switch" topology works **configured
entirely from the manager UI, no operator shell commands**.

The edge programs the per-path policy route itself via **netlink**, using
`CAP_NET_ADMIN` — which the default `bilbycast-edge.service` unit **already
grants** (it's there for `SO_TXTIME`). No new capability, no `CAP_NET_RAW`.

Scope: **sender side only for plain `udp` legs** (the field box's bonded
*output* paths). The hub's bonded *input* does not need gateway selection there
— its NACK/keepalive return traffic rides the hub's single default route, and
NAT on each field-side router steers the return packets back per-path
automatically.

**As-built exception — `relay` legs.** Gateway mode is valid on **both** ends of
a relay leg: both ends dial *out* to the relay, so each can pin its own egress
uplink. The route is programmed by
`tunnel::udp_relay_client::program_tunnel_gateway`, keyed `(tunnel_id, 0)`, for
relay bond legs and for native-UDP relay and direct tunnels alike. Two
behavioural differences from the bonded-output path an operator will otherwise
trip on: it is **best-effort** — a netlink or program failure (refused range
bases included) leaves the tunnel running *without* the policy route, and
raises a Warning `tunnel_gateway_route_failed` event (category `tunnel`,
details `tunnel_id` / `interface` / `source` / `gateway` / `error`) besides the
log line; until this release only the log line said so — and it is **not
covered by the 5 s route-integrity re-assert**: `gateway_legs` in
`output_bonded.rs` matches `udp` transports only.

## 2. The addressing reality (read first — there's no free lunch)

Using N independent uplinks from one box fundamentally needs **N independent L3
attachments**: N source addresses + N gateways. Gateway mode doesn't remove that
— it relocates it:

| Topology | How the N attachments appear | Path-select mechanism |
|----------|------------------------------|-----------------------|
| N physical NICs | DHCP per NIC → auto | interface (`IP_UNICAST_IF`) |
| 1 NIC + N VLANs | DHCP per VLAN subif → auto | interface on VLAN subif |
| **1 NIC + dumb switch + N gateways** | **must give the NIC an address per router subnet** | **gateway (this doc)** |

So gateway mode trades *dongles/VLANs* for two prerequisites:

1. **Each router must be on a distinct LAN subnet** (e.g. `192.168.10.1/24`,
   `192.168.20.1/24`, …). Routers that all default to `192.168.1.0/24` collide on
   the shared L2 and must be re-addressed first. This is router config, done once.
2. **The PC's NIC needs an address in each router's subnet.** Three ways to get
   them, in order of "edge does the work":
   - **a. Operator/static** — assign secondary IPs on the NIC (host config). Simplest to reason about, but it's host setup.
   - **b. Edge-programmed static** — operator supplies `source` per path; the edge adds the address via netlink (`RTM_NEWADDR`, `CAP_NET_ADMIN`). UI-only.
   - **c. Edge-run DHCP-per-path** — the edge runs a lightweight DHCP client per path, `SO_MARK`-bound to egress the right router, to lease an address + learn the gateway automatically. Most "appliance-like," most code.

Decision needed (Open Q1): which of a/b/c we support first. Recommendation: **b**
(operator types gateway + source per path; edge programs addr+route+rule), with
**c** as a later "auto" enhancement.

## 3. Config schema

Add an optional `gateway` (and, for option-b addressing, `source`) to the bonded
UDP path transport. Lives in **both** crates:

- `bilbycast-edge` `BondPathTransportConfig::Udp` (config/models.rs)
- `bonding-transport` — **unchanged**. As built, gateway mode needed no field in
  the bonding crate at all: the policy rule is keyed `from <source>` and the
  bond socket already binds `<source>`. The crate has zero fwmark references and
  never touches routing tables.

```jsonc
// bonded OUTPUT path
{
  "type": "udp",
  "name": "5g-att",
  "remote": "HUB_IP:7400",
  "gateway": "192.168.10.1",          // NEW — the router for this leg
  "source":  "192.168.10.2/24"        // NEW (option-b) — addr the edge ensures on the NIC
  // "interface" still allowed; if set, route is pinned to that dev too
}
```

**Validation** (config/validation.rs `validate_bond_path_transport`):
- `gateway` is a valid unicast IP; reject multicast/loopback/unspecified.
- On a `udp` leg, `gateway` is only permitted on **sender** paths (rejected on a
  bonded *input*). On a `relay` leg it is accepted on **both** ends — see §1.
- If `source` set: valid CIDR; `gateway` must be inside that prefix (else the
  gateway is unreachable → reject at config time, not silently blackhole).
- `gateway` requires **both** `source` and `interface` — the route is installed
  `dev <interface>` and the rule is keyed `from <source>`. `source` without
  `gateway` is refused ("`source` is only meaningful with `gateway`"). Same on
  the `udp` and `relay` arms.

## 4. Routing-table ownership model  ← the part to sign off

The edge owns a **reserved, namespaced slice** of host routing policy. It never
touches `main`/`local`/`default` tables.

| Resource | Reserved range (default) | Env override | Per-path value |
|----------|--------------------------|--------------|----------------|
| Route table id | `48128‑48383` (`0xBC00 + slot`) | `BILBYCAST_BOND_RT_TABLE_BASE`, a base from 256 to 4294967040 | base + slot |
| `ip rule` priority | `10000‑10255` | `BILBYCAST_BOND_RT_PRIO_BASE`, a base from 1 to 744 or from 1001 to 32510 | base + slot |

(`0xBC` echoes the bond wire magic, making our entries recognizable. Ranges are
**host-level OS tuning** → env-configurable, per the project convention that
behavior knobs are manager-config but host/OS knobs are env.)

> An earlier version of this table listed a third row, `fwmark`, with a
> `BILBYCAST_BOND_FWMARK_BASE` override. There is no fwmark in
> `engine::bond_routing` and never was — the leg is selected by source
> address in the `ip rule`, not by a packet mark — and no code reads that
> variable. The row is removed rather than corrected: documenting an
> override that does nothing is worse than documenting nothing, because an
> operator who sets it to dodge a collision believes they have.

**Bounds on the overrides (as built).** The startup sweep below deletes every
route in the table range and every rule in the priority range, and a leg's
teardown deletes every rule at its priority, whatever the rule does, so each
range must hold nothing the kernel owns:

- the 256 tables must clear the kernel's tables 0 (unspec), 253 (`default`),
  254 (`main`) and 255 (`local`) — no base under 256 can — and end at or below
  table 4294967295, the last id;
- the 256 priorities must hold none of the rules the kernel installs itself —
  0 `lookup local` and 32766 `lookup main` in every namespace and family,
  32767 `lookup default` (IPv4, and the multicast-routing families' default
  rule), and 1000, the `l3mdev` rule the VRF driver adds in each family when
  a namespace's first VRF device is created — and must end before
  `lookup main`. That leaves bases 1–744 and 1001–32510. Those four are every
  rule the kernel adds at a fixed priority: `net/ipv4/fib_rules.c`,
  `net/ipv6/fib6_rules.c`, `net/ipv4/ipmr.c` and `net/ipv6/ip6mr.c` are the
  only callers of `fib_default_rule_add`, and `drivers/net/vrf.c` the only
  driver that sets `FRA_PRIORITY`. Past `lookup main` a leg's rule would never
  be consulted for a source the main table routes, which with a default route
  there is every source — a cosmetic bond.

What each value does:

- **Unset, empty or only whitespace** (a systemd `Environment=VAR=` line):
  the default, silently.
- **Not a whole number** (`0xBD00`, `7000x`, a value that is not UTF-8):
  discarded for the default, as every release did, but no longer silently —
  at boot a Warning `bond_rt_base_unparseable` event (category `config`)
  names the variable, the value, the default in use and the bounds.
- **A whole number outside the bounds**, past `u32` included (`4294967296`,
  `-1`): **refused**, and gateway routing is disabled for the process. At
  boot a Critical `bond_rt_base_refused` event (category `config`) names each
  refused variable, its value and its bounds — both in one message when both
  are refused. No sweep runs, and every gateway-mode leg fails to program
  with the same text: a Critical `media` event on a bonded output, which does
  not start; a Warning `tunnel_gateway_route_failed` event on a relay bond leg
  or a native-UDP relay or direct tunnel, which runs on without its route
  (§1); a failed `test_bond_leg` reachability or capacity report. The edge
  does not fall back to the default range, which may be the range the
  operator moved away from and would then be swept. The Starlink dish route
  uses no reserved range and keeps working.

The bases are read at boot for that report and again at the manager's first
use (the first gateway leg, leg probe or dish route), so a change needs an
edge restart. Before this release any `u32` was taken: a table base of 200
with a priority base of 32600 emptied the `local`, `main` and `default`
tables and deleted the `lookup main` and `lookup default` rules at that first
use, a priority base from 745 to 1000 deleted a VRF host's `l3mdev` rule, a
table base within 255 of `u32::MAX` overflowed, and an unparseable value fell
back to the default range without a word.

`slot` = a per-host index allocated from an in-process slab keyed by
`(flow_id, path_id)`, freed on teardown. Max 256 concurrent gateway-mode legs
per host (`MAX_SLOTS`, not configurable); past it `program` fails with
`reserved slot range exhausted`, never a silent cap.

**What gets programmed per path (option-b), all via netlink:**
1. (if `source` not already on the NIC) `RTM_NEWADDR` add `source` to the NIC.
2. `RTM_NEWROUTE` in table `T`: `default via <gateway> [dev <iface>]`.
3. `RTM_NEWRULE` priority `P`: `from <source-ip> lookup <T>`. The bond socket
   binds `<source>`, so its packets match the rule — no packet mark is involved
   anywhere.

Identification of "our" entries for GC = **two independent sweeps**: every rule
whose priority ∈ prio-range, and every route whose table ∈ table-range. No
external tags needed.

## 5. Lifecycle

| Event | Action |
|-------|--------|
| **First use of the manager** (first gateway leg, leg probe or dish route) | **GC first:** scan netlink; delete any rule/route in our reserved ranges (orphans from a prior crash) before programming any leg. Never on refused bases (§4); addresses are never swept. |
| **Bonded-output flow start** | For each gateway path: resolve the interface → allocate slot → program addr/route/rule (a failure removes whatever of the rule and route went in and frees the slot) → bring up bond socket bound to `source` → start the 5 s re-assert task. |
| **Flow stop / path remove** | Close socket → stop the re-assert task (cancel, wait at most 2 s) → delete rule, flush table → free slot. Edge-added addresses stay. Order: socket first, then the re-assert task, then routing teardown. |
| **Hot-swap** (`UpdateFlow`, add/remove path) | Allocate/free incrementally; untouched paths keep their (slot, route, rule). |
| **Edge crash / SIGKILL** | Entries linger until the next process's first use of the manager (first row), which sweeps them; a process that never uses gateway routing never sweeps. Idempotent. |

New edge module: `engine::bond_routing` (or `net::policy_routes`) owns the slab +
netlink handle + GC. `output_bonded.rs` calls it around socket bring-up/teardown.
New dependency: a netlink crate (`rtnetlink` or `neli`).

## 6. Failure modes (no silent collapse)

| Condition | Behavior |
|-----------|----------|
| `CAP_NET_ADMIN` missing (hardened host / netns) | Refuse to start the gateway path; emit a **Critical** `media` event carrying the cap hint. **Do not** fall through to the default route (that silently collapses every path onto one router — the exact trap). If the path also has `interface`, optionally fall back to interface mode. |
| Gateway not in any connected subnet | Rejected at config validation (§3). A runtime route-add failure — EHOSTUNREACH included — aborts the flow start from `BondRouteManager::program` and emits its own **Critical** `media` event naming the path, the gateway and the netlink error (no cap hint, no `error_code`). There is no reachability-specific event. |
| Gateway reachable but dead uplink | Route installs, packets blackhole → bond keepalive marks the path dead (existing health path). No new code. |
| Route or rule disappears after start (link flap, device re-plug, route flush, `ip rule flush`, a network manager removing rules it did not create) | The 5 s re-assert task re-programs it (`is_intact` checks the rule's priority, source and table as well as the table's route). If that fails it emits **Critical** `media` with `error_code: "bond_gateway_route_lost"` (details `output_id` / `path_id` / `path_name` / `gateway`), latched once per failing streak, and says plainly that the leg rides the default route — a cosmetic bond — until it succeeds. UDP gateway legs only; relay legs are not re-asserted (§1). |
| Netlink unavailable | Same as cap-missing. |
| Range bases refused (§4) | **Critical** `config` event `bond_rt_base_refused` at boot, naming each refused variable, its value and its bounds. No sweep runs. Every gateway path fails to program with the same text, through the failed-program **Critical** `media` event (no cap hint, no `error_code`); a relay or tunnel leg through a **Warning** `tunnel_gateway_route_failed`. |
| Range base not a whole number (§4) | The default range is used; **Warning** `config` event `bond_rt_base_unparseable` at boot, naming the variable, its value, the default and the bounds. |

> **As-built:** the two flow-start failures above go out through `emit_flow`
> with **no details object**, so they carry no `error_code` and cannot be
> filtered or alarmed on programmatically — only `bond_gateway_route_lost` is
> coded. `bond_route_program_failed` and `bond_gateway_unreachable`, named in
> earlier drafts of this table, exist in no source tree. Event catalogue:
> [events-and-alarms.md](events-and-alarms.md).

Active mechanism per leg is surfaced on `BondPathLegStats.binding`: `gateway`
for a gateway-mode leg, otherwise the kernel primitive that actually bound the
socket — `so_bindtodevice`, `ip_unicast_if`, `ip_bound_if` or `none`.
Interface-mode legs additionally carry the egress netdev on
`BondPathLegStats.interface`, which is `None` on gateway-mode legs. Neither the
reserved table id nor any mark reaches the stats surface — and no mark is
programmed in the first place.

## 7. Interaction with the rest of the edge (verified)

- Bonded flows use the **Wallclock** master clock — no PTP. ✔
- Bonded outputs **bypass `wire_emit`** — no `SO_TXTIME`/etf qdisc. ✔
- Bond UDP sockets set **no DSCP** — never land on an etf class. ✔
- Single NIC, all gateways on one L2 → return traffic arrives on that one NIC →
  **`rp_filter` strict passes** (reverse path is the same interface). No sysctl
  needed in this topology (unlike the multi-NIC interface mode). ✔
- Source-bound egress + per-router NAT handles the return path; no ingress route
  selection required. ✔

So gateway mode is orthogonal to the precision-timing stack; a box could ingest
ST 2110 (PTP+etf) on one NIC and bond out via gateways on another with zero
interaction.

## 8. Operator UX (what this produces)

1. One-time: set each router to a distinct LAN subnet; plug all routers + the PC
   into the dumb switch.
2. Manager UI → bonded output → per path: **Gateway** field (router LAN IP) +
   **Source** field (the box's address on that subnet). The edge could later
   offer a dropdown by probing the L2 neighbor table / DHCP (option-c).
3. Save. The edge programs addr/route/rule and brings the bond up. No shell.
4. Verify in the bond stats card: distinct per-path RTT + the active-mechanism
   tag; pull a router → its named path goes dead, stream survives.

## 9. Honest comparison & recommendation

| | Interface mode (`IP_UNICAST_IF`) | Gateway mode (this doc) |
|---|---|---|
| Physical | one NIC per router (dongles) or VLANs | **dumb switch, one NIC** |
| Router config | none (DHCP) | **distinct subnet per router** |
| Box addressing | DHCP per NIC (auto) | address per subnet (edge-programmed, opt-b) |
| Privilege | unprivileged (or `CAP_NET_RAW`) | `CAP_NET_ADMIN` (already default) |
| Edge owns host state | no | **yes — scoped routing slice** |
| Code lift | small | moderate (netlink mgr + lifecycle + GC) |

Recommendation: **support both, as a per-path "binding mode."** Gateway mode is
the right default for the dumb-switch topology you described; interface mode stays
for true multi-NIC / VLAN setups. Interface mode is the smaller first step; gateway
mode is the one that matches your physical plan.

## 10. Open questions for sign-off

- **Q1.** Addressing: option **a** (operator/static), **b** (edge-programmed static, *recommended first*), or **c** (edge DHCP-per-path) — which do we build first?
- **Q2.** Reserved-range defaults (table `48128+`, prio `10000+`) — acceptable, or align to an existing convention on your hosts?
- **Q3.** On `CAP_NET_ADMIN` missing: hard-refuse-with-event (recommended) vs fall back to interface mode if `interface` is set?
- **Q4.** Should the edge ever **remove** an address it added on teardown, or leave it (safer for a shared NIC)? Recommendation: only remove addresses the edge itself added; leave pre-existing ones.
- **Q5.** Build it in `bonding-transport` (so the standalone `bilbycast-bonder` gets it too) or keep the netlink/route ownership **edge-only**? Recommendation: **edge-only** route ownership; bonding crate stays I/O-policy-free — as built it needed no change at all.
