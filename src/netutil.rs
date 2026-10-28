//! Network helpers: interface addresses, the static ARP entry, and binding
//! receiving sockets to the diode NIC.

use crate::config::Network;
use std::collections::BTreeMap;
use std::ffi::CStr;
use std::net::Ipv4Addr;
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};
use std::time::Duration;

/// {interface: first IPv4 address} for every interface that has one.
pub fn interface_addresses() -> BTreeMap<String, Ipv4Addr> {
    let mut out = BTreeMap::new();
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs allocates a list we free below.
    if unsafe { libc::getifaddrs(&mut ifap) } != 0 {
        return out;
    }
    let mut cur = ifap;
    while !cur.is_null() {
        // SAFETY: cur points into the list returned by getifaddrs.
        let ifa = unsafe { &*cur };
        if !ifa.ifa_addr.is_null()
            && i32::from(unsafe { (*ifa.ifa_addr).sa_family }) == libc::AF_INET
        {
            // SAFETY: AF_INET means it is a sockaddr_in.
            let sin = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
            let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            // SAFETY: ifa_name is a NUL-terminated C string.
            let name = unsafe { CStr::from_ptr(ifa.ifa_name) }
                .to_string_lossy()
                .into_owned();
            out.entry(name).or_insert(ip);
        }
        cur = ifa.ifa_next;
    }
    // SAFETY: freeing what getifaddrs allocated.
    unsafe { libc::freeifaddrs(ifap) };
    out
}

/// Pin the output box's MAC.  Returns true on success.
///
/// Nothing can answer ARP through a one-way link, so without this entry the
/// kernel cannot resolve the destination MAC and drops every outbound
/// packet locally -- lidi still "sends" and the receiver never sees a
/// thing.  Uses iproute2.
pub fn set_static_arp(net: &Network) -> bool {
    let Some(mac) = &net.out_mac else {
        log::warn!("no dyode_out.mac in config: skipping static ARP entry");
        return false;
    };
    let out_ip = net.out_ip.to_string();
    let res = Command::new("ip")
        .args([
            "neigh",
            "replace",
            &out_ip,
            "lladdr",
            mac,
            "dev",
            &net.in_interface,
            "nud",
            "permanent",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match res {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log::error!("'ip' command not found: install iproute2");
            false
        }
        Err(e) => {
            log::error!("cannot run 'ip neigh': {e}");
            false
        }
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            log::error!(
                "static ARP failed (run as root? is {} up?): {}",
                net.in_interface,
                String::from_utf8_lossy(&o.stderr).trim()
            );
            false
        }
    }
}

/// Re-assert the static ARP entry for as long as DYODE runs.  It is lost
/// whenever the interface goes down and comes back.
pub fn run_arp_keeper(net: &Network) -> Result<(), String> {
    log::info!(
        "ARP keeper: pinning {} -> {} on {} every {}s",
        net.out_ip,
        net.out_mac.as_deref().unwrap_or("?"),
        net.in_interface,
        net.arp_interval
    );
    let mut healthy: Option<bool> = None;
    loop {
        let ok = set_static_arp(net);
        crate::stats::set_arp_ok(ok);
        if Some(ok) != healthy {
            if ok {
                log::info!(
                    "static ARP entry in place: {} -> {} on {}",
                    net.out_ip,
                    net.out_mac.as_deref().unwrap_or("?"),
                    net.in_interface
                );
            } else {
                log::error!(
                    "static ARP entry is NOT in place; nothing this box sends can reach the output side"
                );
            }
            healthy = Some(ok);
        }
        std::thread::sleep(Duration::from_secs_f64(net.arp_interval));
    }
}

/// Bind a socket to an interface (SO_BINDTODEVICE).
pub fn bind_to_device(sock: &impl AsRawFd, iface: &str) -> std::io::Result<()> {
    let name = iface.as_bytes();
    // SAFETY: valid fd, pointer and length.
    let rc = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            name.as_ptr().cast(),
            name.len() as libc::socklen_t,
        )
    };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Set SO_RCVBUF (best effort).
pub fn set_rcvbuf(sock: &impl AsRawFd, bytes: i32) {
    // SAFETY: valid fd and an i32 option value.
    unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            (&bytes as *const i32).cast(),
            std::mem::size_of::<i32>() as libc::socklen_t,
        );
    }
}

/// Enable TCP keepalive (best effort).
pub fn set_keepalive(sock: &impl AsRawFd) {
    let one: libc::c_int = 1;
    // SAFETY: valid fd and option value.
    unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_KEEPALIVE,
            (&one as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

/// Where a viewer page should listen when nothing is configured: the first
/// address that is neither loopback nor on the diode NIC (the IT side).
pub fn default_it_address(net: &Network) -> Ipv4Addr {
    for (name, ip) in interface_addresses() {
        if name == net.out_interface || ip == net.out_ip || ip.is_loopback() {
            continue;
        }
        return ip;
    }
    log::warn!(
        "no non-diode address found; listening on every address (set http_bind to choose one)"
    );
    Ipv4Addr::UNSPECIFIED
}
