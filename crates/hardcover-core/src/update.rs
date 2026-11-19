//! Self-update from GitHub releases.
//!
//! The app updates itself only when it runs from an unpacked release tarball
//! (`<prefix>/bin/hardcover-desktop` next to `<prefix>/share/applications/…`)
//! in a prefix it can write to, such as `~/.local`. Other installs only learn
//! that a newer version exists.
//!
//! Trust chain for an install:
//! 1. The release tarball must be signed by the key in `packaging/minisign.pub`.
//! 2. The signature's trusted comment names the signed file, and that name must
//!    be the tarball name for the advertised version and this architecture. A
//!    valid signature for an older tarball therefore can't be replayed as a
//!    newer release.
//! 3. The advertised version must be newer than the running one, and the
//!    archive's top-level directory must carry the same name as the tarball.
//! 4. Only regular files under `bin/hardcover-desktop` and `share/` are
//!    unpacked, with plain relative paths and a total size cap.
//!
//! Files are staged inside the prefix and renamed into place, the binary last,
//! so every file is replaced atomically. Replacing the running executable is
//! safe on Linux; the new version runs after a restart.

use std::cmp::Ordering;
use std::fmt;
use std::fs;
use std::io::{Read, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use crate::http::{HttpRequest, HttpTransport, Method, UreqTransport};
use crate::{Error, Result};

/// The GitHub repository releases are published to.
pub const REPOSITORY: &str = "AliSayyah/hardcover-desktop";

/// The release signing public key (minisign, base64).
pub fn release_public_key() -> &'static str {
    let file = include_str!("../../../packaging/minisign.pub");
    file.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("untrusted comment:"))
        .expect("packaging/minisign.pub has a key line")
}

const APP_ID: &str = "io.github.alisayyah.HardcoverDesktop";
const BINARY: &str = "hardcover-desktop";
const MAX_TARBALL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 512 * 1024 * 1024;
const STAGING_DIR: &str = ".hardcover-desktop-update";

// ----- versions -----

/// A `major.minor.patch` version with an optional pre-release suffix, which
/// sorts before the release it precedes. Build metadata is ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Option<String>,
}

impl Version {
    /// Parses `1.2.3`, `v1.2.3` or `1.2.3-rc.1`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().strip_prefix('v').unwrap_or(s.trim());
        let s = s.split('+').next()?;
        let (core, pre) = match s.split_once('-') {
            Some((core, pre)) if !pre.is_empty() => (core, Some(pre.to_owned())),
            Some(_) => return None,
            None => (s, None),
        };
        let mut parts = core.split('.');
        let mut next = || -> Option<u64> {
            let p = parts.next()?;
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            p.parse().ok()
        };
        let v = Self {
            major: next()?,
            minor: next()?,
            patch: next()?,
            pre,
        };
        parts.next().is_none().then_some(v)
    }

    /// The version of this build of `hardcover-core`'s caller, from its Cargo
    /// version (pass `env!("CARGO_PKG_VERSION")`).
    pub fn current(cargo_version: &str) -> Self {
        Self::parse(cargo_version).expect("CARGO_PKG_VERSION is a valid version")
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(pre) = &self.pre {
            write!(f, "-{pre}")?;
        }
        Ok(())
    }
}

// ----- install kind -----

/// How the running copy was installed, which decides whether it can update itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// Unpacked from a release tarball into a writable prefix.
    Tarball { prefix: PathBuf },
    /// Anything else: a package, Flatpak, a development build, or a prefix
    /// this user can't write to. Updates are announced, not installed.
    Other,
}

impl InstallKind {
    /// Classifies the executable at `exe` (resolve it once, at startup).
    pub fn detect(exe: &Path) -> Self {
        if std::env::var_os("FLATPAK_ID").is_some() {
            return Self::Other;
        }
        let Some(bin) = exe.parent() else {
            return Self::Other;
        };
        let Some(prefix) = bin.parent() else {
            return Self::Other;
        };
        let packaged = exe.file_name().is_some_and(|n| n == BINARY)
            && bin.file_name().is_some_and(|n| n == "bin")
            && prefix
                .join("share/applications")
                .join(format!("{APP_ID}.desktop"))
                .is_file();
        if packaged && is_writable_dir(bin) && is_writable_dir(&prefix.join("share")) {
            Self::Tarball {
                prefix: prefix.to_owned(),
            }
        } else {
            Self::Other
        }
    }

    pub fn can_self_update(&self) -> bool {
        matches!(self, Self::Tarball { .. })
    }
}

fn is_writable_dir(dir: &Path) -> bool {
    let probe = dir.join(format!(".hardcover-desktop-probe-{}", std::process::id()));
    match fs::File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

// ----- releases -----

/// A published release newer than the running version.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: Version,
    /// Release notes (Markdown).
    pub notes: String,
    /// The release page on GitHub.
    pub page_url: String,
    /// The tarball for this architecture, if the release has one.
    pub asset: Option<Asset>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub signature_url: String,
}

/// The tarball name a release of `version` uses on this architecture.
pub fn tarball_name(version: &Version) -> String {
    format!("{BINARY}-{version}-{}-linux.tar.gz", std::env::consts::ARCH)
}

#[derive(Deserialize)]
struct WireRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<WireAsset>,
}

#[derive(Deserialize)]
struct WireAsset {
    name: String,
    browser_download_url: String,
}

/// Checks for, downloads and installs updates. Blocking: call it off the UI thread.
#[derive(Clone)]
pub struct Updater {
    http: Arc<dyn HttpTransport>,
    api_url: String,
    public_key: String,
}

impl fmt::Debug for Updater {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Updater")
            .field("api_url", &self.api_url)
            .finish_non_exhaustive()
    }
}

impl Default for Updater {
    fn default() -> Self {
        Self::new()
    }
}

impl Updater {
    /// Checks [`REPOSITORY`] and verifies against the release signing key.
    pub fn new() -> Self {
        Self::with_transport(
            Arc::new(UreqTransport::new()),
            format!("https://api.github.com/repos/{REPOSITORY}/releases/latest"),
            release_public_key().to_owned(),
        )
    }

    pub(crate) fn with_transport(
        http: Arc<dyn HttpTransport>,
        api_url: String,
        public_key: String,
    ) -> Self {
        Self {
            http,
            api_url,
            public_key,
        }
    }

    /// The latest release, if it is newer than `current`. `Ok(None)` also when
    /// there is no published release (or the repository isn't public).
    pub fn check(&self, current: &Version) -> Result<Option<Release>> {
        let resp = self.http.send(
            HttpRequest::new(Method::Get, &self.api_url)
                .header("accept", "application/vnd.github+json")
                .header("x-github-api-version", "2022-11-28")
                .timeout(Duration::from_secs(20))
                .max_response_bytes(4 * 1024 * 1024),
        )?;
        match resp.status {
            200 => {}
            404 => return Ok(None),
            403 | 429 => {
                return Err(Error::Network(
                    "GitHub's rate limit was reached; try again later".into(),
                ));
            }
            s => return Err(Error::Network(format!("GitHub answered {s}"))),
        }
        let wire: WireRelease = serde_json::from_slice(&resp.body)
            .map_err(|e| Error::Update(format!("unexpected answer from GitHub: {e}")))?;
        if wire.draft || wire.prerelease {
            return Ok(None);
        }
        let Some(version) = Version::parse(&wire.tag_name) else {
            return Ok(None);
        };
        if version <= *current {
            return Ok(None);
        }
        let name = tarball_name(&version);
        let url_of = |n: &str| {
            wire.assets
                .iter()
                .find(|a| a.name == n)
                .map(|a| a.browser_download_url.clone())
        };
        let asset = match (url_of(&name), url_of(&format!("{name}.minisig"))) {
            (Some(url), Some(signature_url)) => Some(Asset {
                name,
                url,
                signature_url,
            }),
            _ => None,
        };
        Ok(Some(Release {
            version,
            notes: wire.body.unwrap_or_default(),
            page_url: wire.html_url,
            asset,
        }))
    }

    /// Downloads, verifies and installs `release` into `prefix`. The running
    /// app keeps its old code until it restarts.
    pub fn install(&self, release: &Release, current: &Version, prefix: &Path) -> Result<()> {
        if release.version <= *current {
            return Err(update_error("the release is not newer than this version"));
        }
        let asset = release
            .asset
            .as_ref()
            .ok_or_else(|| update_error("the release has no build for this computer"))?;
        let expected_name = tarball_name(&release.version);
        if asset.name != expected_name {
            return Err(update_error(
                "the release's file name doesn't match its version",
            ));
        }

        let signature = self.download(&asset.signature_url, 64 * 1024)?;
        let tarball = self.download(&asset.url, MAX_TARBALL_BYTES)?;
        verify(&self.public_key, &tarball, &signature, &expected_name)?;

        let top = expected_name.trim_end_matches(".tar.gz");
        let staging = prefix.join(STAGING_DIR);
        let result =
            unpack(&tarball, top, &staging).and_then(|files| place(&staging, prefix, &files));
        let _ = fs::remove_dir_all(&staging);
        result
    }

    fn download(&self, url: &str, limit: u64) -> Result<Vec<u8>> {
        let resp = self.http.send(
            HttpRequest::new(Method::Get, url)
                .header("accept", "application/octet-stream")
                .timeout(Duration::from_secs(300))
                .max_response_bytes(limit),
        )?;
        if resp.status != 200 {
            return Err(Error::Network(format!(
                "downloading the update failed ({})",
                resp.status
            )));
        }
        Ok(resp.body)
    }
}

fn update_error(msg: &str) -> Error {
    Error::Update(msg.to_owned())
}

/// Checks the minisign signature over `data` and that it was made for `file_name`.
fn verify(public_key: &str, data: &[u8], signature: &[u8], file_name: &str) -> Result<()> {
    let bad = || update_error("the update's signature is not valid");
    let key = minisign_verify::PublicKey::from_base64(public_key)
        .map_err(|_| update_error("the release signing key is invalid"))?;
    let text = std::str::from_utf8(signature).map_err(|_| bad())?;
    let sig = minisign_verify::Signature::decode(text).map_err(|_| bad())?;
    key.verify(data, &sig, false).map_err(|_| bad())?;
    let signed_file = sig
        .trusted_comment()
        .split('\t')
        .find_map(|field| field.strip_prefix("file:"));
    if signed_file != Some(file_name) {
        return Err(update_error(
            "the update's signature was made for a different file",
        ));
    }
    Ok(())
}

/// Unpacks the allowed files under `top/` into `staging`, returning their
/// paths relative to the prefix.
fn unpack(tarball: &[u8], top: &str, staging: &Path) -> Result<Vec<PathBuf>> {
    let _ = fs::remove_dir_all(staging);
    fs::create_dir_all(staging).map_err(io_error)?;
    let corrupt = |what: &str| update_error(&format!("the update archive is not valid: {what}"));

    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tarball));
    let mut files = Vec::new();
    let mut total: u64 = 0;
    for entry in archive.entries().map_err(|_| corrupt("unreadable"))? {
        let mut entry = entry.map_err(|_| corrupt("unreadable entry"))?;
        let path = entry.path().map_err(|_| corrupt("bad path"))?.into_owned();
        let mut parts = path.components();
        match parts.next() {
            Some(Component::Normal(first)) if first == top => {}
            _ => return Err(corrupt("unexpected top-level directory")),
        }
        let rel: PathBuf = parts.as_path().to_owned();
        if rel.as_os_str().is_empty() {
            continue;
        }
        if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
            return Err(corrupt("path escapes the archive"));
        }
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            continue;
        }
        if !kind.is_file() {
            return Err(corrupt("links and special files are not allowed"));
        }
        if rel != Path::new("bin").join(BINARY) && !rel.starts_with("share") {
            return Err(corrupt("unexpected file"));
        }
        total = total.saturating_add(entry.size());
        if total > MAX_UNPACKED_BYTES {
            return Err(corrupt("too large"));
        }

        let dest = staging.join(&rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(io_error)?;
        }
        let mut out = fs::File::create(&dest).map_err(io_error)?;
        let mut buf = Vec::new();
        entry
            .read_to_end(&mut buf)
            .map_err(|_| corrupt("truncated"))?;
        out.write_all(&buf).map_err(io_error)?;
        out.sync_all().map_err(io_error)?;
        let mode = if rel.starts_with("bin") { 0o755 } else { 0o644 };
        set_mode(&dest, mode)?;
        files.push(rel);
    }
    if !files.iter().any(|f| f == &Path::new("bin").join(BINARY)) {
        return Err(corrupt("no executable"));
    }
    Ok(files)
}

/// Renames each staged file over its destination, the executable last.
fn place(staging: &Path, prefix: &Path, files: &[PathBuf]) -> Result<()> {
    let binary = Path::new("bin").join(BINARY);
    let ordered = files
        .iter()
        .filter(|f| **f != binary)
        .chain(files.iter().filter(|f| **f == binary));
    for rel in ordered {
        let dest = prefix.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(io_error)?;
        }
        fs::rename(staging.join(rel), &dest).map_err(io_error)?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(io_error)
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) -> Result<()> {
    Ok(())
}

fn io_error(e: std::io::Error) -> Error {
    update_error(&format!("installing the update failed: {e}"))
}

#[cfg(test)]
mod tests;
