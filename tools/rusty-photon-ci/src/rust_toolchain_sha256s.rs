//! `cargo ci rust-toolchain-sha256s <release>`: prints the `sha256s` entries
//! of `MODULE.bazel`'s `rust.toolchain` for one Rust release, either of the
//! two it pins:
//!
//! ```text
//! cargo ci rust-toolchain-sha256s 1.99.0              # the stable pin
//! cargo ci rust-toolchain-sha256s nightly/2026-08-15  # the coverage nightly
//! ```
//!
//! `rules_rust` pins the archives of the releases it knows (`known_shas.bzl`):
//! the stable releases up to its own, and a few nightly dates. Any other
//! release downloads unverified unless `MODULE.bazel` names its hashes. They
//! are taken here from the release's channel manifest (the `xz_hash` of each
//! package), after checking the manifest's detached signature against the
//! Rust release key, whose fingerprint is pinned below rather than trusted
//! from the same server that serves the manifest. A nightly's manifest is
//! signed by the same key. For a nightly, once every archive has passed, its
//! rustc version goes to stderr as the comment line that sits directly above
//! the nightly entry in `MODULE.bazel`, ready to paste.
//!
//! `curl` downloads and `gpg` checks the signature, so the developer needs
//! both on their `PATH`. A crate that checked the signature itself would do
//! without `gpg`, and bring its own cryptography into every build of `//...`,
//! for a tool run once a Rust release.
//!
//! [`Release::archives`] is the list of archives. `tools/ci/bazel-clippy.sh`
//! keeps its own copy, to check that `sha256s` holds exactly these; keep the
//! two in step.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;

use crate::process::{self, Cmd, Runner};

/// Rust Language (Tag and Release Signing Key) <rust-key@rust-lang.org>
const FINGERPRINT: &str = "108F66205EAEB0AAA8DD5E1C85AB96E6FA1BE5FE";

const SERVER: &str = "https://static.rust-lang.org";

/// The release key, as the server publishes it.
const KEY: &str = "rust-key.gpg.ascii";

/// Each manifest package `rules_rust` fetches for a host, and its tool name,
/// the archive's file-name prefix.
const TOOLS: [(&str, &str); 7] = [
    ("rustc", "rustc"),
    ("cargo", "cargo"),
    ("clippy-preview", "clippy"),
    ("rustfmt-preview", "rustfmt"),
    ("llvm-tools-preview", "llvm-tools"),
    ("rust-std", "rust-std"),
    ("rust-analyzer-preview", "rust-analyzer"),
];

/// The hosts CI and the developers' machines run on.
const HOSTS: [&str; 5] = [
    "aarch64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
];

/// A Rust release `MODULE.bazel` can pin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Release {
    /// A stable release: `1.99.0`.
    Stable(String),
    /// A nightly, by its date: `2026-08-15`, for `nightly/2026-08-15`.
    Nightly(String),
}

/// `1.99.0`, or `nightly/2026-08-15`: the same as `rust.toolchain`'s
/// `versions` names it.
impl FromStr for Release {
    type Err = String;

    fn from_str(release: &str) -> Result<Self, String> {
        match release.strip_prefix("nightly/") {
            Some(date) if is_date(date) => Ok(Self::Nightly(date.to_owned())),
            None if is_version(release) => Ok(Self::Stable(release.to_owned())),
            _ => Err(format!(
                "expected a stable version (1.99.0) or nightly/<date> (nightly/2026-08-15), not {release:?}"
            )),
        }
    }
}

impl fmt::Display for Release {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stable(version) => f.write_str(version),
            Self::Nightly(date) => write!(f, "nightly/{date}"),
        }
    }
}

/// Three dot-separated numbers.
fn is_version(release: &str) -> bool {
    let parts: Vec<&str> = release.split('.').collect();
    parts.len() == 3 && parts.iter().all(|part| is_number(part))
}

/// `YYYY-MM-DD`.
fn is_date(date: &str) -> bool {
    let parts: Vec<&str> = date.split('-').collect();
    parts.iter().map(|part| part.len()).eq([4, 2, 2]) && parts.iter().all(|part| is_number(part))
}

fn is_number(part: &str) -> bool {
    !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
}

/// One archive of a release, and where its manifest lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Archive {
    /// Its manifest package: `clippy-preview`.
    pub package: &'static str,
    /// Its manifest target: a host, or `*` for rust-src.
    pub target: &'static str,
    /// Its file name: `clippy-1.99.0-x86_64-unknown-linux-gnu.tar.xz`.
    pub name: String,
    /// Its key in `sha256s`.
    pub key: String,
}

impl Release {
    /// The channel manifest's path on the server.
    fn manifest(&self) -> String {
        match self {
            Self::Stable(version) => format!("dist/channel-rust-{version}.toml"),
            Self::Nightly(date) => format!("dist/{date}/channel-rust-nightly.toml"),
        }
    }

    /// The archives `rules_rust` fetches for the release, which `sha256s`
    /// holds the hashes of: each of [`TOOLS`] for each of [`HOSTS`], and
    /// rust-src, one archive for every host, which `rules_rust` fetches for its
    /// rust-analyzer toolchain.
    ///
    /// An archive's key is its name, except for a nightly's. `rules_rust`
    /// names those `<tool>-nightly-<host>.tar.xz` and keys them by date:
    /// `<date>/<tool>-nightly-<host>.tar.xz`. All but rust-src: `load_rust_src`
    /// looks it up by `produce_tool_path`, which has no date, so its key is
    /// `rust-src-nightly.tar.xz`.
    #[must_use]
    pub fn archives(&self) -> Vec<Archive> {
        let (version, date) = match self {
            Self::Stable(version) => (version.as_str(), None),
            Self::Nightly(date) => ("nightly", Some(date)),
        };
        let mut archives = Vec::new();
        for (package, tool) in TOOLS {
            for host in HOSTS {
                let name = format!("{tool}-{version}-{host}.tar.xz");
                let key = date.map_or_else(|| name.clone(), |date| format!("{date}/{name}"));
                archives.push(Archive {
                    package,
                    target: host,
                    name,
                    key,
                });
            }
        }
        let name = format!("rust-src-{version}.tar.xz");
        archives.push(Archive {
            package: "rust-src",
            target: "*",
            key: name.clone(),
            name,
        });
        archives
    }
}

/// What the tool prints.
#[derive(Debug, PartialEq, Eq)]
pub struct Sha256s {
    /// For a nightly, the comment line that goes directly above its entry in
    /// `MODULE.bazel`: `# nightly/<date> is rustc <version>`. For stderr.
    pub comment: Option<String>,
    /// The entries, a line each, ordered by key. For stdout.
    pub entries: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Process(#[from] process::Error),
    #[error("could not write {}: {source}", path.display())]
    Write { path: PathBuf, source: io::Error },
    #[error("{} is not valid Unicode, as the paths the tool gives gpg must be", path.display())]
    NotUnicode { path: PathBuf },
    #[error("{KEY} has fingerprint {found}, expected {FINGERPRINT}")]
    Fingerprint { found: String },
    #[error("{manifest} is not signed by the Rust release key{}", gpg_says(log))]
    NotSigned { manifest: String, log: String },
    #[error("{manifest} is not a channel manifest: {source}")]
    Manifest {
        manifest: String,
        source: toml::de::Error,
    },
    #[error("the manifest is dated {found}, not {expected}")]
    Date { found: String, expected: String },
    #[error("{package} {release} has no{} build", on(target, " "))]
    NoBuild {
        package: &'static str,
        release: Release,
        target: &'static str,
    },
    #[error("unexpected archive name {found} for {package}{}", on(target, " on "))]
    Name {
        found: String,
        package: &'static str,
        target: &'static str,
    },
}

/// What gpg logged, below the error that names it.
fn gpg_says(log: &str) -> String {
    let log = log.trim_end();
    if log.is_empty() {
        String::new()
    } else {
        format!("; gpg says:\n{log}")
    }
}

/// `target` after `prefix`, or nothing for rust-src's `*`.
fn on(target: &str, prefix: &str) -> String {
    if target == "*" {
        String::new()
    } else {
        format!("{prefix}{target}")
    }
}

/// Downloads the release's manifest, checks its signature and returns what to
/// print.
///
/// # Errors
///
/// When a download or `gpg` fails, the release key or the manifest's
/// signature is not the pinned key's, or the manifest lacks an archive or
/// names one otherwise than `rules_rust` does. Nothing is printed then.
pub fn sha256s(runner: &mut impl Runner, release: &Release) -> Result<Sha256s, Error> {
    let work = tempfile::tempdir().map_err(|source| Error::Write {
        path: std::env::temp_dir(),
        source,
    })?;
    sha256s_in(runner, work.path(), release)
}

/// [`sha256s`], with its downloads and gpg's home in `work`.
fn sha256s_in(runner: &mut impl Runner, work: &Path, release: &Release) -> Result<Sha256s, Error> {
    let manifest = release.manifest();
    let url = format!("{SERVER}/{manifest}");
    let text = download(runner, &url)?;
    let signature = download(runner, &format!("{url}.asc"))?;
    let key = download(runner, &format!("{SERVER}/{KEY}"))?;

    // gpg checks the files written from the text parsed below, so what it
    // passes is what the entries come from.
    let manifest_file = write(work, "manifest.toml", &text)?;
    let signature_file = write(work, "manifest.toml.asc", &signature)?;
    let key_file = write(work, "rust-key.asc", &key)?;
    let home = within(work, "gnupg")?;
    create_private_dir(Path::new(&home)).map_err(|source| Error::Write {
        path: PathBuf::from(&home),
        source,
    })?;
    // gpg's messages go to a log, not the terminal: a good signature by a key
    // nobody certified draws a warning, which the pinned fingerprint makes
    // moot. A refused signature quotes the log.
    let import_log = within(work, "import.log")?;
    let verify_log = within(work, "verify.log")?;

    runner.run(&gpg(
        &home,
        &["--log-file", &import_log, "--quiet", "--import", &key_file],
    ))?;
    let listing = runner.read(&gpg(&home, &["--with-colons", "--fingerprint"]))?;
    let found = first_fingerprint(&listing).unwrap_or_default();
    if found != FINGERPRINT {
        return Err(Error::Fingerprint {
            found: found.to_owned(),
        });
    }

    // gpg exits 0 for a good signature by any key in the keyring, and says so
    // on its status lines: VALIDSIG names the key that signed, so a signature
    // by any other key cannot pass.
    let verify = gpg(
        &home,
        &[
            "--log-file",
            &verify_log,
            "--status-fd",
            "1",
            "--verify",
            &signature_file,
            &manifest_file,
        ],
    );
    let not_signed = || Error::NotSigned {
        manifest: manifest.clone(),
        log: fs::read_to_string(&verify_log).unwrap_or_default(),
    };
    match runner.read(&verify) {
        Ok(status) if signed_by_the_release_key(&status) => {}
        Ok(_) | Err(process::Error::Failed { .. }) => return Err(not_signed()),
        Err(error) => return Err(error.into()),
    }

    entries(&manifest, &text, release)
}

fn download(runner: &mut impl Runner, url: &str) -> Result<String, Error> {
    Ok(runner.read(&Cmd::new("curl", &["-fsSL", url]))?)
}

/// `name` in `work`, as an argument for a command line.
fn within(work: &Path, name: &str) -> Result<String, Error> {
    let path = work.join(name);
    path.to_str()
        .map(str::to_owned)
        .ok_or(Error::NotUnicode { path })
}

/// Writes `contents` to `name` in `work`, and returns its path.
fn write(work: &Path, name: &str, contents: &str) -> Result<String, Error> {
    let path = within(work, name)?;
    fs::write(&path, contents).map_err(|source| Error::Write {
        path: PathBuf::from(&path),
        source,
    })?;
    Ok(path)
}

/// gpg warns about a home others can read.
#[cfg(unix)]
fn create_private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(path)
}

/// Windows has no mode bits for gpg to check.
#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

/// A gpg command in `home`. Checking a signature needs no gpg-agent, so
/// `--no-autostart` starts none.
fn gpg(home: &str, args: &[&str]) -> Cmd {
    let mut all = vec!["--homedir", home, "--no-autostart"];
    all.extend_from_slice(args);
    Cmd::new("gpg", &all)
}

/// The fingerprint of the first key in gpg's `--with-colons` listing: field
/// 10 of its first `fpr` record.
fn first_fingerprint(listing: &str) -> Option<&str> {
    listing
        .lines()
        .find(|line| line.starts_with("fpr:"))
        .and_then(|line| line.split(':').nth(9))
}

fn signed_by_the_release_key(status: &str) -> bool {
    let validsig = format!("[GNUPG:] VALIDSIG {FINGERPRINT} ");
    status.lines().any(|line| line.starts_with(&validsig))
}

/// The parts of a channel manifest the tool reads.
#[derive(Deserialize)]
struct Manifest {
    #[serde(default)]
    date: String,
    #[serde(default)]
    pkg: BTreeMap<String, Package>,
}

#[derive(Deserialize)]
struct Package {
    #[serde(default)]
    version: String,
    #[serde(default)]
    target: BTreeMap<String, Target>,
}

#[derive(Deserialize)]
struct Target {
    #[serde(default)]
    available: bool,
    xz_url: Option<String>,
    xz_hash: Option<String>,
}

/// The entries for `release` from its manifest's text. `manifest` is its
/// path, for errors.
fn entries(manifest: &str, text: &str, release: &Release) -> Result<Sha256s, Error> {
    let parsed: Manifest = toml::from_str(text).map_err(|source| Error::Manifest {
        manifest: manifest.to_owned(),
        source,
    })?;
    if let Release::Nightly(date) = release {
        if parsed.date != *date {
            return Err(Error::Date {
                found: parsed.date,
                expected: date.clone(),
            });
        }
    }
    let mut entries = BTreeMap::new();
    for archive in release.archives() {
        let target = parsed
            .pkg
            .get(archive.package)
            .and_then(|package| package.target.get(archive.target))
            .filter(|target| target.available);
        let Some((url, hash)) =
            target.and_then(|target| target.xz_url.as_ref().zip(target.xz_hash.as_ref()))
        else {
            return Err(Error::NoBuild {
                package: archive.package,
                release: release.clone(),
                target: archive.target,
            });
        };
        let found = url.rsplit('/').next().unwrap_or_default();
        if found != archive.name {
            return Err(Error::Name {
                found: found.to_owned(),
                package: archive.package,
                target: archive.target,
            });
        }
        entries.insert(archive.key, hash.as_str());
    }
    // Only now, so a refused release prints an error and no line to paste.
    let comment = match release {
        Release::Stable(_) => None,
        Release::Nightly(date) => {
            let rustc = parsed
                .pkg
                .get("rustc")
                .map_or("", |rustc| rustc.version.as_str());
            Some(format!("        # nightly/{date} is rustc {rustc}"))
        }
    };
    let entries = entries
        .iter()
        .fold(String::new(), |mut lines, (key, hash)| {
            // Writing to a String cannot fail.
            let _ = writeln!(lines, "        \"{key}\": \"{hash}\",");
            lines
        });
    Ok(Sha256s { comment, entries })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::Fake;

    const STABLE: &str = include_str!("../testdata/channel-rust-1.99.0.toml");
    const NIGHTLY: &str = include_str!("../testdata/channel-rust-nightly-2026-08-15.toml");
    // The entries MODULE.bazel pinned for these releases when the manifests
    // were saved. The whole manifests give the same.
    const STABLE_ENTRIES: &str = include_str!("../testdata/channel-rust-1.99.0.sha256s");
    const NIGHTLY_ENTRIES: &str =
        include_str!("../testdata/channel-rust-nightly-2026-08-15.sha256s");

    const SIGNATURE: &str = "-----BEGIN PGP SIGNATURE-----\n\n-----END PGP SIGNATURE-----\n";
    const RELEASE_KEY: &str =
        "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\n-----END PGP PUBLIC KEY BLOCK-----\n";

    /// gpg's `--with-colons --fingerprint` for the release key: the primary
    /// key, then its two subkeys.
    const LISTING: &str = "tru::1:1792501958:0:3:1:5\n\
        pub:-:4096:1:85AB96E6FA1BE5FE:1380237513:::-:::scESC::::::23::0:\n\
        fpr:::::::::108F66205EAEB0AAA8DD5E1C85AB96E6FA1BE5FE:\n\
        uid:-::::1380237513::1E2B616E2358AAAC3CB550DA7D232AEE931D2BC3::Rust Language (Tag and Release Signing Key) <rust-key@rust-lang.org>::::::::::0:\n\
        sub:-:4096:1:8E9AA3F7AB3F5826:1380237513::::::e::::::23:\n\
        fpr:::::::::29D57CE15D76E78CD57D326A8E9AA3F7AB3F5826:\n\
        sub:-:4096:1:5CB4A9347B3B09DC:1418683512::::::s::::::23:\n\
        fpr:::::::::C13466B7E169A085188632165CB4A9347B3B09DC:\n";

    /// gpg's status lines for a good signature by the release key.
    const GOOD: &str = "[GNUPG:] NEWSIG\n\
        [GNUPG:] KEY_CONSIDERED 108F66205EAEB0AAA8DD5E1C85AB96E6FA1BE5FE 0\n\
        [GNUPG:] SIG_ID DnTstf03H8/b/zNjp3YNN6AF/w4 2026-10-01 1790857175\n\
        [GNUPG:] GOODSIG 85AB96E6FA1BE5FE Rust Language (Tag and Release Signing Key) <rust-key@rust-lang.org>\n\
        [GNUPG:] VALIDSIG 108F66205EAEB0AAA8DD5E1C85AB96E6FA1BE5FE 2026-10-01 1790857175 0 4 0 1 10 00 108F66205EAEB0AAA8DD5E1C85AB96E6FA1BE5FE\n\
        [GNUPG:] TRUST_UNDEFINED 0 pgp\n";

    fn stable() -> Release {
        Release::Stable("1.99.0".to_owned())
    }

    fn nightly() -> Release {
        Release::Nightly("2026-08-15".to_owned())
    }

    /// A runner that downloads `manifest`, the signature and the key, and
    /// answers gpg's listing and verification with `listing` and `status`.
    fn answering(manifest: &'static str, listing: &'static str, status: &'static str) -> Fake {
        Fake::answering(&[manifest, SIGNATURE, RELEASE_KEY, listing, status])
    }

    fn run(fake: &mut Fake, release: &Release) -> (tempfile::TempDir, Result<Sha256s, Error>) {
        let work = tempfile::tempdir().unwrap();
        let result = sha256s_in(fake, work.path(), release);
        (work, result)
    }

    #[test]
    fn a_release_is_a_stable_version_or_a_nightly_date() {
        assert_eq!("1.99.0".parse(), Ok(stable()));
        assert_eq!(
            "1.100.12".parse(),
            Ok(Release::Stable("1.100.12".to_owned()))
        );
        assert_eq!("nightly/2026-08-15".parse(), Ok(nightly()));
    }

    #[test]
    fn anything_else_is_refused_naming_both_forms() {
        for release in [
            "",
            "1.99",
            "1.99.0.1",
            "1..0",
            "1.99.x",
            "1.99.0-beta",
            "v1.99.0",
            "nightly",
            "nightly/",
            "nightly/1.99.0",
            "nightly/2026-8-15",
            "nightly/2026-08-15/",
            "nightly/2026-08-1x",
            "beta/2026-08-15",
            "2026-08-15",
        ] {
            let error = release.parse::<Release>().unwrap_err();
            assert_eq!(
                error,
                format!(
                    "expected a stable version (1.99.0) or nightly/<date> (nightly/2026-08-15), not {release:?}"
                ),
                "{release}"
            );
        }
    }

    #[test]
    fn a_release_displays_as_it_is_pinned() {
        assert_eq!(stable().to_string(), "1.99.0");
        assert_eq!(nightly().to_string(), "nightly/2026-08-15");
    }

    #[test]
    fn a_stable_release_has_each_tool_for_each_host_and_rust_src() {
        let archives = stable().archives();

        assert_eq!(archives.len(), TOOLS.len() * HOSTS.len() + 1);
        assert_eq!(
            archives.first().unwrap(),
            &Archive {
                package: "rustc",
                target: "aarch64-apple-darwin",
                name: "rustc-1.99.0-aarch64-apple-darwin.tar.xz".to_owned(),
                key: "rustc-1.99.0-aarch64-apple-darwin.tar.xz".to_owned(),
            }
        );
        assert_eq!(
            archives.last().unwrap(),
            &Archive {
                package: "rust-src",
                target: "*",
                name: "rust-src-1.99.0.tar.xz".to_owned(),
                key: "rust-src-1.99.0.tar.xz".to_owned(),
            }
        );
    }

    #[test]
    fn a_nightly_keys_its_archives_by_date_except_rust_src() {
        let archives = nightly().archives();

        let clippy = archives
            .iter()
            .find(|archive| {
                archive.package == "clippy-preview" && archive.target == "x86_64-pc-windows-msvc"
            })
            .unwrap();
        assert_eq!(clippy.name, "clippy-nightly-x86_64-pc-windows-msvc.tar.xz");
        assert_eq!(
            clippy.key,
            "2026-08-15/clippy-nightly-x86_64-pc-windows-msvc.tar.xz"
        );
        let rust_src = archives.last().unwrap();
        assert_eq!(rust_src.name, "rust-src-nightly.tar.xz");
        assert_eq!(rust_src.key, "rust-src-nightly.tar.xz");
    }

    #[test]
    fn prints_the_stable_releases_entries_from_its_manifest() {
        let mut fake = answering(STABLE, LISTING, GOOD);

        let (_work, result) = run(&mut fake, &stable());

        assert_eq!(
            result.unwrap(),
            Sha256s {
                comment: None,
                entries: STABLE_ENTRIES.to_owned(),
            }
        );
    }

    #[test]
    fn prints_a_nightlys_entries_and_its_rustc_version_from_its_manifest() {
        let mut fake = answering(NIGHTLY, LISTING, GOOD);

        let (_work, result) = run(&mut fake, &nightly());

        assert_eq!(
            result.unwrap(),
            Sha256s {
                comment: Some(
                    "        # nightly/2026-08-15 is rustc 1.99.0-nightly (d453bdd8f 2026-08-14)"
                        .to_owned()
                ),
                entries: NIGHTLY_ENTRIES.to_owned(),
            }
        );
    }

    #[test]
    fn downloads_then_imports_the_key_then_checks_its_fingerprint_then_the_signature() {
        let mut fake = answering(STABLE, LISTING, GOOD);

        let (work, result) = run(&mut fake, &stable());

        result.unwrap();
        let path = |name: &str| work.path().join(name).to_str().unwrap().to_owned();
        let gpg = format!("gpg --homedir {} --no-autostart", path("gnupg"));
        assert_eq!(
            fake.lines(),
            [
                "curl -fsSL https://static.rust-lang.org/dist/channel-rust-1.99.0.toml".to_owned(),
                "curl -fsSL https://static.rust-lang.org/dist/channel-rust-1.99.0.toml.asc"
                    .to_owned(),
                "curl -fsSL https://static.rust-lang.org/rust-key.gpg.ascii".to_owned(),
                format!(
                    "{gpg} --log-file {} --quiet --import {}",
                    path("import.log"),
                    path("rust-key.asc")
                ),
                format!("{gpg} --with-colons --fingerprint"),
                format!(
                    "{gpg} --log-file {} --status-fd 1 --verify {} {}",
                    path("verify.log"),
                    path("manifest.toml.asc"),
                    path("manifest.toml")
                ),
            ]
        );
    }

    #[test]
    fn downloads_a_nightlys_manifest_from_its_date() {
        let mut fake = answering(NIGHTLY, LISTING, GOOD);

        let (_work, result) = run(&mut fake, &nightly());

        result.unwrap();
        assert_eq!(
            fake.lines().get(..2).unwrap(),
            [
                "curl -fsSL https://static.rust-lang.org/dist/2026-08-15/channel-rust-nightly.toml",
                "curl -fsSL https://static.rust-lang.org/dist/2026-08-15/channel-rust-nightly.toml.asc",
            ]
        );
    }

    #[test]
    fn gpg_checks_the_text_the_entries_come_from() {
        let mut fake = answering(STABLE, LISTING, GOOD);

        let (work, result) = run(&mut fake, &stable());

        result.unwrap();
        let read = |name: &str| fs::read_to_string(work.path().join(name)).unwrap();
        assert_eq!(read("manifest.toml"), STABLE);
        assert_eq!(read("manifest.toml.asc"), SIGNATURE);
        assert_eq!(read("rust-key.asc"), RELEASE_KEY);
    }

    #[cfg(unix)]
    #[test]
    fn gpgs_home_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let mut fake = answering(STABLE, LISTING, GOOD);

        let (work, result) = run(&mut fake, &stable());

        result.unwrap();
        let mode = fs::metadata(work.path().join("gnupg"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");
    }

    #[test]
    fn refuses_a_key_with_another_fingerprint_before_checking_the_signature() {
        let listing = "pub:-:4096:1:0123456789ABCDEF:1380237513:::-:::scESC::::::23::0:\n\
            fpr:::::::::00000000000000000000000000000123456789ABCDEF:\n\
            fpr:::::::::108F66205EAEB0AAA8DD5E1C85AB96E6FA1BE5FE:\n";
        let mut fake = answering(STABLE, listing, GOOD);

        let (_work, result) = run(&mut fake, &stable());

        assert_eq!(
            result.unwrap_err().to_string(),
            "rust-key.gpg.ascii has fingerprint 00000000000000000000000000000123456789ABCDEF, expected 108F66205EAEB0AAA8DD5E1C85AB96E6FA1BE5FE"
        );
        assert_eq!(fake.ran.len(), 5, "{:?}", fake.lines());
    }

    #[test]
    fn refuses_a_key_file_with_no_key() {
        let mut fake = answering(STABLE, "tru::1:1792501958:0:3:1:5\n", GOOD);

        let (_work, result) = run(&mut fake, &stable());

        assert!(
            matches!(result, Err(Error::Fingerprint { ref found }) if found.is_empty()),
            "{result:?}"
        );
    }

    #[test]
    fn refuses_a_good_signature_by_another_key() {
        let status = "[GNUPG:] NEWSIG\n\
            [GNUPG:] GOODSIG 0123456789ABCDEF Someone Else <someone@example.com>\n\
            [GNUPG:] VALIDSIG 0000000000000000000000000123456789ABCDEF 2026-10-01 1790857175 0 4 0 1 10 00 108F66205EAEB0AAA8DD5E1C85AB96E6FA1BE5FE\n";
        let mut fake = answering(STABLE, LISTING, status);

        let (_work, result) = run(&mut fake, &stable());

        assert_eq!(
            result.unwrap_err().to_string(),
            "dist/channel-rust-1.99.0.toml is not signed by the Rust release key"
        );
    }

    #[test]
    fn the_signing_key_must_be_the_whole_fingerprint() {
        for status in [
            "[GNUPG:] VALIDSIG 108F66205EAEB0AAA8DD5E1C85AB96E6FA1BE5FE00 2026-10-01\n",
            "[GNUPG:] VALIDSIG 08F66205EAEB0AAA8DD5E1C85AB96E6FA1BE5FE 2026-10-01\n",
            "[GNUPG:] GOODSIG 108F66205EAEB0AAA8DD5E1C85AB96E6FA1BE5FE \n",
            " [GNUPG:] VALIDSIG 108F66205EAEB0AAA8DD5E1C85AB96E6FA1BE5FE 2026-10-01\n",
            "",
        ] {
            assert!(!signed_by_the_release_key(status), "{status:?}");
        }
        assert!(signed_by_the_release_key(GOOD));
    }

    #[test]
    fn refuses_a_manifest_gpg_rejects_with_what_gpg_logged() {
        let mut fake = Fake {
            fail_at: Some(5),
            ..answering(STABLE, LISTING, GOOD)
        };
        let work = tempfile::tempdir().unwrap();
        // What gpg writes to the log before it exits.
        fs::write(
            work.path().join("verify.log"),
            "2026-10-20 06:13:05 gpg[1836745] BAD signature from \"Rust Language\" [unknown]\n",
        )
        .unwrap();

        let error = sha256s_in(&mut fake, work.path(), &stable()).unwrap_err();

        assert_eq!(
            error.to_string(),
            "dist/channel-rust-1.99.0.toml is not signed by the Rust release key; gpg says:\n\
             2026-10-20 06:13:05 gpg[1836745] BAD signature from \"Rust Language\" [unknown]"
        );
    }

    #[test]
    fn a_failed_download_stops_the_tool_before_gpg() {
        let mut fake = Fake {
            fail_at: Some(1),
            ..answering(STABLE, LISTING, GOOD)
        };

        let (_work, result) = run(&mut fake, &stable());

        assert_eq!(
            result.unwrap_err().to_string(),
            "`curl -fsSL https://static.rust-lang.org/dist/channel-rust-1.99.0.toml.asc` failed: exit status: 1"
        );
        assert_eq!(fake.ran.len(), 2);
    }

    #[test]
    fn a_failed_import_stops_the_tool() {
        let mut fake = Fake {
            fail_at: Some(3),
            ..answering(STABLE, LISTING, GOOD)
        };

        let (_work, result) = run(&mut fake, &stable());

        assert!(
            matches!(result, Err(Error::Process(process::Error::Failed { .. }))),
            "{result:?}"
        );
        assert_eq!(fake.ran.len(), 4);
    }

    #[test]
    fn refuses_a_nightly_manifest_of_another_date() {
        let error = entries("m", "date = \"2026-08-16\"\n", &nightly()).unwrap_err();

        assert_eq!(
            error.to_string(),
            "the manifest is dated 2026-08-16, not 2026-08-15"
        );
    }

    #[test]
    fn refuses_text_that_is_not_toml() {
        let error = entries("dist/channel-rust-1.99.0.toml", "<html>", &stable()).unwrap_err();

        assert!(
            error
                .to_string()
                .starts_with("dist/channel-rust-1.99.0.toml is not a channel manifest: "),
            "{error}"
        );
    }

    /// The stable manifest, with `from` replaced by `to` once.
    fn edited(from: &str, to: &str) -> String {
        assert_eq!(STABLE.matches(from).count(), 1, "{from}");
        STABLE.replacen(from, to, 1)
    }

    #[test]
    fn refuses_a_release_without_a_tool_for_a_host() {
        let text = edited(
            "[pkg.rustfmt-preview.target.x86_64-apple-darwin]\navailable = true",
            "[pkg.rustfmt-preview.target.x86_64-apple-darwin]\navailable = false",
        );

        let error = entries("m", &text, &stable()).unwrap_err();

        assert_eq!(
            error.to_string(),
            "rustfmt-preview 1.99.0 has no x86_64-apple-darwin build"
        );
    }

    #[test]
    fn refuses_a_release_whose_host_has_no_xz_archive() {
        let text = edited(
            "xz_hash = \"77171ba2a0345fdf2abc4fedda55d6de078dae7a68527c28be8c77dcc9604bd5\"",
            "",
        );

        let error = entries("m", &text, &stable()).unwrap_err();

        assert_eq!(
            error.to_string(),
            "rustc 1.99.0 has no x86_64-unknown-linux-gnu build"
        );
    }

    #[test]
    fn refuses_a_release_without_rust_src() {
        let text = edited("[pkg.rust-src.target.\"*\"]", "[pkg.rust-src.target.\"x\"]");

        let error = entries("m", &text, &stable()).unwrap_err();

        assert_eq!(error.to_string(), "rust-src 1.99.0 has no build");
    }

    #[test]
    fn refuses_a_package_the_manifest_does_not_have() {
        let error = entries("m", "date = \"2026-10-01\"\n", &stable()).unwrap_err();

        assert_eq!(
            error.to_string(),
            "rustc 1.99.0 has no aarch64-apple-darwin build"
        );
    }

    #[test]
    fn refuses_a_rust_src_named_otherwise() {
        let text = edited(
            "dist/2026-10-01/rust-src-1.99.0.tar.xz",
            "dist/2026-10-01/rust-src.tar.xz",
        );

        let error = entries("m", &text, &stable()).unwrap_err();

        assert_eq!(
            error.to_string(),
            "unexpected archive name rust-src.tar.xz for rust-src"
        );
    }

    #[test]
    fn refuses_an_archive_named_otherwise() {
        let text = edited(
            "dist/2026-10-01/cargo-1.99.0-aarch64-unknown-linux-gnu.tar.xz",
            "dist/2026-10-01/cargo-1.99.1-aarch64-unknown-linux-gnu.tar.xz",
        );

        let error = entries("m", &text, &stable()).unwrap_err();

        assert_eq!(
            error.to_string(),
            "unexpected archive name cargo-1.99.1-aarch64-unknown-linux-gnu.tar.xz for cargo on aarch64-unknown-linux-gnu"
        );
    }

    #[test]
    fn takes_the_xz_hash_not_the_gz_one() {
        let entries = entries("m", STABLE, &stable()).unwrap().entries;

        assert!(
            entries.contains(
                "\"rustc-1.99.0-x86_64-unknown-linux-gnu.tar.xz\": \"77171ba2a0345fdf2abc4fedda55d6de078dae7a68527c28be8c77dcc9604bd5\""
            ),
            "{entries}"
        );
        assert!(
            !entries.contains("238e72b8617f79bc96f27a5bfeb1a208b5755fe1a48f8bc190fe403ef6d54ab8")
        );
    }

    #[test]
    fn first_fingerprint_is_the_first_fpr_records_tenth_field() {
        assert_eq!(first_fingerprint(LISTING), Some(FINGERPRINT));
        assert_eq!(first_fingerprint("fpr:::::::::ABC:\r\n"), Some("ABC"));
        assert_eq!(first_fingerprint("fpr:short\n"), None);
        assert_eq!(first_fingerprint("pub:x\nuid:y\n"), None);
    }
}
