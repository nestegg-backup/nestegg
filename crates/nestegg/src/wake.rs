//! PBS reachability check and Wake-on-LAN.

use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config::{Config, WakeConfig, parse_mac};

/// The PBS API is considered reachable when its TCP port accepts a
/// connection. No credentials are sent.
pub fn is_reachable(config: &Config, timeout: Duration) -> bool {
    let host = config
        .pbs
        .server
        .trim_start_matches('[')
        .trim_end_matches(']');
    let Ok(addrs) = (host, config.pbs.port).to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|addr: SocketAddr| TcpStream::connect_timeout(&addr, timeout).is_ok())
}

/// Build the 102-byte magic packet: 6 × 0xFF followed by 16 × MAC.
pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut packet = [0xFFu8; 102];
    for i in 0..16 {
        packet[6 + i * 6..12 + i * 6].copy_from_slice(&mac);
    }
    packet
}

pub fn send_magic_packet(wake: &WakeConfig) -> Result<()> {
    let mac = parse_mac(&wake.mac)?;
    let socket = UdpSocket::bind(("0.0.0.0", 0)).context("cannot open UDP socket")?;
    socket.set_broadcast(true)?;
    let packet = magic_packet(mac);
    // Send a few times; UDP may drop packets.
    for _ in 0..3 {
        socket
            .send_to(&packet, (wake.broadcast, wake.port))
            .context("cannot send Wake-on-LAN packet")?;
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Availability {
    Reachable,
    ReachableAfterWake { waited_seconds: u64 },
    Unreachable,
}

/// Check the PBS; if it is down and Wake-on-LAN is configured, wake it and
/// poll until it answers or the timeout expires.
pub fn ensure_available(config: &Config, mut on_wake: impl FnMut(&str)) -> Result<Availability> {
    let probe = Duration::from_secs(3);
    if is_reachable(config, probe) {
        return Ok(Availability::Reachable);
    }
    let Some(wake) = &config.wake else {
        return Ok(Availability::Unreachable);
    };
    on_wake("PBS not reachable, sending Wake-on-LAN packet");
    send_magic_packet(wake)?;
    let start = Instant::now();
    let deadline = Duration::from_secs(wake.timeout_seconds);
    let mut last_packet = Instant::now();
    while start.elapsed() < deadline {
        std::thread::sleep(Duration::from_secs(5));
        if is_reachable(config, probe) {
            return Ok(Availability::ReachableAfterWake {
                waited_seconds: start.elapsed().as_secs(),
            });
        }
        if last_packet.elapsed() > Duration::from_secs(60) {
            on_wake("still waiting, sending Wake-on-LAN packet again");
            send_magic_packet(wake)?;
            last_packet = Instant::now();
        }
    }
    Ok(Availability::Unreachable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn magic_packet_layout() {
        let mac = [0x00, 0x00, 0x5e, 0x00, 0x53, 0x01];
        let p = magic_packet(mac);
        assert!(p[..6].iter().all(|b| *b == 0xFF));
        for i in 0..16 {
            assert_eq!(&p[6 + i * 6..12 + i * 6], &mac);
        }
    }

    fn config_for(port: u16) -> Config {
        let mut c = crate::config::example("127.0.0.1", "backups", "backup@pbs!desktop");
        c.pbs.port = port;
        c
    }

    #[test]
    fn reachability() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(is_reachable(&config_for(port), Duration::from_secs(1)));
        drop(listener);
        assert!(!is_reachable(&config_for(port), Duration::from_millis(300)));
    }

    #[test]
    fn unreachable_without_wake_config() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let result = ensure_available(&config_for(port), |_| {}).unwrap();
        assert_eq!(result, Availability::Unreachable);
    }
}
