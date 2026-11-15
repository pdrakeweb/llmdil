//! The Admin API listener's bind choice (auth-plan §5, "Bind
//! choices"): loopback only, all interfaces (the LAN + Tailscale
//! default every existing configuration has), or one specific
//! interface address.
//!
//! Resolution happens when the listener starts. A specific address
//! that is not assigned to this machine at that moment — a DHCP
//! change, an adapter that is down, a config copied from another
//! machine — falls back to **loopback**, never to all interfaces, and
//! the reason is reported for the Remote Access status so the
//! narrower surface is never a surprise either.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use manager_config::{AdminApiBindMode, AdminApiConfig};

/// What the listener will bind, and why when it is not what the
/// configuration asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindPlan {
    /// The address to listen on.
    pub addr: IpAddr,
    /// Set when the configured choice could not be honoured and the
    /// listener fell back to loopback; the operator-facing reason.
    pub fallback: Option<String>,
}

impl BindPlan {
    fn exact(addr: IpAddr) -> Self {
        Self {
            addr,
            fallback: None,
        }
    }

    fn loopback_because(reason: String) -> Self {
        Self {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            fallback: Some(reason),
        }
    }
}

/// Resolve the configured bind choice. `is_local` answers whether an
/// address is currently assigned to this machine (injected so the
/// rule is testable; [`resolve_now`] passes [`address_is_local`]).
pub fn resolve(cfg: &AdminApiConfig, is_local: impl Fn(IpAddr) -> bool) -> BindPlan {
    match cfg.bind_mode {
        AdminApiBindMode::Loopback => BindPlan::exact(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        AdminApiBindMode::All => BindPlan::exact(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
        AdminApiBindMode::Interface => {
            let Some(raw) = cfg.bind_ip.as_deref() else {
                return BindPlan::loopback_because(
                    "no interface address is configured; listening on loopback only".to_string(),
                );
            };
            let ip: IpAddr = match raw.trim().parse() {
                Ok(ip) => ip,
                Err(_) => {
                    return BindPlan::loopback_because(format!(
                        "configured interface address {raw:?} is not an IP address; listening on loopback only"
                    ))
                }
            };
            // A wildcard would widen the surface to every interface;
            // config validation refuses it, and so does this.
            if ip.is_unspecified() || ip.is_multicast() {
                return BindPlan::loopback_because(format!(
                    "configured interface address {ip} is not a single interface; listening on loopback only"
                ));
            }
            if !is_local(ip) {
                return BindPlan::loopback_because(format!(
                    "configured interface address {ip} is not assigned to this machine; listening on loopback only"
                ));
            }
            BindPlan::exact(ip)
        }
    }
}

/// [`resolve`] against the machine's current addresses.
pub fn resolve_now(cfg: &AdminApiConfig) -> BindPlan {
    resolve(cfg, address_is_local)
}

/// Whether `ip` is assigned to this machine right now: the OS only
/// lets a socket bind an address one of its interfaces holds. Nothing
/// is sent and the socket is dropped at once.
pub fn address_is_local(ip: IpAddr) -> bool {
    std::net::UdpSocket::bind(SocketAddr::new(ip, 0)).is_ok()
}

/// The machine's current interface addresses the operator can pick
/// for a specific-interface bind: IPv4 unicast on adapters that are
/// up, excluding loopback (its own mode) and APIPA link-local
/// (169.254/16, never a deliberate choice). Windows: adapter
/// enumeration; elsewhere empty (the desktop app is Windows-only).
pub fn interface_addresses() -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = local_ipv4_addresses()
        .into_iter()
        .filter(|a| !a.is_loopback() && !a.is_link_local() && !a.is_unspecified())
        .map(IpAddr::V4)
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(windows)]
fn local_ipv4_addresses() -> Vec<Ipv4Addr> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

    const ERROR_BUFFER_OVERFLOW: u32 = 111;
    const NO_ERROR: u32 = 0;
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let family = AF_INET as u32;
    let mut size: u32 = 0;
    // SAFETY: a size probe with a null buffer, as documented.
    let probe = unsafe {
        GetAdaptersAddresses(
            family,
            flags,
            std::ptr::null(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if probe != ERROR_BUFFER_OVERFLOW || size == 0 {
        return Vec::new();
    }
    // u64 storage keeps the buffer aligned for IP_ADAPTER_ADDRESSES_LH.
    let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
    // SAFETY: the buffer holds at least `size` bytes.
    let rc = unsafe {
        GetAdaptersAddresses(
            family,
            flags,
            std::ptr::null(),
            buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
            &mut size,
        )
    };
    if rc != NO_ERROR {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut adapter = buffer.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    // SAFETY: the linked lists live inside `buffer`, filled by the
    // successful call above, and are only read while it is alive.
    unsafe {
        while !adapter.is_null() {
            if (*adapter).OperStatus == IfOperStatusUp {
                let mut unicast = (*adapter).FirstUnicastAddress;
                while !unicast.is_null() {
                    let sockaddr = (*unicast).Address.lpSockaddr;
                    if !sockaddr.is_null() && (*sockaddr).sa_family == AF_INET {
                        let sin = sockaddr as *const SOCKADDR_IN;
                        let raw = (*sin).sin_addr.S_un.S_addr;
                        out.push(Ipv4Addr::from(raw.to_ne_bytes()));
                    }
                    unicast = (*unicast).Next;
                }
            }
            adapter = (*adapter).Next;
        }
    }
    out
}

#[cfg(not(windows))]
fn local_ipv4_addresses() -> Vec<Ipv4Addr> {
    Vec::new()
}

/// The base URL clients use for a listener bound to `bind`: the
/// bound address itself for loopback or a specific interface; for
/// all interfaces the primary LAN address when known (`lan_ip`),
/// else loopback.
pub fn base_url(bind: IpAddr, port: u16, lan_ip: Option<IpAddr>) -> String {
    let host = if bind.is_unspecified() {
        lan_ip.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
    } else {
        bind
    };
    match host {
        IpAddr::V6(v6) => format!("https://[{v6}]:{port}"),
        IpAddr::V4(v4) => format!("https://{v4}:{port}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: AdminApiBindMode, ip: Option<&str>) -> AdminApiConfig {
        AdminApiConfig {
            enabled: true,
            port: 47821,
            bind_mode: mode,
            bind_ip: ip.map(str::to_string),
        }
    }

    const LAN: &str = "192.168.1.20";

    fn only_lan(ip: IpAddr) -> bool {
        ip == LAN.parse::<IpAddr>().unwrap()
    }

    #[test]
    fn existing_configs_keep_binding_all_interfaces() {
        let plan = resolve(&AdminApiConfig::default(), |_| false);
        assert_eq!(plan, BindPlan::exact("0.0.0.0".parse().unwrap()));
    }

    #[test]
    fn loopback_and_all_bind_what_they_say() {
        assert_eq!(
            resolve(&cfg(AdminApiBindMode::Loopback, Some(LAN)), only_lan),
            BindPlan::exact("127.0.0.1".parse().unwrap())
        );
        assert_eq!(
            resolve(&cfg(AdminApiBindMode::All, None), only_lan).addr,
            "0.0.0.0".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn present_interface_address_is_bound_exactly() {
        assert_eq!(
            resolve(&cfg(AdminApiBindMode::Interface, Some(LAN)), only_lan),
            BindPlan::exact(LAN.parse().unwrap())
        );
    }

    /// Never silently widen: every way the specific-interface choice
    /// can fail lands on loopback, with a reason.
    #[test]
    fn missing_or_bad_interface_address_falls_back_to_loopback() {
        for (ip, needle) in [
            (Some("10.9.9.9"), "not assigned to this machine"),
            (None, "no interface address"),
            (Some("not-an-ip"), "not an IP address"),
            (Some("0.0.0.0"), "not a single interface"),
            (Some("::"), "not a single interface"),
        ] {
            // Even an "everything is local" answer cannot widen a
            // wildcard: it is refused before the address check.
            if ip == Some("0.0.0.0") {
                let wide = resolve(&cfg(AdminApiBindMode::Interface, ip), |_| true);
                assert!(wide.addr.is_loopback(), "{wide:?}");
            }
            let plan = resolve(&cfg(AdminApiBindMode::Interface, ip), only_lan);
            assert_eq!(plan.addr, "127.0.0.1".parse::<IpAddr>().unwrap(), "{ip:?}");
            let why = plan.fallback.expect("a fallback carries its reason");
            assert!(why.contains(needle), "{ip:?}: {why}");
            assert!(why.contains("loopback"), "{why}");
        }
    }

    #[test]
    fn address_check_uses_the_os() {
        assert!(address_is_local("127.0.0.1".parse().unwrap()));
        // TEST-NET-1 (RFC 5737) is never assigned to a host.
        assert!(!address_is_local("192.0.2.123".parse().unwrap()));
    }

    #[test]
    fn interface_list_holds_only_pickable_addresses() {
        for ip in interface_addresses() {
            assert!(!ip.is_loopback() && !ip.is_unspecified(), "{ip}");
            assert!(address_is_local(ip), "{ip} is listed but not bindable");
        }
    }

    #[test]
    fn base_url_follows_the_bind() {
        let lan: IpAddr = LAN.parse().unwrap();
        assert_eq!(
            base_url("0.0.0.0".parse().unwrap(), 47821, Some(lan)),
            "https://192.168.1.20:47821"
        );
        assert_eq!(
            base_url("0.0.0.0".parse().unwrap(), 47821, None),
            "https://127.0.0.1:47821"
        );
        assert_eq!(
            base_url("127.0.0.1".parse().unwrap(), 47821, Some(lan)),
            "https://127.0.0.1:47821"
        );
        assert_eq!(
            base_url("10.0.0.5".parse().unwrap(), 1, Some(lan)),
            "https://10.0.0.5:1"
        );
        assert_eq!(
            base_url("fd00::1".parse().unwrap(), 2, None),
            "https://[fd00::1]:2"
        );
    }
}
