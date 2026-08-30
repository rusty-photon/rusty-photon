//! Per-scenario Pebble + pebble-challtestsrv harness (docs/skills/testing.md
//! §5.6).
//!
//! Each `@pebble` scenario runs a private Pebble (Let's Encrypt's official
//! ACME test server) on dynamic ports, its HTTPS endpoint served with a
//! `rusty_photon_tls::test_cert`-minted certificate, and points Pebble's
//! validating resolver at the challtestsrv DNS sidecar. Doctor reaches it
//! through the production knobs an internal ACME directory would use:
//! `--directory-url` and `--acme-root`.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// How long the pair has to answer on all three endpoints before the start
/// is abandoned and retried on fresh ports. A wall-clock budget rather than
/// a poll count, so three attempts always fit inside the suite's timeout
/// however slowly a probe fails.
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

/// N distinct free localhost ports, held simultaneously so no two picks
/// collide, then released for the children to claim.
fn free_ports<const N: usize>() -> [u16; N] {
    let listeners: Vec<std::net::TcpListener> = (0..N)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0").expect("bind a free port"))
        .collect();
    let mut ports = [0u16; N];
    for (port, listener) in ports.iter_mut().zip(&listeners) {
        *port = listener.local_addr().expect("bound addr").port();
    }
    ports
}

/// A DNS query for `readiness.probe. A IN`, wire-format per RFC 1035 §4.1.
fn dns_query() -> Vec<u8> {
    let mut msg = Vec::new();
    msg.extend_from_slice(&DNS_PROBE_ID.to_be_bytes());
    // Standard query, recursion desired; one question, no other records.
    msg.extend_from_slice(&[0x01, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
    for label in ["readiness", "probe"] {
        msg.push(label.len() as u8);
        msg.extend_from_slice(label.as_bytes());
    }
    msg.push(0); // Root label ends the name.
    msg.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]); // QTYPE A, QCLASS IN.
    msg
}

/// Whether `msg` is a DNS reply to [`dns_query`] — our transaction, and the
/// QR bit set, which rules out a squatter echoing the query back at us.
fn is_dns_reply(msg: &[u8]) -> bool {
    msg.len() >= 12 && msg[..2] == DNS_PROBE_ID.to_be_bytes() && msg[2] & 0x80 != 0
}

/// One UDP query and its answer.
async fn dns_query_udp(port: u16) -> Result<(), String> {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("probe socket: {e}"))?;
    socket
        .connect(("127.0.0.1", port))
        .await
        .map_err(|e| format!("connect: {e}"))?;
    socket
        .send(&dns_query())
        .await
        .map_err(|e| format!("send: {e}"))?;
    let mut buf = [0u8; 512];
    let read = socket
        .recv(&mut buf)
        .await
        .map_err(|e| format!("recv: {e}"))?;
    if is_dns_reply(&buf[..read]) {
        Ok(())
    } else {
        Err(format!("{read} bytes back, but not our DNS answer"))
    }
}

/// One TCP query and its answer, two-byte length prefixed per RFC 1035 §4.2.2.
async fn dns_query_tcp(port: u16) -> Result<(), String> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let query = dns_query();
    let mut framed = (query.len() as u16).to_be_bytes().to_vec();
    framed.extend_from_slice(&query);
    stream
        .write_all(&framed)
        .await
        .map_err(|e| format!("send: {e}"))?;
    let mut len = [0u8; 2];
    stream
        .read_exact(&mut len)
        .await
        .map_err(|e| format!("read length: {e}"))?;
    let mut reply = vec![0u8; usize::from(u16::from_be_bytes(len))];
    stream
        .read_exact(&mut reply)
        .await
        .map_err(|e| format!("read reply: {e}"))?;
    if is_dns_reply(&reply) {
        Ok(())
    } else {
        Err("answered, but not with our DNS answer".to_owned())
    }
}

/// Whether the sidecar is really serving DNS on `port`, both transports.
/// TCP and UDP are separate port spaces and challtestsrv binds one listener
/// on each, so half of it can be lost while the other half looks healthy.
async fn dns_answers(port: u16) -> Result<(), String> {
    match tokio::time::timeout(DNS_PROBE_TIMEOUT, dns_query_udp(port)).await {
        Ok(result) => result.map_err(|e| format!("udp {e}"))?,
        Err(_) => return Err(format!("udp silent for {DNS_PROBE_TIMEOUT:?}")),
    }
    match tokio::time::timeout(DNS_PROBE_TIMEOUT, dns_query_tcp(port)).await {
        Ok(result) => result.map_err(|e| format!("tcp {e}")),
        Err(_) => Err(format!("tcp silent for {DNS_PROBE_TIMEOUT:?}")),
    }
}

impl PebbleHandle {
    /// Spawn the pair, with Pebble issuing certificates valid for
    /// `validity_seconds`, and wait until the directory answers. The
    /// dynamic ports are picked bind-and-drop, so a concurrent process can
    /// steal one before the children claim it — a failed start is retried
    /// on fresh ports.
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

        let [acme_port, mgmt_port, chall_mgmt_port, dns_port] = free_ports::<4>();
        // The issued-certificate validity rides in the default profile —
        // Pebble ignores the legacy top-level certificateValidityPeriod
        // once profiles exist, and its no-profile default is 90 days.
        let config = serde_json::json!({
            "pebble": {
                "listenAddress": format!("127.0.0.1:{acme_port}"),
                "managementListenAddress": format!("127.0.0.1:{mgmt_port}"),
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
        let challtestsrv = Command::new(&challtestsrv_path)
            .arg("-management")
            .arg(format!(":{chall_mgmt_port}"))
            .arg("-dnsserver")
            .arg(format!("127.0.0.1:{dns_port}"))
            .arg("-doh")
            .arg("")
            .arg("-http01")
            .arg("")
            .arg("-https01")
            .arg("")
            .arg("-tlsalpn01")
            .arg("")
            .stdout(log("challtestsrv.log"))
            .stderr(log("challtestsrv.err.log"))
            .spawn()
            .expect("spawn pebble-challtestsrv");
        let pebble = Command::new(&pebble_path)
            .arg("-config")
            .arg(&config_path)
            .arg("-dnsserver")
            .arg(format!("127.0.0.1:{dns_port}"))
            .env("PEBBLE_VA_NOSLEEP", "1")
            .stdout(log("pebble.log"))
            .stderr(log("pebble.err.log"))
            .spawn()
            .expect("spawn pebble");

        let mut handle = Self {
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
        match handle.wait_ready(dns_port).await {
            Ok(()) => Ok(handle),
            Err(e) => Err(format!("{e}; children output:\n{}", handle.child_logs())),
        }
    }

    /// Poll the directory (through the minted CA), the challtestsrv
    /// management endpoint, and the sidecar's DNS listener until all three
    /// answer; bail out early when either child has already exited (a
    /// stolen port kills it at bind time).
    ///
    /// The DNS probe is the one the other two cannot stand in for. A
    /// challtestsrv that loses its DNS bind does not exit: it logs
    /// `address already in use` and keeps serving management, so an
    /// HTTP-only readiness check passes and the loss surfaces only when
    /// Pebble resolves a challenge mid-scenario — long past the retry that
    /// would have picked fresh ports.
    async fn wait_ready(&mut self, dns_port: u16) -> Result<(), String> {
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
            if !management_ready {
                // Any HTTP answer proves the management server is up.
                management_ready = plain.get(&self.management_url).send().await.is_ok();
            }
            if !dns_ready {
                match dns_answers(dns_port).await {
                    Ok(()) => dns_ready = true,
                    Err(e) => dns_error = e,
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
            "Pebble did not become ready within {READY_BUDGET:?} (directory {} \
             ready: {directory_ready}, management {} ready: {management_ready}, \
             DNS 127.0.0.1:{dns_port} ready: {dns_ready}; last directory error: \
             {directory_error}; last DNS error: {dns_error})",
            self.directory_url, self.management_url
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
