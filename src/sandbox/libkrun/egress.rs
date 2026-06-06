//! The L3/L5 egress stack for the libkrun backend (virtio-net + smoltcp + DNS).
//!
//! libkrun drives the guest's virtio-net L2 frames to a socketpair we own (the
//! passt protocol: `[u32 BE len][Ethernet frame]`). This module runs a
//! **smoltcp** userspace TCP/IP stack on the host end of that pipe — the egress
//! termination point pillbox owns. It runs **in the VMM child** (a thread beside
//! `krun_start_enter`, which never returns), so when the VM shuts down and the
//! child `exit()`s, this thread goes with it.
//!
//! **What lives here:** the transport (the `PasstDevice` + the poll loop) and the
//! **DNS fence** — an allowlisted name resolves to the gateway (so its TLS lands
//! at our MITM) and is **pinned** in [`PinTable`]; anything else gets **NXDOMAIN**
//! (default-deny at the name layer). The L7 TLS MITM that terminates the pinned
//! connection on these sockets, swaps the credential, and forwards to the real
//! upstream is [`super::mitm`] — the poll loop drives it, and it consumes the
//! `PinTable` populated here.

use std::collections::VecDeque;
use std::io::Write;
use std::net::Ipv4Addr;
use std::os::raw::c_int;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::udp;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};

use super::vault::{CredSwap, Vault};

/// Guest NIC MAC (handed to `krun_add_net_unixstream`) and the gateway MAC the
/// stack answers as. The addressing is fixed per microVM — one stack, one guest.
pub(super) const GUEST_MAC: [u8; 6] = [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee];
const GATEWAY_MAC: [u8; 6] = [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0x01];
/// The gateway = the proxy = our DNS server, all on this address. Allowlisted
/// names resolve here so their TLS terminates at our MITM (L5b).
const GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const PREFIX_LEN: u8 = 24;
const DNS_PORT: u16 = 53;

/// Shell commands the guest runs (before exec'ing the agent) to bring its NIC up
/// and route DNS + egress through this stack. Kept here so the addressing lives
/// in one place; `mod.rs` splices the string into the guest entrypoint.
pub(super) fn guest_net_commands() -> String {
    format!(
        "ip link set eth0 up; \
         ip addr add {GUEST_IP}/{PREFIX_LEN} dev eth0; \
         ip route add default via {GATEWAY_IP}; \
         printf 'nameserver {GATEWAY_IP}\\n' > /etc/resolv.conf"
    )
}

/// Model-provider API hosts a non-vault agent (opencode) may egress to — the
/// "standard" egress profile. These are allowed through the DNS fence and the
/// MITM terminates + forwards them with an **empty swap** (no credential
/// substitution — opencode holds its own real key and authenticates directly).
/// Distinct from the vault `intercepted_hosts()`, where the MITM swaps a stub
/// for the real credential; `api.openai.com`/`api.anthropic.com` live there, so
/// they're not repeated here.
///
/// Best-effort, extend freely — covers the providers opencode users reach. A
/// host not listed is fenced (NXDOMAIN); a future `--egress-allow HOST` flag
/// will let a user declare a custom/self-hosted endpoint.
pub(super) fn standard_egress_hosts() -> &'static [&'static str] {
    &[
        "openrouter.ai",                     // OpenRouter (aggregator)
        "api.deepseek.com",                  // DeepSeek
        "api.moonshot.cn",                   // Kimi / Moonshot (CN)
        "api.moonshot.ai",                   // Kimi / Moonshot (intl)
        "api.x.ai",                          // Grok (xAI)
        "generativelanguage.googleapis.com", // Gemini (Google AI Studio)
        "api.z.ai",                          // GLM (z.ai)
        "open.bigmodel.cn",                  // GLM (Zhipu / BigModel)
        "api.mistral.ai",                    // Mistral
        "api.groq.com",                      // Groq
        "models.dev",                        // opencode's model registry
    ]
}

/// Names the guest resolved through our allowlisted resolver. Credential release
/// (L5b) requires the SNI be in here — a forged-SNI / hardcoded-IP connection
/// that skipped DNS can't be, which is the **name-level DNS-pin**. One table per
/// microVM (the trust unit); never shared across stacks.
#[derive(Default)]
pub(super) struct PinTable {
    names: std::collections::HashSet<String>,
}

impl PinTable {
    /// Record a name the guest legitimately resolved. Returns true if newly added.
    fn pin(&mut self, name: &str) -> bool {
        self.names.insert(name.to_ascii_lowercase())
    }

    /// Whether `name` was resolved through our resolver — [`super::mitm`]'s pin
    /// gate consumes this to deny a hardcoded-IP/forged-SNI connection that
    /// skipped DNS.
    pub(super) fn contains(&self, name: &str) -> bool {
        self.names.contains(&name.to_ascii_lowercase())
    }
}

/// Host-side diagnostics sink. libkrun wires the guest console to the VMM child's
/// stdio, so the egress thread's `eprintln` is swallowed — write to a file when
/// one is configured (`PILLBOX_KRUN_EGRESS_LOG`), else fall back to stderr. (A
/// stopgap until L5c routes these as §0 events.) Shared with [`super::mitm`].
pub(super) struct Diag(Option<Mutex<std::fs::File>>);

impl Diag {
    fn open(path: Option<String>) -> Self {
        Self(
            path.and_then(|p| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
                    .ok()
            })
            .map(Mutex::new),
        )
    }

    pub(super) fn log(&self, msg: &str) {
        match &self.0 {
            Some(f) => {
                let _ = writeln!(f.lock().unwrap(), "{msg}");
            }
            None => eprintln!("{msg}"),
        }
    }
}

/// Run the egress stack on the host end of the passt socketpair until the process
/// exits (the sibling `krun_start_enter` thread tears it down). `allowlist` is the
/// DNS fence: only these hosts resolve; everything else is NXDOMAIN'd. When
/// `ca_dir` is set, a TLS MITM ([`Vault`]) terminates the allowlisted hosts'
/// connections at the gateway; otherwise the stack is DNS-fence only.
pub(super) fn run(
    fd: c_int,
    allowlist: Vec<String>,
    ca_dir: Option<String>,
    swap_pairs: Vec<CredSwap>,
    diag_path: Option<String>,
    local_forward_port: Option<u16>,
) {
    let diag = Diag::open(diag_path);
    let vault = match ca_dir {
        Some(dir) => match Vault::new(&dir, allowlist.clone()) {
            Ok(v) => Some(v),
            // Degrade safely to DNS-fence-only: the allowlisted TLS then has no
            // listener (RST), so default-deny still holds — just no MITM.
            Err(e) => {
                diag.log(&format!("krun-egress: MITM disabled ({e}); DNS fence only"));
                None
            }
        },
        None => None,
    };
    let rx: RxQueue = Arc::new(Mutex::new(VecDeque::new()));
    {
        let rx = rx.clone();
        thread::spawn(move || {
            while let Some(frame) = read_frame(fd) {
                rx.lock().unwrap().push_back(frame);
            }
            eprintln!("krun-egress: frame socket closed");
        });
    }

    let mut device = PasstDevice { fd, rx };
    let start = Instant::now();
    let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(GATEWAY_MAC)));
    config.random_seed = 0x5a94_efe4;
    let mut iface = Interface::new(config, &mut device, now(start));
    iface.update_ip_addrs(|addrs| {
        let g = GATEWAY_IP.octets();
        addrs
            .push(IpCidr::new(
                IpAddress::v4(g[0], g[1], g[2], g[3]),
                PREFIX_LEN,
            ))
            .unwrap();
    });

    let mut sockets = SocketSet::new(vec![]);
    let dns_buf = || udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0u8; 4096]);
    let dns = sockets.add(udp::Socket::new(dns_buf(), dns_buf()));
    sockets
        .get_mut::<udp::Socket>(dns)
        .bind(DNS_PORT)
        .expect("bind :53");

    let mut pins = PinTable::default();
    let mut listeners: Vec<super::mitm::Listener> = Vec::new();
    if vault.is_some() {
        super::mitm::replenish_listeners(&mut listeners, &mut sockets);
    }
    let mut forwarders: Vec<super::local_forward::Forwarder> = Vec::new();
    if let Some(port) = local_forward_port {
        super::local_forward::replenish(&mut forwarders, &mut sockets, port);
    }
    diag.log(&format!(
        "krun-egress: smoltcp up (gw 10.0.2.2, dns :{DNS_PORT}, mitm :{}, local-fwd :{}); fence allowlist={allowlist:?}",
        if vault.is_some() { "443" } else { "off" },
        local_forward_port.map_or_else(|| "off".to_string(), |p| p.to_string()),
    ));

    loop {
        iface.poll(now(start), &mut device, &mut sockets);
        serve_dns(
            sockets.get_mut::<udp::Socket>(dns),
            &allowlist,
            &mut pins,
            &diag,
        );
        if let Some(vault) = &vault {
            super::mitm::drive_listeners(
                &mut listeners,
                &mut sockets,
                vault,
                &pins,
                &swap_pairs,
                &diag,
            );
            super::mitm::replenish_listeners(&mut listeners, &mut sockets);
        }
        if let Some(port) = local_forward_port {
            super::local_forward::drive(&mut forwarders, &mut sockets, port, &diag);
            super::local_forward::replenish(&mut forwarders, &mut sockets, port);
        }
        thread::sleep(Duration::from_millis(2));
    }
}

// ── smoltcp Device over the passt socketpair ────────────────────────────────

type RxQueue = Arc<Mutex<VecDeque<Vec<u8>>>>;

struct PasstDevice {
    fd: c_int,
    rx: RxQueue,
}

impl Device for PasstDevice {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken;

    fn receive(&mut self, _t: SmolInstant) -> Option<(RxToken, TxToken)> {
        let frame = self.rx.lock().unwrap().pop_front()?;
        Some((RxToken { frame }, TxToken { fd: self.fd }))
    }

    fn transmit(&mut self, _t: SmolInstant) -> Option<TxToken> {
        Some(TxToken { fd: self.fd })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = 1500;
        c
    }
}

struct RxToken {
    frame: Vec<u8>,
}
impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.frame)
    }
}

struct TxToken {
    fd: c_int,
}
impl phy::TxToken for TxToken {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        write_frame(self.fd, &buf);
        r
    }
}

// ── DNS fence ───────────────────────────────────────────────────────────────

fn allowlisted(allowlist: &[String], name: &str) -> bool {
    allowlist.iter().any(|h| h.eq_ignore_ascii_case(name))
}

/// DNS resolver: NXDOMAIN for non-allowlisted names (the default-deny fence); for
/// an allowlisted name, answer A=gateway-IP and **pin** it (so L5b's TLS gate can
/// check the SNI against what the guest legitimately resolved).
fn serve_dns(sock: &mut udp::Socket, allowlist: &[String], pins: &mut PinTable, diag: &Diag) {
    while sock.can_recv() {
        let (query, remote) = match sock.recv() {
            Ok((data, meta)) => (data.to_vec(), meta.endpoint),
            Err(_) => break,
        };
        let Some((resp, outcome)) = build_dns_response(&query, allowlist) else {
            continue;
        };
        match outcome {
            DnsOutcome::Pinned(name) => {
                if pins.pin(&name) {
                    diag.log(&format!(
                        "krun-egress: [dns] {name} → A 10.0.2.2 (allowlisted, pinned)"
                    ));
                }
            }
            DnsOutcome::NxDomain(name) => {
                diag.log(&format!(
                    "krun-egress: [dns] NXDOMAIN {name} (not on allowlist — fenced)"
                ));
            }
            DnsOutcome::Empty => {}
        }
        let _ = sock.send_slice(&resp, remote);
    }
}

/// What `build_dns_response` decided, for the caller's pin table + logging.
enum DnsOutcome {
    /// Allowlisted A query → answered with the gateway IP; pin this name.
    Pinned(String),
    /// Non-allowlisted → NXDOMAIN (the fence).
    NxDomain(String),
    /// Allowlisted non-A (e.g. AAAA) → NOERROR with no answers; nothing to pin.
    Empty,
}

/// Minimal DNS responder. Parses the first question; answers an allowlisted A
/// query with `GATEWAY_IP` (so the name's TLS lands at our MITM) and reports it
/// for pinning, NXDOMAINs anything not on the allowlist (the default-deny fence),
/// and returns an empty NOERROR for an allowlisted non-A query so the guest falls
/// back to A. Returns `(response_bytes, outcome)`.
fn build_dns_response(q: &[u8], allowlist: &[String]) -> Option<(Vec<u8>, DnsOutcome)> {
    if q.len() < 12 {
        return None;
    }
    let qdcount = u16::from_be_bytes([q[4], q[5]]);
    if qdcount < 1 {
        return None;
    }
    // Parse QNAME (labels) starting at offset 12.
    let mut p = 12;
    let mut name = String::new();
    loop {
        let len = *q.get(p)? as usize;
        p += 1;
        if len == 0 {
            break;
        }
        if len > 63 || p + len > q.len() {
            return None; // no compression in a question; bail on anything odd
        }
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(&String::from_utf8_lossy(&q[p..p + len]));
        p += len;
    }
    let qtype = u16::from_be_bytes([*q.get(p)?, *q.get(p + 1)?]);
    let q_end = p + 4; // qtype(2) + qclass(2)
    let question = q.get(12..q_end)?;
    let lname = name.to_ascii_lowercase();

    // Response header: echo ID, QR=1 RD=1 RA=1, qdcount=1.
    let mut r = Vec::with_capacity(64);
    r.extend_from_slice(&q[0..2]); // ID
    let allow = allowlisted(allowlist, &lname);
    let answer_a = allow && qtype == 1; // A record
    let rcode_nx = !allow;
    // flags: hi=0x81 (QR=1, RD=1), lo=0x80 (RA=1) | rcode (0=NOERROR, 3=NXDOMAIN).
    r.extend_from_slice(&[0x81, if rcode_nx { 0x83 } else { 0x80 }]);
    r.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    r.extend_from_slice(&(answer_a as u16).to_be_bytes()); // ANCOUNT
    r.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    r.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    r.extend_from_slice(question); // echo the question
    if answer_a {
        r.extend_from_slice(&[0xc0, 0x0c]); // NAME = pointer to the question
        r.extend_from_slice(&1u16.to_be_bytes()); // TYPE A
        r.extend_from_slice(&1u16.to_be_bytes()); // CLASS IN
        r.extend_from_slice(&60u32.to_be_bytes()); // TTL
        r.extend_from_slice(&4u16.to_be_bytes()); // RDLENGTH
        r.extend_from_slice(&GATEWAY_IP.octets()); // RDATA
    }

    let outcome = if rcode_nx {
        DnsOutcome::NxDomain(lname)
    } else if answer_a {
        DnsOutcome::Pinned(lname)
    } else {
        DnsOutcome::Empty
    };
    Some((r, outcome))
}

fn now(start: Instant) -> SmolInstant {
    SmolInstant::from_micros(start.elapsed().as_micros() as i64)
}

// ── passt framing helpers ───────────────────────────────────────────────────

fn read_frame(fd: c_int) -> Option<Vec<u8>> {
    let mut lenb = [0u8; 4];
    if !read_exact(fd, &mut lenb) {
        return None;
    }
    let len = u32::from_be_bytes(lenb) as usize;
    if len == 0 || len > 65_536 {
        return None;
    }
    let mut frame = vec![0u8; len];
    if !read_exact(fd, &mut frame) {
        return None;
    }
    Some(frame)
}

fn write_frame(fd: c_int, frame: &[u8]) {
    // Don't emit a body without its length header — a half-written frame would
    // desync the passt stream. On a write failure the peer is gone (the VM is
    // shutting down), so dropping the frame is safe: the stream dies with it.
    let hdr = (frame.len() as u32).to_be_bytes();
    if write_all(fd, &hdr) {
        let _ = write_all(fd, frame);
    }
}

fn read_exact(fd: c_int, buf: &mut [u8]) -> bool {
    let mut off = 0;
    while off < buf.len() {
        let n = unsafe {
            libc::read(
                fd,
                buf[off..].as_mut_ptr() as *mut libc::c_void,
                buf.len() - off,
            )
        };
        if n > 0 {
            off += n as usize;
        } else if n < 0 && interrupted() {
            continue; // signal mid-read — retry rather than tear the stream down
        } else {
            return false; // EOF (0) or a hard error
        }
    }
    true
}

fn write_all(fd: c_int, buf: &[u8]) -> bool {
    let mut off = 0;
    while off < buf.len() {
        let n = unsafe {
            libc::write(
                fd,
                buf[off..].as_ptr() as *const libc::c_void,
                buf.len() - off,
            )
        };
        if n > 0 {
            off += n as usize;
        } else if n < 0 && interrupted() {
            continue;
        } else {
            return false; // peer gone or hard error
        }
    }
    true
}

/// Whether the last syscall failed with `EINTR` (interrupted by a signal).
fn interrupted() -> bool {
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&1u16.to_be_bytes()); // class IN
        q
    }

    #[test]
    fn allowlisted_a_query_answers_gateway_and_pins() {
        let allow = vec!["api.anthropic.com".to_string()];
        let (resp, outcome) = build_dns_response(&query("api.anthropic.com", 1), &allow).unwrap();
        assert!(matches!(outcome, DnsOutcome::Pinned(ref n) if n == "api.anthropic.com"));
        // ANCOUNT == 1, RDATA == gateway IP at the tail.
        assert_eq!(u16::from_be_bytes([resp[6], resp[7]]), 1);
        assert_eq!(resp[resp.len() - 4..], GATEWAY_IP.octets());
        assert_eq!(resp[3] & 0x0f, 0); // NOERROR
    }

    #[test]
    fn non_allowlisted_query_is_nxdomain() {
        let allow = vec!["api.anthropic.com".to_string()];
        let (resp, outcome) = build_dns_response(&query("evil.example", 1), &allow).unwrap();
        assert!(matches!(outcome, DnsOutcome::NxDomain(ref n) if n == "evil.example"));
        assert_eq!(resp[3] & 0x0f, 3); // NXDOMAIN rcode
        assert_eq!(u16::from_be_bytes([resp[6], resp[7]]), 0); // no answers
    }

    #[test]
    fn allowlisted_non_a_query_is_empty_noerror() {
        let allow = vec!["api.anthropic.com".to_string()];
        let (resp, outcome) = build_dns_response(&query("api.anthropic.com", 28), &allow).unwrap();
        assert!(matches!(outcome, DnsOutcome::Empty));
        assert_eq!(resp[3] & 0x0f, 0); // NOERROR
        assert_eq!(u16::from_be_bytes([resp[6], resp[7]]), 0); // no answers (AAAA)
    }

    #[test]
    fn matching_is_case_insensitive_and_pins_lowercase() {
        let allow = vec!["API.Anthropic.COM".to_string()];
        let (_, outcome) = build_dns_response(&query("api.anthropic.com", 1), &allow).unwrap();
        assert!(matches!(outcome, DnsOutcome::Pinned(_)));
        let mut pins = PinTable::default();
        pins.pin("API.ANTHROPIC.COM");
        assert!(pins.contains("api.anthropic.com"));
    }
}
