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
        match handle.wait_ready().await {
            Ok(()) => Ok(handle),
            Err(e) => Err(format!("{e}; children output:\n{}", handle.child_logs())),
        }
    }

    /// Poll the directory (through the minted CA) and the challtestsrv
    /// management endpoint until both answer; bail out early when either
    /// child has already exited (a stolen port kills it at bind time).
    async fn wait_ready(&mut self) -> Result<(), String> {
        let client = rusty_photon_tls::client::build_reqwest_client(Some(&self.ca_pem))
            .expect("client trusting the pebble CA");
        let plain = reqwest::Client::new();
        let mut directory_error = String::new();
        let mut directory_ready = false;
        let mut management_ready = false;
        for _ in 0..150 {
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
            if directory_ready && management_ready {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        Err(format!(
            "Pebble did not become ready (directory {} ready: {directory_ready}, \
             management {} ready: {management_ready}; last directory error: \
             {directory_error})",
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
