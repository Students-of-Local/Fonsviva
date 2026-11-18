//! Topology probe: the links this machine shares with a peer, the route the
//! kernel really takes to it, and the round trip with and without binding to
//! the link's address. Each step is a parser over a command's output, so tests
//! feed fixtures and never depend on the host.

use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

use crate::hardware::{self, Hardware};

pub const SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    Cable,
    Tailnet,
    Lan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub addr: Ipv4Addr,
    pub prefix: u8,
}

impl Interface {
    pub fn kind(&self) -> LinkKind {
        kind_of(self.addr)
    }

    fn holds(&self, peer: Ipv4Addr) -> bool {
        if self.prefix == 32 {
            return self.kind() == LinkKind::Tailnet && kind_of(peer) == LinkKind::Tailnet;
        }
        let mask = u32::MAX
            .checked_shl(32 - u32::from(self.prefix))
            .unwrap_or(0);
        u32::from(self.addr) & mask == u32::from(peer) & mask
    }
}

pub fn kind_of(addr: Ipv4Addr) -> LinkKind {
    let [a, b, ..] = addr.octets();
    if a == 169 && b == 254 {
        LinkKind::Cable
    } else if a == 100 && (64..128).contains(&b) {
        LinkKind::Tailnet
    } else {
        LinkKind::Lan
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Rtt {
    pub sent: u32,
    pub received: u32,
    pub avg_ms: f64,
    pub max_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Link {
    pub kind: LinkKind,
    pub interface: String,
    pub local: Ipv4Addr,
    pub peer: Ipv4Addr,
    /// The interface the kernel routed the peer through before any bound traffic.
    pub route_interface: Option<String>,
    /// When that route is a host route macOS cloned from earlier traffic, the
    /// interface of the network route under it, which takes over when it expires.
    pub network_route_interface: Option<String>,
    /// False when an unbound socket to the peer would, or later may, leave by another interface.
    pub routed_on_link: bool,
    pub unbound: Option<Rtt>,
    pub bound: Option<Rtt>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Peer {
    pub name: String,
    pub links: Vec<Link>,
    /// Peer addresses no local interface shares a subnet with.
    pub no_link: Vec<Ipv4Addr>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Topology {
    pub schema: u32,
    pub probed_at: String,
    pub machine: Hardware,
    pub peers: Vec<Peer>,
}

/// macOS `ifconfig`.
pub fn parse_ifconfig(out: &str) -> Vec<Interface> {
    let mut found = Vec::new();
    let mut name = String::new();
    for line in out.lines() {
        if !line.starts_with(char::is_whitespace) {
            name = line.split(':').next().unwrap_or_default().to_string();
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.first() != Some(&"inet") {
            continue;
        }
        let Some(addr) = words.get(1).and_then(|w| w.parse().ok()) else {
            continue;
        };
        let prefix = words
            .iter()
            .position(|w| *w == "netmask")
            .and_then(|i| words.get(i + 1))
            .and_then(|m| u32::from_str_radix(m.trim_start_matches("0x"), 16).ok())
            .map_or(32, |mask| mask.count_ones() as u8);
        push(&mut found, &name, addr, prefix);
    }
    found
}

/// Linux `ip -o -4 addr show`.
pub fn parse_ip_addr(out: &str) -> Vec<Interface> {
    let mut found = Vec::new();
    for line in out.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some(i) = words.iter().position(|w| *w == "inet") else {
            continue;
        };
        let (Some(name), Some(cidr)) = (words.get(1), words.get(i + 1)) else {
            continue;
        };
        let mut parts = cidr.split('/');
        let addr = parts.next().and_then(|a| a.parse().ok());
        let prefix = parts.next().and_then(|p| p.parse().ok()).unwrap_or(32);
        if let Some(addr) = addr {
            push(&mut found, name, addr, prefix);
        }
    }
    found
}

fn push(found: &mut Vec<Interface>, name: &str, addr: Ipv4Addr, prefix: u8) {
    if !addr.is_loopback() && !name.is_empty() {
        found.push(Interface {
            name: name.to_string(),
            addr,
            prefix,
        });
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub interface: String,
    pub cloned: bool,
    pub network: Option<String>,
}

/// macOS `route -n get <addr>`.
pub fn parse_route_get(out: &str) -> Option<Route> {
    let interface = out
        .lines()
        .find_map(|line| line.trim().strip_prefix("interface:"))?
        .trim()
        .to_string();
    let cloned = out
        .lines()
        .filter_map(|line| line.trim().strip_prefix("flags:"))
        .any(|flags| flags.contains("WASCLONED"));
    Some(Route {
        interface,
        cloned,
        network: None,
    })
}

/// Linux `ip route get <addr>`.
pub fn parse_ip_route_get(out: &str) -> Option<Route> {
    let words: Vec<&str> = out.split_whitespace().collect();
    let i = words.iter().position(|w| *w == "dev")?;
    words.get(i + 1).map(|name| Route {
        interface: name.to_string(),
        cloned: false,
        network: None,
    })
}

/// The summary lines of `ping -q` on macOS and Linux.
pub fn parse_ping(out: &str) -> Option<Rtt> {
    let mut sent = None;
    let mut received = None;
    let mut times = None;
    for line in out.lines() {
        if line.contains("packets transmitted") {
            let numbers: Vec<u32> = line
                .split(|c: char| !c.is_ascii_digit())
                .filter_map(|w| w.parse().ok())
                .collect();
            sent = numbers.first().copied();
            received = numbers.get(1).copied();
        }
        if let Some((_, values)) = line.split_once(" = ") {
            let values: Vec<f64> = values
                .trim_end_matches(" ms")
                .split('/')
                .filter_map(|v| v.trim().parse().ok())
                .collect();
            if values.len() >= 3 {
                times = Some((values[1], values[2]));
            }
        }
    }
    let (avg_ms, max_ms) = times?;
    Some(Rtt {
        sent: sent?,
        received: received?,
        avg_ms,
        max_ms,
    })
}

/// Builds a peer's links. `route` answers the interface the kernel uses for an
/// address; `ping` measures it, bound to a local address when one is given.
/// The route is read before any bound traffic, because on macOS a bound ping
/// leaves a cloned host route that hides the misroute for about 20 minutes.
pub fn links(
    name: &str,
    peer_addrs: &[Ipv4Addr],
    interfaces: &[Interface],
    route: impl Fn(Ipv4Addr, &Interface) -> Option<Route>,
    ping: impl Fn(Ipv4Addr, Option<Ipv4Addr>) -> Option<Rtt>,
) -> Peer {
    let mut found = Vec::new();
    let mut no_link = Vec::new();
    let mut routes = Vec::new();
    for &peer in peer_addrs {
        match interfaces.iter().find(|i| i.holds(peer)) {
            Some(interface) => routes.push((interface, peer, route(peer, interface))),
            None => no_link.push(peer),
        }
    }
    for (interface, peer, route) in routes {
        let network_route_interface = route.as_ref().and_then(|route| route.network.clone());
        let route_interface = route.map(|route| route.interface);
        let on = |name: &Option<String>| name.as_deref() == Some(interface.name.as_str());
        let routed_on_link = on(&route_interface)
            && network_route_interface
                .as_ref()
                .is_none_or(|_| on(&network_route_interface));
        let unbound = ping(peer, None);
        let bound = ping(peer, Some(interface.addr));
        found.push(Link {
            kind: interface.kind(),
            interface: interface.name.clone(),
            local: interface.addr,
            peer,
            route_interface,
            network_route_interface,
            routed_on_link,
            unbound,
            bound,
        });
    }
    Peer {
        name: name.to_string(),
        links: found,
        no_link,
    }
}

pub fn local_interfaces() -> Vec<Interface> {
    if cfg!(target_os = "macos") {
        hardware::run_probe("ifconfig", &[])
            .map(|out| parse_ifconfig(&out))
            .unwrap_or_default()
    } else {
        hardware::run_probe("ip", &["-o", "-4", "addr", "show"])
            .map(|out| parse_ip_addr(&out))
            .unwrap_or_default()
    }
}

pub fn kernel_route(peer: Ipv4Addr, local: &Interface) -> Option<Route> {
    let peer = peer.to_string();
    if cfg!(target_os = "macos") {
        let mut route = hardware::run_probe("route", &["-n", "get", &peer])
            .and_then(|out| parse_route_get(&out))?;
        if route.cloned && local.prefix < 32 {
            let mask = u32::MAX
                .checked_shl(32 - u32::from(local.prefix))
                .unwrap_or(0);
            let net = format!(
                "{}/{}",
                Ipv4Addr::from(u32::from(local.addr) & mask),
                local.prefix
            );
            route.network = hardware::run_probe("route", &["-n", "get", "-net", &net])
                .and_then(|out| parse_route_get(&out))
                .map(|net| net.interface);
        }
        Some(route)
    } else {
        hardware::run_probe("ip", &["route", "get", &peer]).and_then(|out| parse_ip_route_get(&out))
    }
}

pub fn ping(peer: Ipv4Addr, from: Option<Ipv4Addr>, count: u32) -> Option<Rtt> {
    let count = count.to_string();
    let peer = peer.to_string();
    let from = from.map(|addr| addr.to_string());
    let bind = if cfg!(target_os = "macos") {
        "-S"
    } else {
        "-I"
    };
    let mut args = vec!["-c", count.as_str(), "-i", "0.2", "-q"];
    if let Some(from) = &from {
        args.extend([bind, from.as_str()]);
    }
    args.push(peer.as_str());
    hardware::run_probe("ping", &args).and_then(|out| parse_ping(&out))
}

/// Probes every peer from this machine. Blocking; call from a blocking task.
pub fn probe(peers: &[(String, Vec<Ipv4Addr>)], count: u32) -> Topology {
    let interfaces = local_interfaces();
    Topology {
        schema: SCHEMA,
        probed_at: crate::util::rfc3339(crate::util::unix_now()),
        machine: hardware::detect(),
        peers: peers
            .iter()
            .map(|(name, addrs)| {
                links(name, addrs, &interfaces, kernel_route, |peer, from| {
                    ping(peer, from, count)
                })
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IFCONFIG: &str = "lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST> mtu 16384
\tinet 127.0.0.1 netmask 0xff000000
bridge0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
\tinet 169.254.87.94 netmask 0xffff0000 broadcast 169.254.255.255
en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
\tinet 10.0.0.89 netmask 0xffffff00 broadcast 10.0.0.255
utun14: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1280
\tinet 100.107.206.73 --> 100.107.206.73 netmask 0xffffffff
";

    const IP_ADDR: &str = "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever
3: wlp0s20f3    inet 10.0.0.77/24 brd 10.0.0.255 scope global dynamic noprefixroute wlp0s20f3\\       valid_lft 82474sec
59: tailscale0    inet 100.104.23.40/32 scope global tailscale0\\       valid_lft forever preferred_lft forever
1425: thunderbolt0    inet 169.254.222.99/16 brd 169.254.255.255 scope link noprefixroute thunderbolt0\\       valid_lft forever
";

    const MAC_PING: &str = "--- 169.254.222.99 ping statistics ---
20 packets transmitted, 20 packets received, 0.0% packet loss
round-trip min/avg/max/stddev = 0.220/0.330/0.390/0.050 ms
";

    const LINUX_PING: &str = "--- 169.254.87.94 ping statistics ---
20 packets transmitted, 19 received, 5% packet loss, time 3810ms
rtt min/avg/max/mdev = 0.301/0.412/0.630/0.081 ms
";

    fn addr(text: &str) -> Ipv4Addr {
        text.parse().unwrap()
    }

    #[test]
    fn macos_interfaces_skip_loopback_and_read_masks() {
        let found = parse_ifconfig(IFCONFIG);
        let names: Vec<_> = found
            .iter()
            .map(|i| (i.name.as_str(), i.prefix, i.kind()))
            .collect();
        assert_eq!(
            names,
            [
                ("bridge0", 16, LinkKind::Cable),
                ("en0", 24, LinkKind::Lan),
                ("utun14", 32, LinkKind::Tailnet),
            ]
        );
    }

    #[test]
    fn linux_interfaces_read_cidr() {
        let found = parse_ip_addr(IP_ADDR);
        let names: Vec<_> = found
            .iter()
            .map(|i| (i.name.as_str(), i.prefix, i.kind()))
            .collect();
        assert_eq!(
            names,
            [
                ("wlp0s20f3", 24, LinkKind::Lan),
                ("tailscale0", 32, LinkKind::Tailnet),
                ("thunderbolt0", 16, LinkKind::Cable),
            ]
        );
    }

    #[test]
    fn routes_and_pings_parse_on_both_systems() {
        let mac = "   route to: 169.254.222.99\ndestination: 169.254.0.0\n       mask: 255.255.0.0\n  interface: en0\n";
        assert_eq!(
            parse_route_get(mac),
            Some(Route {
                interface: "en0".into(),
                cloned: false,
                network: None,
            })
        );
        let cloned = "   route to: 169.254.222.99\ndestination: 169.254.222.99\n  interface: bridge0\n      flags: <UP,HOST,DONE,LLINFO,STATIC,b016,WASCLONED>\n";
        assert_eq!(
            parse_route_get(cloned),
            Some(Route {
                interface: "bridge0".into(),
                cloned: true,
                network: None,
            })
        );
        let linux = "169.254.87.94 dev thunderbolt0 src 169.254.222.99 uid 1000 \n    cache \n";
        assert_eq!(
            parse_ip_route_get(linux),
            Some(Route {
                interface: "thunderbolt0".into(),
                cloned: false,
                network: None,
            })
        );
        assert_eq!(
            parse_ping(MAC_PING),
            Some(Rtt {
                sent: 20,
                received: 20,
                avg_ms: 0.33,
                max_ms: 0.39
            })
        );
        assert_eq!(
            parse_ping(LINUX_PING),
            Some(Rtt {
                sent: 20,
                received: 19,
                avg_ms: 0.412,
                max_ms: 0.63
            })
        );
        assert_eq!(parse_ping("ping: sendto: No route to host\n"), None);
    }

    #[test]
    fn a_cable_whose_network_route_is_wifi_is_reported_even_under_a_cloned_host_route() {
        let interfaces = parse_ifconfig(IFCONFIG);
        let order = std::cell::RefCell::new(Vec::new());
        let peer = links(
            "parish1",
            &[
                addr("169.254.222.99"),
                addr("100.104.23.40"),
                addr("192.168.9.9"),
            ],
            &interfaces,
            |peer, _| {
                order.borrow_mut().push(format!("route {peer}"));
                Some(if kind_of(peer) == LinkKind::Cable {
                    Route {
                        interface: "bridge0".into(),
                        cloned: true,
                        network: Some("en0".into()),
                    }
                } else {
                    Route {
                        interface: "utun14".into(),
                        cloned: false,
                        network: None,
                    }
                })
            },
            |peer, from| {
                order
                    .borrow_mut()
                    .push(format!("ping {peer} {}", from.is_some()));
                let avg_ms = match (kind_of(peer), from) {
                    (LinkKind::Cable, Some(_)) => 0.33,
                    (LinkKind::Cable, None) => 32.8,
                    _ => 14.3,
                };
                Some(Rtt {
                    sent: 20,
                    received: 20,
                    avg_ms,
                    max_ms: avg_ms,
                })
            },
        );
        let cable = &peer.links[0];
        assert_eq!(
            (cable.kind, cable.interface.as_str()),
            (LinkKind::Cable, "bridge0")
        );
        assert_eq!(cable.route_interface.as_deref(), Some("bridge0"));
        assert_eq!(cable.network_route_interface.as_deref(), Some("en0"));
        assert!(!cable.routed_on_link);
        assert!(cable.bound.unwrap().avg_ms < 1.0);
        assert!(cable.unbound.unwrap().avg_ms > 10.0);
        let tailnet = &peer.links[1];
        assert_eq!(
            (tailnet.kind, tailnet.interface.as_str()),
            (LinkKind::Tailnet, "utun14")
        );
        assert!(tailnet.routed_on_link);
        assert_eq!(peer.no_link, [addr("192.168.9.9")]);
        let order = order.into_inner();
        let first_ping = order
            .iter()
            .position(|step| step.starts_with("ping"))
            .unwrap();
        assert!(
            order[..first_ping]
                .iter()
                .filter(|step| step.starts_with("route"))
                .count()
                == 2
        );
    }
}
