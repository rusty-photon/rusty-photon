//! `cargo ci brew-formulas`: renders the Homebrew formulas for the
//! `rusty-photon/homebrew-rusty-photon` tap (docs/packaging-macos.md).
//!
//! One renderer serves both channels. `release.yml` renders the stable
//! formulas, `rusty-photon-<svc>.rb`, on a `v*` tag; `nightly-packages.yml`
//! renders their `-nightly` siblings on every publish; and
//! `cargo ci verify-brew` renders either with `file://` URLs against
//! just-built tarballs, which a developer runs too. The versions and
//! sha256s change with every run, so they are stamped here. What makes each
//! formula what it is stays in this repository: the packaged services
//! (`services/*/pkg`, the discovery the builders share), each service's
//! `Cargo.toml`
//! `description`, and the particulars below: which services load libusb,
//! which bundle a ZWO SDK dylib, sentinel's doctor and renewal job, and the
//! caveats.
//!
//! Each service's formula installs its arm64 tarball's binary (no source
//! build), gives `brew services` a launchd service that is kept alive, as
//! systemd's `Restart=on-failure` keeps the Linux unit, and conflicts with
//! its sibling on the other channel, which installs the same binary. The
//! meta-formula, `rusty-photon[-nightly]`, depends on the whole channel, for
//! a one-command install; a formula must download something, so its `url`
//! is the release's `SHA256SUMS.txt`.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use crate::build_tarballs::tarball;
use crate::packaging::{self, io};

/// The services whose binary loads Homebrew's libusb: qhy-camera links it
/// for the QHYCCD static library, and the ZWO camera library loads it by
/// `@rpath`, which `cargo ci build-tarballs` rewrites to Homebrew's copy. The EAF
/// focuser library uses the system's HID frameworks alone.
const NEEDS_LIBUSB: &[&str] = &["qhy-camera", "zwo-camera"];

/// The services whose tarball bundles a ZWO SDK dylib and its MIT license.
/// Their formula's license is the package's, MIT on top of the family's.
const ZWO_DYLIB: &[&str] = &["zwo-camera", "zwo-focuser"];

const HOMEPAGE: &str = "https://github.com/rusty-photon/rusty-photon";

/// Which tap formulas to render.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Channel {
    /// `rusty-photon-<svc>.rb`, from a release.
    Stable,
    /// `rusty-photon-<svc>-nightly.rb`, from the nightly prerelease.
    Nightly,
}

impl Channel {
    /// What the channel's formula and file names end with.
    const fn suffix(self) -> &'static str {
        match self {
            Self::Stable => "",
            Self::Nightly => "-nightly",
        }
    }

    /// What the channel's class names end with.
    const fn class_suffix(self) -> &'static str {
        match self {
            Self::Stable => "",
            Self::Nightly => "Nightly",
        }
    }

    /// The other channel's suffix: the formula each one conflicts with.
    const fn sibling_suffix(self) -> &'static str {
        match self {
            Self::Stable => Self::Nightly.suffix(),
            Self::Nightly => Self::Stable.suffix(),
        }
    }
}

/// What a run renders, and from what.
#[derive(Clone, Debug)]
pub struct Request {
    pub channel: Channel,
    /// The formulas' `version`: a release's, or the full nightly stamp,
    /// which Homebrew orders correctly.
    pub version: String,
    /// The tarballs, named `rusty-photon-<svc>-<version>-aarch64-apple-darwin.tar.gz`,
    /// and `SHA256SUMS.txt`, which the meta-formula downloads.
    pub dist: PathBuf,
    /// Where the formulas download from: a release's download URL, or
    /// `file://<dir>` to verify locally.
    pub url_base: String,
    /// Where the formulas are written.
    pub output: PathBuf,
    /// Only these services, and no meta-formula, which would depend on
    /// formulas not rendered. `None` renders every packaged service.
    pub services: Option<Vec<String>>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Packaging(#[from] packaging::Error),
    #[error("{path} missing — run `cargo ci build-tarballs` first")]
    MissingTarball { path: String },
    #[error("{path} missing — the meta-formula downloads the checksum manifest")]
    MissingSums { path: String },
    #[error("{service}: no description in services/{service}/Cargo.toml")]
    NoDescription { service: String },
}

/// A service's `Cargo.toml` `description`.
fn description(root: &Path, service: &str) -> Result<String, Error> {
    let path = root.join("services").join(service).join("Cargo.toml");
    let text = fs::read_to_string(&path).map_err(io(&path))?;
    text.parse::<toml::Table>()
        .ok()
        .and_then(|manifest| {
            manifest
                .get("package")?
                .get("description")?
                .as_str()
                .map(str::to_owned)
        })
        .filter(|description| !description.is_empty())
        .ok_or_else(|| Error::NoDescription {
            service: service.to_owned(),
        })
}

/// `text` as the inside of a Ruby double-quoted string.
fn ruby(text: &str) -> String {
    text.replace('\\', r"\\")
        .replace('"', "\\\"")
        .replace("#{", "\\#{")
}

/// Homebrew's class for a formula file name: each dash-separated word
/// capitalized, `rusty-photon-dsd-fp2` to `RustyPhotonDsdFp2`.
fn class_name(formula: &str) -> String {
    formula
        .split('-')
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        })
        .collect()
}

/// Doctor rides in sentinel's formula: no doctor formula exists. Its
/// renewal job is authored here rather than shipped in the tarball, so its
/// paths are this keg's; a formula manages one `brew services` daemon, so
/// the operator loads it (see the caveats).
const SENTINEL_INSTALL: &str = r#"    bin.install "rusty-photon-doctor"
    (prefix/"rusty-photon-renew.plist").write <<~EOS
      <?xml version="1.0" encoding="UTF-8"?>
      <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
      <plist version="1.0">
      <dict>
        <key>Label</key>
        <string>space.rustyphoton.renew</string>
        <key>ProgramArguments</key>
        <array>
          <string>#{opt_bin}/rusty-photon-doctor</string>
          <string>tls</string>
          <string>renew</string>
        </array>
        <key>StartCalendarInterval</key>
        <dict>
          <key>Hour</key>
          <integer>3</integer>
          <key>Minute</key>
          <integer>0</integer>
        </dict>
        <key>StandardOutPath</key>
        <string>#{var}/log/rusty-photon-renew.log</string>
        <key>StandardErrorPath</key>
        <string>#{var}/log/rusty-photon-renew.log</string>
      </dict>
      </plist>
    EOS
"#;

const QHY_CAMERA_CAVEATS: &str = r#"
  def caveats
    <<~EOS
      QHYCCD's proprietary SDK is linked statically; nothing to install.
      Caveat: a factory-fresh ("cold") QHY camera needs a one-time firmware
      upload that this build does not yet perform on macOS — connect a
      camera that has been used before (e.g. flashed on a Linux host), or
      see docs/packaging-macos.md in the rusty-photon repo.
    EOS
  end
"#;

const ZWO_FOCUSER_CAVEATS: &str = r"
  def caveats
    <<~EOS
      EAF discovery uses macOS HID APIs that need a privacy grant for the
      service binary; under `brew services` without one the service blocks
      at startup without serving. Grant it under System Settings → Privacy
      & Security (or run the binary once in a terminal), then restart the
      service. Details: docs/packaging-macos.md in the rusty-photon repo.
    EOS
  end
";

const SENTINEL_CAVEATS: &str = r"
  def caveats
    <<~EOS
      This formula also installs rusty-photon-doctor (the diagnosis and
      repair tool; there is no separate doctor formula). To arm the daily
      TLS renewal job — a no-op until `rusty-photon-doctor tls issue` has
      staged certificates — link and load the bundled launchd plist once
      (the opt path stays valid across upgrades, and launchd only
      auto-loads jobs from the LaunchAgents directory):
        ln -sfv #{opt_prefix}/rusty-photon-renew.plist ~/Library/LaunchAgents/
        launchctl bootstrap gui/$UID ~/Library/LaunchAgents/rusty-photon-renew.plist
      Details: docs/packaging-macos.md in the rusty-photon repo.
    EOS
  end
";

/// The first line of every formula, so no one edits one in the tap.
const GENERATED: &str = "# Generated by `cargo ci brew-formulas` — do not edit in the tap.\n";

/// The formula for one service. `sha256` is its tarball's.
fn formula(request: &Request, service: &str, description: &str, sha256: &str) -> String {
    let channel = request.channel;
    let sibling = channel.sibling_suffix();
    let class = class_name(&format!("rusty-photon-{service}"));
    let license = if ZWO_DYLIB.contains(&service) {
        r#"all_of: [{ any_of: ["MIT", "Apache-2.0"] }, "MIT"]"#
    } else {
        r#"any_of: ["MIT", "Apache-2.0"]"#
    };
    let mut text = String::from(GENERATED);
    let _ = write!(
        text,
        "class {class}{class_suffix} < Formula\n  desc \"{desc}\"\n  homepage \"{HOMEPAGE}\"\n  \
         version \"{version}\"\n  license {license}\n  url \"{url_base}/{tarball}\"\n  \
         sha256 \"{sha256}\"\n\n  depends_on :macos\n  depends_on arch: :arm64\n",
        class_suffix = channel.class_suffix(),
        desc = ruby(description),
        version = request.version,
        url_base = request.url_base,
        tarball = tarball(service, &request.version),
    );
    if NEEDS_LIBUSB.contains(&service) {
        text.push_str("  depends_on \"libusb\"\n");
    }
    let _ = write!(
        text,
        "\n  conflicts_with \"rusty-photon-{service}{sibling}\",\n    \
         because: \"both channels install the same rusty-photon-{service} binary\"\n\n  \
         def install\n    bin.install \"rusty-photon-{service}\"\n"
    );
    if ZWO_DYLIB.contains(&service) {
        text.push_str(
            "    lib.install Dir[\"lib/*.dylib\"]\n    doc.install \"ZWO-SDK-LICENSE.txt\"\n",
        );
    }
    if service == "sentinel" {
        text.push_str(SENTINEL_INSTALL);
    }
    let _ = write!(
        text,
        "  end\n\n  service do\n    run [opt_bin/\"rusty-photon-{service}\"]\n    keep_alive true\n    \
         log_path var/\"log/rusty-photon-{service}.log\"\n    \
         error_log_path var/\"log/rusty-photon-{service}.log\"\n  end\n"
    );
    text.push_str(match service {
        "qhy-camera" => QHY_CAMERA_CAVEATS,
        "zwo-focuser" => ZWO_FOCUSER_CAVEATS,
        "sentinel" => SENTINEL_CAVEATS,
        _ => "",
    });
    let _ = write!(
        text,
        "\n  test do\n    assert_match \"{service}\", shell_output(\"#{{bin}}/rusty-photon-{service} --help\")\n"
    );
    if service == "sentinel" {
        text.push_str(
            "    assert_match \"doctor\", shell_output(\"#{bin}/rusty-photon-doctor --help\")\n",
        );
    }
    text.push_str("  end\nend\n");
    text
}

/// The meta-formula, which depends on every service's. `sha256` is the
/// checksum manifest's.
fn meta_formula(request: &Request, services: &[String], sha256: &str) -> String {
    let channel = request.channel;
    let suffix = channel.suffix();
    let mut text = String::from(GENERATED);
    let _ = write!(
        text,
        "class RustyPhoton{class_suffix} < Formula\n  \
         desc \"Rusty Photon astrophotography service family (every service)\"\n  \
         homepage \"{HOMEPAGE}\"\n  version \"{version}\"\n  \
         license any_of: [\"MIT\", \"Apache-2.0\"]\n  \
         # A formula must download something; the channel's checksum manifest is\n  \
         # the natural tiny asset for a meta-formula that only pulls dependencies.\n  \
         url \"{url_base}/SHA256SUMS.txt\"\n  sha256 \"{sha256}\"\n\n  \
         depends_on :macos\n  depends_on arch: :arm64\n",
        class_suffix = channel.class_suffix(),
        version = request.version,
        url_base = request.url_base,
    );
    for service in services {
        let _ = writeln!(text, "  depends_on \"rusty-photon-{service}{suffix}\"");
    }
    let _ = write!(
        text,
        "\n  conflicts_with \"rusty-photon{sibling}\",\n    \
         because: \"both channels install the same service family\"\n\n  \
         def install\n    prefix.install \"SHA256SUMS.txt\"\n  end\n\n  \
         test do\n    assert_path_exists prefix/\"SHA256SUMS.txt\"\n  end\nend\n",
        sibling = channel.sibling_suffix(),
    );
    text
}

/// Renders the formulas `request` asks for, from the packaged services
/// under `root`, and returns the files it wrote, in order.
///
/// # Errors
///
/// When a service is not packaged or has no macOS tarball, a tarball, the
/// checksum manifest or a description is missing, or a file cannot be read
/// or written. Formulas written before the error stay written.
pub fn render(root: &Path, request: &Request) -> Result<Vec<PathBuf>, Error> {
    let packaged = packaging::packaged(root)?;
    let services = packaging::macos_services(&packaged, request.services.as_deref())?;
    fs::create_dir_all(&request.output).map_err(io(&request.output))?;
    let suffix = request.channel.suffix();
    let mut written = Vec::new();
    for service in &services {
        let path = request.dist.join(tarball(service, &request.version));
        if !path.is_file() {
            return Err(Error::MissingTarball {
                path: path.display().to_string(),
            });
        }
        let text = formula(
            request,
            service,
            &description(root, service)?,
            &packaging::sha256(&path)?,
        );
        let out = request
            .output
            .join(format!("rusty-photon-{service}{suffix}.rb"));
        fs::write(&out, text).map_err(io(&out))?;
        written.push(out);
    }
    if request.services.is_none() {
        let sums = request.dist.join("SHA256SUMS.txt");
        if !sums.is_file() {
            return Err(Error::MissingSums {
                path: sums.display().to_string(),
            });
        }
        let text = meta_formula(request, &services, &packaging::sha256(&sums)?);
        let out = request.output.join(format!("rusty-photon{suffix}.rb"));
        fs::write(&out, text).map_err(io(&out))?;
        written.push(out);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::packaging::hex;

    const VERSION: &str = "0.1.0+nightly.202610220507.gabc1234";
    const NIGHTLY: &str = "https://github.com/rusty-photon/rusty-photon/releases/download/nightly";
    const STABLE: &str = "https://github.com/rusty-photon/rusty-photon/releases/download/v0.1.0";
    const ZWO_CAMERA: &str = "ASCOM Alpaca Camera (+ EFW FilterWheel) driver for ZWO ASI hardware";

    fn request(channel: Channel, url_base: &str) -> Request {
        Request {
            channel,
            version: VERSION.to_owned(),
            dist: PathBuf::new(),
            url_base: url_base.to_owned(),
            output: PathBuf::new(),
            services: None,
        }
    }

    /// The SHA-256 of a test tarball, which holds its service's name, or of
    /// the test checksum manifest, which holds `sums`.
    fn sha_of(text: &str) -> String {
        hex(Sha256::digest(text.as_bytes()).as_slice())
    }

    // testdata/brew-formulas holds the formulas these inputs render to.
    #[test]
    fn renders_each_kind_of_service_as_the_shell_did() {
        let nightly = request(Channel::Nightly, NIGHTLY);
        for (service, description, expected) in [
            (
                "filemonitor",
                "File monitoring service for Rusty Photon",
                include_str!("../testdata/brew-formulas/rusty-photon-filemonitor-nightly.rb"),
            ),
            (
                "qhy-camera",
                "ASCOM Alpaca Camera (+ FilterWheel) driver for QHYCCD hardware",
                include_str!("../testdata/brew-formulas/rusty-photon-qhy-camera-nightly.rb"),
            ),
            (
                "zwo-camera",
                ZWO_CAMERA,
                include_str!("../testdata/brew-formulas/rusty-photon-zwo-camera-nightly.rb"),
            ),
            (
                "zwo-focuser",
                "ASCOM Alpaca Focuser driver for the ZWO EAF",
                include_str!("../testdata/brew-formulas/rusty-photon-zwo-focuser-nightly.rb"),
            ),
            (
                "sentinel",
                "Observatory monitoring and notification service",
                include_str!("../testdata/brew-formulas/rusty-photon-sentinel-nightly.rb"),
            ),
        ] {
            let sha256 = sha_of(&format!("{service}\n"));
            assert_eq!(
                formula(&nightly, service, description, &sha256),
                expected,
                "{service}"
            );
        }
        let stable = request(Channel::Stable, STABLE);
        assert_eq!(
            formula(&stable, "zwo-camera", ZWO_CAMERA, &sha_of("zwo-camera\n")),
            include_str!("../testdata/brew-formulas/rusty-photon-zwo-camera.rb")
        );
    }

    #[test]
    fn renders_the_meta_formula_as_the_shell_did() {
        let services = ["filemonitor", "sentinel", "zwo-camera"].map(str::to_owned);
        assert_eq!(
            meta_formula(
                &request(Channel::Nightly, NIGHTLY),
                &services,
                &sha_of("sums\n")
            ),
            include_str!("../testdata/brew-formulas/rusty-photon-nightly.rb")
        );
        assert_eq!(
            meta_formula(
                &request(Channel::Stable, STABLE),
                &services,
                &sha_of("sums\n")
            ),
            include_str!("../testdata/brew-formulas/rusty-photon.rb")
        );
    }

    #[test]
    fn writes_a_description_as_a_ruby_string() {
        assert_eq!(ruby(r#"a "b" \ #{c}"#), r#"a \"b\" \\ \#{c}"#);
        let text = formula(
            &request(Channel::Nightly, NIGHTLY),
            "rp",
            r#"The "rp" service"#,
            "0",
        );
        assert!(
            text.contains("  desc \"The \\\"rp\\\" service\"\n"),
            "{text}"
        );
    }

    /// A repository whose packaged services are `services`, each with a
    /// description, plus doctor, a member with no `pkg/`; and a dist with a
    /// tarball for each and the checksum manifest.
    struct Fixture {
        root: tempfile::TempDir,
        request: Request,
    }

    fn fixture(services: &[&str]) -> Fixture {
        let root = tempfile::tempdir().unwrap();
        for service in services {
            let dir = root.path().join("services").join(service);
            fs::create_dir_all(dir.join("pkg")).unwrap();
            let manifest = format!(
                "[package]\nname = \"{service}\"\ndescription = \"The {service} service\"\n"
            );
            fs::write(dir.join("Cargo.toml"), manifest).unwrap();
        }
        fs::create_dir_all(root.path().join("services/doctor")).unwrap();
        let dist = root.path().join("dist");
        fs::create_dir_all(&dist).unwrap();
        for service in services {
            fs::write(dist.join(tarball(service, VERSION)), format!("{service}\n")).unwrap();
        }
        fs::write(dist.join("SHA256SUMS.txt"), "sums\n").unwrap();
        let request = Request {
            dist,
            output: root.path().join("formulas"),
            ..request(Channel::Nightly, NIGHTLY)
        };
        Fixture { root, request }
    }

    fn names(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn renders_every_packaged_service_but_svbony_camera_then_the_meta_formula() {
        let fixture = fixture(&["zwo-camera", "filemonitor", "svbony-camera"]);
        let written = render(fixture.root.path(), &fixture.request).unwrap();
        assert_eq!(
            names(&written),
            [
                "rusty-photon-filemonitor-nightly.rb",
                "rusty-photon-zwo-camera-nightly.rb",
                "rusty-photon-nightly.rb",
            ]
        );
        let filemonitor = fs::read_to_string(&written[0]).unwrap();
        assert!(
            filemonitor.contains("  desc \"The filemonitor service\"\n"),
            "{filemonitor}"
        );
        let sha256 = format!("  sha256 \"{}\"\n", sha_of("filemonitor\n"));
        assert!(filemonitor.contains(&sha256), "{filemonitor}");
        let meta = fs::read_to_string(&written[2]).unwrap();
        let depends =
            "  depends_on arch: :arm64\n  depends_on \"rusty-photon-filemonitor-nightly\"\n  \
                       depends_on \"rusty-photon-zwo-camera-nightly\"\n\n";
        assert!(meta.contains(depends), "{meta}");
        assert!(
            meta.contains(&format!("  sha256 \"{}\"\n", sha_of("sums\n"))),
            "{meta}"
        );
    }

    #[test]
    fn names_the_stable_formulas_without_a_suffix() {
        let fixture = fixture(&["filemonitor"]);
        let request = Request {
            channel: Channel::Stable,
            ..fixture.request.clone()
        };
        let written = render(fixture.root.path(), &request).unwrap();
        assert_eq!(
            names(&written),
            ["rusty-photon-filemonitor.rb", "rusty-photon.rb"]
        );
    }

    #[test]
    fn renders_only_the_services_asked_for_in_that_order_and_no_meta_formula() {
        let fixture = fixture(&["filemonitor", "sentinel", "zwo-camera"]);
        let request = Request {
            services: Some(vec!["zwo-camera".to_owned(), "filemonitor".to_owned()]),
            ..fixture.request.clone()
        };
        let written = render(fixture.root.path(), &request).unwrap();
        assert_eq!(
            names(&written),
            [
                "rusty-photon-zwo-camera-nightly.rb",
                "rusty-photon-filemonitor-nightly.rb",
            ]
        );
    }

    #[test]
    fn refuses_a_service_that_is_not_packaged() {
        let fixture = fixture(&["filemonitor", "svbony-camera"]);
        let request = Request {
            services: Some(vec!["doctor".to_owned()]),
            ..fixture.request.clone()
        };
        let error = render(fixture.root.path(), &request).unwrap_err();
        assert_eq!(
            error.to_string(),
            "unknown or unpackaged service: doctor (packaged: filemonitor svbony-camera)"
        );
    }

    #[test]
    fn refuses_svbony_camera_which_has_no_macos_tarball() {
        let fixture = fixture(&["filemonitor", "svbony-camera"]);
        let request = Request {
            services: Some(vec!["svbony-camera".to_owned()]),
            ..fixture.request.clone()
        };
        let error = render(fixture.root.path(), &request).unwrap_err();
        assert!(
            matches!(error, Error::Packaging(packaging::Error::NoMacosSdk)),
            "{error}"
        );
    }

    #[test]
    fn refuses_a_missing_tarball() {
        let fixture = fixture(&["filemonitor"]);
        let tarball = fixture.request.dist.join(tarball("filemonitor", VERSION));
        fs::remove_file(&tarball).unwrap();
        let error = render(fixture.root.path(), &fixture.request).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "{} missing — run `cargo ci build-tarballs` first",
                tarball.display()
            )
        );
    }

    #[test]
    fn refuses_a_missing_checksum_manifest() {
        let fixture = fixture(&["filemonitor"]);
        let sums = fixture.request.dist.join("SHA256SUMS.txt");
        fs::remove_file(&sums).unwrap();
        let error = render(fixture.root.path(), &fixture.request).unwrap_err();
        assert!(matches!(error, Error::MissingSums { .. }), "{error}");
    }

    #[test]
    fn refuses_a_service_without_a_description() {
        let fixture = fixture(&["filemonitor"]);
        let manifest = fixture.root.path().join("services/filemonitor/Cargo.toml");
        fs::write(&manifest, "[package]\nname = \"filemonitor\"\n").unwrap();
        let error = render(fixture.root.path(), &fixture.request).unwrap_err();
        assert_eq!(
            error.to_string(),
            "filemonitor: no description in services/filemonitor/Cargo.toml"
        );
    }
}
