//! Account configuration.
//!
//! Account metadata (name, email, servers, username) lives in
//! `~/.config/vireo/accounts.toml`. Passwords are kept in the system keyring
//! (secret-service, e.g. gnome-keyring) — never written to disk. The `password`
//! field is read from the TOML if present (older configs / manual setup) and
//! migrated into the keyring on first use, then stripped from the file.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Service name used for keyring entries; password items are keyed by email.
const KEYRING_SERVICE: &str = "co.hyprlab.Vireo";

/// Incoming-mail protocol for an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Imap,
    Pop3,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AccountConfig {
    pub name: String,
    pub email: String,
    /// Incoming-mail protocol (IMAP or POP3).
    #[serde(default)]
    pub protocol: Protocol,
    /// Incoming server host (IMAP or POP3, per `protocol`).
    pub imap_host: String,
    /// Incoming server port (IMAP or POP3, per `protocol`).
    #[serde(default = "default_imap_port")]
    pub imap_port: u16,
    /// Upgrade a plaintext IMAP connection with STARTTLS (normally port 143).
    /// False means TLS from the first byte (normally port 993).
    #[serde(default)]
    pub imap_starttls: bool,
    /// Explicit implicit-TLS mode, needed when GOA uses a custom/nonstandard
    /// port (including an unusual implicit-TLS service on port 143).
    #[serde(default)]
    pub imap_implicit_tls: bool,
    /// SMTP server. If empty, derived from `imap_host` (imap.* → smtp.*).
    #[serde(default)]
    pub smtp_host: String,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    /// Use TLS from the first byte instead of STARTTLS. Port 465 implies this
    /// for compatibility; GOA may also specify implicit TLS on a custom port.
    #[serde(default)]
    pub smtp_implicit_tls: bool,
    /// Whether the SMTP server requires authentication. GOA exposes this
    /// explicitly; native accounts default to authenticated SMTP.
    #[serde(default = "default_smtp_auth")]
    pub smtp_auth: bool,
    pub username: String,
    /// Read from TOML if present (legacy/manual), but never written back —
    /// passwords belong in the keyring. Usually empty after the first run.
    #[serde(default, skip_serializing)]
    pub password: String,
    /// Use distinct SMTP credentials instead of the IMAP ones.
    #[serde(default)]
    pub smtp_separate: bool,
    /// SMTP username (used only when `smtp_separate`).
    #[serde(default)]
    pub smtp_username: String,
    /// SMTP password — kept in the keyring (separate entry), never on disk.
    #[serde(default, skip_serializing)]
    pub smtp_password: String,
    /// Sidebar avatar background colour ("#rrggbb"). Falls back to the auto accent.
    #[serde(default)]
    pub color: Option<String>,
    /// Sidebar avatar emoji; when absent, the account-name initials are shown.
    #[serde(default)]
    pub emoji: Option<String>,
    /// Composition signature appended to new messages from this account.
    #[serde(default)]
    pub signature: Option<String>,
    /// Whether `signature` is HTML (vs. plain text).
    #[serde(default)]
    pub signature_html: bool,
    /// How this account is labelled in the UI (e.g. the All Inboxes view).
    /// When unset, the email address is shown.
    #[serde(default)]
    pub label: Option<String>,
    /// Whether the account is active. Disabled accounts stay configured but don't
    /// connect, sync, or appear in the sidebar.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// When imported from GNOME Online Accounts, the GOA account id (so its
    /// settings/credentials trace back to the system account).
    #[serde(default)]
    pub goa_id: Option<String>,
    /// Temporarily disabled because Mail was switched off in GOA. This preserves
    /// local presentation settings and restores the prior enabled state later.
    #[serde(default)]
    pub goa_mail_disabled: bool,
    /// Local enabled state to restore after GOA Mail is switched back on.
    #[serde(default)]
    pub goa_enabled_before_mail_disabled: bool,
    /// Authenticate with OAuth2 (XOAUTH2) instead of a stored password. The token
    /// comes from GOA (`goa_id`) or, for accounts added directly in Vireo, from
    /// refreshing `oauth_settings` with the keyring-stored refresh token.
    #[serde(default)]
    pub oauth: bool,
    /// OAuth2 endpoints/client for a natively-added OAuth account (no GOA).
    #[serde(default)]
    pub oauth_settings: Option<OAuthSettings>,
    /// OAuth2 refresh token — kept in the keyring, never on disk. Transient in
    /// memory (like `password`); stored on save.
    #[serde(default, skip_serializing)]
    pub oauth_refresh: String,
}

/// OAuth2 client configuration for an account added directly in Vireo.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct OAuthSettings {
    pub auth_url: String,
    pub token_url: String,
    pub client_id: String,
    /// Optional; public/installed clients often have none.
    #[serde(default)]
    pub client_secret: String,
    pub scopes: String,
}

fn default_enabled() -> bool {
    true
}

impl AccountConfig {
    /// The account's UI label: the custom label, or the email address.
    pub fn display_label(&self) -> String {
        match self.label.as_deref() {
            Some(l) if !l.trim().is_empty() => l.to_string(),
            _ => self.email.clone(),
        }
    }

    /// Port 143 is the conventional IMAP STARTTLS endpoint. Treat it as such
    /// even for older configs written before `imap_starttls` existed; this also
    /// prevents accidentally attempting implicit TLS against a plaintext port.
    pub fn imap_uses_starttls(&self) -> bool {
        !self.imap_implicit_tls
            && (self.imap_starttls || (self.protocol == Protocol::Imap && self.imap_port == 143))
    }

    pub fn smtp_uses_implicit_tls(&self) -> bool {
        self.smtp_implicit_tls || self.smtp_port == 465
    }
}

fn default_imap_port() -> u16 {
    993
}

fn default_smtp_port() -> u16 {
    587
}

fn default_smtp_auth() -> bool {
    true
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ConfigFile {
    #[serde(default)]
    accounts: Vec<AccountConfig>,
}

/// Path to the accounts config file (`~/.config/vireo/accounts.toml`).
pub fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("vireo").join("accounts.toml"))
}

/// Whether the accounts file is absent or parses successfully. Automatic
/// discovery must not overwrite an existing malformed file that the user may
/// still repair.
pub fn accounts_file_is_parseable() -> bool {
    let Some(path) = path() else {
        return false;
    };
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str::<ConfigFile>(&text).is_ok(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

/// Returns the configured accounts, or `None` if there is no usable config
/// (missing file, parse error, or empty list) — in which case the app falls
/// back to the offline sample backend.
pub fn load() -> Option<Vec<AccountConfig>> {
    let path = path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    match toml::from_str::<ConfigFile>(&text) {
        Ok(cfg) if !cfg.accounts.is_empty() => {
            tracing::info!("loaded {} account(s) from {}", cfg.accounts.len(), path.display());
            Some(cfg.accounts)
        }
        Ok(_) => None,
        Err(e) => {
            tracing::error!("failed to parse {}: {e}", path.display());
            None
        }
    }
}

/// Write account metadata to disk (no passwords) and store each password in the
/// keyring.
///
/// Passwords live only in the keyring, so an in-memory `AccountConfig` loaded
/// from disk has an empty `password`. We must NEVER store an empty password —
/// doing so would wipe the keyring entry of any account that wasn't just edited.
pub fn save(accounts: &[AccountConfig]) -> std::io::Result<()> {
    write_config(accounts)?;
    for account in accounts {
        if !account.password.is_empty() {
            if let Err(e) = store_password(&account.email, &account.password) {
                tracing::error!("could not store password for {}: {e}", account.email);
            }
        }
        // Same empty-guard for the separate SMTP password.
        if account.smtp_separate && !account.smtp_password.is_empty() {
            if let Err(e) = store_smtp_password(&account.email, &account.smtp_password) {
                tracing::error!("could not store SMTP password for {}: {e}", account.email);
            }
        }
        // OAuth refresh token (never overwrite a stored one with an empty value).
        if !account.oauth_refresh.is_empty() {
            if let Err(e) = store_oauth_refresh(&account.email, &account.oauth_refresh) {
                tracing::error!("could not store OAuth token for {}: {e}", account.email);
            }
        }
    }
    Ok(())
}

/// Rewrite the config file from the current accounts (dropping any plaintext
/// passwords still on disk). Used after migrating a legacy password.
pub fn strip_passwords_on_disk() {
    if let Some(accounts) = load() {
        if let Err(e) = write_config(&accounts) {
            tracing::warn!("could not rewrite config without passwords: {e}");
        }
    }
}

fn write_config(accounts: &[AccountConfig]) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};

    let path = path().ok_or_else(|| Error::new(ErrorKind::NotFound, "no config directory"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }

    let file = ConfigFile {
        accounts: accounts.to_vec(),
    };
    let toml =
        toml::to_string_pretty(&file).map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
    std::fs::write(&path, toml)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }

    tracing::info!("saved {} account(s) to {}", accounts.len(), path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Keyring (secret-service)
// ---------------------------------------------------------------------------

fn keyring_entry(key: &str) -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, key)
}

/// Keyring key for an account's separate SMTP password.
fn smtp_key(email: &str) -> String {
    format!("smtp:{email}")
}

/// Keyring key for a natively-added OAuth account's refresh token.
fn oauth_key(email: &str) -> String {
    format!("oauth:{email}")
}

pub fn store_oauth_refresh(email: &str, token: &str) -> keyring::Result<()> {
    keyring_entry(&oauth_key(email))?.set_password(token)
}

pub fn load_oauth_refresh(email: &str) -> Option<String> {
    load_key(&oauth_key(email))
}

pub fn store_password(email: &str, password: &str) -> keyring::Result<()> {
    keyring_entry(email)?.set_password(password)
}

pub fn load_password(email: &str) -> Option<String> {
    load_key(email)
}

pub fn store_smtp_password(email: &str, password: &str) -> keyring::Result<()> {
    keyring_entry(&smtp_key(email))?.set_password(password)
}

pub fn load_smtp_password(email: &str) -> Option<String> {
    load_key(&smtp_key(email))
}

fn load_key(key: &str) -> Option<String> {
    match keyring_entry(key).and_then(|e| e.get_password()) {
        Ok(password) => Some(password),
        Err(keyring::Error::NoEntry) => load_legacy_key(key),
        Err(e) => {
            tracing::warn!("could not read keyring entry for {key}: {e}");
            None
        }
    }
}

/// Keyring service name used before the 1.6.0 rename (Veem → Vireo).
const LEGACY_KEYRING_SERVICE: &str = "com.getveem.Veem";

/// Fall back to an entry stored under the pre-rename service, moving it to the
/// current service so accounts added as Veem keep working after the rename.
fn load_legacy_key(key: &str) -> Option<String> {
    let old = keyring::Entry::new(LEGACY_KEYRING_SERVICE, key).ok()?;
    let password = old.get_password().ok()?;
    if let Ok(new) = keyring_entry(key) {
        if new.set_password(&password).is_ok() {
            let _ = old.delete_credential();
        }
    }
    Some(password)
}

pub fn delete_password(email: &str) {
    delete_key(email);
    // Also drop the account's separate SMTP password and OAuth token, if any.
    delete_key(&smtp_key(email));
    delete_key(&oauth_key(email));
}

fn delete_key(key: &str) {
    if let Ok(entry) = keyring_entry(key) {
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(e) => tracing::warn!("could not delete keyring entry for {key}: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Privacy settings (remote-content allowlist)
// ---------------------------------------------------------------------------

/// How email message content is themed, independent of the app UI theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageTheme {
    /// Follow the system / app light-dark preference.
    #[default]
    System,
    Light,
    Dark,
}

impl MessageTheme {
    /// Forced dark flag for message content, or `None` to follow the system.
    pub fn dark_override(self) -> Option<bool> {
        match self {
            MessageTheme::System => None,
            MessageTheme::Light => Some(false),
            MessageTheme::Dark => Some(true),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct PrivacyFile {
    #[serde(default)]
    allowed_senders: Vec<String>,
    /// Whether to load sender avatars from Gravatar (off by default — it sends
    /// a hash of each sender's email to a third party).
    #[serde(default)]
    gravatar: bool,
    /// Seconds between automatic mail checks; 0 = manual only.
    #[serde(default = "default_fetch_interval")]
    fetch_interval_secs: u64,
    /// Whether to use IMAP IDLE push for instant new-mail delivery.
    #[serde(default = "default_push")]
    push: bool,
    /// Addresses or whole domains whose incoming mail is auto-deleted (to Trash).
    /// Stored lowercased; a bare domain like "spam.com" matches any sender there.
    #[serde(default)]
    blacklist: Vec<String>,
    /// Seconds the message-list Actions Palette stays open after the cursor
    /// leaves it before auto-collapsing. (A prior `palette_delay_ms` setting in
    /// milliseconds is intentionally not migrated — its meaning has changed.)
    #[serde(default = "default_palette_collapse")]
    palette_collapse_secs: u64,
    /// Group messages into conversation threads in the list.
    #[serde(default = "default_threading")]
    threading: bool,
    /// Whether conversation threads start expanded in the message list
    /// (collapsed to their newest message by default).
    #[serde(default)]
    threads_expanded: bool,
    /// How email content is themed (independent of the app UI theme).
    #[serde(default)]
    message_theme: MessageTheme,
    /// Whether to post desktop notifications (new mail, error alerts).
    #[serde(default = "default_notifications")]
    notifications: bool,
}

fn default_fetch_interval() -> u64 {
    300
}

fn default_push() -> bool {
    true
}

fn default_threading() -> bool {
    true
}

fn default_palette_collapse() -> u64 {
    5
}

fn default_notifications() -> bool {
    true
}

impl Default for PrivacyFile {
    fn default() -> Self {
        Self {
            allowed_senders: Vec::new(),
            gravatar: false,
            fetch_interval_secs: default_fetch_interval(),
            push: default_push(),
            blacklist: Vec::new(),
            palette_collapse_secs: default_palette_collapse(),
            threading: default_threading(),
            threads_expanded: false,
            message_theme: MessageTheme::default(),
            notifications: default_notifications(),
        }
    }
}

fn privacy_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("vireo").join("privacy.toml"))
}

fn load_privacy() -> PrivacyFile {
    let Some(path) = privacy_path() else {
        return PrivacyFile::default();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return PrivacyFile::default();
    };
    toml::from_str::<PrivacyFile>(&text).unwrap_or_default()
}

/// Senders whose messages may auto-load remote content. Stored lowercased.
pub fn load_allowed_senders() -> Vec<String> {
    load_privacy().allowed_senders
}

/// Whether Gravatar avatar loading is enabled.
pub fn load_gravatar() -> bool {
    load_privacy().gravatar
}

/// Seconds between automatic mail checks (0 = manual only).
pub fn load_fetch_interval() -> u64 {
    load_privacy().fetch_interval_secs
}

/// Whether IMAP IDLE push is enabled.
pub fn load_push() -> bool {
    load_privacy().push
}

/// Senders/domains whose incoming mail is auto-deleted. Stored lowercased.
pub fn load_blacklist() -> Vec<String> {
    load_privacy().blacklist
}

/// Seconds the message-list Actions Palette stays open after the cursor leaves it.
pub fn load_palette_collapse() -> u64 {
    load_privacy().palette_collapse_secs
}

/// Whether messages are grouped into conversation threads.
pub fn load_threading() -> bool {
    load_privacy().threading
}

/// Whether conversation threads start expanded (collapsed by default).
pub fn load_threads_expanded() -> bool {
    load_privacy().threads_expanded
}

/// How email message content is themed.
pub fn load_message_theme() -> MessageTheme {
    load_privacy().message_theme
}

/// Whether desktop notifications (new mail, error alerts) are enabled.
pub fn load_notifications() -> bool {
    load_privacy().notifications
}

/// Persist all app settings together (so no field is clobbered).
#[allow(clippy::too_many_arguments)]
pub fn save_privacy(
    senders: &[String],
    gravatar: bool,
    fetch_interval_secs: u64,
    push: bool,
    blacklist: &[String],
    palette_collapse_secs: u64,
    threading: bool,
    threads_expanded: bool,
    message_theme: MessageTheme,
    notifications: bool,
) {
    let Some(path) = privacy_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = PrivacyFile {
        allowed_senders: senders.to_vec(),
        gravatar,
        fetch_interval_secs,
        push,
        blacklist: blacklist.to_vec(),
        palette_collapse_secs,
        threading,
        threads_expanded,
        message_theme,
        notifications,
    };
    match toml::to_string_pretty(&file) {
        Ok(toml) => {
            if let Err(e) = std::fs::write(&path, toml) {
                tracing::warn!("could not save privacy settings: {e}");
            }
        }
        Err(e) => tracing::warn!("could not serialize privacy settings: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Sidebar state (account display order + collapsed accounts), keyed by email
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize, Serialize)]
struct SidebarFile {
    /// Account emails in the user's preferred display order.
    #[serde(default)]
    order: Vec<String>,
    /// Account emails whose folder list is collapsed.
    #[serde(default)]
    collapsed: Vec<String>,
    /// Account emails whose custom-folders section is expanded (default hidden).
    #[serde(default)]
    folders_expanded: Vec<String>,
    /// Whether the whole sidebar is in icon-only (collapsed) mode.
    #[serde(default)]
    icon_only: bool,
}

fn sidebar_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("vireo").join("sidebar.toml"))
}

/// Sidebar state persisted across restarts.
#[derive(Debug, Default)]
pub struct SidebarState {
    /// Account emails in display order.
    pub order: Vec<String>,
    /// Account emails whose folder list is collapsed.
    pub collapsed: Vec<String>,
    /// Account emails whose custom-folders section is expanded (default hidden).
    pub folders_expanded: Vec<String>,
    /// Whether the sidebar is in icon-only mode.
    pub icon_only: bool,
}

pub fn load_sidebar_state() -> SidebarState {
    let Some(path) = sidebar_path() else {
        return SidebarState::default();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return SidebarState::default();
    };
    toml::from_str::<SidebarFile>(&text)
        .map(|s| SidebarState {
            order: s.order,
            collapsed: s.collapsed,
            folders_expanded: s.folders_expanded,
            icon_only: s.icon_only,
        })
        .unwrap_or_default()
}

pub fn save_sidebar_state(state: &SidebarState) {
    let Some(path) = sidebar_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = SidebarFile {
        order: state.order.clone(),
        collapsed: state.collapsed.clone(),
        folders_expanded: state.folders_expanded.clone(),
        icon_only: state.icon_only,
    };
    match toml::to_string_pretty(&file) {
        Ok(toml) => {
            if let Err(e) = std::fs::write(&path, toml) {
                tracing::warn!("could not save sidebar state: {e}");
            }
        }
        Err(e) => tracing::warn!("could not serialize sidebar state: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Window state (size + maximized). Position/monitor can't be persisted on
// Wayland — the compositor owns window placement.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
struct WindowFile {
    width: i32,
    height: i32,
    #[serde(default)]
    maximized: bool,
}

impl Default for WindowFile {
    fn default() -> Self {
        Self { width: 1280, height: 840, maximized: false }
    }
}

fn window_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("vireo").join("window.toml"))
}

/// Returns the saved `(width, height, maximized)`, or sensible defaults.
pub fn load_window_state() -> (i32, i32, bool) {
    let file = window_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| toml::from_str::<WindowFile>(&t).ok())
        .unwrap_or_default();
    // Guard against absurd/zero sizes from a bad file.
    let width = if file.width >= 360 { file.width } else { 1280 };
    let height = if file.height >= 300 { file.height } else { 840 };
    (width, height, file.maximized)
}

pub fn save_window_state(width: i32, height: i32, maximized: bool) {
    let Some(path) = window_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = WindowFile { width, height, maximized };
    if let Ok(toml) = toml::to_string_pretty(&file) {
        let _ = std::fs::write(&path, toml);
    }
}

// ---------------------------------------------------------------------------
// Keyring health check + one-time setup-help flag
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize, Serialize)]
struct StateFile {
    /// Set once the user dismisses the Linux Mint keyring setup tip.
    #[serde(default)]
    mint_keyring_help_dismissed: bool,
    /// In-message attachment drawer: collapsed (showing only its header). This is
    /// the only drawer setting we remember — height and thumbnail size always
    /// start at their defaults.
    #[serde(default)]
    drawer_collapsed: bool,
}

fn state_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("vireo").join("state.toml"))
}

fn load_state() -> StateFile {
    state_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| toml::from_str::<StateFile>(&t).ok())
        .unwrap_or_default()
}

fn save_state(state: &StateFile) {
    let Some(path) = state_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(toml) = toml::to_string_pretty(state) {
        let _ = std::fs::write(&path, toml);
    }
}

/// Whether the one-time Mint keyring setup tip has already been dismissed.
pub fn mint_keyring_help_dismissed() -> bool {
    load_state().mint_keyring_help_dismissed
}

/// Persist that the user dismissed the Mint keyring setup tip ("Don't show again").
pub fn dismiss_mint_keyring_help() {
    let mut state = load_state();
    state.mint_keyring_help_dismissed = true;
    save_state(&state);
}

/// Persisted state of the in-message attachment drawer.
#[derive(Debug, Clone, Copy)]
pub struct DrawerState {
    /// Expanded content height in px.
    pub height: i32,
    /// Whether the drawer is collapsed to just its header.
    pub collapsed: bool,
    /// Thumbnail edge in px.
    pub thumb: i32,
}

impl Default for DrawerState {
    fn default() -> Self {
        Self { height: 160, collapsed: false, thumb: 56 }
    }
}

/// Load the attachment drawer's remembered state. Only the collapsed flag is
/// persisted; height and thumbnail size always start at their defaults.
pub fn load_drawer_state() -> DrawerState {
    DrawerState { collapsed: load_state().drawer_collapsed, ..DrawerState::default() }
}

/// Persist whether the attachment drawer is collapsed.
pub fn save_drawer_collapsed(collapsed: bool) {
    let mut s = load_state();
    s.drawer_collapsed = collapsed;
    save_state(&s);
}


#[cfg(test)]
mod tests {
    use super::{ConfigFile, PrivacyFile};

    #[test]
    fn old_account_config_gets_secure_transport_defaults() {
        let cfg: ConfigFile = toml::from_str(
            r#"[[accounts]]
name = "Example"
email = "user@example.com"
imap_host = "imap.example.com"
username = "user@example.com"
"#,
        )
        .unwrap();
        let account = &cfg.accounts[0];
        assert!(!account.imap_uses_starttls());
        assert!(account.smtp_auth);
    }

    #[test]
    fn old_port_143_config_is_upgraded_to_starttls() {
        let cfg: ConfigFile = toml::from_str(
            r#"[[accounts]]
name = "Example"
email = "user@example.com"
imap_host = "imap.example.com"
imap_port = 143
username = "user@example.com"
"#,
        )
        .unwrap();
        assert!(cfg.accounts[0].imap_uses_starttls());
    }

    #[test]
    fn notifications_default_on_when_absent() {
        // An older privacy.toml with no `notifications` key opts in by default.
        let p: PrivacyFile = toml::from_str("").unwrap();
        assert!(p.notifications);
    }

    #[test]
    fn notifications_can_be_disabled() {
        let p: PrivacyFile = toml::from_str("notifications = false").unwrap();
        assert!(!p.notifications);
    }
}
