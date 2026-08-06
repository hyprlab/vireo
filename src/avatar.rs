//! Sender-avatar loading and caching. Local GNOME Contacts / CardDAV photos are
//! preferred. Gravatar is an optional fallback and is only queried when enabled.
//!
//! Privacy note: local contact photos are read from Evolution Data Server's
//! on-disk cache and make no network request. Gravatar sends a hash of the
//! sender's email to Automattic, so it remains off by default.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use gtk::prelude::Cast;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvatarSource {
    Contact,
    Gravatar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchMode {
    ContactOnly,
    ContactThenGravatar,
    GravatarOnly,
}

#[derive(Debug, Clone)]
struct DecodedImage {
    pixels: Arc<[u8]>,
    width: i32,
    height: i32,
}

#[derive(Debug)]
pub struct FetchedAvatar {
    image: DecodedImage,
    pub source: AvatarSource,
}

#[derive(Debug)]
pub enum FetchOutcome {
    Found(FetchedAvatar),
    Missing,
    /// A transient Gravatar failure; retry on a later render.
    Retry,
}

pub enum CacheLookup {
    Texture(gtk::gdk::Texture),
    Missing,
    Fetch { generation: u64, mode: FetchMode },
}

struct ContactCache {
    generation: u64,
    texture: Option<gtk::gdk::Texture>,
}

thread_local! {
    // Contact state is generation-bound; Gravatar state survives EDS changes so
    // a CardDAV sync never causes unnecessary third-party requests.
    static CONTACT_CACHE: RefCell<HashMap<String, ContactCache>> = RefCell::new(HashMap::new());
    static GRAVATAR_CACHE: RefCell<HashMap<String, Option<gtk::gdk::Texture>>> =
        RefCell::new(HashMap::new());
}

fn key(email: &str) -> String {
    email.trim().to_lowercase()
}

/// Consult the main-thread texture/miss caches without doing I/O.
pub fn lookup(email: &str, allow_gravatar: bool) -> CacheLookup {
    let key = key(email);
    let generation = crate::contacts::photo_generation();
    if !crate::contacts::photos_ready() {
        // Never disclose a sender hash to Gravatar before the local EDS index
        // has had a chance to answer.
        return CacheLookup::Fetch {
            generation,
            mode: FetchMode::ContactOnly,
        };
    }
    let contact = CONTACT_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache
            .get(&key)
            .is_some_and(|cached| cached.generation != generation)
        {
            cache.remove(&key);
        }
        cache.get(&key).map(|cached| cached.texture.clone())
    });

    if let Some(Some(texture)) = contact {
        return CacheLookup::Texture(texture);
    }
    let gravatar = GRAVATAR_CACHE.with(|cache| cache.borrow().get(&key).cloned());
    if contact.is_some() {
        return match (allow_gravatar, gravatar) {
            (false, _) | (true, Some(None)) => CacheLookup::Missing,
            (true, Some(Some(texture))) => CacheLookup::Texture(texture),
            (true, None) => CacheLookup::Fetch {
                generation,
                mode: FetchMode::GravatarOnly,
            },
        };
    }

    // Contact status is unknown for this EDS generation. Check it first, but do
    // not refetch a Gravatar whose result is already cached.
    let mode = if allow_gravatar && gravatar.is_none() {
        FetchMode::ContactThenGravatar
    } else {
        FetchMode::ContactOnly
    };
    CacheLookup::Fetch { generation, mode }
}

fn gravatar_url(email: &str) -> String {
    let digest = md5::compute(key(email));
    format!("https://www.gravatar.com/avatar/{digest:x}?s=160&d=404")
}

/// Blocking sender-avatar lookup. The chain is local vCard PHOTO, optional
/// Gravatar, then the UI's initials fallback. Call off the main thread.
pub fn fetch(email: &str, mode: FetchMode) -> FetchOutcome {
    if mode != FetchMode::GravatarOnly {
        for bytes in crate::contacts::contact_photos(email) {
            // Decode/downscale on this background thread. A corrupt candidate
            // must not hide a valid duplicate or Gravatar fallback.
            if supported_raster(&bytes) {
                if let Some(image) = decode_image(&bytes) {
                    return FetchOutcome::Found(FetchedAvatar {
                        image,
                        source: AvatarSource::Contact,
                    });
                }
            }
        }
        if mode == FetchMode::ContactOnly {
            return FetchOutcome::Missing;
        }
    }

    match gravatar_image(email) {
        Ok(Some(image)) => FetchOutcome::Found(FetchedAvatar {
            image,
            source: AvatarSource::Gravatar,
        }),
        Ok(None) => FetchOutcome::Missing,
        Err(()) => FetchOutcome::Retry,
    }
}

/// Record a completed request. Contact results are stamped with the generation
/// that was actually queried; stale results are discarded rather than masking a
/// newly synchronized vCard photo.
pub fn cache_result(
    email: &str,
    generation: u64,
    mode: FetchMode,
    outcome: FetchOutcome,
) -> bool {
    let key = key(email);
    let generation_current = crate::contacts::photo_generation() == generation;
    match outcome {
        FetchOutcome::Found(fetched) => {
            let bytes = gtk::glib::Bytes::from(fetched.image.pixels.as_ref());
            let texture = Some(
                gtk::gdk::MemoryTexture::new(
                    fetched.image.width,
                    fetched.image.height,
                    gtk::gdk::MemoryFormat::R8g8b8a8,
                    &bytes,
                    fetched.image.width as usize * 4,
                )
                .upcast::<gtk::gdk::Texture>(),
            );
            match fetched.source {
                AvatarSource::Contact if generation_current => {
                    CONTACT_CACHE.with(|cache| {
                        cache.borrow_mut().insert(
                            key,
                            ContactCache {
                                generation,
                                texture,
                            },
                        );
                    });
                }
                AvatarSource::Gravatar => {
                    GRAVATAR_CACHE.with(|cache| {
                        cache.borrow_mut().insert(key.clone(), texture);
                    });
                    if mode == FetchMode::ContactThenGravatar && generation_current {
                        cache_missing_contact(&key, generation);
                    }
                }
                AvatarSource::Contact => {}
            }
        }
        FetchOutcome::Missing => {
            if mode != FetchMode::GravatarOnly && generation_current {
                cache_missing_contact(&key, generation);
            }
            if mode != FetchMode::ContactOnly {
                GRAVATAR_CACHE.with(|cache| {
                    cache.borrow_mut().insert(key, None);
                });
            }
        }
        FetchOutcome::Retry => {
            if mode == FetchMode::ContactThenGravatar && generation_current {
                cache_missing_contact(&key, generation);
            }
        }
    }
    !generation_current && mode != FetchMode::GravatarOnly
}

fn cache_missing_contact(email: &str, generation: u64) {
    CONTACT_CACHE.with(|cache| {
        cache.borrow_mut().insert(
            email.to_string(),
            ContactCache {
                generation,
                texture: None,
            },
        );
    });
}

// Raw Gravatar results coalesce duplicate rows for one sender and preserve a
// definitive 404 across list rebuilds. Transient failures are never cached.
enum RawGravatar {
    Found(DecodedImage),
    Missing,
    RetryAfter(std::time::Instant),
}

type RawGravatarCache = HashMap<String, RawGravatar>;
static GRAVATAR_BYTES: OnceLock<Mutex<RawGravatarCache>> = OnceLock::new();
static GRAVATAR_EMAIL_LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
static GRAVATAR_GATE: OnceLock<(Mutex<usize>, Condvar)> = OnceLock::new();
const MAX_GRAVATAR_REQUESTS: usize = 4;

struct GravatarSlot;

impl GravatarSlot {
    fn acquire() -> Self {
        let (active, wake) = GRAVATAR_GATE.get_or_init(|| (Mutex::new(0), Condvar::new()));
        let mut active = active.lock().unwrap_or_else(|p| p.into_inner());
        while *active >= MAX_GRAVATAR_REQUESTS {
            active = wake.wait(active).unwrap_or_else(|p| p.into_inner());
        }
        *active += 1;
        Self
    }
}

impl Drop for GravatarSlot {
    fn drop(&mut self) {
        let (active, wake) = GRAVATAR_GATE.get().expect("Gravatar gate initialized");
        let mut active = active.lock().unwrap_or_else(|p| p.into_inner());
        *active = active.saturating_sub(1);
        wake.notify_one();
    }
}

fn gravatar_image(email: &str) -> Result<Option<DecodedImage>, ()> {
    use std::io::Read;

    const MAX_BYTES: usize = 2_000_000;
    let key = key(email);
    let email_lock = {
        let mut locks = GRAVATAR_EMAIL_LOCKS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        locks
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };
    let _email_guard = email_lock.lock().unwrap_or_else(|p| p.into_inner());
    {
        let mut cache = GRAVATAR_BYTES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        match cache.get(&key) {
            Some(RawGravatar::Found(bytes)) => return Ok(Some(bytes.clone())),
            Some(RawGravatar::Missing) => return Ok(None),
            Some(RawGravatar::RetryAfter(when)) if *when > std::time::Instant::now() => {
                return Err(());
            }
            Some(RawGravatar::RetryAfter(_)) => {
                cache.remove(&key);
            }
            None => {}
        }
    }

    let _slot = GravatarSlot::acquire();
    let response = match ureq::get(&gravatar_url(email))
        .timeout(std::time::Duration::from_secs(10))
        .call()
    {
        Ok(response) => response,
        Err(ureq::Error::Status(404, _)) => {
            GRAVATAR_BYTES
                .get()
                .unwrap()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(key, RawGravatar::Missing);
            return Ok(None);
        }
        Err(_) => {
            cache_gravatar_retry(key);
            return Err(());
        }
    };
    let mut bytes = Vec::new();
    if response
        .into_reader()
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        cache_gravatar_retry(key);
        return Err(());
    }
    let image = if bytes.len() <= MAX_BYTES && supported_raster(&bytes) {
        decode_image(&bytes)
    } else {
        None
    };
    let Some(image) = image else {
        // Only a 404 is a definitive absence. A malformed 200 response may be a
        // proxy/captive-portal/CDN failure, so coalesce it behind a short retry.
        cache_gravatar_retry(key);
        return Err(());
    };
    GRAVATAR_BYTES
        .get()
        .unwrap()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(key, RawGravatar::Found(image.clone()));
    Ok(Some(image))
}

fn cache_gravatar_retry(key: String) {
    GRAVATAR_BYTES
        .get()
        .expect("Gravatar cache initialized")
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(
            key,
            RawGravatar::RetryAfter(
                std::time::Instant::now() + std::time::Duration::from_secs(30),
            ),
        );
}

/// Only hand known raster formats to GdkPixbuf. In particular, an address-book
/// entry cannot smuggle an SVG with external references into the decoder.
fn supported_raster(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(&[0xff, 0xd8, 0xff])
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || bytes.starts_with(b"BM")
        || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"))
}

/// Decode through a size-prepared PixbufLoader so large source images are
/// downscaled during decoding. A pixel limit also rejects decompression bombs.
fn decode_image(bytes: &[u8]) -> Option<DecodedImage> {
    use gtk::gdk_pixbuf::prelude::*;

    const MAX_PIXELS: i64 = 4_194_304;
    const THUMBNAIL_EDGE: i32 = 160;
    let loader = gtk::gdk_pixbuf::PixbufLoader::new();
    let valid = Rc::new(Cell::new(false));
    loader.connect_size_prepared({
        let valid = valid.clone();
        move |loader, width, height| {
            if width <= 0 || height <= 0 || i64::from(width) * i64::from(height) > MAX_PIXELS {
                loader.set_size(1, 1);
                return;
            }
            valid.set(true);
            let longest = width.max(height);
            if longest > THUMBNAIL_EDGE {
                loader.set_size(
                    (width * THUMBNAIL_EDGE / longest).max(1),
                    (height * THUMBNAIL_EDGE / longest).max(1),
                );
            }
        }
    });
    if loader.write(bytes).is_err() {
        let _ = loader.close();
        return None;
    }
    if loader.close().is_err() || !valid.get() {
        return None;
    }
    let pixbuf = loader.pixbuf()?;
    if pixbuf.bits_per_sample() != 8 || pixbuf.n_channels() < 3 {
        return None;
    }
    let width = pixbuf.width();
    let height = pixbuf.height();
    let channels = pixbuf.n_channels() as usize;
    let rowstride = pixbuf.rowstride() as usize;
    let source = pixbuf.read_pixel_bytes();
    let source = source.as_ref();
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height as usize {
        for x in 0..width as usize {
            let offset = y * rowstride + x * channels;
            pixels.extend_from_slice(&source[offset..offset + 3]);
            pixels.push(if channels >= 4 { source[offset + 3] } else { 255 });
        }
    }
    Some(DecodedImage {
        pixels: pixels.into(),
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::supported_raster;

    #[test]
    fn accepts_common_rasters_but_not_svg() {
        assert!(supported_raster(b"\x89PNG\r\n\x1a\nrest"));
        assert!(supported_raster(b"\xff\xd8\xffrest"));
        assert!(!supported_raster(b"<svg xmlns='http://www.w3.org/2000/svg'/>"));
    }
}
