//! Finds devices that announce `_p4fleet._tcp` over mDNS.
//!
//! In Docker this needs `network_mode: host` (multicast does not cross the
//! bridge network). Devices can always be added by address instead.

use crate::{db, Shared};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::net::IpAddr;

const SERVICE: &str = "_p4fleet._tcp.local.";

/// Routable IPv4 first, then link-local IPv4 (169.254/16), then IPv6.
fn rank(a: &IpAddr) -> u8 {
    match a {
        IpAddr::V4(v4) if !v4.is_link_local() && !v4.is_loopback() => 0,
        IpAddr::V4(_) => 1,
        IpAddr::V6(_) => 2,
    }
}

fn best_addr(addrs: &[IpAddr]) -> Option<IpAddr> {
    addrs.iter().copied().min_by_key(rank)
}

pub fn start(app: Shared) {
    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("mDNS unavailable, discovery off: {e}");
            return;
        }
    };
    let rx = match daemon.browse(SERVICE) {
        Ok(rx) => rx,
        Err(e) => {
            tracing::warn!("mDNS browse failed, discovery off: {e}");
            return;
        }
    };
    tracing::info!("browsing for {SERVICE}");
    tokio::spawn(async move {
        let _daemon = daemon; // keep the responder thread alive
        while let Ok(ev) = rx.recv_async().await {
            let ServiceEvent::ServiceResolved(info) = ev else { continue };
            let id = info.get_property_val_str("id").unwrap_or("").to_lowercase();
            if id.is_empty() {
                continue;
            }
            let addrs: Vec<IpAddr> = info.get_addresses().iter().copied().collect();
            let port = info.get_port();
            let Some(addr) = best_addr(&addrs) else { continue };
            let host = addr.to_string();
            let known = db::device(&app.db(), &id).ok().flatten();
            if let Some(d) = &known {
                // still announced at an address as good as the best one: keep it
                let current = addrs.iter().find(|a| a.to_string() == d.host);
                if d.port == port && current.is_some_and(|c| rank(c) <= rank(&addr)) {
                    continue;
                }
            }
            if let Err(e) = db::upsert_device(&app.db(), &id, &host, port, "mdns") {
                tracing::error!("storing discovered device: {e:#}");
                continue;
            }
            match known {
                None => {
                    let label = info.get_property_val_str("label").unwrap_or("device").to_string();
                    app.event(Some(&id), "info", "discovered", &format!("{label} found at {host}:{port}"));
                    // poll right away instead of waiting for the next round
                    let app2 = app.clone();
                    tokio::spawn(async move { crate::poller::poll_one(&app2, &id).await });
                }
                Some(d) if d.source == "mdns" => {
                    app.event(Some(&id), "info", "address", &format!("address changed {}:{} -> {host}:{port}", d.host, d.port));
                }
                Some(_) => {}
            }
        }
    });
}
