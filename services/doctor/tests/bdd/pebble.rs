//! Per-scenario Pebble + pebble-challtestsrv harness (docs/skills/testing.md
//! §5.6).
//!
//! Each `@pebble` scenario runs a private Pebble (Let's Encrypt's official
//! ACME test server) on dynamic ports, its HTTPS endpoint served with a
//! `rusty_photon_tls::test_cert`-minted certificate, and points Pebble's
//! validating resolver at the challtestsrv DNS sidecar. Doctor reaches it
//! through the production knobs an internal ACME directory would use:
//! `--directory-url` and `--acme-root`.

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// How long the pair has to answer on all three endpoints before the start
/// is abandoned and retried on fresh ports. A wall-clock budget rather than
/// a poll count, so three attempts always fit inside the suite's timeout
/// however slowly a probe fails. It is checked between polls, so an attempt
/// runs over by at most one round of probes — the bound that has to hold is
/// that three attempts fit, not that any one of them stops on the second.
const READY_BUDGET: Duration = Duration::from_secs(30);

/// Budget for one DNS round trip on loopback: generous for a live sidecar,
/// and the bound that matters when a squatter swallows the query instead.
const DNS_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Budget for one readiness HTTP request, connect included. reqwest applies
/// no timeout of its own, and a port whose squatter swallows the SYN parks a
/// connect on the OS retry schedule — minutes on Linux — which would make
/// [`READY_BUDGET`] a bound the loop only checks after the damage is done.
const HTTP_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The transaction id the readiness query carries. Fixed is enough: each
/// probe uses a fresh socket, so no stale answer can arrive, and the reply
/// check also demands the QR bit that our own query does not set.
const DNS_PROBE_ID: u16 = 0x5250;

/// The two binaries, from the `OMNISIM_PATH`-style env vars. `None` when
/// either is unset — the suite skips the `@pebble` scenarios then.
pub fn env_paths() -> Option<(String, String)> {
    let pebble = std::env::var("PEBBLE_PATH")
        .ok()
        .filter(|v| !v.is_empty())?;
    let challtestsrv = std::env::var("PEBBLE_CHALLTESTSRV_PATH")
        .ok()
        .filter(|v| !v.is_empty())?;
    Some((pebble, challtestsrv))
}

/// A running Pebble + challtestsrv pair; both children are killed on drop.
pub struct PebbleHandle {
    /// The ACME directory URL doctor's `--directory-url` targets.
    pub directory_url: String,
    /// challtestsrv's management base URL — carried to the doctor binary
    /// via `--dns-token` (the challtestsrv provider's credential slot).
    pub management_url: String,
    /// The minted CA that signed Pebble's HTTPS endpoint certificate —
    /// doctor's `--acme-root`.
    pub ca_pem: PathBuf,
    _dir: tempfile::TempDir,
    pebble: Child,
    challtestsrv: Child,
}

impl std::fmt::Debug for PebbleHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PebbleHandle")
            .field("directory_url", &self.directory_url)
            .field("management_url", &self.management_url)
            .finish_non_exhaustive()
    }
}

impl Drop for PebbleHandle {
    fn drop(&mut self) {
        for child in [&mut self.pebble, &mut self.challtestsrv] {
            child.kill().ok();
            child.wait().ok();
        }
    }
}

/// The zone every readiness query asks under; each start asks about its own
/// name in it ([`Sidecar::probe_name`]).
const PROBE_ZONE: &str = "readiness.probe.";

/// DNS record types the readiness probes ask for (RFC 1035 §3.2.2).
const TYPE_A: u16 = 1;
const TYPE_TXT: u16 = 16;

/// What only this start's challtestsrv knows: the DNS port it was handed,
/// and the address it answers every A query with.
#[derive(Clone, Copy)]
struct Sidecar {
    dns_port: u16,
    address: Ipv4Addr,
}

impl Sidecar {
    /// The name this start's probes ask about, minted with its address.
    /// challtestsrv keeps every TXT value set for a name and serves the
    /// oldest first, so on a name every start shared, another start that
    /// reached this sidecar's management API first would shadow this
    /// start's own record for good.
    fn probe_name(self) -> String {
        format!("{}.{PROBE_ZONE}", self.address)
    }
}

/// Why a readiness probe did not pass.
enum ProbeError {
    /// Nothing usable answered yet; a later poll may pass.
    NotYet(String),
    /// The port gave an answer this start's sidecar never gives, so another
    /// process's server holds it. The sidecar has lost that bind and never
    /// retries it, so no later poll can pass.
    Stranger(String),
}

impl ProbeError {
    /// The same verdict, its message prefixed with `prefix`.
    fn context(self, prefix: &str) -> Self {
        match self {
            Self::NotYet(e) => Self::NotYet(format!("{prefix} {e}")),
            Self::Stranger(e) => Self::Stranger(format!("{prefix} {e}")),
        }
    }
}

/// A DNS query for `name` of type `qtype`, wire-format per RFC 1035 §4.1.
fn dns_query(name: &str, qtype: u16) -> Vec<u8> {
    let mut msg = Vec::new();
    msg.extend_from_slice(&DNS_PROBE_ID.to_be_bytes());
    // Standard query, recursion desired; one question, no other records.
    msg.extend_from_slice(&[0x01, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
    for label in name.split_terminator('.') {
        msg.push(label.len() as u8);
        msg.extend_from_slice(label.as_bytes());
    }
    msg.push(0); // Root label ends the name.
    msg.extend_from_slice(&qtype.to_be_bytes());
    msg.extend_from_slice(&[0x00, 0x01]); // QCLASS IN.
    msg
}

/// Whether `msg` is a DNS reply to [`dns_query`] — our transaction, and the
/// QR bit set, which rules out a squatter echoing the query back at us.
fn is_dns_reply(msg: &[u8]) -> bool {
    msg.len() >= 12 && msg[..2] == DNS_PROBE_ID.to_be_bytes() && msg[2] & 0x80 != 0
}

/// The offset just past the domain name that starts at `at` in `msg`.
fn skip_name(msg: &[u8], mut at: usize) -> Option<usize> {
    loop {
        match *msg.get(at)? {
            0 => return Some(at + 1),
            // A compression pointer (RFC 1035 §4.1.4) ends the name.
            len if len & 0xC0 == 0xC0 => return Some(at + 2),
            len => at += 1 + usize::from(len),
        }
    }
}

/// The data of the first answer record in `reply` (a reply to one
/// question), when that record is of type `rtype`.
fn first_answer(reply: &[u8], rtype: u16) -> Option<&[u8]> {
    let answers = u16::from_be_bytes([*reply.get(6)?, *reply.get(7)?]);
    if answers == 0 {
        return None;
    }
    // The question: its name, then QTYPE and QCLASS.
    let at = skip_name(reply, 12)? + 4;
    // The answer: its name, then TYPE, CLASS, TTL, RDLENGTH and the data.
    let at = skip_name(reply, at)?;
    let fixed = reply.get(at..at + 10)?;
    if u16::from_be_bytes([fixed[0], fixed[1]]) != rtype {
        return None;
    }
    let len = usize::from(u16::from_be_bytes([fixed[8], fixed[9]]));
    reply.get(at + 10..at + 10 + len)
}

/// One UDP query and the reply to it. Socket errors are
/// [`ProbeError::NotYet`]: a refused query is the port nothing has bound
/// yet.
async fn dns_query_udp(port: u16, query: &[u8]) -> Result<Vec<u8>, ProbeError> {
    let not_yet = |what: &str, e: std::io::Error| ProbeError::NotYet(format!("{what}: {e}"));
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .map_err(|e| not_yet("probe socket", e))?;
    socket
        .connect(("127.0.0.1", port))
        .await
        .map_err(|e| not_yet("connect", e))?;
    socket.send(query).await.map_err(|e| not_yet("send", e))?;
    let mut buf = [0u8; 512];
    let read = socket
        .recv(&mut buf)
        .await
        .map_err(|e| not_yet("recv", e))?;
    if is_dns_reply(&buf[..read]) {
        Ok(buf[..read].to_vec())
    } else {
        Err(ProbeError::Stranger(format!(
            "{read} bytes back, but not a DNS answer: another process's server"
        )))
    }
}

/// One TCP query and the reply to it, two-byte length prefixed per RFC 1035
/// §4.2.2. Connection errors are [`ProbeError::NotYet`], as for UDP.
async fn dns_query_tcp(port: u16, query: &[u8]) -> Result<Vec<u8>, ProbeError> {
    let not_yet = |what: &str, e: std::io::Error| ProbeError::NotYet(format!("{what}: {e}"));
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|e| not_yet("connect", e))?;
    let mut framed = (query.len() as u16).to_be_bytes().to_vec();
    framed.extend_from_slice(query);
    stream
        .write_all(&framed)
        .await
        .map_err(|e| not_yet("send", e))?;
    let mut len = [0u8; 2];
    stream
        .read_exact(&mut len)
        .await
        .map_err(|e| not_yet("read length", e))?;
    let mut reply = vec![0u8; usize::from(u16::from_be_bytes(len))];
    stream
        .read_exact(&mut reply)
        .await
        .map_err(|e| not_yet("read reply", e))?;
    if is_dns_reply(&reply) {
        Ok(reply)
    } else {
        Err(ProbeError::Stranger(
            "answered, but not with a DNS answer: another process's server".to_owned(),
        ))
    }
}

/// One query over UDP, bounded by [`DNS_PROBE_TIMEOUT`].
async fn ask_udp(port: u16, query: &[u8]) -> Result<Vec<u8>, ProbeError> {
    match tokio::time::timeout(DNS_PROBE_TIMEOUT, dns_query_udp(port, query)).await {
        Ok(result) => result.map_err(|e| e.context("udp")),
        Err(_) => Err(ProbeError::NotYet(format!(
            "udp silent for {DNS_PROBE_TIMEOUT:?}"
        ))),
    }
}

/// One query over TCP, bounded by [`DNS_PROBE_TIMEOUT`].
async fn ask_tcp(port: u16, query: &[u8]) -> Result<Vec<u8>, ProbeError> {
    match tokio::time::timeout(DNS_PROBE_TIMEOUT, dns_query_tcp(port, query)).await {
        Ok(result) => result.map_err(|e| e.context("tcp")),
        Err(_) => Err(ProbeError::NotYet(format!(
            "tcp silent for {DNS_PROBE_TIMEOUT:?}"
        ))),
    }
}

/// Whether this start's sidecar serves DNS on its port, both transports:
/// each answers an A query with the address only this start's challtestsrv
/// was given. TCP and UDP are separate port spaces and challtestsrv binds
/// one listener on each, so half of it can be lost while the other half
/// looks healthy.
async fn sidecar_answers(sidecar: Sidecar) -> Result<(), ProbeError> {
    let query = dns_query(&sidecar.probe_name(), TYPE_A);
    let expected = sidecar.address.octets();
    for (transport, reply) in [
        ("udp", ask_udp(sidecar.dns_port, &query).await?),
        ("tcp", ask_tcp(sidecar.dns_port, &query).await?),
    ] {
        if first_answer(&reply, TYPE_A) != Some(&expected[..]) {
            return Err(ProbeError::Stranger(format!(
                "{transport} answered, but not with {}: another process's DNS server",
                sidecar.address
            )));
        }
    }
    Ok(())
}

/// Whether the management API at `url` belongs to this start's sidecar: a
/// TXT record set through it must come back from the sidecar's DNS port,
/// which [`sidecar_answers`] has already proven this start's own.
async fn management_reaches(
    client: &reqwest::Client,
    url: &str,
    sidecar: Sidecar,
) -> Result<(), ProbeError> {
    let name = sidecar.probe_name();
    let token = sidecar.address.to_string();
    let response = client
        .post(format!("{url}/set-txt"))
        .json(&serde_json::json!({ "host": name, "value": token }))
        .send()
        .await
        .map_err(|e| ProbeError::NotYet(format!("set-txt: {e:?}")))?;
    // The sidecar takes any set-txt with a host and a value, so an error
    // status is some other server's answer.
    if !response.status().is_success() {
        return Err(ProbeError::Stranger(format!(
            "{url} answered set-txt with {}: another process's server",
            response.status()
        )));
    }
    let reply = ask_udp(sidecar.dns_port, &dns_query(&name, TYPE_TXT)).await?;
    // TXT data is length-prefixed character strings (RFC 1035 §3.3.14).
    let mut expected = vec![token.len() as u8];
    expected.extend_from_slice(token.as_bytes());
    if first_answer(&reply, TYPE_TXT) == Some(&expected[..]) {
        Ok(())
    } else {
        Err(ProbeError::Stranger(format!(
            "{url} took the record, but the sidecar does not serve it: \
             another process's management API"
        )))
    }
}

impl PebbleHandle {
    /// Spawn the pair, with Pebble issuing certificates valid for
    /// `validity_seconds`, and wait until every endpoint is proven this
    /// start's own. Neither child can bind port 0 and report the port it
    /// got, so the ports are picked bind-and-drop, and a concurrent process
    /// can take one before the children claim it — a start that cannot
    /// prove its endpoints is retried on fresh ports.
    pub async fn start(validity_seconds: u64) -> Self {
        let mut last_error = String::new();
        for attempt in 1..=3 {
            match Self::try_start(validity_seconds).await {
                Ok(handle) => return handle,
                Err(e) => {
                    eprintln!("pebble start attempt {attempt} failed: {e}");
                    last_error = e;
                }
            }
        }
        panic!("could not start Pebble after 3 attempts; last error: {last_error}");
    }

    async fn try_start(validity_seconds: u64) -> Result<Self, String> {
        let (mut handle, sidecar) =
            bdd_infra::off_loop(move || Self::spawn_pair(validity_seconds)).await;
        match handle.wait_ready(sidecar).await {
            Ok(()) => Ok(handle),
            Err(e) => Err(format!("{e}; children output:\n{}", handle.child_logs())),
        }
    }

    /// The synchronous half of a start, run off the poll loop every
    /// scenario shares (docs/skills/testing.md §5.7): mint the endpoint
    /// certificate, pick the ports, write Pebble's config and spawn both
    /// children. Returns the handle and what identifies its sidecar.
    fn spawn_pair(validity_seconds: u64) -> (Self, Sidecar) {
        let (pebble_path, challtestsrv_path) =
            env_paths().expect("@pebble scenario ran without PEBBLE_PATH/PEBBLE_CHALLTESTSRV_PATH");
        let dir = tempfile::tempdir().expect("pebble scratch dir");

        // Mint the HTTPS endpoint certificate: a test CA plus a "localhost"
        // cert (SANs cover localhost and the loopback addresses).
        rusty_photon_tls::test_cert::generate_ca(dir.path()).expect("pebble CA");
        let ca_cert_pem =
            std::fs::read_to_string(dir.path().join("ca.pem")).expect("pebble ca.pem");
        let ca_key_pem =
            std::fs::read_to_string(dir.path().join("ca-key.pem")).expect("pebble ca-key.pem");
        rusty_photon_tls::test_cert::generate_service_cert(
            &ca_cert_pem,
            &ca_key_pem,
            "localhost",
            dir.path(),
        )
        .expect("pebble endpoint cert");

        let [acme_port, chall_mgmt_port, dns_port] = bdd_infra::ephemeral_ports::<3>();
        // The address the sidecar answers every A query with, minted for
        // this start so a DNS answer carrying it can only come from this
        // start's challtestsrv. Never in 127.0.x.x, so never the sidecar's
        // own default of 127.0.0.1. Nothing connects to it: doctor's
        // challenges are DNS-01, which ask only for TXT records.
        let sidecar = Sidecar {
            dns_port,
            address: Ipv4Addr::new(
                127,
                rand::random_range(1..=255),
                rand::random(),
                rand::random(),
            ),
        };
        // The issued-certificate validity rides in the default profile —
        // Pebble ignores the legacy top-level certificateValidityPeriod
        // once profiles exist, and its no-profile default is 90 days.
        // There is no managementListenAddress: doctor never uses Pebble's
        // management API, and without one Pebble opens no listener for it.
        let config = serde_json::json!({
            "pebble": {
                "listenAddress": format!("127.0.0.1:{acme_port}"),
                "certificate": dir.path().join("localhost.pem"),
                "privateKey": dir.path().join("localhost-key.pem"),
                "httpPort": 5002,
                "tlsPort": 5001,
                "ocspResponderURL": "",
                "profiles": {
                    "default": {
                        "description": "doctor BDD profile",
                        "validityPeriod": validity_seconds
                    }
                }
            }
        });
        let config_path = dir.path().join("pebble-config.json");
        std::fs::write(
            &config_path,
            serde_json::to_string_pretty(&config).expect("pebble config"),
        )
        .expect("pebble config file");

        // Only the DNS and management servers are wanted; every other
        // challenge responder (and DoH) is disabled via an empty bind.
        // Both children log into the scratch dir for failure diagnostics.
        let log = |name: &str| {
            Stdio::from(std::fs::File::create(dir.path().join(name)).expect("child log file"))
        };
        let mut challtestsrv = Command::new(&challtestsrv_path);
        challtestsrv
            .arg("-management")
            // The loopback host is spelled out: a bare ":<port>" is Go's
            // all-interfaces bind, which puts a dual-stack listener on
            // an ephemeral port for the whole run. That is the address
            // any sibling suite's IPv4-only stub needs free on `[::1]`
            // to own its own port by name.
            .arg(format!("127.0.0.1:{chall_mgmt_port}"))
            .arg("-dnsserver")
            .arg(format!("127.0.0.1:{dns_port}"))
            .arg("-defaultIPv4")
            .arg(sidecar.address.to_string())
            .arg("-doh")
            .arg("")
            .arg("-http01")
            .arg("")
            .arg("-https01")
            .arg("")
            .arg("-tlsalpn01")
            .arg("")
            .stdout(log("challtestsrv.log"))
            .stderr(log("challtestsrv.err.log"));
        let mut pebble = Command::new(&pebble_path);
        pebble
            .arg("-config")
            .arg(&config_path)
            .arg("-dnsserver")
            .arg(format!("127.0.0.1:{dns_port}"))
            .env("PEBBLE_VA_NOSLEEP", "1")
            .stdout(log("pebble.log"))
            .stderr(log("pebble.err.log"));
        // Each process creation under the lock every spawn in this test
        // process shares (`bdd_infra::with_spawn_lock`).
        let mut challtestsrv =
            bdd_infra::with_spawn_lock(|| challtestsrv.spawn()).expect("spawn pebble-challtestsrv");
        let pebble = match bdd_infra::with_spawn_lock(|| pebble.spawn()) {
            Ok(pebble) => pebble,
            Err(e) => {
                // Nothing owns the sidecar yet, so stop and reap it here
                // rather than leave it running for the rest of the suite.
                challtestsrv.kill().ok();
                challtestsrv.wait().ok();
                panic!("spawn pebble: {e}");
            }
        };

        let handle = Self {
            // 127.0.0.1 rather than localhost: Pebble binds IPv4 loopback
            // only, and localhost resolves to ::1 first on some hosts. The
            // endpoint cert carries loopback IP SANs, so the IP URL verifies.
            directory_url: format!("https://127.0.0.1:{acme_port}/dir"),
            management_url: format!("http://127.0.0.1:{chall_mgmt_port}"),
            ca_pem: dir.path().join("ca.pem"),
            _dir: dir,
            pebble,
            challtestsrv,
        };
        (handle, sidecar)
    }

    /// Poll until every port the pair was handed is proven this start's
    /// own; bail out early when either child has already exited (a stolen
    /// port kills Pebble at bind time).
    ///
    /// Any of the ports can be held by another process instead, another
    /// scenario's pair among them, and a check that asks only "does
    /// something answer?" adopts it. So each check asks for something only
    /// this start's children have:
    ///
    /// - the ACME directory answers through a client that trusts only the
    ///   CA minted for this start;
    /// - the sidecar's DNS port answers an A query, on both transports,
    ///   with the address minted for this start;
    /// - a TXT record set through the management API comes back from that
    ///   DNS port.
    ///
    /// The sidecar's two checks are the ones nothing else stands in for: a
    /// challtestsrv that loses a bind does not exit. It logs `address
    /// already in use` and keeps serving its other listener, so the loss
    /// would otherwise surface only when Pebble resolves a challenge
    /// mid-scenario — long past the retry that would have picked fresh
    /// ports. Nor does it ever retry the bind, so once either check gets
    /// an answer this start's sidecar never gives, the attempt ends at once
    /// rather than at the end of [`READY_BUDGET`].
    async fn wait_ready(&mut self, sidecar: Sidecar) -> Result<(), String> {
        let client = rusty_photon_tls::client::client_builder(Some(&self.ca_pem))
            .expect("builder trusting the pebble CA")
            .timeout(HTTP_PROBE_TIMEOUT)
            .build()
            .expect("client trusting the pebble CA");
        let plain = reqwest::Client::builder()
            .timeout(HTTP_PROBE_TIMEOUT)
            .build()
            .expect("plain http client");
        let mut directory_error = String::new();
        let mut dns_error = String::new();
        let mut management_error = String::new();
        let mut directory_ready = false;
        let mut management_ready = false;
        let mut dns_ready = false;
        let started = Instant::now();
        loop {
            for (name, child) in [
                ("pebble", &mut self.pebble),
                ("pebble-challtestsrv", &mut self.challtestsrv),
            ] {
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(format!("{name} exited at startup ({status})"));
                }
            }
            if !directory_ready {
                // Pebble 400s any request without a User-Agent.
                let request = client
                    .get(&self.directory_url)
                    .header("user-agent", "rusty-photon-doctor-bdd");
                match request.send().await {
                    Ok(response) if response.status().is_success() => directory_ready = true,
                    Ok(response) => {
                        let status = response.status();
                        let body = response.text().await.unwrap_or_default();
                        directory_error = format!("HTTP {status}: {body}");
                    }
                    Err(e) => directory_error = format!("{e:?}"),
                }
            }
            if !dns_ready {
                match sidecar_answers(sidecar).await {
                    Ok(()) => dns_ready = true,
                    Err(ProbeError::NotYet(e)) => dns_error = e,
                    Err(ProbeError::Stranger(e)) => {
                        return Err(format!("DNS 127.0.0.1:{}: {e}", sidecar.dns_port));
                    }
                }
            }
            // Only once the DNS port is proven: the management check reads
            // its record back through it.
            if dns_ready && !management_ready {
                match management_reaches(&plain, &self.management_url, sidecar).await {
                    Ok(()) => management_ready = true,
                    Err(ProbeError::NotYet(e)) => management_error = e,
                    Err(ProbeError::Stranger(e)) => return Err(e),
                }
            }
            if directory_ready && management_ready && dns_ready {
                return Ok(());
            }
            if started.elapsed() >= READY_BUDGET {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        Err(format!(
            "Pebble did not become ready in {:?} (directory {} \
             ready: {directory_ready}, management {} ready: {management_ready}, \
             DNS 127.0.0.1:{} ready: {dns_ready}; last directory error: \
             {directory_error}; last DNS error: {dns_error}; last management \
             error: {management_error})",
            started.elapsed(),
            self.directory_url,
            self.management_url,
            sidecar.dns_port
        ))
    }

    /// The children's captured output, for failure messages.
    fn child_logs(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for name in [
            "pebble.log",
            "pebble.err.log",
            "challtestsrv.log",
            "challtestsrv.err.log",
        ] {
            let content = std::fs::read_to_string(self._dir.path().join(name)).unwrap_or_default();
            if !content.trim().is_empty() {
                writeln!(out, "--- {name}:\n{content}").unwrap();
            }
        }
        out
    }
}
