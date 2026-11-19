//! Update tests. Releases, tarballs and signatures are made up per test, signed
//! with a throwaway key.

use std::io::Write as _;

use serde_json::{Value, json};

use super::*;
use crate::http::fake::FakeTransport;

const API: &str = "https://api.test/releases/latest";

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

struct Key {
    pair: minisign::KeyPair,
}

impl Key {
    fn new() -> Self {
        Self {
            pair: minisign::KeyPair::generate_unencrypted_keypair().unwrap(),
        }
    }
    fn public(&self) -> String {
        self.pair.pk.to_base64()
    }
    /// Signs like `minisign -S` does: prehashed, `file:<name>` in the trusted comment.
    fn sign(&self, data: &[u8], file_name: &str) -> Vec<u8> {
        let comment = format!("timestamp:1\tfile:{file_name}\thashed");
        minisign::sign(None, &self.pair.sk, data, Some(&comment), None)
            .unwrap()
            .into_string()
            .into_bytes()
    }
}

/// A gzipped tarball of `(path, contents)` regular files.
fn tarball(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    for (path, data) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_path(path).unwrap();
        header.set_cksum();
        builder.append(&header, *data).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

/// A gzipped tarball with one raw entry whose name bypasses `set_path` checks.
fn raw_tarball(name: &str, kind: tar::EntryType, link: Option<&str>) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    let mut header = tar::Header::new_gnu();
    header.set_size(if link.is_some() { 0 } else { 3 });
    header.set_mode(0o644);
    header.set_entry_type(kind);
    let gnu = header.as_gnu_mut().unwrap();
    gnu.name[..name.len()].copy_from_slice(name.as_bytes());
    if let Some(target) = link {
        gnu.linkname[..target.len()].copy_from_slice(target.as_bytes());
    }
    header.set_cksum();
    let data: &[u8] = if link.is_some() { b"" } else { b"bad" };
    builder.append(&header, data).unwrap();
    builder.into_inner().unwrap().finish().unwrap()
}

fn top(version: &str) -> String {
    tarball_name(&v(version))
        .trim_end_matches(".tar.gz")
        .to_owned()
}

fn release_tarball(version: &str) -> Vec<u8> {
    let t = top(version);
    let bin = format!("{t}/bin/hardcover-desktop");
    let desktop = format!("{t}/share/applications/{APP_ID}.desktop");
    let body = format!("binary {version}");
    tarball(&[(&bin, body.as_bytes()), (&desktop, b"[Desktop Entry]\n")])
}

fn release(version: &str) -> Release {
    let name = tarball_name(&v(version));
    Release {
        version: v(version),
        notes: String::new(),
        page_url: "https://example.test/release".into(),
        asset: Some(Asset {
            url: format!("https://dl.test/{name}"),
            signature_url: format!("https://dl.test/{name}.minisig"),
            name,
        }),
    }
}

fn updater(key: &Key) -> (Updater, Arc<FakeTransport>) {
    let fake = Arc::new(FakeTransport::new());
    let updater = Updater::with_transport(fake.clone(), API.into(), key.public());
    (updater, fake)
}

/// A prefix laid out like an unpacked older release.
fn old_install() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    fs::create_dir_all(p.join("bin")).unwrap();
    fs::create_dir_all(p.join("share/applications")).unwrap();
    fs::write(p.join("bin/hardcover-desktop"), "binary 0.1.0").unwrap();
    fs::write(
        p.join(format!("share/applications/{APP_ID}.desktop")),
        "old",
    )
    .unwrap();
    dir
}

fn binary(prefix: &Path) -> String {
    fs::read_to_string(prefix.join("bin/hardcover-desktop")).unwrap()
}

// ----- versions -----

#[test]
fn versions_parse_and_order() {
    assert_eq!(v("v1.2.3"), v("1.2.3"));
    assert_eq!(v("1.2.3+build.5"), v("1.2.3"));
    assert!(v("0.10.0") > v("0.9.9"));
    assert!(v("1.0.0") > v("1.0.0-rc.1"));
    assert!(v("1.0.0-rc.2") > v("1.0.0-rc.1"));
    assert_eq!(v("2.0.1-beta").to_string(), "2.0.1-beta");
    for bad in ["", "1.2", "1.2.3.4", "1.x.3", "1.2.3-", "-1.2.3", "1..3"] {
        assert_eq!(Version::parse(bad), None, "{bad:?}");
    }
}

#[test]
fn the_bundled_public_key_parses() {
    assert!(minisign_verify::PublicKey::from_base64(release_public_key()).is_ok());
}

// ----- checking -----

fn github_release(tag: &str, assets: &[&str]) -> String {
    json!({
        "tag_name": tag,
        "body": "Notes",
        "html_url": format!("https://github.test/{tag}"),
        "draft": false,
        "prerelease": false,
        "assets": assets.iter().map(|n| json!({
            "name": n,
            "browser_download_url": format!("https://dl.test/{n}"),
        })).collect::<Vec<_>>(),
    })
    .to_string()
}

#[test]
fn check_finds_a_newer_release_with_its_assets() {
    let key = Key::new();
    let (u, fake) = updater(&key);
    let name = tarball_name(&v("0.2.0"));
    let sig = format!("{name}.minisig");
    fake.push(200, &github_release("v0.2.0", &[&name, &sig, "other.zip"]));

    let r = u.check(&v("0.1.0")).unwrap().unwrap();
    assert_eq!(r.version, v("0.2.0"));
    assert_eq!(r.notes, "Notes");
    let asset = r.asset.unwrap();
    assert_eq!(asset.url, format!("https://dl.test/{name}"));
    assert_eq!(asset.signature_url, format!("https://dl.test/{sig}"));

    let req = &fake.requests()[0];
    assert_eq!(req.url, API);
    assert_eq!(req.header("accept"), Some("application/vnd.github+json"));
}

#[test]
fn check_ignores_same_older_draft_and_prerelease() {
    let key = Key::new();
    let (u, fake) = updater(&key);
    fake.push(200, &github_release("v0.1.0", &[]));
    fake.push(200, &github_release("v0.0.9", &[]));
    let mut draft: Value = serde_json::from_str(&github_release("v0.2.0", &[])).unwrap();
    draft["draft"] = json!(true);
    fake.push(200, &draft.to_string());
    let mut pre: Value = serde_json::from_str(&github_release("v0.2.0", &[])).unwrap();
    pre["prerelease"] = json!(true);
    fake.push(200, &pre.to_string());
    for _ in 0..4 {
        assert_eq!(u.check(&v("0.1.0")).unwrap(), None);
    }
}

#[test]
fn check_without_releases_or_build_for_this_arch() {
    let key = Key::new();
    let (u, fake) = updater(&key);
    fake.push(404, r#"{"message":"Not Found"}"#);
    assert_eq!(u.check(&v("0.1.0")).unwrap(), None);

    // Newer, but only the tarball: no signature means nothing to install.
    fake.push(
        200,
        &github_release("v0.2.0", &[&tarball_name(&v("0.2.0"))]),
    );
    let r = u.check(&v("0.1.0")).unwrap().unwrap();
    assert_eq!(r.asset, None);

    fake.push(403, "{}");
    assert!(u.check(&v("0.1.0")).is_err());
}

// ----- installing -----

#[test]
fn install_replaces_the_files_and_cleans_up() {
    let key = Key::new();
    let (u, fake) = updater(&key);
    let r = release("0.2.0");
    let data = release_tarball("0.2.0");
    let name = &r.asset.as_ref().unwrap().name;
    fake.push_bytes(200, &key.sign(&data, name));
    fake.push_bytes(200, &data);
    let prefix = old_install();

    u.install(&r, &v("0.1.0"), prefix.path()).unwrap();

    assert_eq!(binary(prefix.path()), "binary 0.2.0");
    let desktop = prefix
        .path()
        .join(format!("share/applications/{APP_ID}.desktop"));
    assert_eq!(fs::read_to_string(desktop).unwrap(), "[Desktop Entry]\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(prefix.path().join("bin/hardcover-desktop"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }
    assert!(!prefix.path().join(STAGING_DIR).exists());
}

fn install_fails(sig: Vec<u8>, data: Vec<u8>, r: &Release, key: &Key) -> (String, String) {
    let (u, fake) = updater(key);
    fake.push_bytes(200, &sig);
    fake.push_bytes(200, &data);
    let prefix = old_install();
    let err = u.install(r, &v("0.1.0"), prefix.path()).unwrap_err();
    assert!(!prefix.path().join(STAGING_DIR).exists());
    (err.to_string(), binary(prefix.path()))
}

#[test]
fn install_rejects_a_signature_from_another_key() {
    let key = Key::new();
    let r = release("0.2.0");
    let data = release_tarball("0.2.0");
    let sig = Key::new().sign(&data, &r.asset.as_ref().unwrap().name);
    let (err, bin) = install_fails(sig, data, &r, &key);
    assert!(err.contains("signature is not valid"), "{err}");
    assert_eq!(bin, "binary 0.1.0");
}

#[test]
fn install_rejects_tampered_bytes() {
    let key = Key::new();
    let r = release("0.2.0");
    let data = release_tarball("0.2.0");
    let sig = key.sign(&data, &r.asset.as_ref().unwrap().name);
    let mut tampered = data.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 1;
    let (err, bin) = install_fails(sig, tampered, &r, &key);
    assert!(err.contains("signature is not valid"), "{err}");
    assert_eq!(bin, "binary 0.1.0");
}

#[test]
fn install_rejects_an_old_signed_release_served_as_new() {
    // A genuinely signed 0.1.5 tarball, advertised as 0.2.0.
    let key = Key::new();
    let old = release_tarball("0.1.5");
    let sig = key.sign(&old, &tarball_name(&v("0.1.5")));
    let (err, bin) = install_fails(sig, old, &release("0.2.0"), &key);
    assert!(err.contains("different file"), "{err}");
    assert_eq!(bin, "binary 0.1.0");
}

#[test]
fn install_rejects_a_mismatched_top_directory() {
    // Signed for the right name, but the archive holds another version's tree.
    let key = Key::new();
    let r = release("0.2.0");
    let data = release_tarball("0.1.5");
    let sig = key.sign(&data, &r.asset.as_ref().unwrap().name);
    let (err, bin) = install_fails(sig, data, &r, &key);
    assert!(err.contains("top-level"), "{err}");
    assert_eq!(bin, "binary 0.1.0");
}

#[test]
fn install_rejects_unsafe_entries() {
    let t = top("0.2.0");
    let cases = [
        (
            raw_tarball(&format!("{t}/../evil"), tar::EntryType::Regular, None),
            "escapes",
        ),
        (
            raw_tarball(
                &format!("{t}/share/link"),
                tar::EntryType::Symlink,
                Some("/etc/passwd"),
            ),
            "links",
        ),
        (
            raw_tarball(&format!("{t}/etc/evil"), tar::EntryType::Regular, None),
            "unexpected file",
        ),
        (
            raw_tarball(&format!("{t}/share/readme"), tar::EntryType::Regular, None),
            "no executable",
        ),
    ];
    for (data, expected) in cases {
        let key = Key::new();
        let r = release("0.2.0");
        let sig = key.sign(&data, &r.asset.as_ref().unwrap().name);
        let (err, bin) = install_fails(sig, data, &r, &key);
        assert!(err.contains(expected), "{expected}: {err}");
        assert_eq!(bin, "binary 0.1.0");
    }
}

#[test]
fn install_refuses_a_release_that_is_not_newer() {
    let key = Key::new();
    let (u, fake) = updater(&key);
    let prefix = old_install();
    let err = u
        .install(&release("0.1.0"), &v("0.1.0"), prefix.path())
        .unwrap_err();
    assert!(err.to_string().contains("not newer"));
    assert_eq!(fake.requests().len(), 0);
}

// ----- install kind -----

#[test]
fn detects_an_unpacked_tarball_in_a_writable_prefix() {
    let prefix = old_install();
    let exe = prefix.path().join("bin/hardcover-desktop");
    assert_eq!(
        InstallKind::detect(&exe),
        InstallKind::Tarball {
            prefix: prefix.path().to_owned()
        }
    );

    fs::remove_file(
        prefix
            .path()
            .join(format!("share/applications/{APP_ID}.desktop")),
    )
    .unwrap();
    assert_eq!(InstallKind::detect(&exe), InstallKind::Other);

    let dev = tempfile::tempdir().unwrap();
    let mut f = fs::File::create(dev.path().join("hardcover-desktop")).unwrap();
    f.write_all(b"x").unwrap();
    assert_eq!(
        InstallKind::detect(&dev.path().join("hardcover-desktop")),
        InstallKind::Other
    );
}
