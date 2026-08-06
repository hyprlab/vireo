//! GNOME Contacts integration via Evolution Data Server (EDS).
//!
//! Reading is done straight from the EDS address-book SQLite caches (covers
//! both the local address book and CardDAV books, which EDS mirrors locally).
//! Writing goes through the EDS D-Bus API so changes are tracked by EDS and
//! synced back to CardDAV servers.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

/// A name + email pair from the address book.
#[derive(Debug, Clone)]
pub struct Contact {
    pub name: String,
    pub email: String,
}

/// A recipient autocomplete suggestion (from Contacts or mail history).
#[derive(Debug, Clone)]
pub struct Suggestion {
    pub name: String,
    pub email: String,
    /// True when this came from the address book.
    pub from_contacts: bool,
    /// How often this address appears in mail history (0 for contacts-only).
    pub score: u32,
}

impl Suggestion {
    /// "Name <email>" for inserting into a recipient field (email alone if no name).
    pub fn display(&self) -> String {
        if self.name.trim().is_empty() || self.name == self.email {
            self.email.clone()
        } else {
            format!("{} <{}>", self.name, self.email)
        }
    }

    /// Whether the suggestion matches a typed fragment (name or email).
    pub fn matches(&self, q: &str) -> bool {
        let q = q.to_lowercase();
        self.email.to_lowercase().contains(&q) || self.name.to_lowercase().contains(&q)
    }
}

/// Combined, de-duplicated recipient suggestions from Contacts and mail history
/// (addresses sent to / received from), excluding the user's own `exclude`
/// addresses. History frequency becomes each suggestion's `score` so frequently
/// used addresses — contact or not — can rank ahead.
pub fn suggestions(exclude: &[String]) -> Vec<Suggestion> {
    let exclude: HashSet<String> = exclude.iter().map(|e| e.to_lowercase()).collect();
    let mut map: std::collections::HashMap<String, Suggestion> = std::collections::HashMap::new();

    for c in read_contacts() {
        let key = c.email.to_lowercase();
        if exclude.contains(&key) {
            continue;
        }
        map.entry(key).or_insert(Suggestion {
            name: c.name,
            email: c.email,
            from_contacts: true,
            score: 0,
        });
    }

    if let Ok(cache) = crate::cache::Cache::open() {
        for (name, email, count) in cache.address_history() {
            let key = email.to_lowercase();
            if exclude.contains(&key) {
                continue;
            }
            map.entry(key)
                .and_modify(|s| s.score = s.score.max(count))
                .or_insert(Suggestion { name, email, from_contacts: false, score: count });
        }
    }

    let mut out: Vec<Suggestion> = map.into_values().collect();
    // Default order: most-used first, then name (the field filter re-ranks live).
    out.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

/// A writable address book the user can add contacts to.
#[derive(Debug, Clone)]
pub struct Book {
    /// EDS source UID (what `OpenAddressBook` expects).
    pub uid: String,
    pub name: String,
}

// Inside a Flatpak sandbox `dirs::{data,cache,config}_dir()` are redirected into
// ~/.var/app/<app-id>/, but EDS's books live in the *host's* real XDG dirs (made
// visible by the `--filesystem=xdg-{data,cache,config}/evolution:ro` grants). So
// under Flatpak resolve them from the real home instead of the redirected XDG env.
fn host_xdg(sandbox: fn() -> Option<PathBuf>, subdir: &str) -> Option<PathBuf> {
    if crate::platform::is_flatpak() {
        dirs::home_dir().map(|h| h.join(subdir))
    } else {
        sandbox()
    }
}

fn data_dir() -> Option<PathBuf> {
    host_xdg(dirs::data_dir, ".local/share").map(|d| d.join("evolution/addressbook"))
}

fn cache_dir() -> Option<PathBuf> {
    host_xdg(dirs::cache_dir, ".cache").map(|d| d.join("evolution/addressbook"))
}

fn config_dir() -> Option<PathBuf> {
    host_xdg(dirs::config_dir, ".config").map(|d| d.join("evolution"))
}

/// Read every address email from the local + cached (CardDAV) EDS books.
pub fn read_contacts() -> Vec<Contact> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Local books: table `folder_id` + `folder_id_email_list`. Read the vCard
    // (not `full_name`, which EDS stores case-folded for search) so the display
    // name keeps its original capitalisation.
    if let Some(dir) = data_dir() {
        for db in find_dbs(&dir, "contacts.db") {
            read_book_db(
                &db,
                "SELECT f.vcard, e.value FROM folder_id f \
                 JOIN folder_id_email_list e ON f.uid = e.uid",
                &mut out,
                &mut seen,
            );
        }
    }
    // Cached books (CardDAV etc.): table `ECacheObjects` + `attrlist_email_list`.
    if let Some(dir) = cache_dir() {
        for db in find_dbs(&dir, "cache.db") {
            read_book_db(
                &db,
                "SELECT o.ECacheOBJ, e.value FROM ECacheObjects o \
                 JOIN attrlist_email_list e ON o.ECacheUID = e.uid",
                &mut out,
                &mut seen,
            );
        }
    }
    out
}

/// Photo bytes and the EDS database state they came from. The inexpensive
/// fingerprint check lets a long-running Vireo notice CardDAV synchronizations
/// without subscribing to backend-specific EDS signals.
#[derive(Clone)]
struct PhotoLocation {
    db: PathBuf,
    uid: String,
    cached_book: bool,
}

struct ContactPhotoIndex {
    photos: HashMap<String, Vec<PhotoLocation>>,
    fingerprint: Vec<(PathBuf, u64, u128)>,
    generation: u64,
    ready: bool,
}

static CONTACT_PHOTOS: OnceLock<Arc<Mutex<ContactPhotoIndex>>> = OnceLock::new();
static PHOTO_LOAD_STARTED: OnceLock<()> = OnceLock::new();
static PHOTO_WATCH_STARTED: OnceLock<()> = OnceLock::new();
type PhotoChangeCallback = Box<dyn Fn() + Send + Sync + 'static>;
static PHOTO_CHANGE_CALLBACKS: OnceLock<Mutex<Vec<PhotoChangeCallback>>> = OnceLock::new();
const PHOTO_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

fn new_photo_index() -> Arc<Mutex<ContactPhotoIndex>> {
    Arc::new(Mutex::new(ContactPhotoIndex {
        photos: HashMap::new(),
        fingerprint: Vec::new(),
        generation: 1,
        ready: false,
    }))
}

fn load_stable_photo_index() -> (HashMap<String, Vec<PhotoLocation>>, Vec<(PathBuf, u64, u128)>) {
    let mut last = (HashMap::new(), Vec::new());
    for _ in 0..3 {
        let before = photo_db_fingerprint();
        let photos = load_contact_photo_index();
        let after = photo_db_fingerprint();
        last = (photos, after.clone());
        if before == after {
            break;
        }
    }
    last
}

fn notify_photo_change() {
    if let Some(callbacks) = PHOTO_CHANGE_CALLBACKS.get() {
        for callback in callbacks.lock().unwrap_or_else(|p| p.into_inner()).iter() {
            callback();
        }
    }
}

fn start_photo_load() {
    PHOTO_LOAD_STARTED.get_or_init(|| {
        let index = CONTACT_PHOTOS.get_or_init(new_photo_index).clone();
        let _ = std::thread::Builder::new()
            .name("eds-photo-load".into())
            .spawn(move || {
                let (photos, fingerprint) = load_stable_photo_index();
                let mut current = index.lock().unwrap_or_else(|p| p.into_inner());
                current.photos = photos;
                current.fingerprint = fingerprint;
                current.generation = current.generation.wrapping_add(1).max(1);
                current.ready = true;
                drop(current);
                notify_photo_change();
            });
    });
}

fn start_photo_watcher() {
    PHOTO_WATCH_STARTED.get_or_init(|| {
        let index = CONTACT_PHOTOS.get_or_init(new_photo_index).clone();
        let _ = std::thread::Builder::new()
            .name("eds-photo-watch".into())
            .spawn(move || loop {
                std::thread::sleep(PHOTO_REFRESH_INTERVAL);
                let fingerprint = photo_db_fingerprint();
                let changed = {
                    let current = index.lock().unwrap_or_else(|p| p.into_inner());
                    fingerprint != current.fingerprint
                };
                if !changed {
                    continue;
                }
                // Scan outside the lock. UI lookups continue using the previous
                // immutable snapshot until the replacement is ready.
                let (photos, stable_fingerprint) = load_stable_photo_index();
                let mut current = index.lock().unwrap_or_else(|p| p.into_inner());
                current.photos = photos;
                current.fingerprint = stable_fingerprint;
                current.generation = current.generation.wrapping_add(1).max(1);
                drop(current);
                notify_photo_change();
            });
    });
}

pub fn watch_photo_changes<F: Fn() + Send + Sync + 'static>(callback: F) {
    PHOTO_CHANGE_CALLBACKS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(Box::new(callback));
    start_photo_load();
    start_photo_watcher();
}

/// Current index generation. Avatar textures and negative results use this to
/// invalidate themselves after an EDS/CardDAV database change.
pub fn photo_generation() -> u64 {
    start_photo_load();
    start_photo_watcher();
    CONTACT_PHOTOS
        .get_or_init(new_photo_index)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .generation
}

pub fn photos_ready() -> bool {
    start_photo_load();
    CONTACT_PHOTOS
        .get_or_init(new_photo_index)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .ready
}

/// Find contact-photo candidates by email in the local EDS caches. Multiple
/// books can contain the same address; the avatar loader tries each candidate.
/// CardDAV backends such as Nextcloud keep PHOTO in the cached vCard, so this
/// does not contact the address-book server or disclose the sender address to a third
/// party.
pub fn contact_photos(email: &str) -> Vec<Arc<[u8]>> {
    let key = email.trim().to_lowercase();
    if key.is_empty() {
        return Vec::new();
    }
    start_photo_load();
    start_photo_watcher();
    let locations = CONTACT_PHOTOS
        .get_or_init(new_photo_index)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .photos
        .get(&key)
        .cloned()
        .unwrap_or_default();
    locations
        .iter()
        .filter_map(read_photo_at)
        .map(Arc::from)
        .collect()
}

fn photo_db_fingerprint() -> Vec<(PathBuf, u64, u128)> {
    let mut files = Vec::new();
    for db in photo_db_paths() {
        files.push(db.clone());
        for suffix in ["-journal", "-wal"] {
            files.push(PathBuf::from(format!("{}{suffix}", db.to_string_lossy())));
        }
    }
    files.sort();
    files
        .into_iter()
        .filter_map(|path| {
            let metadata = std::fs::metadata(&path).ok()?;
            let modified = metadata
                .modified()
                .unwrap_or(SystemTime::UNIX_EPOCH)
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            Some((path, metadata.len(), modified))
        })
        .collect()
}

fn photo_db_paths() -> Vec<PathBuf> {
    let mut dbs = Vec::new();
    if let Some(dir) = data_dir() {
        dbs.extend(find_dbs(&dir, "contacts.db"));
    }
    if let Some(dir) = cache_dir() {
        dbs.extend(find_dbs(&dir, "cache.db"));
    }
    dbs.sort();
    dbs
}

fn load_contact_photo_index() -> HashMap<String, Vec<PhotoLocation>> {
    let mut photos = HashMap::new();
    if let Some(dir) = data_dir() {
        for db in find_dbs(&dir, "contacts.db") {
            read_photo_locations(
                &db,
                "SELECT e.value, f.uid FROM folder_id f \
                 JOIN folder_id_email_list e ON f.uid = e.uid \
                 WHERE f.vcard LIKE '%PHOTO%'",
                false,
                &mut photos,
            );
        }
    }
    if let Some(dir) = cache_dir() {
        for db in find_dbs(&dir, "cache.db") {
            read_photo_locations(
                &db,
                "SELECT e.value, o.ECacheUID FROM ECacheObjects o \
                 JOIN attrlist_email_list e ON o.ECacheUID = e.uid \
                 WHERE o.ECacheOBJ LIKE '%PHOTO%'",
                true,
                &mut photos,
            );
        }
    }
    tracing::debug!(count = photos.len(), "indexed EDS contact photos");
    photos
}

fn read_photo_locations(
    path: &std::path::Path,
    query: &str,
    cached_book: bool,
    photos: &mut HashMap<String, Vec<PhotoLocation>>,
) {
    let conn = match open_book_db(path) {
        Ok(conn) => conn,
        Err(e) => {
            tracing::warn!("contacts: cannot open {} for photos: {e}", path.display());
            return;
        }
    };
    let mut stmt = match conn.prepare(query) {
        Ok(stmt) => stmt,
        Err(e) => {
            tracing::warn!("contacts: photo query failed on {}: {e}", path.display());
            return;
        }
    };
    let rows = match stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("contacts: photo rows failed on {}: {e}", path.display());
            return;
        }
    };
    for (email, uid) in rows.flatten() {
        let key = email.trim().to_lowercase();
        if !key.is_empty() {
            let location = PhotoLocation {
                db: path.to_path_buf(),
                uid,
                cached_book,
            };
            photos.entry(key).or_default().push(location);
        }
    }
}

fn read_photo_at(location: &PhotoLocation) -> Option<Vec<u8>> {
    let conn = open_book_db(&location.db).ok()?;
    let query = if location.cached_book {
        "SELECT ECacheOBJ FROM ECacheObjects WHERE ECacheUID = ?1"
    } else {
        "SELECT vcard FROM folder_id WHERE uid = ?1"
    };
    let vcard: Option<String> = conn
        .query_row(query, [&location.uid], |row| row.get(0))
        .ok()?;
    vcard.as_deref().and_then(vcard_photo)
}

fn find_dbs(root: &std::path::Path, file: &str) -> Vec<PathBuf> {
    let mut dbs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let db = entry.path().join(file);
            if db.is_file() {
                dbs.push(db);
            }
        }
    }
    dbs.sort();
    dbs
}

/// Extract the display name (`FN`) from a vCard, preserving its capitalisation.
/// Handles line folding (a CRLF/LF followed by a space or tab continues the
/// previous line) and the standard text escapes.
fn unfold_vcard(vcard: &str) -> String {
    vcard
        .replace("\r\n ", "")
        .replace("\r\n\t", "")
        .replace("\n ", "")
        .replace("\n\t", "")
}

fn vcard_display_name(vcard: &str) -> Option<String> {
    for line in unfold_vcard(vcard).lines() {
        let Some((prop, value)) = line.split_once(':') else {
            continue;
        };
        // The property name is everything before the first ';' (which begins any
        // parameters), e.g. `FN` or `FN;PID=1.1`.
        let name = prop.split(';').next().unwrap_or("").trim();
        if name.eq_ignore_ascii_case("FN") {
            let value = unescape_vcard_text(value);
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Decode an embedded vCard PHOTO. Handles both Nextcloud's escaped data URI
/// form and the conventional `PHOTO;ENCODING=b` form. Remote PHOTO URLs are
/// deliberately not fetched, both for privacy and to avoid tracking.
fn vcard_photo(vcard: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;

    const MAX_PHOTO_BYTES: usize = 2_000_000;
    for line in unfold_vcard(vcard).lines() {
        let Some((property, raw_value)) = line.split_once(':') else {
            continue;
        };
        let name = property
            .split(';')
            .next()
            .unwrap_or("")
            .rsplit('.')
            .next()
            .unwrap_or("")
            .trim();
        if !name.eq_ignore_ascii_case("PHOTO") {
            continue;
        }

        let value = raw_value.replace("\\;", ";").replace("\\,", ",");
        let property_upper = property.to_ascii_uppercase();
        let payload = if value.to_ascii_lowercase().starts_with("data:image/") {
            let Some((metadata, payload)) = value.split_once(',') else {
                continue;
            };
            if !metadata.to_ascii_lowercase().ends_with(";base64") {
                continue;
            }
            payload
        } else if property_upper.contains("ENCODING=B")
            || property_upper.contains("ENCODING=BASE64")
        {
            value.as_str()
        } else if value.to_ascii_lowercase().starts_with("file://") {
            if let Some(bytes) = read_eds_photo_file(&value, MAX_PHOTO_BYTES) {
                return Some(bytes);
            }
            continue;
        } else {
            // Never fetch http(s) or other remote PHOTO URIs implicitly.
            continue;
        };

        let encoded: String = payload.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        // Base64 expands data by roughly 4/3. Reject oversized input before
        // allocating the decoded image buffer.
        if encoded.len() > (MAX_PHOTO_BYTES * 4 / 3 + 4) {
            continue;
        }
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) {
            if !bytes.is_empty() && bytes.len() <= MAX_PHOTO_BYTES {
                return Some(bytes);
            }
        }
    }
    None
}

/// Read an EDS-materialized `file://` photo, but only from an address-book data
/// or cache directory. A CardDAV vCard must not be able to make Vireo read an
/// arbitrary local file.
fn read_eds_photo_file(uri: &str, max_bytes: usize) -> Option<Vec<u8>> {
    use std::io::Read;

    let (path, host) = gtk::glib::filename_from_uri(uri).ok()?;
    if host.is_some() {
        return None;
    }
    let path = path.canonicalize().ok()?;
    let allowed = [data_dir(), cache_dir()]
        .into_iter()
        .flatten()
        .filter_map(|root| root.canonicalize().ok())
        .any(|root| path.starts_with(root));
    if !allowed {
        return None;
    }

    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (!bytes.is_empty() && bytes.len() <= max_bytes).then_some(bytes)
}

/// Unescape a vCard TEXT value: `\n`/`\N` → newline, `\,` → `,`, `\;` → `;`,
/// `\\` → `\`.
fn unescape_vcard_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn open_book_db(path: &std::path::Path) -> rusqlite::Result<rusqlite::Connection> {
    use rusqlite::OpenFlags;
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    // Under Flatpak the book DB lives on a read-only host mount; a WAL database
    // can't be opened read-only there (it needs to touch the -shm/-wal files), so
    // open it `immutable=1` to read the committed snapshot without any locking.
    if crate::platform::is_flatpak() {
        rusqlite::Connection::open_with_flags(
            format!("file:{}?immutable=1", path.to_string_lossy()),
            flags,
        )
    } else {
        rusqlite::Connection::open_with_flags(path, flags)
    }
}

fn read_book_db(path: &std::path::Path, query: &str, out: &mut Vec<Contact>, seen: &mut HashSet<String>) {
    let conn = match open_book_db(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("contacts: cannot open {}: {e}", path.display());
            return;
        }
    };
    let mut stmt = match conn.prepare(query) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("contacts: query failed on {}: {e}", path.display());
            return;
        }
    };
    let rows = stmt.query_map([], |row| {
        let vcard: Option<String> = row.get(0).ok();
        let email: String = row.get(1)?;
        let name = vcard.as_deref().and_then(vcard_display_name).unwrap_or_default();
        Ok((name, email))
    });
    if let Ok(rows) = rows {
        for (name, email) in rows.flatten() {
            let email = email.trim().to_string();
            if email.is_empty() || !email.contains('@') {
                continue;
            }
            let key = email.to_lowercase();
            if seen.insert(key) {
                let name = if name.trim().is_empty() { email.clone() } else { name };
                out.push(Contact { name, email });
            }
        }
    }
}

/// Address books the user can add contacts to (local + cached/CardDAV).
pub fn writable_books() -> Vec<Book> {
    let mut books = Vec::new();
    if let Some(dir) = data_dir() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if !entry.path().join("contacts.db").is_file() {
                continue;
            }
            let folder = entry.file_name().to_string_lossy().to_string();
            // The built-in local book's source UID is "system-address-book".
            let uid = if folder == "system" {
                "system-address-book".to_string()
            } else {
                folder
            };
            let name = source_display_name(&uid).unwrap_or_else(|| "On This Computer".to_string());
            books.push(Book { uid, name });
        }
    }
    if let Some(dir) = cache_dir() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if !entry.path().join("cache.db").is_file() {
                continue;
            }
            let uid = entry.file_name().to_string_lossy().to_string();
            let name = source_display_name(&uid).unwrap_or_else(|| "CardDAV Address Book".to_string());
            books.push(Book { uid, name });
        }
    }
    books
}

/// Best-effort display name from the EDS source file for a book UID.
fn source_display_name(uid: &str) -> Option<String> {
    let path = config_dir()?.join(format!("sources/{uid}.source"));
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("DisplayName="))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// Writing (EDS D-Bus)
// ---------------------------------------------------------------------------

/// Discover the versioned AddressBook factory bus name (e.g. `…AddressBook10`).
fn factory_dest() -> Option<String> {
    let dir = std::path::Path::new("/usr/share/dbus-1/services");
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.contains("AddressBook") && name.ends_with(".service") {
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                if let Some(n) = text.lines().find_map(|l| l.strip_prefix("Name=")) {
                    let n = n.trim();
                    if n.contains("AddressBook") {
                        return Some(n.to_string());
                    }
                }
            }
        }
    }
    None
}

const FACTORY_PATH: &str = "/org/gnome/evolution/dataserver/AddressBookFactory";
const FACTORY_IFACE: &str = "org.gnome.evolution.dataserver.AddressBookFactory";
const BOOK_IFACE: &str = "org.gnome.evolution.dataserver.AddressBook";

/// Open a book by source UID; returns (bus_name, object_path) for the book.
fn open_book(
    conn: &zbus::blocking::Connection,
    dest: &str,
    uid: &str,
) -> Result<(String, String), String> {
    let reply = conn
        .call_method(Some(dest), FACTORY_PATH, Some(FACTORY_IFACE), "OpenAddressBook", &(uid,))
        .map_err(|e| e.to_string())?;
    // Returns (object_path, bus_name).
    let (object_path, bus_name): (String, String) =
        reply.body().deserialize().map_err(|e| e.to_string())?;
    conn.call_method(Some(bus_name.as_str()), object_path.as_str(), Some(BOOK_IFACE), "Open", &())
        .map_err(|e| e.to_string())?;
    Ok((bus_name, object_path))
}

/// What happened when adding an email to Contacts.
#[derive(Debug)]
pub enum AddOutcome {
    /// A new contact was created.
    Created,
    /// The email was merged into an existing contact (carries its name).
    Merged(String),
    /// The email was already in the address book (carries the contact's name).
    AlreadyPresent(String),
}

/// Add `email` (with `name`) to Contacts, detecting duplicates and merging:
/// - if the email already exists anywhere, do nothing;
/// - else if a contact with the same name exists in the book, append the email;
/// - else create a new contact. Blocking — call from a background thread.
pub fn add_or_merge(book_uid: &str, name: &str, email: &str) -> Result<AddOutcome, String> {
    // 1. Already present (in any book)? Don't create a duplicate.
    let email_l = email.trim().to_lowercase();
    if let Some(existing) = read_contacts()
        .into_iter()
        .find(|c| c.email.to_lowercase() == email_l)
    {
        return Ok(AddOutcome::AlreadyPresent(existing.name));
    }

    let dest = factory_dest().ok_or("Evolution Data Server is not available")?;
    let conn = zbus::blocking::Connection::session().map_err(|e| e.to_string())?;
    let (bus, path) = open_book(&conn, &dest, book_uid)?;

    // 2. A contact with the same name in this book → merge the email into it.
    if !name.trim().is_empty() {
        let query = format!("(is \"full_name\" \"{}\")", sexpr_escape(name.trim()));
        let uids = book_get_uids(&conn, &bus, &path, &query)?;
        if let Some(uid) = uids.first() {
            let vcard = book_get_contact(&conn, &bus, &path, uid)?;
            let merged = add_email_to_vcard(&vcard, email.trim());
            book_call(&conn, &bus, &path, "ModifyContacts", &(vec![merged], 0u32))?;
            return Ok(AddOutcome::Merged(name.trim().to_string()));
        }
    }

    // 3. Create a new contact.
    book_call(
        &conn,
        &bus,
        &path,
        "CreateContacts",
        &(vec![vcard_for(name, email)], 0u32),
    )?;
    Ok(AddOutcome::Created)
}

fn book_call<B>(
    conn: &zbus::blocking::Connection,
    bus: &str,
    path: &str,
    method: &str,
    body: &B,
) -> Result<zbus::Message, String>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    conn.call_method(Some(bus), path, Some(BOOK_IFACE), method, body)
        .map_err(|e| e.to_string())
}

fn book_get_uids(
    conn: &zbus::blocking::Connection,
    bus: &str,
    path: &str,
    query: &str,
) -> Result<Vec<String>, String> {
    let reply = book_call(conn, bus, path, "GetContactListUids", &(query,))?;
    let (uids,): (Vec<String>,) = reply.body().deserialize().map_err(|e| e.to_string())?;
    Ok(uids)
}

fn book_get_contact(
    conn: &zbus::blocking::Connection,
    bus: &str,
    path: &str,
    uid: &str,
) -> Result<String, String> {
    let reply = book_call(conn, bus, path, "GetContact", &(uid,))?;
    let (vcard,): (String,) = reply.body().deserialize().map_err(|e| e.to_string())?;
    Ok(vcard)
}

/// Insert an EMAIL line into an existing vCard, just before END:VCARD.
fn add_email_to_vcard(vcard: &str, email: &str) -> String {
    let line = format!("EMAIL;TYPE=INTERNET:{email}\r\n");
    match vcard.rfind("END:VCARD") {
        Some(pos) => {
            let mut s = String::with_capacity(vcard.len() + line.len());
            s.push_str(&vcard[..pos]);
            if !s.ends_with('\n') {
                s.push_str("\r\n");
            }
            s.push_str(&line);
            s.push_str(&vcard[pos..]);
            s
        }
        None => vcard.to_string(),
    }
}

/// Escape a value for an EDS S-expression string literal.
fn sexpr_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn vcard_for(name: &str, email: &str) -> String {
    let name = if name.trim().is_empty() { email } else { name.trim() };
    // Minimal vCard 3.0; EDS assigns the UID.
    format!(
        "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:{name}\r\nN:;{name};;;\r\n\
         EMAIL;TYPE=INTERNET:{email}\r\nEND:VCARD\r\n"
    )
}

#[cfg(test)]
mod tests {
    use super::{vcard_display_name, vcard_photo};

    const ONE_PIXEL_PNG: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    #[test]
    fn fn_preserves_case() {
        let v = "BEGIN:VCARD\r\nVERSION:3.0\r\nN:Arnwine;Aaron;;;\r\nFN:Aaron Arnwine\r\nEND:VCARD\r\n";
        assert_eq!(vcard_display_name(v).as_deref(), Some("Aaron Arnwine"));
    }

    #[test]
    fn fn_with_parameters() {
        let v = "FN;PID=1.1:Jane O'Brien\r\n";
        assert_eq!(vcard_display_name(v).as_deref(), Some("Jane O'Brien"));
    }

    #[test]
    fn fn_line_folded() {
        // A folded FN value continues on the next line after a leading space.
        let v = "FN:Reallylongfirst\r\n Lastname\r\n";
        assert_eq!(vcard_display_name(v).as_deref(), Some("ReallylongfirstLastname"));
    }

    #[test]
    fn fn_escaped_comma() {
        let v = "FN:Smith\\, John\r\n";
        assert_eq!(vcard_display_name(v).as_deref(), Some("Smith, John"));
    }

    #[test]
    fn empty_or_missing_fn() {
        assert_eq!(vcard_display_name("FN:\r\nN:x;;;;\r\n"), None);
        assert_eq!(vcard_display_name("N:Doe;John;;;\r\n"), None);
    }

    #[test]
    fn photo_decodes_nextcloud_data_uri() {
        let vcard = format!("item1.PHOTO:data:image/image/png\\;base64\\,{ONE_PIXEL_PNG}\r\n");
        let photo = vcard_photo(&vcard).unwrap();
        assert_eq!(&photo[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn photo_decodes_folded_encoding_b() {
        let split = ONE_PIXEL_PNG.len() / 2;
        let vcard = format!(
            "PHOTO;ENCODING=b;TYPE=PNG:{}\r\n {}\r\n",
            &ONE_PIXEL_PNG[..split],
            &ONE_PIXEL_PNG[split..]
        );
        assert!(vcard_photo(&vcard).is_some());
    }

    #[test]
    fn photo_does_not_fetch_remote_uri() {
        assert!(vcard_photo("PHOTO;VALUE=uri:https://example.com/tracker.png\r\n").is_none());
    }
}
