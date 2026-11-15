//! DHCPv4 client state machine.
//!
//! The state machine is pure: callers pass `Instant` values and receive
//! [`Action`] values. The daemon owns the socket and turns `Send` into a UDP
//! send and `Lease`/`Deconfigure` into netlink changes. This makes retry,
//! renew, rebind and expiry testable without root or a real network.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::dhcp::lease::Lease;
use crate::dhcp::packet::{DhcpError, MessageType, Packet};

/// The initial retransmission interval.
const FIRST_RETRY: Duration = Duration::from_secs(4);
/// The largest retransmission interval.
const MAX_RETRY: Duration = Duration::from_secs(64);

/// One action the daemon must perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Send this packet to the DHCP server (broadcast or unicast).
    Send(Packet),
    /// Apply this lease.
    Lease(Lease),
    /// Remove the previous lease's address, routes and DNS.
    Deconfigure,
}

/// The client's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// No lease; start with DISCOVER.
    Init,
    /// DISCOVER sent, waiting for OFFER.
    Selecting,
    /// REQUEST sent, waiting for ACK/NAK.
    Requesting {
        requested: Ipv4Addr,
        server: Ipv4Addr,
    },
    /// Lease is valid.
    Bound {
        lease: Lease,
        renew_at: Instant,
        rebind_at: Instant,
        expires_at: Instant,
    },
    /// Unicast renewal.
    Renewing {
        lease: Lease,
        rebind_at: Instant,
        expires_at: Instant,
    },
    /// Broadcast rebinding.
    Rebinding { lease: Lease, expires_at: Instant },
}

/// A DHCPv4 client for one interface.
#[derive(Debug, Clone)]
pub struct Client {
    /// Interface name.
    pub iface: String,
    /// Hardware address.
    pub mac: [u8; 6],
    /// Optional hostname (option 12).
    pub hostname: Option<String>,
    state: State,
    xid: u32,
    retry_at: Option<Instant>,
    attempts: u8,
    fallback_lease: u32,
}

impl Client {
    /// Build a client.
    pub fn new(iface: impl Into<String>, mac: [u8; 6]) -> Self {
        Client {
            iface: iface.into(),
            mac,
            hostname: None,
            state: State::Init,
            xid: random_xid(),
            retry_at: None,
            attempts: 0,
            fallback_lease: 3600,
        }
    }

    /// Set the DHCP hostname.
    pub fn with_hostname(mut self, hostname: Option<String>) -> Self {
        self.hostname = hostname;
        self
    }

    /// Set the lease duration used when the server omits option 51.
    pub fn with_fallback_lease(mut self, seconds: u32) -> Self {
        self.fallback_lease = seconds.max(1);
        self
    }

    /// The current state.
    pub fn state(&self) -> &State {
        &self.state
    }

    /// The current lease, if bound.
    pub fn lease(&self) -> Option<&Lease> {
        match &self.state {
            State::Bound { lease, .. }
            | State::Renewing { lease, .. }
            | State::Rebinding { lease, .. } => Some(lease),
            _ => None,
        }
    }

    /// The next instant this client needs to be polled.
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        match &self.state {
            State::Init => Some(now),
            State::Selecting | State::Requesting { .. } => self.retry_at,
            State::Bound {
                renew_at,
                expires_at,
                ..
            } => Some((*renew_at).min(*expires_at)),
            State::Renewing {
                rebind_at,
                expires_at,
                ..
            } => Some(
                (*rebind_at)
                    .min(*expires_at)
                    .min(self.retry_at.unwrap_or(*expires_at)),
            ),
            State::Rebinding { expires_at, .. } => {
                Some((*expires_at).min(self.retry_at.unwrap_or(*expires_at)))
            }
        }
    }

    /// Release the current lease and return to INIT.
    pub fn release(&mut self) -> Vec<Action> {
        let Some(lease) = self.lease().cloned() else {
            self.state = State::Init;
            self.retry_at = None;
            return Vec::new();
        };
        let mut packet =
            Packet::client(MessageType::Release, random_xid(), self.mac, lease.address);
        packet.options.server_id = Some(lease.server);
        self.state = State::Init;
        self.retry_at = None;
        vec![Action::Send(packet), Action::Deconfigure]
    }

    /// Start the first DISCOVER.
    pub fn start(&mut self, now: Instant) -> Vec<Action> {
        self.state = State::Init;
        self.tick(now)
    }

    /// Advance timers and produce the actions due at `now`.
    pub fn tick(&mut self, now: Instant) -> Vec<Action> {
        match self.state.clone() {
            State::Init => self.discover(now),
            State::Selecting => {
                if self.retry_due(now) {
                    self.attempts = self.attempts.saturating_add(1);
                    self.retry_at = Some(now + backoff(self.attempts));
                    vec![Action::Send(self.discover_packet())]
                } else {
                    Vec::new()
                }
            }
            State::Requesting { requested, server } => {
                if self.retry_due(now) {
                    self.attempts = self.attempts.saturating_add(1);
                    self.retry_at = Some(now + backoff(self.attempts));
                    vec![Action::Send(self.request_packet(
                        requested,
                        Some(server),
                        Ipv4Addr::UNSPECIFIED,
                    ))]
                } else {
                    Vec::new()
                }
            }
            State::Bound {
                lease,
                renew_at,
                rebind_at,
                expires_at,
            } => {
                if now >= expires_at {
                    self.state = State::Init;
                    self.retry_at = None;
                    return vec![Action::Deconfigure, Action::Send(self.discover_packet())];
                }
                if now >= renew_at {
                    self.state = State::Renewing {
                        lease: lease.clone(),
                        rebind_at,
                        expires_at,
                    };
                    self.retry_at = Some(now + backoff(1));
                    self.attempts = 1;
                    return vec![Action::Send(self.request_packet(
                        lease.address,
                        Some(lease.server),
                        lease.address,
                    ))];
                }
                Vec::new()
            }
            State::Renewing {
                lease,
                rebind_at,
                expires_at,
            } => {
                if now >= expires_at {
                    self.state = State::Init;
                    self.retry_at = None;
                    return vec![Action::Deconfigure, Action::Send(self.discover_packet())];
                }
                if now >= rebind_at {
                    self.state = State::Rebinding {
                        lease: lease.clone(),
                        expires_at,
                    };
                    self.retry_at = Some(now + backoff(1));
                    self.attempts = 1;
                    return vec![Action::Send(self.request_packet(
                        lease.address,
                        None,
                        lease.address,
                    ))];
                }
                if self.retry_due(now) {
                    self.attempts = self.attempts.saturating_add(1);
                    self.retry_at = Some(now + backoff(self.attempts));
                    return vec![Action::Send(self.request_packet(
                        lease.address,
                        Some(lease.server),
                        lease.address,
                    ))];
                }
                Vec::new()
            }
            State::Rebinding { lease, expires_at } => {
                if now >= expires_at {
                    self.state = State::Init;
                    self.retry_at = None;
                    return vec![Action::Deconfigure, Action::Send(self.discover_packet())];
                }
                if self.retry_due(now) {
                    self.attempts = self.attempts.saturating_add(1);
                    self.retry_at = Some(now + backoff(self.attempts));
                    return vec![Action::Send(self.request_packet(
                        lease.address,
                        None,
                        lease.address,
                    ))];
                }
                Vec::new()
            }
        }
    }

    /// Handle one received packet.
    pub fn handle(&mut self, packet: &Packet, now: Instant) -> Result<Vec<Action>, DhcpError> {
        if packet.xid != self.xid || packet.mac() != Some(self.mac) {
            return Ok(Vec::new());
        }
        match (self.state.clone(), packet.options.message_type) {
            (State::Selecting, Some(MessageType::Offer)) => {
                let server = packet
                    .options
                    .server_id
                    .or_else(|| (!packet.siaddr.is_unspecified()).then_some(packet.siaddr))
                    .ok_or_else(|| DhcpError::new("OFFER has no server identifier"))?;
                if packet.yiaddr.is_unspecified() {
                    return Err(DhcpError::new("OFFER has no offered address"));
                }
                let requested = packet.yiaddr;
                self.state = State::Requesting { requested, server };
                self.retry_at = Some(now + FIRST_RETRY);
                self.attempts = 1;
                Ok(vec![Action::Send(self.request_packet(
                    requested,
                    Some(server),
                    Ipv4Addr::UNSPECIFIED,
                ))])
            }
            (State::Requesting { .. }, Some(MessageType::Ack)) => {
                let lease = Lease::from_ack(packet, self.fallback_lease)?;
                let mut actions = vec![Action::Lease(lease.clone())];
                let renew_at = now + Duration::from_secs(u64::from(lease.t1));
                let rebind_at = now + Duration::from_secs(u64::from(lease.t2));
                let expires_at = now + Duration::from_secs(u64::from(lease.lease_time));
                self.state = State::Bound {
                    lease,
                    renew_at,
                    rebind_at,
                    expires_at,
                };
                self.retry_at = None;
                self.attempts = 0;
                actions.shrink_to_fit();
                Ok(actions)
            }
            (State::Requesting { .. }, Some(MessageType::Nak)) => {
                self.state = State::Init;
                self.retry_at = None;
                let mut actions = vec![Action::Deconfigure];
                actions.extend(self.discover(now));
                Ok(actions)
            }
            (State::Renewing { .. } | State::Rebinding { .. }, Some(MessageType::Ack)) => {
                let lease = Lease::from_ack(packet, self.fallback_lease)?;
                let renew_at = now + Duration::from_secs(u64::from(lease.t1));
                let rebind_at = now + Duration::from_secs(u64::from(lease.t2));
                let expires_at = now + Duration::from_secs(u64::from(lease.lease_time));
                self.state = State::Bound {
                    lease: lease.clone(),
                    renew_at,
                    rebind_at,
                    expires_at,
                };
                self.retry_at = None;
                self.attempts = 0;
                Ok(vec![Action::Lease(lease)])
            }
            (State::Renewing { .. } | State::Rebinding { .. }, Some(MessageType::Nak)) => {
                self.state = State::Init;
                self.retry_at = None;
                let mut actions = vec![Action::Deconfigure];
                actions.extend(self.discover(now));
                Ok(actions)
            }
            _ => Ok(Vec::new()),
        }
    }

    fn discover(&mut self, now: Instant) -> Vec<Action> {
        self.xid = random_xid();
        self.state = State::Selecting;
        self.attempts = 0;
        self.retry_at = Some(now + FIRST_RETRY);
        vec![Action::Send(self.discover_packet())]
    }

    fn discover_packet(&self) -> Packet {
        let mut packet = Packet::client(
            MessageType::Discover,
            self.xid,
            self.mac,
            Ipv4Addr::UNSPECIFIED,
        );
        packet.options.hostname = self.hostname.clone();
        packet
    }

    fn request_packet(
        &self,
        requested: Ipv4Addr,
        server: Option<Ipv4Addr>,
        ciaddr: Ipv4Addr,
    ) -> Packet {
        let mut packet = Packet::client(MessageType::Request, self.xid, self.mac, ciaddr);
        packet.options.hostname = self.hostname.clone();
        if ciaddr.is_unspecified() {
            packet.options.requested_ip = Some(requested);
        }
        packet.options.server_id = server;
        packet
    }

    fn retry_due(&self, now: Instant) -> bool {
        self.retry_at.is_some_and(|retry| now >= retry)
    }
}

fn backoff(attempts: u8) -> Duration {
    let shift = u32::from(attempts.min(6));
    FIRST_RETRY.saturating_mul(1 << shift).min(MAX_RETRY)
}

/// A transaction id suitable for DHCP. It need not be cryptographic; it only
/// has to be unlikely to collide on the local segment.
pub fn random_xid() -> u32 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let nanos = now.subsec_nanos();
    now.as_secs() as u32 ^ nanos ^ (std::process::id() << 16) ^ 0x9e37_79b9
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dhcp::packet::Options;

    fn mac() -> [u8; 6] {
        [2, 0, 0, 0, 0, 1]
    }

    fn reply(xid: u32, kind: MessageType, yiaddr: Ipv4Addr) -> Packet {
        Packet {
            op: 2,
            htype: 1,
            hlen: 6,
            hops: 0,
            xid,
            secs: 0,
            flags: 0,
            ciaddr: Ipv4Addr::UNSPECIFIED,
            yiaddr,
            siaddr: Ipv4Addr::new(192, 168, 1, 1),
            giaddr: Ipv4Addr::UNSPECIFIED,
            chaddr: [
                mac()[0],
                mac()[1],
                mac()[2],
                mac()[3],
                mac()[4],
                mac()[5],
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            ],
            options: Options {
                message_type: Some(kind),
                server_id: Some(Ipv4Addr::new(192, 168, 1, 1)),
                lease_time: Some(3600),
                t1: Some(1800),
                t2: Some(3150),
                subnet_mask: Some(Ipv4Addr::new(255, 255, 255, 0)),
                routers: vec![Ipv4Addr::new(192, 168, 1, 1)],
                dns: vec![Ipv4Addr::new(9, 9, 9, 9)],
                ..Options::default()
            },
        }
    }

    #[test]
    fn discover_offer_ack_binds() {
        let now = Instant::now();
        let mut client = Client::new("eth0", mac());
        let actions = client.start(now);
        let Action::Send(discover) = &actions[0] else {
            panic!("expected discover");
        };
        assert_eq!(discover.options.message_type, Some(MessageType::Discover));
        let xid = discover.xid;
        let offer = reply(xid, MessageType::Offer, Ipv4Addr::new(192, 168, 1, 50));
        let actions = client.handle(&offer, now).expect("offer");
        assert!(matches!(actions[0], Action::Send(_)));
        let ack = reply(xid, MessageType::Ack, Ipv4Addr::new(192, 168, 1, 50));
        let actions = client.handle(&ack, now).expect("ack");
        assert!(matches!(actions[0], Action::Lease(_)));
        assert!(client.lease().is_some());
    }

    #[test]
    fn wrong_xid_is_ignored() {
        let now = Instant::now();
        let mut client = Client::new("eth0", mac());
        client.start(now);
        let offer = reply(0, MessageType::Offer, Ipv4Addr::new(192, 168, 1, 50));
        assert!(client.handle(&offer, now).expect("ignore").is_empty());
    }

    #[test]
    fn renew_and_expiry_are_scheduled() {
        let now = Instant::now();
        let mut client = Client::new("eth0", mac());
        let actions = client.start(now);
        let Action::Send(discover) = &actions[0] else {
            panic!("expected discover");
        };
        let xid = discover.xid;
        client
            .handle(
                &reply(xid, MessageType::Offer, Ipv4Addr::new(192, 168, 1, 50)),
                now,
            )
            .expect("offer");
        client
            .handle(
                &reply(xid, MessageType::Ack, Ipv4Addr::new(192, 168, 1, 50)),
                now,
            )
            .expect("ack");
        let renew = client.tick(now + Duration::from_secs(1801));
        assert!(matches!(renew.first(), Some(Action::Send(_))));
        let expired = client.tick(now + Duration::from_secs(3601));
        assert!(matches!(expired.first(), Some(Action::Deconfigure)));
        assert!(client.lease().is_none());
    }

    #[test]
    fn rebind_and_release_are_reachable() {
        let now = Instant::now();
        let mut client = Client::new("eth0", mac());
        let actions = client.start(now);
        let Action::Send(discover) = &actions[0] else {
            panic!("expected discover");
        };
        let xid = discover.xid;
        client
            .handle(
                &reply(xid, MessageType::Offer, Ipv4Addr::new(192, 168, 1, 50)),
                now,
            )
            .expect("offer");
        client
            .handle(
                &reply(xid, MessageType::Ack, Ipv4Addr::new(192, 168, 1, 50)),
                now,
            )
            .expect("ack");
        let renew = client.tick(now + Duration::from_secs(1801));
        assert!(matches!(renew.first(), Some(Action::Send(_))));
        let rebind = client.tick(now + Duration::from_secs(3151));
        assert!(matches!(rebind.first(), Some(Action::Send(_))));
        let released = client.release();
        assert!(matches!(released.first(), Some(Action::Send(_))));
        assert!(matches!(released.get(1), Some(Action::Deconfigure)));
        assert!(client.lease().is_none());
    }

    #[test]
    fn nak_returns_to_init() {
        let now = Instant::now();
        let mut client = Client::new("eth0", mac());
        let actions = client.start(now);
        let Action::Send(discover) = &actions[0] else {
            panic!("expected discover");
        };
        let xid = discover.xid;
        client
            .handle(
                &reply(xid, MessageType::Offer, Ipv4Addr::new(192, 168, 1, 50)),
                now,
            )
            .expect("offer");
        let actions = client
            .handle(&reply(xid, MessageType::Nak, Ipv4Addr::UNSPECIFIED), now)
            .expect("nak");
        assert!(matches!(actions[0], Action::Deconfigure));
    }
}
