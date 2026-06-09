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
  **pure-Rust netlink** (`rtnetlink` 0.21), using the edge process's
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

Runtime caveat unchanged: the netlink **route-programming syscalls
require a live kernel + CAP_NET_ADMIN and have NOT been exercised on real
hardware here** (no root + modems in this environment). Only the pure
`SlotAllocator` / `SourceNet` logic is unit-tested. Live validation
pending — run on a box with real interfaces.

---

# Design proposal (original) — gateway-based path selection

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

Scope: **sender side only** (the field box's bonded *output* paths). The hub's
bonded *input* does not need gateway selection — its NACK/keepalive return
traffic rides the hub's single default route, and NAT on each field-side router
steers the return packets back per-path automatically.

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
- `bonding-transport` `PathTransport::Udp` — gains only a `fwmark: Option<u32>`
  (the edge fills it; the bonding crate just calls `setsockopt(SO_MARK)`). The
  bonding crate never touches routing tables.

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
- `gateway` only permitted on **sender** paths (reject on bonded *input*).
- If `source` set: valid CIDR; `gateway` must be inside that prefix (else the
  gateway is unreachable → reject at config time, not silently blackhole).
- `gateway` + `interface` may co-exist (route gets `dev <interface>`).

## 4. Routing-table ownership model  ← the part to sign off

The edge owns a **reserved, namespaced slice** of host routing policy. It never
touches `main`/`local`/`default` tables.

| Resource | Reserved range (default) | Env override | Per-path value |
|----------|--------------------------|--------------|----------------|
| Route table id | `48128‑48383` (`0xBC00 + slot`) | `BILBYCAST_BOND_RT_TABLE_BASE` | base + slot |
| `ip rule` priority | `10000‑10255` | `BILBYCAST_BOND_RT_PRIO_BASE` | base + slot |
| fwmark | `0xBC000000 + slot` (mask `0xFFFFFF00`) | `BILBYCAST_BOND_FWMARK_BASE` | base + slot |

(`0xBC` echoes the bond wire magic, making our entries recognizable. Ranges are
**host-level OS tuning** → env-configurable, per the project convention that
behavior knobs are manager-config but host/OS knobs are env.)

`slot` = a per-host index allocated from an in-process slab keyed by
`(flow_id, path_id)`, freed on teardown. Max 256 concurrent bonded-output paths
per host (configurable); exceeding it is a loud config error, never a silent cap.

**What gets programmed per path (option-b), all via netlink:**
1. (if `source` not already on the NIC) `RTM_NEWADDR` add `source` to the NIC.
2. `RTM_NEWROUTE` in table `T`: `default via <gateway> [dev <iface>]`.
3. `RTM_NEWRULE` priority `P`: `fwmark <M> lookup <T>`.
4. Bond path socket created with `SO_MARK = M` (in the bonding-transport adapter).

Identification of "our" entries for GC = anything whose table ∈ table-range **and**
rule-priority ∈ prio-range **and** fwmark ∈ fwmark-range. No external tags needed.

## 5. Lifecycle

| Event | Action |
|-------|--------|
| **Edge startup** | **GC first:** scan netlink; delete any rule/route/addr in our reserved ranges (orphans from a prior crash) before starting any flow. |
| **Bonded-output flow start** | For each gateway path: allocate slot → program addr/route/rule → set `fwmark` on the socket config → bring up bond socket. |
| **Flow stop / path remove** | Drop socket → delete rule, route, flush table, (optionally) remove edge-added addr → free slot. Order: socket first, then routing teardown. |
| **Hot-swap** (`UpdateFlow`, add/remove path) | Allocate/free incrementally; untouched paths keep their (slot, route, rule). |
| **Edge crash / SIGKILL** | Entries linger until next startup GC (step 1). Idempotent. |

New edge module: `engine::bond_routing` (or `net::policy_routes`) owns the slab +
netlink handle + GC. `output_bonded.rs` calls it around socket bring-up/teardown.
New dependency: a netlink crate (`rtnetlink` or `neli`).

## 6. Failure modes (no silent collapse)

| Condition | Behavior |
|-----------|----------|
| `CAP_NET_ADMIN` missing (hardened host / netns) | Refuse to start the gateway path; emit **Critical** `bond_route_program_failed` with the cap hint. **Do not** fall through to the default route (that silently collapses every path onto one router — the exact trap). If the path also has `interface`, optionally fall back to interface mode. |
| Gateway not in any connected subnet | Rejected at config validation (§3). At runtime (route add EHOSTUNREACH) → Critical `bond_gateway_unreachable`. |
| Gateway reachable but dead uplink | Route installs, packets blackhole → bond keepalive marks the path dead (existing health path). No new code. |
| fwmark collides with host firewall (nftables/Docker/VPN) | Configurable base (§4) + startup log of the chosen marks; documented. |
| Netlink unavailable | Same as cap-missing. |

Active mechanism per path surfaced in bond path stats (mirrors
`OutputStats.wire_pacing_tier`): `gateway` / `interface` / `none`, plus the
programmed `(table, mark)` for observability.

## 7. Interaction with the rest of the edge (verified)

- Bonded flows use the **Wallclock** master clock — no PTP. ✔
- Bonded outputs **bypass `wire_emit`** — no `SO_TXTIME`/etf qdisc. ✔
- Bond UDP sockets set **no DSCP** — never land on an etf class. ✔
- Single NIC, all gateways on one L2 → return traffic arrives on that one NIC →
  **`rp_filter` strict passes** (reverse path is the same interface). No sysctl
  needed in this topology (unlike the multi-NIC interface mode). ✔
- `SO_MARK` egress + per-router NAT handles the return path; no ingress route
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
- **Q2.** Reserved-range defaults (table `48128+`, prio `10000+`, fwmark `0xBC000000+`) — acceptable, or align to an existing convention on your hosts?
- **Q3.** On `CAP_NET_ADMIN` missing: hard-refuse-with-event (recommended) vs fall back to interface mode if `interface` is set?
- **Q4.** Should the edge ever **remove** an address it added on teardown, or leave it (safer for a shared NIC)? Recommendation: only remove addresses the edge itself added; leave pre-existing ones.
- **Q5.** Build it in `bonding-transport` (so the standalone `bilbycast-bonder` gets it too) or keep the netlink/route ownership **edge-only** and leave the bonding crate with just the `fwmark` field? Recommendation: **edge-only** route ownership; bonding crate stays I/O-policy-free.
