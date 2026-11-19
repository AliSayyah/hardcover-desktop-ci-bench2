//! Application services shared by every feature: the API client, the session,
//! the library cache and the cover cache, plus the lazy cover assets.
//!
//! Every core call blocks, so features run them with `cx.background_spawn` and
//! apply results on the foreground. The services are GPUI globals:
//! [`CoverService`] lives for the whole run, [`Services`] only while signed in.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

use async_lock::Semaphore;
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Asset, Global, RenderImage, Window};
use hardcover_core::auth::{KeyringTokenStore, OAuthConfig, Session, StaticToken};
use hardcover_core::covers::CoverCache;
use hardcover_core::store::LibraryCache;
use hardcover_core::{Client, Cover};

/// Services available while signed in.
#[derive(Clone)]
pub struct Services {
    client: Client,
    /// `None` when signed in with the development token.
    session: Option<Arc<Session>>,
    /// `None` if the cache could not be opened; the app then works online only.
    cache: Option<Arc<LibraryCache>>,
}

impl Global for Services {}

impl Services {
    pub fn new(
        client: Client,
        session: Option<Arc<Session>>,
        cache: Option<Arc<LibraryCache>>,
    ) -> Self {
        Self {
            client,
            session,
            cache,
        }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn session(&self) -> Option<&Arc<Session>> {
        self.session.as_ref()
    }

    pub fn cache(&self) -> Option<&Arc<LibraryCache>> {
        self.cache.as_ref()
    }

    pub fn global(cx: &App) -> &Self {
        cx.global::<Self>()
    }
}

/// How the app starts, decided off the UI thread.
pub enum Startup {
    SignedIn {
        client: Client,
        session: Option<Arc<Session>>,
        cache: Option<Arc<LibraryCache>>,
    },
    SignedOut {
        cache: Option<Arc<LibraryCache>>,
    },
}

/// Blocking: reads the environment, the keyring and opens the caches.
pub fn startup() -> Startup {
    let cache = match LibraryCache::open_default() {
        Ok(cache) => Some(Arc::new(cache)),
        Err(e) => {
            eprintln!("hardcover: library cache unavailable: {e}");
            None
        }
    };
    if let Some(token) = StaticToken::from_env() {
        eprintln!("hardcover: using HARDCOVER_TOKEN (development only)");
        return Startup::SignedIn {
            client: Client::new(Arc::new(token)),
            session: None,
            cache,
        };
    }
    let restored = KeyringTokenStore::new()
        .and_then(|store| Session::restore(OAuthConfig::default(), Arc::new(store)));
    match restored {
        Ok(Some(session)) => {
            let session = Arc::new(session);
            Startup::SignedIn {
                client: Client::new(session.clone()),
                session: Some(session),
                cache,
            }
        }
        Ok(None) => Startup::SignedOut { cache },
        Err(e) => {
            eprintln!("hardcover: could not restore the saved session: {e}");
            Startup::SignedOut { cache }
        }
    }
}

/// Follows the system light/dark appearance. `HARDCOVER_THEME=light|dark`
/// overrides it (development aid for screenshots).
pub fn follow_appearance(window: &mut Window, cx: &mut App) {
    match std::env::var("HARDCOVER_THEME").ok().as_deref() {
        Some("dark") => Theme::change(ThemeMode::Dark, Some(window), cx),
        Some("light") => Theme::change(ThemeMode::Light, Some(window), cx),
        _ => {
            Theme::sync_system_appearance(Some(window), cx);
            window
                .observe_window_appearance(|window, cx| {
                    Theme::sync_system_appearance(Some(window), cx);
                })
                .detach();
        }
    }
}

// ----- covers -----

/// Downloads at most this many covers at once.
const COVER_DOWNLOADS: usize = 4;
/// Height of decoded row thumbnails, in image pixels (2x the row's thumbnail).
const THUMBNAIL_HEIGHT: u32 = 144;
/// Height of the decoded detail cover, in image pixels. The cover is 12rem
/// tall, so this is sharp up to 2.5x scale with the default rem.
const COVER_HEIGHT: u32 = 480;

/// The cover cache, the download limiter and the decoder thread.
pub struct CoverService {
    cache: Option<Arc<CoverCache>>,
    downloads: Arc<Semaphore>,
    decoder: CoverDecoder,
    /// URLs whose download failed, so they can be retried after a later sync.
    failed: Arc<Mutex<HashSet<String>>>,
}

impl Global for CoverService {}

impl CoverService {
    /// Blocking: creates the cache directory.
    pub fn open() -> Self {
        let cache = match CoverCache::open_default() {
            Ok(cache) => Some(Arc::new(cache)),
            Err(e) => {
                eprintln!("hardcover: cover cache unavailable: {e}");
                None
            }
        };
        Self {
            cache,
            downloads: Arc::new(Semaphore::new(COVER_DOWNLOADS)),
            decoder: CoverDecoder::spawn(),
            failed: Arc::default(),
        }
    }

    /// Forgets failed loads so the next render tries them again.
    pub fn retry_failed(cx: &mut App) {
        let Some(service) = cx.try_global::<Self>() else {
            return;
        };
        let failed: Vec<String> =
            std::mem::take(&mut *service.failed.lock().unwrap_or_else(|p| p.into_inner()))
                .into_iter()
                .collect();
        for url in failed {
            cx.remove_asset::<CoverFile>(&url);
            cx.remove_asset::<CoverThumbnail>(&url);
            cx.remove_asset::<CoverImage>(&url);
        }
    }

    fn loader(cx: &App) -> Option<CoverLoader> {
        let service = cx.try_global::<Self>()?;
        Some(CoverLoader {
            cache: service.cache.clone()?,
            downloads: service.downloads.clone(),
            decoder: service.decoder.clone(),
            failed: service.failed.clone(),
        })
    }
}

struct CoverLoader {
    cache: Arc<CoverCache>,
    downloads: Arc<Semaphore>,
    decoder: CoverDecoder,
    failed: Arc<Mutex<HashSet<String>>>,
}

/// One request to [`CoverDecoder`]: the file, the target height and where to
/// send the result.
struct DecodeJob {
    path: Arc<Path>,
    max_height: u32,
    reply: async_channel::Sender<Option<Arc<RenderImage>>>,
}

/// Decodes covers on one dedicated thread.
///
/// Decoding a large cover allocates tens of megabytes that are freed right
/// after scaling. glibc keeps freed memory in the arena of the thread that
/// allocated it, so decoding on GPUI's many background threads left one
/// bloated arena per thread: hundreds of megabytes of resident memory after
/// browsing a few hundred covers. One thread means one arena, which is reused
/// for every decode. A cover takes a few milliseconds, so one thread keeps up.
#[derive(Clone)]
struct CoverDecoder {
    jobs: async_channel::Sender<DecodeJob>,
}

impl CoverDecoder {
    fn spawn() -> Self {
        let (jobs, queue) = async_channel::unbounded::<DecodeJob>();
        let spawned = thread::Builder::new()
            .name("cover-decoder".into())
            .spawn(move || {
                while let Ok(job) = queue.recv_blocking() {
                    // The requester may have gone away; nothing to do then.
                    let _ = job
                        .reply
                        .send_blocking(decode_scaled(&job.path, job.max_height));
                }
            });
        if let Err(e) = spawned {
            // Without the thread every decode below resolves to `None`, which
            // the views show as the placeholder cover.
            eprintln!("hardcover: cover decoder unavailable: {e}");
        }
        Self { jobs }
    }

    async fn decode(&self, path: Arc<Path>, max_height: u32) -> Option<Arc<RenderImage>> {
        let (reply, result) = async_channel::bounded(1);
        self.jobs
            .send(DecodeJob {
                path,
                max_height,
                reply,
            })
            .await
            .ok()?;
        result.recv().await.ok().flatten()
    }
}

impl CoverLoader {
    /// The cover file on disk, downloading it (behind the limiter) if needed.
    async fn file(&self, url: &str) -> Option<Arc<Path>> {
        let cover = Cover::new(url);
        if let Some(path) = self.cache.cached_path(&cover) {
            return Some(path.into());
        }
        let _permit = self.downloads.acquire().await;
        match self.cache.fetch(&cover) {
            Ok(path) => Some(path.into()),
            Err(_) => {
                self.failed
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(url.to_owned());
                None
            }
        }
    }
}

/// A cover image on disk, keyed by its URL. `None` if it could not be fetched.
pub enum CoverFile {}

impl Asset for CoverFile {
    type Source = String;
    type Output = Option<Arc<Path>>;

    fn load(url: String, cx: &mut App) -> impl Future<Output = Self::Output> + Send + 'static {
        let loader = CoverService::loader(cx);
        async move { loader?.file(&url).await }
    }
}

/// A small decoded cover for list rows. Row thumbnails stay in memory for the
/// whole run, so they are scaled down instead of keeping full-size covers.
pub enum CoverThumbnail {}

impl Asset for CoverThumbnail {
    type Source = String;
    type Output = Option<Arc<RenderImage>>;

    fn load(url: String, cx: &mut App) -> impl Future<Output = Self::Output> + Send + 'static {
        load_scaled(CoverService::loader(cx), url, THUMBNAIL_HEIGHT)
    }
}

/// The detail pane's cover, scaled to [`COVER_HEIGHT`]. Unlike thumbnails it
/// is released when the pane moves on (see [`release_cover_image`]).
pub enum CoverImage {}

impl Asset for CoverImage {
    type Source = String;
    type Output = Option<Arc<RenderImage>>;

    fn load(url: String, cx: &mut App) -> impl Future<Output = Self::Output> + Send + 'static {
        load_scaled(CoverService::loader(cx), url, COVER_HEIGHT)
    }
}

async fn load_scaled(
    loader: Option<CoverLoader>,
    url: String,
    max_height: u32,
) -> Option<Arc<RenderImage>> {
    let loader = loader?;
    let path = loader.file(&url).await?;
    let image = loader.decoder.decode(path, max_height).await;
    if image.is_none() {
        loader
            .failed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(url);
    }
    image
}

/// Decodes an image file, scaled down to at most `max_height` pixels tall.
fn decode_scaled(path: &Path, max_height: u32) -> Option<Arc<RenderImage>> {
    let image = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    let height = max_height.min(image.height().max(1));
    let width = (image.width() as u64 * height as u64 / image.height().max(1) as u64).max(1) as u32;
    let mut rgba = image
        .resize(width, height, image::imageops::FilterType::Triangle)
        .into_rgba8();
    // GPUI's sprite atlas expects BGRA.
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(rgba)])))
}

/// The cover file for `url`, starting a background load on first use. The
/// calling view re-renders when it finishes.
pub fn cover_file(url: &str, window: &mut Window, cx: &mut App) -> Option<Arc<Path>> {
    window.use_asset::<CoverFile>(&url.to_owned(), cx).flatten()
}

/// The row thumbnail for `url`, loaded like [`cover_file`].
pub fn cover_thumbnail(url: &str, window: &mut Window, cx: &mut App) -> Option<Arc<RenderImage>> {
    window
        .use_asset::<CoverThumbnail>(&url.to_owned(), cx)
        .flatten()
}

/// The detail cover for `url`, loaded like [`cover_file`].
pub fn cover_image(url: &str, window: &mut Window, cx: &mut App) -> Option<Arc<RenderImage>> {
    window
        .use_asset::<CoverImage>(&url.to_owned(), cx)
        .flatten()
}

/// Forgets a detail cover the pane no longer shows. GPUI keeps loaded assets
/// and their textures for the whole run unless told otherwise, which adds up
/// when browsing hundreds of books.
pub fn release_cover_image(
    url: &str,
    image: Option<Arc<RenderImage>>,
    window: &mut Window,
    cx: &mut App,
) {
    cx.remove_asset::<CoverImage>(&url.to_owned());
    if let Some(image) = image {
        cx.drop_image(image, Some(window));
    }
}
