// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Smoke-test the REAL `engine::bond_routing` netlink code against the
//! live kernel. Programs one gateway-mode leg, dumps the kernel state
//! *its own code* installed, verifies steering, then tears it down and
//! confirms it's gone. This is the code path a bonded output uses for a
//! `gateway`-mode leg — exercised here without needing a full flow.
//!
//! Needs `CAP_NET_ADMIN` (run as root):
//!
//! ```text
//! cargo build --example bond_route_smoke
//! sudo ./target/debug/examples/bond_route_smoke <iface> <source/prefix> <gateway> [test-ip]
//! # 4G dongle on this box:
//! sudo ./target/debug/examples/bond_route_smoke enx0c5b8f279a64 192.168.8.100/24 192.168.8.1 1.1.1.1
//! ```

use std::process::Command;

use bilbycast_edge::engine::bond_routing::{BondRouteManager, SourceNet};

/// Run a read-only shell command and echo its output (for inspecting the
/// kernel routing state that the Rust code created).
fn sh(cmd: &str) {
    println!("  $ {cmd}");
    match Command::new("sh").arg("-c").arg(cmd).output() {
        Ok(o) => {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                println!("    {line}");
            }
            let err = String::from_utf8_lossy(&o.stderr);
            if !err.trim().is_empty() {
                println!("    [stderr] {}", err.trim());
            }
        }
        Err(e) => println!("    (could not run: {e})"),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let iface = args.get(1).cloned().unwrap_or_else(|| "enx0c5b8f279a64".into());
    let source = args.get(2).cloned().unwrap_or_else(|| "192.168.8.100/24".into());
    let gateway = args.get(3).cloned().unwrap_or_else(|| "192.168.8.1".into());
    let test_ip = args.get(4).cloned().unwrap_or_else(|| "1.1.1.1".into());

    let src = SourceNet::parse(&source)?;
    let gw: std::net::IpAddr = gateway
        .parse()
        .map_err(|_| anyhow::anyhow!("bad gateway IP: {gateway}"))?;
    let flow_id = "smoke";
    let path_id: u8 = 0;

    println!("== engine::bond_routing smoke test ==");
    println!("   iface={iface}  source={source}  gateway={gateway}\n");

    println!("[1] BEFORE — where does the kernel route a packet FROM {} today?", src.addr);
    sh(&format!("ip route get {test_ip} from {}", src.addr));

    println!("\n[2] BondRouteManager::global() — opens netlink, runs startup GC sweep");
    let mgr = BondRouteManager::global().await?;

    println!("\n[3] mgr.program(flow=\"{flow_id}\", path={path_id}, ...) — REAL netlink RTM_NEW{{ADDR,ROUTE,RULE}}");
    match mgr.program(flow_id, path_id, &iface, src, gw).await {
        Ok(()) => println!("    program() => Ok"),
        Err(e) => {
            println!("    program() => ERROR: {e}");
            println!("    (need CAP_NET_ADMIN — run with sudo)");
            return Err(e);
        }
    }

    println!("\n[4] AFTER program — kernel state the Rust code just installed:");
    sh("ip rule show | grep -E '48128|10000' || echo '(NO RULE — unexpected)'");
    sh("ip route show table 48128");
    println!("    steering check — FROM {} should now go via {gateway}:", src.addr);
    sh(&format!("ip route get {test_ip} from {}", src.addr));

    println!("\n[5] mgr.teardown(flow=\"{flow_id}\", path={path_id}) — del rule + flush table");
    mgr.teardown(flow_id, path_id).await;
    println!("    teardown() returned");

    println!("\n[6] AFTER teardown — should be clean:");
    sh("ip rule show | grep -E '48128|10000' || echo '(clean — rule gone)'");
    sh("ip route show table 48128 | grep . || echo '(clean — table empty)'");

    println!("\n== done — the Rust code programmed and removed the leg's policy route ==");
    Ok(())
}
