//! On-disk cache of cover images (and avatars).
//!
//! Files live in `$XDG_CACHE_HOME/hardcover-desktop/covers/`, named by the
//! SHA-256 of the image URL plus the URL's extension. Downloads are atomic
//! (temp file + rename) and capped in size; only `https` URLs are fetched.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use url::Url;

use crate::api::Cover;
use crate::http::{HttpRequest, HttpTransport, Method, UreqTransport};
use crate::{Error, Result};

/// Largest image we will download.
pub const MAX_COVER_BYTES: u64 = 10 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
const EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "svg", "avif"];

pub struct CoverCache {
    dir: PathBuf,
    http: Arc<dyn HttpTransport>,
}

impl std::fmt::Debug for CoverCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoverCache")
            .field("dir", &self.dir)
            .finish()
    }
}

impl CoverCache {
    /// `$XDG_CACHE_HOME/hardcover-desktop/covers`.
    pub fn default_dir() -> Result<PathBuf> {
        Ok(crate::project_dirs()?.cache_dir().join("covers"))
    }

    pub fn open_default() -> Result<Self> {
        Self::new(Self::default_dir()?)
    }

    /// Uses (and creates) `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self> {
        Self::with_transport(dir, Arc::new(UreqTransport::new()))
    }

    pub(crate) fn with_transport(
        dir: impl Into<PathBuf>,
        http: Arc<dyn HttpTransport>,
    ) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir, http })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file for `cover` if it has been downloaded already. Never touches the network.
    pub fn cached_path(&self, cover: &Cover) -> Option<PathBuf> {
        let path = self.dir.join(file_name(&cover.url).ok()?);
        path.is_file().then_some(path)
    }

    /// The cached file, downloading it first if needed. Blocking.
    pub fn fetch(&self, cover: &Cover) -> Result<PathBuf> {
        let name = file_name(&cover.url)?;
        let path = self.dir.join(&name);
        if path.is_file() {
            return Ok(path);
        }
        let req = HttpRequest::new(Method::Get, cover.url.as_str())
            .header("accept", "image/*")
            .timeout(TIMEOUT)
            .max_response_bytes(MAX_COVER_BYTES);
        let resp = self.http.send(req)?;
        if resp.status != 200 {
            return Err(Error::Network(format!(
                "cover download failed with HTTP {}",
                resp.status
            )));
        }
        if let Some(ct) = resp.header("content-type")
            && !ct.trim_start().to_ascii_lowercase().starts_with("image/")
        {
            return Err(Error::Network(format!(
                "cover download returned {ct}, not an image"
            )));
        }
        if resp.body.is_empty() || resp.body.len() as u64 > MAX_COVER_BYTES {
            return Err(Error::Network(format!(
                "cover has an unusable size ({} bytes)",
                resp.body.len()
            )));
        }
        let suffix = crate::auth::random_suffix();
        let tmp = self.dir.join(format!(".{name}.{suffix}.tmp"));
        let write = || -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&resp.body)?;
            f.sync_all()?;
            std::fs::rename(&tmp, &path)
        };
        if let Err(e) = write() {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(path)
    }
}

/// `sha256(url).ext`, rejecting anything that is not an https URL with a host.
fn file_name(url: &str) -> Result<String> {
    let parsed = Url::parse(url).map_err(|e| Error::InvalidInput(format!("cover URL: {e}")))?;
    if parsed.scheme() != "https" || parsed.host_str().is_none_or(str::is_empty) {
        return Err(Error::InvalidInput("cover URLs must be https".into()));
    }
    let ext = parsed
        .path_segments()
        .and_then(|mut s| s.next_back())
        .and_then(|last| last.rsplit_once('.'))
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|ext| EXTENSIONS.contains(&ext.as_str()))
        .unwrap_or_else(|| "img".to_owned());
    let hash = Sha256::digest(url.as_bytes());
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{hex}.{ext}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fake::FakeTransport;

    #[test]
    fn names_keep_extension_and_reject_non_https() {
        let n = file_name("https://assets.hardcover.app/edition/1/abc.JPG").unwrap();
        assert!(n.ends_with(".jpg") && n.len() == 64 + 4);
        assert!(
            file_name("https://assets.hardcover.app/x")
                .unwrap()
                .ends_with(".img")
        );
        assert!(
            file_name("https://assets.hardcover.app/x.exe")
                .unwrap()
                .ends_with(".img")
        );
        assert!(
            file_name("https://assets.hardcover.app/p.svg?v=2")
                .unwrap()
                .ends_with(".svg")
        );
        assert!(file_name("http://assets.hardcover.app/x.jpg").is_err());
        assert!(file_name("file:///etc/passwd").is_err());
        assert!(file_name("not a url").is_err());
        assert_ne!(
            file_name("https://a.example/1.jpg").unwrap(),
            file_name("https://a.example/2.jpg").unwrap()
        );
    }

    #[test]
    fn fetch_downloads_once() {
        let dir = tempfile::tempdir().unwrap();
        let http = Arc::new(FakeTransport::new());
        http.push_with_headers(200, &[("content-type", "image/jpeg")], "JPEGDATA");
        let cache = CoverCache::with_transport(dir.path().join("covers"), http.clone()).unwrap();
        let cover = Cover::new("https://assets.hardcover.app/edition/1/a.jpg");
        assert!(cache.cached_path(&cover).is_none());
        let p = cache.fetch(&cover).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"JPEGDATA");
        assert_eq!(cache.cached_path(&cover), Some(p.clone()));
        assert_eq!(cache.fetch(&cover).unwrap(), p);
        assert_eq!(http.requests().len(), 1);
        assert_eq!(http.requests()[0].method, Method::Get);
        // No temp files left behind.
        let files: Vec<_> = std::fs::read_dir(cache.dir()).unwrap().collect();
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn fetch_rejects_bad_responses() {
        let dir = tempfile::tempdir().unwrap();
        let http = Arc::new(FakeTransport::new());
        let cache = CoverCache::with_transport(dir.path(), http.clone()).unwrap();
        let cover = Cover::new("https://assets.hardcover.app/a.png");
        http.push(404, "");
        assert!(matches!(cache.fetch(&cover), Err(Error::Network(_))));
        http.push_with_headers(200, &[("content-type", "text/html")], "<html>");
        assert!(matches!(cache.fetch(&cover), Err(Error::Network(_))));
        http.push(200, "");
        assert!(matches!(cache.fetch(&cover), Err(Error::Network(_))));
        assert!(matches!(
            cache.fetch(&Cover::new("http://x.example/a.png")),
            Err(Error::InvalidInput(_))
        ));
        assert!(cache.cached_path(&cover).is_none());
        assert_eq!(std::fs::read_dir(cache.dir()).unwrap().count(), 0);
    }
}
