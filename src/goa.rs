//! GNOME Online Accounts (GOA) integration.
//!
//! Reads mail-capable accounts configured in GNOME Settings → Online Accounts via
//! the `org.gnome.OnlineAccounts` D-Bus service (session bus), so the user can
//! enable them in Vireo without re-entering server settings. Password-based
//! providers (generic IMAP/SMTP) have their password retrieved from GOA and stored
//! in Vireo's keyring on import; OAuth2 providers (Gmail, Microsoft) authenticate
//! with a GOA-issued access token (XOAUTH2) fetched fresh at connect time.

use std::collections::{HashMap, HashSet};

use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use crate::config::{AccountConfig, Protocol};

const GOA_DEST: &str = "org.gnome.OnlineAccounts";
const GOA_PATH: &str = "/org/gnome/OnlineAccounts";
const IFACE_ACCOUNT: &str = "org.gnome.OnlineAccounts.Account";
const IFACE_MAIL: &str = "org.gnome.OnlineAccounts.Mail";
const IFACE_PASSWORD: &str = "org.gnome.OnlineAccounts.PasswordBased";
const IFACE_OAUTH2: &str = "org.gnome.OnlineAccounts.OAuth2Based";

/// A mail account discovered in GNOME Online Accounts.
#[derive(Debug, Clone)]
pub struct GoaMailAccount {
    /// GOA account id (e.g. "account_1699…").
    pub id: String,
    pub email: String,
    pub name: String,
    pub provider: String,
    pub imap_host: String,
    pub imap_port: u16,
    /// GOA's `ImapUseTls`: connect in plaintext, then require STARTTLS.
    pub imap_starttls: bool,
    pub imap_implicit_tls: bool,
    pub imap_user: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_implicit_tls: bool,
    pub smtp_user: String,
    pub smtp_auth: bool,
    /// Whether GOA can hand us a password (generic IMAP).
    pub password_based: bool,
    /// Whether the provider uses OAuth2 — imported accounts authenticate with a
    /// GOA-issued token (XOAUTH2) fetched fresh at connect time.
    pub oauth2: bool,
}

impl GoaMailAccount {
    /// Turn a discovered GOA account into a Vireo [`AccountConfig`]. Pass the
    /// password for password-based providers, or `oauth = true` for OAuth ones
    /// (the token is fetched from GOA at connect time).
    pub fn to_config(
        &self,
        password: String,
        smtp_password: String,
        oauth: bool,
    ) -> AccountConfig {
        // GOA stores IMAP and SMTP passwords under distinct credential ids even
        // when their values/usernames happen to match.
        let smtp_separate = self.smtp_auth && !oauth;
        AccountConfig {
            name: if self.name.trim().is_empty() {
                self.email.clone()
            } else {
                self.name.clone()
            },
            email: self.email.clone(),
            protocol: Protocol::Imap,
            imap_host: self.imap_host.clone(),
            imap_port: self.imap_port,
            imap_starttls: self.imap_starttls,
            imap_implicit_tls: self.imap_implicit_tls,
            smtp_host: self.smtp_host.clone(),
            smtp_port: self.smtp_port,
            smtp_implicit_tls: self.smtp_implicit_tls,
            smtp_auth: self.smtp_auth,
            username: if self.imap_user.is_empty() {
                self.email.clone()
            } else {
                self.imap_user.clone()
            },
            password,
            smtp_separate,
            smtp_username: if self.smtp_user.is_empty() {
                if self.imap_user.is_empty() {
                    self.email.clone()
                } else {
                    self.imap_user.clone()
                }
            } else {
                self.smtp_user.clone()
            },
            smtp_password,
            color: None,
            emoji: None,
            signature: None,
            signature_html: false,
            label: None,
            enabled: true,
            goa_id: Some(self.id.clone()),
            goa_mail_disabled: false,
            goa_enabled_before_mail_disabled: false,
            oauth,
            oauth_settings: None,
            oauth_refresh: String::new(),
        }
    }
}

fn get_str(map: &HashMap<String, OwnedValue>, key: &str) -> String {
    map.get(key)
        .and_then(|v| <&str>::try_from(v).ok())
        .unwrap_or_default()
        .to_string()
}

fn get_bool(map: &HashMap<String, OwnedValue>, key: &str) -> bool {
    map.get(key).and_then(|v| bool::try_from(v).ok()).unwrap_or(false)
}

/// GOA permits a port in the host property (`mail.example.com:1143`), and Geary
/// honours it. Split that form while also accepting bracketed IPv6 addresses.
fn host_and_port(value: String, default_port: u16) -> (String, u16) {
    if let Some(rest) = value.strip_prefix('[') {
        if let Some((host, suffix)) = rest.split_once(']') {
            let port = suffix
                .strip_prefix(':')
                .and_then(|p| p.parse().ok())
                .unwrap_or(default_port);
            return (host.to_string(), port);
        }
    }
    if value.matches(':').count() == 1 {
        if let Some((host, port)) = value.rsplit_once(':') {
            return (host.to_string(), port.parse().unwrap_or(default_port));
        }
    }
    (value, default_port)
}

type ManagedObjects =
    HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

#[derive(Debug, Clone)]
pub struct GoaSnapshot {
    pub accounts: Vec<GoaMailAccount>,
    /// Every GOA account object, including ones with Mail disabled.
    pub account_ids: HashSet<String>,
    /// Accounts whose Mail service is explicitly disabled in GNOME Settings.
    pub disabled_mail_ids: HashSet<String>,
}

/// Fetch all GOA objects once and derive both the usable mail accounts and their
/// ids. `None` means GOA is unavailable, never "there are no accounts".
pub fn snapshot() -> Option<GoaSnapshot> {
    match try_snapshot() {
        Ok(snapshot) => Some(snapshot),
        Err(e) => {
            tracing::debug!("GOA discovery skipped: {e}");
            None
        }
    }
}

/// List mail-capable GNOME Online Accounts. Returns an empty list if GOA isn't
/// running or has no mail accounts — never errors into the UI.
pub fn list_mail_accounts() -> Vec<GoaMailAccount> {
    snapshot().map(|snapshot| snapshot.accounts).unwrap_or_default()
}

fn try_snapshot() -> Result<GoaSnapshot, String> {
    let conn = zbus::blocking::Connection::session().map_err(|e| e.to_string())?;
    let reply = conn
        .call_method(
            Some(GOA_DEST),
            GOA_PATH,
            Some("org.freedesktop.DBus.ObjectManager"),
            "GetManagedObjects",
            &(),
        )
        .map_err(|e| e.to_string())?;
    let (objects,): (ManagedObjects,) = reply.body().deserialize().map_err(|e| e.to_string())?;

    let mut out = Vec::new();
    let mut account_ids = HashSet::new();
    let mut disabled_mail_ids = HashSet::new();
    for ifaces in objects.values() {
        let Some(account) = ifaces.get(IFACE_ACCOUNT) else {
            continue;
        };
        let id = get_str(account, "Id");
        if id.is_empty() {
            continue;
        }
        account_ids.insert(id.clone());
        let Some(mail) = ifaces.get(IFACE_MAIL) else {
            continue;
        };
        // Keep the id above, but skip importing/syncing when Mail is disabled.
        if get_bool(account, "MailDisabled") {
            disabled_mail_ids.insert(id);
            continue;
        }

        let imap_ssl = get_bool(mail, "ImapUseSsl");
        let imap_tls = get_bool(mail, "ImapUseTls");
        let smtp_ssl = get_bool(mail, "SmtpUseSsl");
        let smtp_tls = get_bool(mail, "SmtpUseTls");
        let smtp_auth = get_bool(mail, "SmtpUseAuth");
        let email = {
            let e = get_str(mail, "EmailAddress");
            if e.is_empty() {
                get_str(account, "PresentationIdentity")
            } else {
                e
            }
        };
        if email.is_empty() {
            continue;
        }
        let imap_user = get_str(mail, "ImapUserName");
        let smtp_user = get_str(mail, "SmtpUserName");
        let imap_host_raw = get_str(mail, "ImapHost");
        let smtp_host_raw = get_str(mail, "SmtpHost");
        let has_auth = ifaces.contains_key(IFACE_PASSWORD) || ifaces.contains_key(IFACE_OAUTH2);
        // Vireo deliberately has no plaintext-mail mode. Ignore incomplete or
        // insecure GOA mail services rather than risk sending credentials without
        // transport encryption.
        if !get_bool(mail, "ImapSupported")
            || !get_bool(mail, "SmtpSupported")
            || imap_host_raw.is_empty()
            || smtp_host_raw.is_empty()
            || (!imap_ssl && !imap_tls)
            || (!smtp_ssl && !smtp_tls)
            || !has_auth
        {
            continue;
        }

        let (imap_host, imap_port) =
            host_and_port(imap_host_raw, if imap_ssl { 993 } else { 143 });
        let (smtp_host, smtp_port) = host_and_port(
            smtp_host_raw,
            if smtp_ssl { 465 } else { 587 },
        );

        out.push(GoaMailAccount {
            id,
            email,
            name: {
                let n = get_str(mail, "Name");
                if n.is_empty() {
                    get_str(account, "PresentationIdentity")
                } else {
                    n
                }
            },
            provider: get_str(account, "ProviderName"),
            imap_host,
            imap_port,
            imap_starttls: !imap_ssl && imap_tls,
            imap_implicit_tls: imap_ssl,
            imap_user: imap_user.clone(),
            smtp_host,
            smtp_port,
            smtp_implicit_tls: smtp_ssl,
            smtp_user: smtp_user.clone(),
            smtp_auth,
            password_based: ifaces.contains_key(IFACE_PASSWORD),
            oauth2: ifaces.contains_key(IFACE_OAUTH2),
        });
    }
    Ok(GoaSnapshot {
        accounts: out,
        account_ids,
        disabled_mail_ids,
    })
}

/// Watch GNOME Online Accounts for additions, removals and property changes so
/// newly enabled mail accounts and server-setting edits appear without restart.
pub fn watch_changes<F: Fn() + Send + 'static>(on_change: F) {
    // ObjectManager and PropertiesChanged commonly arrive in bursts for one
    // Settings edit. Coalesce them before doing a single background snapshot.
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let _ = std::thread::Builder::new()
        .name("goa-change-dispatch".into())
        .spawn(move || {
            while rx.recv().is_ok() {
                while rx.recv_timeout(std::time::Duration::from_millis(300)).is_ok() {}
                on_change();
            }
        });

    for added in [true, false] {
        let changed = tx.clone();
        let name = if added { "goa-added-watch" } else { "goa-removed-watch" };
        let _ = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let callback = || {
                    let _ = changed.send(());
                };
                if let Err(e) = watch_object_manager(&callback, added) {
                    tracing::debug!("GOA watch stopped: {e}");
                }
            });
    }
    let changed = tx;
    let _ = std::thread::Builder::new()
        .name("goa-properties-watch".into())
        .spawn(move || {
            let callback = || {
                let _ = changed.send(());
            };
            if let Err(e) = watch_properties(&callback) {
                tracing::debug!("GOA properties watch stopped: {e}");
            }
        });
}

fn watch_object_manager<F: Fn()>(
    on_change: &F,
    added: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let conn = zbus::blocking::Connection::session()?;
    let om = zbus::blocking::fdo::ObjectManagerProxy::builder(&conn)
        .destination(GOA_DEST)?
        .path(GOA_PATH)?
        .build()?;
    if added {
        let mut signals = om.receive_interfaces_added()?;
        for _ in signals.by_ref() {
            on_change();
        }
    } else {
        let mut signals = om.receive_interfaces_removed()?;
        for _ in signals.by_ref() {
            on_change();
        }
    }
    Ok(())
}

fn watch_properties<F: Fn()>(on_change: &F) -> Result<(), Box<dyn std::error::Error>> {
    let conn = zbus::blocking::Connection::session()?;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(GOA_DEST)?
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?
        .path_namespace("/org/gnome/OnlineAccounts/Accounts")?
        .build();
    let signals = zbus::blocking::MessageIterator::for_match_rule(rule, &conn, Some(32))?;
    for signal in signals {
        signal?;
        on_change();
    }
    Ok(())
}

/// Fetch a fresh OAuth2 access token for a GOA account (by id). GOA refreshes the
/// token as needed, so this always returns a currently-valid token. Blocking.
pub fn oauth_token(goa_id: &str) -> Option<String> {
    let conn = zbus::blocking::Connection::session().ok()?;
    let path = account_path(goa_id);
    // Follow GOA's documented flow (and Geary's implementation): ensure/renew
    // credentials before asking for the current token.
    let _ = ensure_credentials_with(&conn, &path);
    let reply = conn
        .call_method(
            Some(GOA_DEST),
            path.as_str(),
            Some(IFACE_OAUTH2),
            "GetAccessToken",
            &(),
        )
        .ok()?;
    // GetAccessToken() -> (access_token: s, expires_in: i)
    let (token, _expires): (String, i32) = reply.body().deserialize().ok()?;
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

fn account_path(goa_id: &str) -> String {
    format!("/org/gnome/OnlineAccounts/Accounts/{goa_id}")
}

/// Ask GOA to validate/renew credentials. A second attempt mirrors Geary's
/// retry after an authorization failure. Password lookup is still attempted if
/// this fails: GOA may have a usable cached secret while the provider is offline.
fn ensure_credentials_with(conn: &zbus::blocking::Connection, path: &str) -> bool {
    let call = || {
        conn.call_method(
            Some(GOA_DEST),
            path,
            Some(IFACE_ACCOUNT),
            "EnsureCredentials",
            &(),
        )
    };
    call().or_else(|_| call()).is_ok()
}

fn fetch_password_with(
    conn: &zbus::blocking::Connection,
    path: &str,
    credential_id: &str,
) -> Option<String> {
    let reply = conn
        .call_method(
            Some(GOA_DEST),
            path,
            Some(IFACE_PASSWORD),
            "GetPassword",
            &(credential_id,),
        )
        .ok()?;
    let (password,): (String,) = reply.body().deserialize().ok()?;
    (!password.is_empty()).then_some(password)
}

/// Fetch both password-based mail credentials directly from GOA. They stay in
/// worker memory and are not copied into Vireo's own keyring.
pub fn mail_passwords(goa_id: &str) -> (Option<String>, Option<String>) {
    let conn = match zbus::blocking::Connection::session() {
        Ok(conn) => conn,
        Err(_) => return (None, None),
    };
    let path = account_path(goa_id);
    let _ = ensure_credentials_with(&conn, &path);
    (
        fetch_password_with(&conn, &path, "imap-password"),
        fetch_password_with(&conn, &path, "smtp-password"),
    )
}

#[cfg(test)]
mod tests {
    use super::{host_and_port, GoaMailAccount};

    fn mail_account() -> GoaMailAccount {
        GoaMailAccount {
            id: "account_1".into(),
            email: "user@example.com".into(),
            name: "Example User".into(),
            provider: "IMAP and SMTP".into(),
            imap_host: "imap.example.com".into(),
            imap_port: 143,
            imap_starttls: true,
            imap_implicit_tls: false,
            imap_user: "incoming-user".into(),
            smtp_host: "smtp.example.com".into(),
            smtp_port: 587,
            smtp_implicit_tls: false,
            smtp_user: "outgoing-user".into(),
            smtp_auth: true,
            password_based: true,
            oauth2: false,
        }
    }

    #[test]
    fn goa_host_may_include_a_custom_port() {
        assert_eq!(
            host_and_port("mail.example.com:1143".into(), 143),
            ("mail.example.com".into(), 1143)
        );
        assert_eq!(
            host_and_port("[2001:db8::1]:1993".into(), 993),
            ("2001:db8::1".into(), 1993)
        );
        assert_eq!(
            host_and_port("mail.example.com".into(), 993),
            ("mail.example.com".into(), 993)
        );
    }

    #[test]
    fn password_account_keeps_goa_transport_and_separate_credentials() {
        let config = mail_account().to_config("imap-secret".into(), "smtp-secret".into(), false);
        assert!(config.imap_starttls);
        assert!(config.smtp_auth);
        assert!(config.smtp_separate);
        assert_eq!(config.username, "incoming-user");
        assert_eq!(config.smtp_username, "outgoing-user");
        assert_eq!(config.password, "imap-secret");
        assert_eq!(config.smtp_password, "smtp-secret");
    }

    #[test]
    fn oauth_account_does_not_enable_password_credentials() {
        let config = mail_account().to_config(String::new(), String::new(), true);
        assert!(config.oauth);
        assert!(!config.smtp_separate);
        assert_eq!(config.goa_id.as_deref(), Some("account_1"));
    }

    #[test]
    fn custom_smtp_port_preserves_implicit_tls() {
        let mut account = mail_account();
        account.smtp_port = 8465;
        account.smtp_implicit_tls = true;
        let config = account.to_config(String::new(), String::new(), true);
        assert!(config.smtp_uses_implicit_tls());
    }
}


