//! World for the doctor BDD suite: a scratch config directory, the
//! platform facts being staged, and the last run's captured output.

use std::path::PathBuf;
use std::process::Output;

use cucumber::World;
use doctor::facts::{Platform, PlatformFacts, UnitFacts};
use tempfile::TempDir;

#[derive(Debug, World)]
#[world(init = Self::new)]
pub struct DoctorWorld {
    /// Owns the scenario's scratch tree: `config/` plus the facts file.
    pub temp: TempDir,
    pub facts: PlatformFacts,
    /// PEM files staged by the TLS steps.
    pub pem_paths: Vec<PathBuf>,
    /// The data directory staged by the rp session steps.
    pub data_dir: Option<PathBuf>,
    /// Overrides the scenario's config dir for the exit-2 scenario.
    pub config_dir_override: Option<PathBuf>,
    pub output: Option<Output>,
    pub report: Option<serde_json::Value>,
    /// The check the last "contains a check" assertion matched, so
    /// follow-up detail/suggestion assertions have a subject.
    pub last_check: Option<serde_json::Value>,
    /// What each config file was staged with, for is-unchanged assertions.
    pub staged: std::collections::HashMap<String, String>,
    /// pki file bytes snapshotted by the "has already run" givens, for
    /// unchanged/changed assertions after a second provisioning run.
    pub pki_staged: std::collections::HashMap<String, Vec<u8>>,
    /// The ACME flag values the last `tls issue --acme` run passed, so the
    /// acme.json content assertions have expected values.
    pub acme_flags: Option<AcmeFlags>,
    /// HTTP status captured by the TLS roundtrip steps.
    pub tls_https_status: Option<u16>,
    /// The service whose cert pair the TLS roundtrip serves.
    pub tls_roundtrip_service: Option<String>,
    /// The port the last stub management endpoint bound (aggregation
    /// scenarios point the staged config's `server.port` at it).
    pub stub_port: Option<u16>,
    /// Shutdown handles keeping stub endpoints alive; dropped with the
    /// world at scenario end.
    pub stub_shutdowns: Vec<tokio::sync::oneshot::Sender<()>>,
    /// The stub per-service binary staged for the shell-out probe.
    pub stub_binary: Option<PathBuf>,
    /// The scenario's private Pebble + challtestsrv (killed on drop).
    pub pebble: Option<crate::pebble::PebbleHandle>,
    /// The file a staged post-renewal hook writes.
    pub pebble_marker: Option<PathBuf>,
    /// The in-process hot-reloading HTTPS server's bound address.
    pub hot_reload_addr: Option<std::net::SocketAddr>,
    /// Peer certificate DER captured before / after `doctor tls renew`.
    pub peer_cert_before: Option<Vec<u8>>,
    pub peer_cert_after: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct AcmeFlags {
    pub domain: String,
    pub email: String,
    pub dns_provider: String,
}

impl DoctorWorld {
    fn new() -> Self {
        let temp = TempDir::new().expect("scratch dir");
        std::fs::create_dir(temp.path().join("config")).expect("config dir");
        Self {
            temp,
            facts: PlatformFacts {
                platform: Platform::Linux,
                units: Vec::new(),
                polkit_grants_sentinel_restart: None,
                hardware: None,
                // A staged facts file is its scenario's whole truth: the
                // binary must never probe the BDD host underneath it.
                probe_hardware: false,
                dns: None,
                probe_dns: false,
            },
            pem_paths: Vec::new(),
            data_dir: None,
            config_dir_override: None,
            output: None,
            report: None,
            last_check: None,
            staged: std::collections::HashMap::new(),
            pki_staged: std::collections::HashMap::new(),
            acme_flags: None,
            tls_https_status: None,
            tls_roundtrip_service: None,
            stub_port: None,
            stub_shutdowns: Vec::new(),
            stub_binary: None,
            pebble: None,
            pebble_marker: None,
            hot_reload_addr: None,
            peer_cert_before: None,
            peer_cert_after: None,
        }
    }

    pub fn config_dir(&self) -> PathBuf {
        self.temp.path().join("config")
    }

    pub fn pki_dir(&self) -> PathBuf {
        self.config_dir().join("pki")
    }

    /// Absolute path (forward slashes on every platform) to a never-created
    /// directory inside the scenario's scratch dir. Feature files write the
    /// `{missing}` token where a config value must point at files that do
    /// not exist while staying genuinely absolute: a literal like
    /// `/nonexistent` is drive-relative on Windows, and doctor refuses to
    /// judge paths it cannot anchor.
    pub fn missing_dir(&self) -> String {
        self.temp
            .path()
            .join("missing")
            .to_str()
            .expect("utf8 scratch path")
            .replace('\\', "/")
    }

    /// Expand the `{missing}` token against this scenario's world.
    pub fn expand(&self, text: &str) -> String {
        text.replace("{missing}", &self.missing_dir())
    }

    /// The credential plaintext from the canonical pki copy.
    pub fn credential(&self) -> String {
        std::fs::read_to_string(self.pki_dir().join("credential"))
            .expect("pki/credential exists")
            .trim()
            .to_string()
    }

    /// Stage a CA with an arbitrary `not_after` into the pki tree, shaped
    /// like the one `doctor tls issue` mints, and snapshot the tree.
    pub fn stage_ca(&mut self, not_after: time::OffsetDateTime) {
        let pki = self.pki_dir();
        std::fs::create_dir_all(&pki).expect("pki dir");
        let mut params = rcgen::CertificateParams::default();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Rusty Photon Observatory CA");
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        params.not_before = not_after - time::Duration::days(3650);
        params.not_after = not_after;
        let key = rcgen::KeyPair::generate().expect("CA key");
        let cert = params.self_signed(&key).expect("CA cert");
        std::fs::write(pki.join("ca.pem"), cert.pem()).expect("ca.pem");
        std::fs::write(pki.join("ca-key.pem"), key.serialize_pem()).expect("ca-key.pem");
        self.snapshot_pki();
    }

    /// Stage a service pair with an arbitrary `not_after` and extra DNS
    /// SANs, signed by the staged CA exactly like doctor's issuance shapes
    /// its certs, and snapshot the tree. The pair's mtimes are backdated so
    /// a later re-issue is a visible change to the hot-reloading resolver.
    pub fn stage_service_pair(
        &mut self,
        service: &str,
        not_after: time::OffsetDateTime,
        extra_sans: &[&str],
    ) {
        let pki = self.pki_dir();
        let ca_cert_pem = std::fs::read_to_string(pki.join("ca.pem")).expect("stage the CA first");
        let ca_key_pem = std::fs::read_to_string(pki.join("ca-key.pem")).expect("ca-key.pem");
        let ca_key = rcgen::KeyPair::from_pem(&ca_key_pem).expect("CA key parses");
        let issuer = rcgen::Issuer::from_ca_cert_pem(&ca_cert_pem, &ca_key).expect("CA issuer");

        let mut sans = vec!["localhost".to_string()];
        sans.extend(extra_sans.iter().map(|s| (*s).to_string()));
        let mut params = rcgen::CertificateParams::new(sans).expect("SANs");
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, service);
        params.is_ca = rcgen::IsCa::ExplicitNoCa;
        params.use_authority_key_identifier_extension = true;
        params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        params.not_before = not_after - time::Duration::days(365);
        params.not_after = not_after;
        params
            .subject_alt_names
            .push(rcgen::SanType::IpAddress(std::net::IpAddr::V4(
                std::net::Ipv4Addr::LOCALHOST,
            )));
        params
            .subject_alt_names
            .push(rcgen::SanType::IpAddress(std::net::IpAddr::V6(
                std::net::Ipv6Addr::LOCALHOST,
            )));
        let key = rcgen::KeyPair::generate().expect("service key");
        let cert = params.signed_by(&key, &issuer).expect("service cert");

        let cert_path = pki.join(format!("{service}.pem"));
        let key_path = pki.join(format!("{service}-key.pem"));
        std::fs::write(&cert_path, cert.pem()).expect("service cert file");
        std::fs::write(&key_path, key.serialize_pem()).expect("service key file");
        for path in [&cert_path, &key_path] {
            let file = std::fs::File::options()
                .write(true)
                .open(path)
                .expect("reopen for backdating");
            file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_hours(1))
                .expect("backdate mtime");
        }
        self.snapshot_pki();
    }

    /// Stage a representative (not field-complete) `acme.json` so the
    /// provisioning pass sees an ACME install — doctor keys only on the
    /// file's presence and never parses it.
    pub fn stage_acme_config(&mut self, domain: &str) {
        let content = serde_json::json!({
            "email": format!("ops@{domain}"),
            "domain": domain,
            "dns_provider": "cloudflare",
            "dns_credentials": { "api_token": "test-token" },
            "staging": false,
            "renewal_days_before_expiry": 30,
            "post_renewal_hooks": [],
        });
        std::fs::write(
            self.config_dir().join("acme.json"),
            serde_json::to_string_pretty(&content).expect("acme.json serializes"),
        )
        .expect("acme.json");
    }

    /// Stage a stand-in ACME wildcard pair (`acme-cert.pem`/`acme-key.pem`)
    /// and snapshot the tree. The cert is rcgen self-signed — doctor only
    /// needs a parsable pair under the filenames a completed
    /// `tls issue --acme` order leaves, not a real ACME chain.
    pub fn stage_acme_pair(&mut self, domain: &str, not_after: time::OffsetDateTime) {
        let pki = self.pki_dir();
        std::fs::create_dir_all(&pki).expect("pki dir");
        let mut params =
            rcgen::CertificateParams::new(vec![format!("*.{domain}"), domain.to_string()])
                .expect("SANs");
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, format!("*.{domain}"));
        params.not_before = not_after - time::Duration::days(90);
        params.not_after = not_after;
        let key = rcgen::KeyPair::generate().expect("wildcard key");
        let cert = params.self_signed(&key).expect("wildcard cert");
        std::fs::write(pki.join("acme-cert.pem"), cert.pem()).expect("acme-cert.pem");
        std::fs::write(pki.join("acme-key.pem"), key.serialize_pem()).expect("acme-key.pem");
        self.snapshot_pki();
    }

    /// Snapshot every pki file's bytes for unchanged/changed assertions.
    pub fn snapshot_pki(&mut self) {
        self.pki_staged.clear();
        let dir = self.pki_dir();
        let entries = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("pki dir missing at snapshot time: {e}"));
        for entry in entries {
            let entry = entry.expect("pki dir entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            let bytes = std::fs::read(entry.path()).expect("pki file readable");
            self.pki_staged.insert(name, bytes);
        }
    }

    pub fn write_config(&mut self, name: &str, content: &str) {
        std::fs::write(self.config_dir().join(name), content)
            .unwrap_or_else(|e| panic!("writing {name}: {e}"));
        self.staged.insert(name.to_string(), content.to_string());
    }

    /// The current on-disk JSON of a staged config file.
    pub fn config_value(&self, name: &str) -> serde_json::Value {
        let content = std::fs::read_to_string(self.config_dir().join(name))
            .unwrap_or_else(|e| panic!("reading {name}: {e}"));
        serde_json::from_str(&content)
            .unwrap_or_else(|e| panic!("{name} is not valid JSON ({e}): {content}"))
    }

    pub fn add_unit(&mut self, name: &str) {
        self.facts.units.push(UnitFacts {
            name: name.to_string(),
            enabled: true,
            condition_path: None,
            source_name: None,
            supplementary_groups: Vec::new(),
            active: None,
            failed: None,
            binary_path: None,
        });
    }

    /// Stage aggregation facts on an already-added unit: its run state and
    /// (for the shell-out path) the binary to run.
    pub fn set_unit_probe_facts(
        &mut self,
        name: &str,
        active: bool,
        binary_path: Option<std::path::PathBuf>,
    ) {
        let unit = self
            .facts
            .units
            .iter_mut()
            .find(|u| u.name == name)
            .unwrap_or_else(|| panic!("no staged unit named {name}"));
        unit.active = Some(active);
        unit.binary_path = binary_path;
    }

    /// The staged hardware facts, created on first touch — scenarios that
    /// never call this keep `hardware` absent and the family skipped.
    pub fn hardware(&mut self) -> &mut rusty_photon_doctor_checks::HardwareFacts {
        self.facts
            .hardware
            .get_or_insert_with(rusty_photon_doctor_checks::HardwareFacts::default)
    }

    /// Run the doctor binary against the staged config dir and facts.
    pub async fn run_doctor(&mut self, json: bool) {
        self.run_doctor_args(json, false).await;
    }

    /// [`run_doctor`], optionally with `--fix`.
    pub async fn run_doctor_args(&mut self, json: bool, fix: bool) {
        let facts_path = self.temp.path().join("facts.json");
        std::fs::write(
            &facts_path,
            serde_json::to_string(&self.facts).expect("facts serialize"),
        )
        .expect("facts file");
        let config_dir = self
            .config_dir_override
            .clone()
            .unwrap_or_else(|| self.config_dir());

        let config_dir = config_dir.to_str().expect("utf8 path").to_string();
        let facts_path = facts_path.to_str().expect("utf8 path").to_string();
        let mut args = vec![
            "--config-dir",
            config_dir.as_str(),
            "--platform-facts",
            facts_path.as_str(),
        ];
        if json {
            args.push("--json");
        }
        if fix {
            args.push("--fix");
        }
        let output = bdd_infra::run_once_async("doctor", &args, None).await;
        self.report = if json && output.status.code() != Some(2) {
            Some(serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
                panic!(
                    "--json output is not valid JSON ({e}): {}",
                    String::from_utf8_lossy(&output.stdout)
                )
            }))
        } else {
            None
        };
        self.output = Some(output);
    }

    /// Run the doctor binary with a subcommand (`tls issue`, `auth rotate`,
    /// …) against the staged config dir and facts. The global
    /// `--config-dir`/`--platform-facts` flags precede the subcommand.
    pub async fn run_doctor_subcommand(&mut self, subcommand_args: &[&str], stdin: Option<&[u8]>) {
        let facts_path = self.temp.path().join("facts.json");
        std::fs::write(
            &facts_path,
            serde_json::to_string(&self.facts).expect("facts serialize"),
        )
        .expect("facts file");
        let config_dir = self
            .config_dir_override
            .clone()
            .unwrap_or_else(|| self.config_dir());
        let config_dir = config_dir.to_str().expect("utf8 path").to_string();
        let facts_path = facts_path.to_str().expect("utf8 path").to_string();
        let mut args = vec![
            "--config-dir",
            config_dir.as_str(),
            "--platform-facts",
            facts_path.as_str(),
        ];
        args.extend_from_slice(subcommand_args);
        let json = subcommand_args.contains(&"--json");
        let output = bdd_infra::run_once_async("doctor", &args, stdin).await;
        self.report = if json && output.status.code() == Some(0) {
            Some(serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
                panic!(
                    "--json output is not valid JSON ({e}): {}",
                    String::from_utf8_lossy(&output.stdout)
                )
            }))
        } else {
            None
        };
        self.output = Some(output);
    }

    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.as_ref().expect("run doctor first").stderr)
            .into_owned()
    }

    pub const fn report(&self) -> &serde_json::Value {
        self.report.as_ref().expect("run doctor with --json first")
    }

    pub fn checks(&self) -> &Vec<serde_json::Value> {
        self.report()
            .get("checks")
            .and_then(|c| c.as_array())
            .expect("report has a checks array")
    }

    pub fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.output.as_ref().expect("run doctor first").stdout)
            .into_owned()
    }
}
