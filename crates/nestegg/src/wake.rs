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

/// Progress reported by [`ensure_available`] while it wakes the PBS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeEvent<'a> {
    /// Normal progress ("sending Wake-on-LAN packet").
    Info(&'a str),
    /// The magic packet could not be sent. The wait goes on; a backup run
    /// records this as a warning. Reported at most once per call.
    SendFailed(&'a str),
    /// A packet went out after an earlier attempt had failed.
    SendRecovered,
    /// A magic packet went out (every time one does). Nothing for the log:
    /// it lets a caller know that a wake attempt really happened.
    PacketSent,
}

impl std::fmt::Display for WakeEvent<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WakeEvent::Info(m) => f.write_str(m),
            WakeEvent::SendFailed(m) => write!(f, "warning: {m}"),
            WakeEvent::SendRecovered => f.write_str("Wake-on-LAN packet sent"),
            WakeEvent::PacketSent => f.write_str("Wake-on-LAN packet went out"),
        }
    }
}

/// How often the PBS is probed while waiting for it to wake up.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Pause before the magic packet is repeated while waiting: 10 s after the
/// first packet, then doubling up to one minute (10, 20, 40, 60, 60, ...).
/// A lost first packet (UDP, or a network card that was not ready yet) is
/// made up for quickly, and a server that is switched off is not flooded.
const RESEND_FIRST: Duration = Duration::from_secs(10);
const RESEND_MAX: Duration = Duration::from_secs(60);

/// Pause before resend number `resends_so_far + 1`.
fn resend_pause(resends_so_far: u32) -> Duration {
    RESEND_FIRST
        .saturating_mul(1u32 << resends_so_far.min(8))
        .min(RESEND_MAX)
}

/// Check the PBS; if it is down and Wake-on-LAN is configured, wake it and
/// poll until it answers or the timeout expires.
///
/// A magic packet that cannot be sent (for example `Network is unreachable`
/// on a laptop that is offline or still reconnecting after a resume) is not
/// an error: it is reported once as [`WakeEvent::SendFailed`], the packet is
/// retried at every poll until it goes out, and after the timeout the result
/// is [`Availability::Unreachable`], the same as for a PBS that does not
/// wake up.
pub fn ensure_available(config: &Config, on_event: impl FnMut(WakeEvent)) -> Result<Availability> {
    ensure_available_with(config, send_magic_packet, POLL_INTERVAL, 1, on_event)
}

fn ensure_available_with(
    config: &Config,
    mut send: impl FnMut(&WakeConfig) -> Result<()>,
    poll_interval: Duration,
    // Divides the resend pauses (tests run the schedule in milliseconds).
    speedup: u32,
    mut on_event: impl FnMut(WakeEvent),
) -> Result<Availability> {
    let probe = Duration::from_secs(3);
    if is_reachable(config, probe) {
        return Ok(Availability::Reachable);
    }
    let Some(wake) = config.active_wake() else {
        return Ok(Availability::Unreachable);
    };
    on_event(WakeEvent::Info(
        "PBS not reachable, sending Wake-on-LAN packet",
    ));
    let mut sender = PacketSender::default();
    let mut last_sent = sender
        .send(wake, &mut send, &mut on_event)
        .then(Instant::now);
    let mut resends = 0u32;
    let start = Instant::now();
    let deadline = Duration::from_secs(wake.timeout_seconds);
    // Sleep in short steps so a cancel request is noticed quickly.
    let step = poll_interval / 10;
    while start.elapsed() < deadline {
        for _ in 0..10 {
            if crate::cancel::requested() {
                return Ok(Availability::Unreachable);
            }
            std::thread::sleep(step);
        }
        if is_reachable(config, probe) {
            return Ok(Availability::ReachableAfterWake {
                waited_seconds: start.elapsed().as_secs(),
            });
        }
        match last_sent {
            // No packet went out yet, or the last resend failed: try again at
            // every poll so it leaves as soon as the network is back.
            None => {
                if sender.send(wake, &mut send, &mut on_event) {
                    last_sent = Some(Instant::now());
                }
            }
            Some(sent) if sent.elapsed() > resend_pause(resends) / speedup => {
                on_event(WakeEvent::Info(
                    "still waiting, sending Wake-on-LAN packet again",
                ));
                resends += 1;
                last_sent = sender
                    .send(wake, &mut send, &mut on_event)
                    .then(Instant::now);
            }
            Some(_) => {}
        }
    }
    Ok(Availability::Unreachable)
}

/// Sends the magic packet and turns a failure into one clear warning instead
/// of an error that would abort the run.
#[derive(Default)]
struct PacketSender {
    warned: bool,
    failing: bool,
}

impl PacketSender {
    /// Returns true when the packet went out.
    fn send(
        &mut self,
        wake: &WakeConfig,
        send: &mut impl FnMut(&WakeConfig) -> Result<()>,
        on_event: &mut impl FnMut(WakeEvent),
    ) -> bool {
        match send(wake) {
            Ok(()) => {
                on_event(WakeEvent::PacketSent);
                if self.failing {
                    on_event(WakeEvent::SendRecovered);
                }
                self.failing = false;
                true
            }
            Err(e) => {
                if !self.warned {
                    on_event(WakeEvent::SendFailed(&format!(
                        "could not send the Wake-on-LAN packet that wakes the backup server \
                         ({}). Is this computer connected to the network? Waiting up to \
                         {} s for the backup server and trying again.",
                        e.root_cause(),
                        wake.timeout_seconds
                    )));
                    self.warned = true;
                }
                self.failing = true;
                false
            }
        }
    }
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
        let closed = ClosedPort::reserve();
        assert!(!is_reachable(
            &config_for(closed.port),
            Duration::from_millis(300)
        ));
    }

    #[test]
    fn magic_packet_is_sent() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let wake = WakeConfig {
            mac: "00:00:5e:00:53:01".into(),
            broadcast: "127.0.0.1".parse().unwrap(),
            port: receiver.local_addr().unwrap().port(),
            timeout_seconds: 1,
            enabled: true,
        };
        send_magic_packet(&wake).unwrap();
        let mut buf = [0u8; 200];
        let (n, _) = receiver.recv_from(&mut buf).unwrap();
        assert_eq!(n, 102);
        assert_eq!(&buf[..102], &magic_packet([0, 0, 0x5e, 0, 0x53, 1])[..]);
    }

    #[test]
    fn unreachable_without_wake_config() {
        let closed = ClosedPort::reserve();
        let result = ensure_available(&config_for(closed.port), |_| {}).unwrap();
        assert_eq!(result, Availability::Unreachable);
    }

    /// A TCP port on 127.0.0.1 that refuses connections for as long as the
    /// value lives: a socket that is bound but never listens. Unlike "bind,
    /// read the port, close", no other process (tests running in parallel)
    /// can start listening on it in the meantime.
    struct ClosedPort {
        port: u16,
        _socket: std::os::fd::OwnedFd,
    }

    impl ClosedPort {
        fn reserve() -> ClosedPort {
            use nix::libc;
            use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
            let size = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
            // SAFETY: plain socket calls on a descriptor owned right away,
            // with a zero-initialised address of the size passed along.
            unsafe {
                let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
                assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
                let socket = OwnedFd::from_raw_fd(fd);
                let mut addr: libc::sockaddr_in = std::mem::zeroed();
                addr.sin_family = libc::AF_INET as libc::sa_family_t;
                addr.sin_addr.s_addr = u32::from(std::net::Ipv4Addr::LOCALHOST).to_be();
                let addr_ptr = std::ptr::addr_of_mut!(addr).cast::<libc::sockaddr>();
                assert_eq!(libc::bind(socket.as_raw_fd(), addr_ptr, size), 0);
                let mut len = size;
                assert_eq!(libc::getsockname(socket.as_raw_fd(), addr_ptr, &mut len), 0);
                ClosedPort {
                    port: u16::from_be(addr.sin_port),
                    _socket: socket,
                }
            }
        }
    }

    fn wake_config(timeout_seconds: u64) -> WakeConfig {
        WakeConfig {
            mac: "00:00:5e:00:53:01".into(),
            broadcast: "255.255.255.255".parse().unwrap(),
            port: 9,
            timeout_seconds,
            enabled: true,
        }
    }

    fn offline() -> anyhow::Error {
        anyhow::Error::new(std::io::Error::from_raw_os_error(101))
            .context("cannot send Wake-on-LAN packet")
    }

    #[derive(Debug, PartialEq)]
    enum Seen {
        Info(String),
        Warning(String),
    }

    fn record(seen: &mut Vec<Seen>) -> impl FnMut(WakeEvent) + '_ {
        move |e| {
            seen.push(match e {
                WakeEvent::Info(m) => Seen::Info(m.into()),
                WakeEvent::SendFailed(m) => Seen::Warning(m.into()),
                WakeEvent::SendRecovered => Seen::Info(e.to_string()),
                // Not a log line; counted by the test that is about it.
                WakeEvent::PacketSent => return,
            })
        }
    }

    #[test]
    fn packet_sent_is_reported_for_each_packet_that_went_out_and_only_then() {
        let closed = ClosedPort::reserve();
        let mut config = config_for(closed.port);
        config.wake = Some(wake_config(1));
        type Send = Box<dyn FnMut(&WakeConfig) -> Result<()>>;
        let count = |mut send: Send| {
            let (mut out, mut reported) = (0u32, 0u32);
            let _ = ensure_available_with(
                &config,
                |w| {
                    let r = send(w);
                    if r.is_ok() {
                        out += 1;
                    }
                    r
                },
                Duration::from_millis(50),
                1,
                |e| {
                    if e == WakeEvent::PacketSent {
                        reported += 1;
                    }
                },
            )
            .unwrap();
            (out, reported)
        };
        // Nothing ever goes out: nothing is reported.
        assert_eq!(count(Box::new(|_| Err(offline()))), (0, 0));
        // Every send works: each one is reported.
        let (out, reported) = count(Box::new(|_| Ok(())));
        assert!(out >= 1);
        assert_eq!(out, reported);
        // The first sends fail, later ones go out.
        let mut n = 0;
        let (out, reported) = count(Box::new(move |_| {
            n += 1;
            if n <= 2 { Err(offline()) } else { Ok(()) }
        }));
        assert!(out >= 1, "a later send went out");
        assert_eq!(out, reported);
    }

    #[test]
    fn send_failure_is_a_warning_and_ends_unreachable() {
        let closed = ClosedPort::reserve();
        let mut config = config_for(closed.port);
        config.wake = Some(wake_config(1));
        let mut attempts = 0;
        let mut seen = Vec::new();
        let result = ensure_available_with(
            &config,
            |_| {
                attempts += 1;
                Err(offline())
            },
            Duration::from_millis(50),
            1,
            record(&mut seen),
        )
        .unwrap();
        assert_eq!(result, Availability::Unreachable);
        assert!(attempts > 1, "the packet is retried, got {attempts}");
        let warnings: Vec<_> = seen
            .iter()
            .filter_map(|s| match s {
                Seen::Warning(m) => Some(m),
                Seen::Info(_) => None,
            })
            .collect();
        assert_eq!(warnings.len(), 1, "warned exactly once: {seen:?}");
        assert!(
            warnings[0].contains("Network is unreachable")
                && warnings[0].contains("connected to the network"),
            "{}",
            warnings[0]
        );
    }

    #[test]
    fn packet_goes_out_once_the_network_is_back() {
        let mut closed = Some(ClosedPort::reserve());
        let port = closed.as_ref().unwrap().port;
        let mut config = config_for(port);
        config.wake = Some(wake_config(10));
        let mut attempts = 0;
        // The second attempt "works" and the PBS wakes up.
        let mut pbs = None;
        let mut seen = Vec::new();
        let result = ensure_available_with(
            &config,
            |_| {
                attempts += 1;
                if attempts == 1 {
                    return Err(offline());
                }
                closed.take();
                pbs = Some(TcpListener::bind(("127.0.0.1", port)).unwrap());
                Ok(())
            },
            Duration::from_millis(50),
            1,
            record(&mut seen),
        )
        .unwrap();
        assert!(
            matches!(result, Availability::ReachableAfterWake { .. }),
            "{result:?}"
        );
        assert_eq!(attempts, 2);
        assert!(seen.contains(&Seen::Info("Wake-on-LAN packet sent".into())));
        assert_eq!(
            seen.iter()
                .filter(|s| matches!(s, Seen::Warning(_)))
                .count(),
            1
        );
    }

    #[test]
    fn real_send_failure_does_not_abort() {
        // Linux refuses UDP datagrams to port 0 with EINVAL, a deterministic
        // stand-in for "Network is unreachable".
        let closed = ClosedPort::reserve();
        let mut config = config_for(closed.port);
        let mut wake = wake_config(0);
        wake.port = 0;
        config.wake = Some(wake);
        let mut seen = Vec::new();
        let result = ensure_available(&config, record(&mut seen)).unwrap();
        assert_eq!(result, Availability::Unreachable);
        assert!(
            seen.iter().any(|s| matches!(s, Seen::Warning(_))),
            "{seen:?}"
        );
    }

    #[test]
    fn switched_off_wake_sends_nothing() {
        let closed = ClosedPort::reserve();
        let mut config = config_for(closed.port);
        let mut wake = wake_config(10);
        wake.enabled = false;
        config.wake = Some(wake);
        let mut attempts = 0;
        let mut seen = Vec::new();
        let start = Instant::now();
        let result = ensure_available_with(
            &config,
            |_| {
                attempts += 1;
                Ok(())
            },
            Duration::from_millis(10),
            1,
            record(&mut seen),
        )
        .unwrap();
        assert_eq!(result, Availability::Unreachable);
        assert_eq!(attempts, 0);
        assert!(seen.is_empty(), "{seen:?}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn resend_pause_grows_up_to_a_minute() {
        let secs: Vec<u64> = (0..6).map(|n| resend_pause(n).as_secs()).collect();
        assert_eq!(secs, [10, 20, 40, 60, 60, 60]);
        assert_eq!(resend_pause(u32::MAX), RESEND_MAX);
    }

    #[test]
    fn packet_is_resent_with_growing_pauses_until_the_pbs_answers() {
        let mut closed = Some(ClosedPort::reserve());
        let port = closed.as_ref().unwrap().port;
        let mut config = config_for(port);
        config.wake = Some(wake_config(30));
        // Pauses of 10, 20 and 40 ms instead of seconds; the "PBS" starts
        // listening when the fourth packet arrives.
        let mut sent_at = Vec::new();
        let mut pbs = None;
        let mut seen = Vec::new();
        let result = ensure_available_with(
            &config,
            |_| {
                sent_at.push(Instant::now());
                if sent_at.len() == 4 {
                    closed.take();
                    pbs = Some(TcpListener::bind(("127.0.0.1", port)).unwrap());
                }
                Ok(())
            },
            Duration::from_millis(5),
            1000,
            record(&mut seen),
        )
        .unwrap();
        assert!(
            matches!(result, Availability::ReachableAfterWake { .. }),
            "{result:?}"
        );
        assert_eq!(sent_at.len(), 4, "one packet plus three resends");
        let gaps: Vec<Duration> = sent_at.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(gaps[0] >= Duration::from_millis(10), "{gaps:?}");
        assert!(gaps[1] >= Duration::from_millis(20), "{gaps:?}");
        assert!(gaps[2] >= Duration::from_millis(40), "{gaps:?}");
        assert_eq!(
            seen.iter()
                .filter(
                    |s| **s == Seen::Info("still waiting, sending Wake-on-LAN packet again".into())
                )
                .count(),
            3
        );
    }

    #[test]
    fn warning_display_is_prefixed() {
        assert_eq!(WakeEvent::Info("x").to_string(), "x");
        assert_eq!(WakeEvent::SendFailed("x").to_string(), "warning: x");
        assert_eq!(
            WakeEvent::SendRecovered.to_string(),
            "Wake-on-LAN packet sent"
        );
    }
}
