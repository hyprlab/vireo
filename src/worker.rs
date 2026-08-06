//! Background mail worker.
//!
//! IMAP is async and its sessions are stateful, while the relm4 UI is driven
//! synchronously on the GTK main thread. To bridge the two, [`spawn`] starts a
//! dedicated OS thread running a tokio runtime that owns the IMAP session. The
//! UI sends [`MailRequest`]s over an unbounded channel; the worker performs the
//! network I/O and pushes [`WorkerEvent`]s back via a caller-supplied callback
//! (in practice, the component's input sender). The mock path implements the
//! exact same protocol so the app behaves identically offline.

use std::time::Duration;

use async_imap::types::{Fetch, Flag, NameAttribute};
use async_imap::Session;
use async_native_tls::TlsStream;
use chrono::Datelike;
use futures::TryStreamExt;
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message as LettreMessage, Tokio1Executor};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::backend::{MailBackend, MockBackend};
use crate::cache::Cache;
use crate::config::AccountConfig;
use crate::models::{Account, Folder, FolderKind, Message};

/// Number of most-recent messages to fetch attachment info (BODYSTRUCTURE) for;
/// older messages get an envelope-only index row and resolve attachments on open.
const PAGE_SIZE: u32 = 50;

/// Number of most-recent messages shown instantly when a folder is first opened
/// (never synced before). The rest of the folder is indexed in the background so
/// browsing is immediate and search fills in shortly after — like Apple Mail.
const FIRST_PAGE: u32 = 200;

/// Background index backfill: how many messages to fetch per idle drain step.
/// Bigger = fewer round-trips; smaller = more responsive to interleaved requests.
const BACKFILL_CHUNK: usize = 1_000;

/// Pre-download attachments only for this many of the most recent messages;
/// older attachment messages download on demand (with a spinner) when opened.
const PREFETCH_LIMIT: usize = 25;

/// Attachments gallery: cap on how many items to load per inbox, and the largest
/// file whose bytes are loaded eagerly (for instant thumbnails/preview). Bigger
/// files carry no bytes in the gallery and are fetched on demand when opened.
const GALLERY_LIMIT: u32 = 300;
const GALLERY_DATA_CAP: u64 = 6 * 1024 * 1024;

/// Pre-download message *bodies* for this many of the most recent messages in a
/// synced folder, so new mail opens instantly with no network wait. Bodies are
/// small, so this stays cheap; older messages load on demand.
const PREFETCH_BODY_LIMIT: usize = 50;

/// A request from the UI to the worker.
#[derive(Debug)]
pub enum MailRequest {
    /// Load the message summaries for a folder.
    LoadMessages { folder_id: u32, path: String },
    /// Load cached attachments across the account's folders, for the gallery.
    LoadGallery,
    /// Load the full body of a single message.
    LoadBody {
        message_id: u32,
        path: String,
        uid: u32,
    },
    /// Load the raw RFC 822 source of a single message.
    LoadSource {
        message_id: u32,
        path: String,
        uid: u32,
    },
    /// Load the attachments of a single message. When `download` is false, only
    /// serve from cache (otherwise reply `AttachmentsPending`) — never hits the
    /// network. The user explicitly opts in to downloading older attachments.
    LoadAttachments {
        message_id: u32,
        path: String,
        uid: u32,
        download: bool,
    },
    /// Mark a message as spam: tag `$Junk` (so the server filter can learn) and
    /// move it to the Junk folder.
    MarkSpam { path: String, uid: u32, dest: String },
    /// Add or remove the `\Seen` flag.
    SetSeen { path: String, uid: u32, seen: bool },
    /// Mark every message in a folder as read (`\Seen`).
    MarkAllRead { folder_id: u32, path: String },
    /// Add or remove the `\Flagged` flag.
    SetFlagged {
        path: String,
        uid: u32,
        flagged: bool,
    },
    /// Move a message to another mailbox (archive / trash).
    MoveMessage {
        path: String,
        uid: u32,
        dest: String,
    },
    /// Move many messages from one mailbox to another in a single UID MOVE (bulk
    /// archive / delete / spam). Far faster and more reliable than one request per
    /// message on large mailboxes.
    MoveMessages {
        path: String,
        uids: Vec<u32>,
        dest: String,
    },
    /// Create a new mailbox (folder) at `path`.
    CreateFolder { path: String },
    /// Delete a mailbox, first moving its contents to `trash` (if set).
    DeleteFolder { path: String, trash: Option<String> },
    /// Send a new message over SMTP, optionally APPENDing a copy to `sent_path`.
    Send {
        message: Box<OutgoingMessage>,
        sent_path: Option<String>,
    },
    /// Save a message to the Drafts folder (`folder_id`/`path`) without sending.
    SaveDraft {
        message: Box<OutgoingMessage>,
        folder_id: u32,
        path: String,
    },
    /// Force a fresh connection and re-list folders (e.g. after a failure).
    Reconnect,
}

/// A message composed by the user, ready to send.
#[derive(Debug, Clone)]
pub struct OutgoingMessage {
    /// The account to send from.
    pub from_account_id: u32,
    /// Comma-separated recipient addresses.
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    /// Plain-text body (always present; the `text/plain` alternative).
    pub body: String,
    /// HTML body. When non-empty the mail is sent multipart/alternative with this
    /// as the `text/html` part and `body` as the plain fallback.
    pub html: String,
    /// File paths to attach.
    pub attachments: Vec<String>,
    /// When editing an existing draft, the draft being replaced (removed from the
    /// Drafts folder after this message is saved or sent).
    pub draft_origin: Option<crate::models::DraftOrigin>,
}

/// An event pushed from the worker back to the UI.
#[derive(Debug)]
pub enum WorkerEvent {
    Account(Account),
    Folders(Vec<Folder>),
    Messages { folder_id: u32, messages: Vec<Message> },
    /// Additional indexed message summaries for a folder, produced by the
    /// background backfill. Merged into the existing index without replacing it.
    MessagesAppend { folder_id: u32, messages: Vec<Message> },
    /// Cached attachments for an inbox, for the attachments gallery.
    Gallery { items: Vec<crate::models::GalleryItem> },
    /// The background backfill for a folder finished — its whole index is now
    /// present, so the UI can stop expecting more rows to stream in.
    BackfillDone { folder_id: u32 },
    /// Server-side unread count for a folder (from STATUS/SEARCH, independent of
    /// the loaded window — accurate even for multi-thousand mailboxes).
    FolderUnread { folder_id: u32, unread: u32 },
    Body { message_id: u32, body: String },
    Source { text: String },
    Attachments { message_id: u32, items: Vec<crate::models::Attachment> },
    /// The message has attachments that aren't cached; the UI should offer to
    /// download them rather than fetching automatically.
    AttachmentsPending { message_id: u32 },
    /// A message flagged as having an attachment turned out to have none once its
    /// body was fetched (e.g. iCloud marketing mail whose only extra parts are
    /// inline `cid:` images). The UI should drop its paperclip.
    NoAttachments { message_id: u32 },
    Sent,
    /// A draft was saved to the Drafts folder.
    DraftSaved,
    /// A bulk MoveMessages request finished (success or failure) — drives the
    /// bulk-action spinner in the UI.
    BulkComplete,
    Status(String),
    /// `connectivity` marks connection/sync errors that should auto-clear once
    /// a later connect or sync succeeds.
    Error { text: String, connectivity: bool },
}

type ImapSession = Session<TlsStream<TcpStream>>;

/// A distinct accent colour per account (cycles through a small palette).
fn accent_for(account_id: u32) -> &'static str {
    const PALETTE: [&str; 6] = [
        "#3584e4", "#2ec27e", "#e5a50a", "#e66100", "#9141ac", "#c01c28",
    ];
    PALETTE[(account_id.saturating_sub(1) as usize) % PALETTE.len()]
}

/// Start a worker thread for one account (`Some`) or the offline sample data
/// (`None`). Returns the sender used to issue requests. `account_id` stamps all
/// emitted folders/messages and keys the cache.
pub fn spawn(
    account_id: u32,
    account: Option<AccountConfig>,
    emit: impl Fn(WorkerEvent) + Send + 'static,
) -> mpsc::UnboundedSender<MailRequest> {
    let (tx, rx) = mpsc::unbounded_channel();

    std::thread::Builder::new()
        .name(format!("vireo-mail-{account_id}"))
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    emit(WorkerEvent::Error {
                        text: format!("runtime error: {e}"),
                        connectivity: false,
                    });
                    return;
                }
            };
            rt.block_on(run(account_id, account, rx, emit));
        })
        .expect("failed to spawn mail worker thread");

    tx
}

async fn run(
    account_id: u32,
    account: Option<AccountConfig>,
    rx: mpsc::UnboundedReceiver<MailRequest>,
    emit: impl Fn(WorkerEvent),
) {
    match account {
        Some(account) if account.protocol == crate::config::Protocol::Pop3 => {
            run_pop3(account_id, account, rx, emit).await
        }
        Some(account) => run_imap(account_id, account, rx, emit).await,
        None => run_mock(account_id, rx, emit).await,
    }
}

// ---------------------------------------------------------------------------
// IMAP path
// ---------------------------------------------------------------------------

/// Resolve account credentials without copying GOA-owned secrets into Vireo's
/// keyring. GOA remains the source of truth; passwords live only in worker memory.
async fn resolve_credentials(account: &mut AccountConfig, persist_supplied: bool) {
    let from_goa = account.goa_id.is_some() && !account.oauth;
    if let Some(goa_id) = account.goa_id.clone().filter(|_| !account.oauth) {
        if let Ok((incoming, smtp)) =
            tokio::task::spawn_blocking(move || crate::goa::mail_passwords(&goa_id)).await
        {
            if let Some(password) = incoming.as_ref() {
                account.password = password.clone();
            }
            if let Some(password) = smtp.or(incoming) {
                // Some GOA backends expose one shared mail password even though
                // the standard credential ids are separate.
                account.smtp_password = password;
            }
        }
    }

    if account.password.is_empty() {
        if let Some(pw) = crate::config::load_password(&account.email) {
            account.password = pw;
        }
    } else if !from_goa && persist_supplied {
        let _ = crate::config::store_password(&account.email, &account.password);
        crate::config::strip_passwords_on_disk();
    }
    if account.smtp_separate && account.smtp_password.is_empty() {
        if let Some(pw) = crate::config::load_smtp_password(&account.email) {
            account.smtp_password = pw;
        }
    } else if account.smtp_separate
        && !account.smtp_password.is_empty()
        && !from_goa
        && persist_supplied
    {
        let _ = crate::config::store_smtp_password(&account.email, &account.smtp_password);
    }
}

async fn run_imap(
    account_id: u32,
    mut account: AccountConfig,
    mut rx: mpsc::UnboundedReceiver<MailRequest>,
    emit: impl Fn(WorkerEvent),
) {
    resolve_credentials(&mut account, true).await;

    let cache = Cache::open()
        .map_err(|e| tracing::warn!("cache unavailable: {e}"))
        .ok();

    // Show the account + any cached folders immediately, before any network.
    emit(WorkerEvent::Account(Account {
        id: account_id,
        name: account.name.clone(),
        email: account.email.clone(),
        label: account.display_label(),
        accent: accent_for(account_id).into(),
    }));

    let cached_folders = cache
        .as_ref()
        .map(|c| c.load_folders(account_id))
        .unwrap_or_default();
    let have_cached_folders = !cached_folders.is_empty();
    if have_cached_folders {
        emit(WorkerEvent::Folders(cached_folders));
    }

    // The worker stays alive even if connecting fails, so the UI can retry. With
    // cached folders we connect lazily (on the first request) so cached mail can
    // render without waiting on the network; with an empty cache we connect now
    // to bootstrap the folder list.
    let mut session = if have_cached_folders {
        None
    } else {
        connect_and_list(account_id, &account, cache.as_ref(), &emit).await
    };

    // Attachments queued for background pre-download (folder_path, uid).
    let mut prefetch: std::collections::VecDeque<(String, u32)> = std::collections::VecDeque::new();
    // Message bodies queued for background pre-download, so new mail opens with no
    // network wait (folder_path, uid).
    let mut body_prefetch: std::collections::VecDeque<(String, u32)> =
        std::collections::VecDeque::new();
    // Bodies already pushed to the UI's in-memory cache this session, so they're
    // not re-sent on every folder re-sync.
    let mut body_emitted: std::collections::HashSet<(String, u32)> =
        std::collections::HashSet::new();
    // Folders queued for background index backfill (the rest of the mailbox past
    // the fast first page), and the set already enqueued this session.
    let mut backfill: std::collections::VecDeque<Backfill> = std::collections::VecDeque::new();
    let mut backfill_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    // IMAP IDLE push: watch the most recently loaded folder for new mail.
    let push_enabled = crate::config::load_push();
    let mut idle_folder: Option<(u32, String)> = None;
    // Set after prefetching; triggers one re-sync (to catch mail that arrived
    // while the connection was busy) before settling into the long IDLE.
    let mut pending_resync = false;
    // Whether to use IMAP's structured ENVELOPE/BODYSTRUCTURE. Disabled for the
    // session (falling back to raw-header parsing) if the server sends responses
    // our IMAP parser can't handle (e.g. iCloud).
    let mut use_envelope = true;

    // Queue every known folder for background indexing so search covers the whole
    // mailbox shortly after the first sync (like Apple Mail). The backfill skips
    // UIDs already cached, so this is cheap on subsequent runs.
    for f in cache
        .as_ref()
        .map(|c| c.load_folders(account_id))
        .unwrap_or_default()
    {
        if backfill_seen.insert(f.path.clone()) {
            backfill.push_back(Backfill {
                folder_id: f.id,
                gallery: gallery_folder(f.kind),
                path: f.path,
                remaining: None,
            });
        }
    }

    loop {
        // Always prefer incoming requests. When idle: drain the attachment prefetch
        // queue (fast), then index a chunk of the background backfill, then — if
        // push is on — re-sync once to catch any mail that arrived while busy, then
        // sit in a long IMAP IDLE for instant delivery. Without push, block for the
        // next request.
        let req = match rx.try_recv() {
            Ok(req) => req,
            Err(mpsc::error::TryRecvError::Disconnected) => break,
            Err(mpsc::error::TryRecvError::Empty) => {
                if !body_prefetch.is_empty() {
                    // Highest priority: get new mail's body cached so opening it is
                    // instant (no network wait).
                    run_one_body_prefetch(
                        &mut body_prefetch,
                        &mut session,
                        &account,
                        account_id,
                        cache.as_ref(),
                        &mut body_emitted,
                        &emit,
                    )
                    .await;
                    continue;
                } else if !prefetch.is_empty() {
                    run_one_prefetch(
                        &mut prefetch,
                        &mut session,
                        &account,
                        account_id,
                        cache.as_ref(),
                        &emit,
                    )
                    .await;
                    pending_resync = true;
                    continue;
                } else if !backfill.is_empty() {
                    // Index the rest of the mailbox in the background. Connect first
                    // for cached-folder accounts (which connect lazily); if still
                    // offline, wait for a request instead of spinning.
                    if session.is_none() {
                        session =
                            connect_and_list(account_id, &account, cache.as_ref(), &emit).await;
                    }
                    if session.is_some() {
                        run_one_backfill(
                            &mut backfill,
                            &mut session,
                            &account,
                            account_id,
                            cache.as_ref(),
                            &mut prefetch,
                            &mut use_envelope,
                            &emit,
                        )
                        .await;
                        continue;
                    } else {
                        match rx.recv().await {
                            Some(req) => req,
                            None => break,
                        }
                    }
                } else if push_enabled && session.is_some() && idle_folder.is_some() {
                    let (fid, fpath) = idle_folder.clone().unwrap();
                    // Catch mail delivered while the connection was busy prefetching.
                    if pending_resync {
                        pending_resync = false;
                        if let Ok(messages) = load_messages_retry(
                            account_id,
                            &mut session,
                            &account,
                            fid,
                            &fpath,
                            &mut use_envelope,
                            cache.as_ref(),
                        )
                        .await
                        {
                            if let Some(c) = cache.as_ref() {
                                c.upsert_messages(account_id, &fpath, &messages);
                            }
                            queue_body_prefetch(
                                &mut body_prefetch,
                                &fpath,
                                &messages,
                                &body_emitted,
                            );
                            queue_attachment_prefetch(
                                &mut prefetch,
                                &fpath,
                                &messages,
                                cache.as_ref(),
                                account_id,
                            );
                            emit(WorkerEvent::Messages { folder_id: fid, messages });
                        }
                        continue;
                    }
                    match idle_wait(
                        &mut session,
                        &account,
                        account_id,
                        fid,
                        &fpath,
                        &mut rx,
                        cache.as_ref(),
                        &mut use_envelope,
                        &mut body_prefetch,
                        &mut prefetch,
                        &body_emitted,
                        &emit,
                        1740,
                    )
                    .await
                    {
                        IdleOutcome::Request(req) => req,
                        IdleOutcome::Refreshed | IdleOutcome::Quiet => continue,
                        IdleOutcome::Closed => break,
                    }
                } else {
                    match rx.recv().await {
                        Some(req) => req,
                        None => break,
                    }
                }
            }
        };

        if matches!(req, MailRequest::Reconnect) {
            session = connect_and_list(account_id, &account, cache.as_ref(), &emit).await;
            continue;
        }

        // Serve from cache first so mail appears instantly (and offline).
        match &req {
            MailRequest::LoadMessages { folder_id, path } => {
                if let Some(c) = cache.as_ref() {
                    let cached = c.load_messages(account_id, path, *folder_id);
                    if !cached.is_empty() {
                        emit(WorkerEvent::Messages {
                            folder_id: *folder_id,
                            messages: cached,
                        });
                    }
                }
            }
            MailRequest::LoadGallery => {
                if let Some(c) = cache.as_ref() {
                    let items = c.gallery_items(account_id, GALLERY_DATA_CAP, GALLERY_LIMIT);
                    emit(WorkerEvent::Gallery { items });
                }
                continue; // cache-only, never hits the network
            }
            MailRequest::LoadBody {
                message_id,
                path,
                uid,
            } => {
                if let Some(body) = cache.as_ref().and_then(|c| c.load_body(account_id, path, *uid))
                {
                    emit(WorkerEvent::Body {
                        message_id: *message_id,
                        body,
                    });
                    continue; // already cached; no network needed
                }
            }
            MailRequest::LoadAttachments {
                message_id,
                path,
                uid,
                download,
            } => {
                if let Some(c) = cache.as_ref() {
                    let items = c.load_attachments(account_id, path, *uid);
                    if !items.is_empty() {
                        emit(WorkerEvent::Attachments {
                            message_id: *message_id,
                            items,
                        });
                        continue; // already cached; no network needed
                    }
                }
                // Not cached and the user hasn't asked to download — tell the UI
                // so it can offer a "Load attachments" button instead of fetching.
                if !download {
                    emit(WorkerEvent::AttachmentsPending { message_id: *message_id });
                    continue;
                }
            }
            _ => {}
        }

        // Everything below needs a live session.
        if session.is_none() {
            session = connect_and_list(account_id, &account, cache.as_ref(), &emit).await;
            if session.is_none() {
                continue; // still offline; cached data (if any) was already sent
            }
        }
        // On a connection-shaped failure we drop the session to force a reconnect.
        let mut lost = false;

        match req {
            // Served from cache before this network match; never reached here.
            MailRequest::LoadGallery => {}
            MailRequest::LoadMessages { folder_id, path } => {
                emit(WorkerEvent::Status("Syncing…".into()));
                // Fast first page (or a recent-window refresh over the cached
                // index); the background backfill indexes the rest of the folder.
                // Reads retry once across a reconnect, so an idle-dropped session
                // recovers transparently instead of surfacing an EOF.
                match load_messages_retry(
                    account_id,
                    &mut session,
                    &account,
                    folder_id,
                    &path,
                    &mut use_envelope,
                    cache.as_ref(),
                )
                .await
                {
                    Ok(messages) => {
                        if let Some(c) = cache.as_ref() {
                            // Upsert (not replace) so the background-indexed tail
                            // isn't wiped by a fast first-page load.
                            c.upsert_messages(account_id, &path, &messages);
                        }
                        // Ensure this folder gets fully indexed (if not already
                        // queued this session).
                        if backfill_seen.insert(path.clone()) {
                            backfill.push_back(Backfill {
                                folder_id,
                                gallery: folder_is_gallery(cache.as_ref(), account_id, &path),
                                path: path.clone(),
                                remaining: None,
                            });
                        }
                        // Pre-download recent bodies so opening them is instant.
                        queue_body_prefetch(
                            &mut body_prefetch,
                            &path,
                            &messages,
                            &body_emitted,
                        );
                        // Queue background attachment pre-downloads for recent
                        // messages that have them — older ones download on demand.
                        queue_attachment_prefetch(
                            &mut prefetch,
                            &path,
                            &messages,
                            cache.as_ref(),
                            account_id,
                        );
                        idle_folder = Some((folder_id, path.clone()));
                        emit(WorkerEvent::Messages { folder_id, messages });
                        // Refresh the true unread count (catches new mail and
                        // reads from other clients beyond the loaded window).
                        if let Some(sess) = session.as_mut() {
                            if let Some(unread) = selected_unseen(sess).await {
                                emit(WorkerEvent::FolderUnread { folder_id, unread });
                            }
                        }
                    }
                    Err(e) => {
                        emit(WorkerEvent::Error {
                            text: format!("Could not load {path}: {e}"),
                            connectivity: true,
                        });
                    }
                }
                emit(WorkerEvent::Status(prefetch_status(prefetch.len())));
            }

            MailRequest::LoadBody {
                message_id,
                path,
                uid,
            } => match load_body_retry(&mut session, &account, &path, uid).await {
                Ok(body) => {
                    if let Some(c) = cache.as_ref() {
                        c.save_body(account_id, &path, uid, &body);
                    }
                    emit(WorkerEvent::Body { message_id, body });
                }
                Err(e) => {
                    emit(WorkerEvent::Error {
                        text: format!("Could not load message: {e}"),
                        connectivity: true,
                    });
                }
            },

            MailRequest::LoadSource { path, uid, .. } => match load_source_retry(
                &mut session,
                &account,
                &path,
                uid,
            )
            .await
            {
                Ok(text) => emit(WorkerEvent::Source { text }),
                Err(e) => {
                    emit(WorkerEvent::Error {
                        text: format!("Could not load source: {e}"),
                        connectivity: true,
                    });
                }
            },

            MailRequest::LoadAttachments {
                message_id,
                path,
                uid,
                download: _,
            } => match load_raw_retry(&mut session, &account, &path, uid).await {
                Ok(raw) => {
                    let items = extract_attachments(&raw);
                    if let Some(c) = cache.as_ref() {
                        c.save_attachments(account_id, &path, uid, &items);
                        c.mark_attachments_checked(account_id, &path, uid);
                    }
                    emit(WorkerEvent::Attachments { message_id, items });
                }
                Err(e) => {
                    emit(WorkerEvent::Error {
                        text: format!("Could not load attachments: {e}"),
                        connectivity: true,
                    });
                }
            },

            MailRequest::SetSeen { path, uid, seen } => {
                let sess = session.as_mut().unwrap();
                if let Err(e) = store_flag(sess, &path, uid, "\\Seen", seen).await {
                    emit(WorkerEvent::Error {
                        text: format!("Could not update message: {e}"),
                        connectivity: false,
                    });
                    lost = true;
                } else if let Some(c) = cache.as_ref() {
                    c.set_unread(account_id, &path, uid, !seen);
                }
            }

            MailRequest::SetFlagged {
                path,
                uid,
                flagged,
            } => {
                let sess = session.as_mut().unwrap();
                if let Err(e) = store_flag(sess, &path, uid, "\\Flagged", flagged).await {
                    emit(WorkerEvent::Error {
                        text: format!("Could not flag message: {e}"),
                        connectivity: false,
                    });
                    lost = true;
                } else if let Some(c) = cache.as_ref() {
                    c.set_starred(account_id, &path, uid, flagged);
                }
            }

            MailRequest::MarkAllRead { folder_id, path } => {
                let sess = session.as_mut().unwrap();
                match mark_all_read(sess, &path).await {
                    Ok(()) => {
                        if let Some(c) = cache.as_ref() {
                            c.mark_folder_read(account_id, &path);
                        }
                        emit(WorkerEvent::FolderUnread { folder_id, unread: 0 });
                    }
                    Err(e) => {
                        emit(WorkerEvent::Error {
                            text: format!("Could not mark folder read: {e}"),
                            connectivity: false,
                        });
                        lost = true;
                    }
                }
            }

            MailRequest::MarkSpam { path, uid, dest } => {
                let sess = session.as_mut().unwrap();
                match mark_spam(sess, &path, uid, &dest).await {
                    Ok(created) => {
                        if let Some(c) = cache.as_ref() {
                            c.delete_message(account_id, &path, uid);
                        }
                        if created {
                            refresh_folders(account_id, sess, cache.as_ref(), &emit).await;
                        }
                    }
                    Err(e) => {
                        emit(WorkerEvent::Error {
                            text: format!("Could not mark as spam: {e}"),
                            connectivity: false,
                        });
                        lost = true;
                    }
                }
            }

            MailRequest::MoveMessage { path, uid, dest } => {
                let sess = session.as_mut().unwrap();
                match move_message(sess, &path, uid, &dest).await {
                    Ok(created) => {
                        if let Some(c) = cache.as_ref() {
                            c.delete_message(account_id, &path, uid);
                        }
                        if created {
                            refresh_folders(account_id, sess, cache.as_ref(), &emit).await;
                        }
                    }
                    Err(e) => {
                        emit(WorkerEvent::Error {
                            text: format!("Could not move message: {e}"),
                            connectivity: false,
                        });
                        lost = true;
                    }
                }
            }

            MailRequest::MoveMessages { path, uids, dest } => {
                let sess = session.as_mut().unwrap();
                match move_messages(sess, &path, &uids, &dest).await {
                    Ok(created) => {
                        if let Some(c) = cache.as_ref() {
                            for uid in &uids {
                                c.delete_message(account_id, &path, *uid);
                            }
                        }
                        if created {
                            refresh_folders(account_id, sess, cache.as_ref(), &emit).await;
                        }
                    }
                    Err(e) => {
                        emit(WorkerEvent::Error {
                            text: format!("Could not move {} messages: {e}", uids.len()),
                            connectivity: false,
                        });
                        lost = true;
                    }
                }
                // Always signal completion so the UI's bulk spinner clears.
                emit(WorkerEvent::BulkComplete);
            }

            MailRequest::CreateFolder { path } => {
                let sess = session.as_mut().unwrap();
                match create_folder(sess, &path).await {
                    Ok(()) => refresh_folders(account_id, sess, cache.as_ref(), &emit).await,
                    Err(e) => {
                        emit(WorkerEvent::Error {
                            text: format!("Could not create folder: {e}"),
                            connectivity: false,
                        });
                        lost = true;
                    }
                }
            }

            MailRequest::DeleteFolder { path, trash } => {
                let sess = session.as_mut().unwrap();
                match delete_folder(sess, &path, trash.as_deref()).await {
                    Ok(()) => refresh_folders(account_id, sess, cache.as_ref(), &emit).await,
                    Err(e) => {
                        emit(WorkerEvent::Error {
                            text: format!("Could not delete folder: {e}"),
                            connectivity: false,
                        });
                        lost = true;
                    }
                }
            }

            MailRequest::Send { message, sent_path } => {
                emit(WorkerEvent::Status("Sending…".into()));
                match send_smtp(&account, &message).await {
                    Ok(raw) => {
                        emit(WorkerEvent::Status(String::new()));
                        record_sent_addresses(cache.as_ref(), &message);
                        // Save a copy to the Sent folder; sending still counts as
                        // success even if this part fails.
                        if let Some(path) = sent_path {
                            let sess = session.as_mut().unwrap();
                            if let Err(e) = append_to_sent(sess, &path, &raw).await {
                                emit(WorkerEvent::Error {
                                    text: format!("Message sent, but saving to Sent failed: {e}"),
                                    connectivity: false,
                                });
                            }
                        }
                        // If sending an edited draft (from this account), remove the
                        // now-obsolete draft and refresh the Drafts folder.
                        if let Some(o) = message.draft_origin.clone() {
                            if o.account_id == account_id {
                                {
                                    let sess = session.as_mut().unwrap();
                                    let _ = delete_draft(sess, &o.path, o.uid).await;
                                }
                                if let Some(c) = cache.as_ref() {
                                    c.delete_message(account_id, &o.path, o.uid);
                                }
                                if let Ok(messages) = load_messages_retry(
                                    account_id, &mut session, &account, o.folder_id, &o.path,
                                    &mut use_envelope, cache.as_ref(),
                                )
                                .await
                                {
                                    if let Some(c) = cache.as_ref() {
                                        c.upsert_messages(account_id, &o.path, &messages);
                                    }
                                    emit(WorkerEvent::Messages { folder_id: o.folder_id, messages });
                                }
                            }
                        }
                        emit(WorkerEvent::Sent);
                    }
                    Err(e) => {
                        emit(WorkerEvent::Status(String::new()));
                        emit(WorkerEvent::Error {
                            text: format!("Send failed: {e}"),
                            connectivity: false,
                        });
                    }
                }
            }

            MailRequest::SaveDraft { message, folder_id, path } => {
                emit(WorkerEvent::Status("Saving draft…".into()));
                match build_email(&account, &message) {
                    Ok(email) => {
                        let raw = email.formatted();
                        let append_res = {
                            let sess = session.as_mut().unwrap();
                            let r = append_draft(sess, &path, &raw).await;
                            // Replace the previous version of this draft (same account).
                            if r.is_ok() {
                                if let Some(o) = &message.draft_origin {
                                    if o.account_id == account_id {
                                        let _ = delete_draft(sess, &o.path, o.uid).await;
                                    }
                                }
                            }
                            r
                        };
                        match append_res {
                            Ok(()) => {
                                if let Some(o) = &message.draft_origin {
                                    if o.account_id == account_id {
                                        if let Some(c) = cache.as_ref() {
                                            c.delete_message(account_id, &o.path, o.uid);
                                        }
                                    }
                                }
                                // Reload Drafts so the saved draft appears.
                                if let Ok(messages) = load_messages_retry(
                                    account_id, &mut session, &account, folder_id, &path,
                                    &mut use_envelope, cache.as_ref(),
                                )
                                .await
                                {
                                    if let Some(c) = cache.as_ref() {
                                        c.upsert_messages(account_id, &path, &messages);
                                    }
                                    emit(WorkerEvent::Messages { folder_id, messages });
                                }
                                // Surface a newly-created Drafts folder in the sidebar.
                                if let Some(sess) = session.as_mut() {
                                    refresh_folders(account_id, sess, cache.as_ref(), &emit).await;
                                }
                                emit(WorkerEvent::Status(String::new()));
                                emit(WorkerEvent::DraftSaved);
                            }
                            Err(e) => {
                                emit(WorkerEvent::Status(String::new()));
                                emit(WorkerEvent::Error {
                                    text: format!("Could not save draft: {e}"),
                                    connectivity: false,
                                });
                                lost = true;
                            }
                        }
                    }
                    Err(e) => {
                        emit(WorkerEvent::Status(String::new()));
                        emit(WorkerEvent::Error {
                            text: format!("Could not save draft: {e}"),
                            connectivity: false,
                        });
                    }
                }
            }

            MailRequest::Reconnect => unreachable!("handled above"),
        }

        if lost {
            session = None;
        }
    }

    if let Some(mut session) = session {
        let _ = session.logout().await;
    }
}

/// Connect, announce the account, and list folders, updating the cache. Emits a
/// fresh `Folders` event only when the listing differs from the cache (so an
/// unchanged list doesn't trigger a redundant UI rebuild). Returns `None` (after
/// emitting an error) if the connection could not be established.
async fn connect_and_list(
    account_id: u32,
    account: &AccountConfig,
    cache: Option<&Cache>,
    emit: &impl Fn(WorkerEvent),
) -> Option<ImapSession> {
    emit(WorkerEvent::Status(format!("Connecting to {}…", account.imap_host)));

    let result = match connect(account).await {
        Ok(mut session) => {
            emit(WorkerEvent::Account(Account {
                id: account_id,
                name: account.name.clone(),
                email: account.email.clone(),
                label: account.display_label(),
                accent: accent_for(account_id).into(),
            }));
            match list_folders(account_id, &mut session).await {
                Ok(folders) => {
                    let changed = cache
                        .map(|c| !crate::cache::folders_equal(&c.load_folders(account_id), &folders))
                        .unwrap_or(true);
                    if let Some(c) = cache {
                        c.save_folders(account_id, &folders);
                    }
                    if changed {
                        emit(WorkerEvent::Folders(folders));
                    }
                }
                Err(e) => emit(WorkerEvent::Error {
                    text: format!("Could not list folders: {e}"),
                    connectivity: true,
                }),
            }
            Some(session)
        }
        Err(e) => {
            emit(WorkerEvent::Error {
                text: format!("Connection failed: {e}"),
                connectivity: true,
            });
            None
        }
    };

    emit(WorkerEvent::Status(String::new()));
    result
}

/// Run `load_messages`, retrying once over a fresh login if the first attempt
/// fails (typically a server-dropped idle connection: "unexpected EOF"). On
/// success the (possibly new) session is stored back; on failure it is dropped.
async fn load_messages_retry(
    account_id: u32,
    session: &mut Option<ImapSession>,
    account: &AccountConfig,
    folder_id: u32,
    path: &str,
    use_envelope: &mut bool,
    cache: Option<&Cache>,
) -> Result<Vec<Message>, async_imap::error::Error> {
    let mut s = session.take().expect("session ensured before call");
    let first = load_messages(account_id, &mut s, folder_id, path, *use_envelope, cache).await;

    // A non-empty success is trustworthy — keep the session and return it.
    if matches!(&first, Ok(msgs) if !msgs.is_empty()) {
        *session = Some(s);
        return first;
    }

    // Otherwise the result is an error (stale connection → EOF, or a BODYSTRUCTURE
    // our parser rejected) or an empty mailbox (which a stale session can return
    // without erroring). Re-verify on a fresh login. An *unverified* empty result
    // is treated as a failure so it can never wipe cached mail.
    match connect(account).await {
        Ok(fresh) => {
            s = fresh;
            // If the first attempt errored while parsing the structured ENVELOPE/
            // BODYSTRUCTURE, the server likely sends non-compliant responses (e.g.
            // iCloud). Fall back to raw-header parsing for the rest of the session.
            if first.is_err() && *use_envelope {
                *use_envelope = false;
            }
            let second =
                load_messages(account_id, &mut s, folder_id, path, *use_envelope, cache).await;
            if second.is_ok() {
                *session = Some(s);
            }
            second
        }
        Err(_) => match first {
            Ok(_) => Err(async_imap::error::Error::ConnectionLost),
            Err(e) => Err(e),
        },
    }
}

/// Like [`load_messages_retry`], but for a single body.
async fn load_body_retry(
    session: &mut Option<ImapSession>,
    account: &AccountConfig,
    path: &str,
    uid: u32,
) -> Result<String, async_imap::error::Error> {
    let mut s = session.take().expect("session ensured before call");
    let mut res = load_body(&mut s, path, uid).await;
    if res.is_err() {
        if let Ok(fresh) = connect(account).await {
            s = fresh;
            res = load_body(&mut s, path, uid).await;
        }
    }
    if res.is_ok() {
        *session = Some(s);
    }
    res
}

async fn load_source_retry(
    session: &mut Option<ImapSession>,
    account: &AccountConfig,
    path: &str,
    uid: u32,
) -> Result<String, async_imap::error::Error> {
    let mut s = session.take().expect("session ensured before call");
    let mut res = load_source(&mut s, path, uid).await;
    if res.is_err() {
        if let Ok(fresh) = connect(account).await {
            s = fresh;
            res = load_source(&mut s, path, uid).await;
        }
    }
    if res.is_ok() {
        *session = Some(s);
    }
    res
}

/// Fetch the raw RFC 822 source (headers + body) of a message, undecoded.
async fn load_source(
    session: &mut ImapSession,
    path: &str,
    uid: u32,
) -> Result<String, async_imap::error::Error> {
    session.select(path).await?;

    let fetches: Vec<Fetch> = session
        .uid_fetch(uid.to_string(), "(BODY.PEEK[])")
        .await?
        .try_collect()
        .await?;

    let raw = fetches
        .iter()
        .find_map(|f| f.body())
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_else(|| "(empty message)".to_string());

    Ok(raw)
}

/// Result of an IDLE wait: a request to handle, a folder that was refreshed
/// (new mail arrived), or the channel closing.
enum IdleOutcome {
    Request(MailRequest),
    Refreshed,
    Quiet,
    Closed,
}

/// Download and cache one queued attachment (body + attachments), if the session
/// is live. Connects on demand; clears the queue if offline.
async fn run_one_prefetch(
    prefetch: &mut std::collections::VecDeque<(String, u32)>,
    session: &mut Option<ImapSession>,
    account: &AccountConfig,
    account_id: u32,
    cache: Option<&Cache>,
    emit: &impl Fn(WorkerEvent),
) {
    let Some((path, uid)) = prefetch.front().cloned() else {
        return;
    };
    if session.is_none() {
        *session = connect_and_list(account_id, account, cache, emit).await;
    }
    if session.is_some() {
        prefetch.pop_front();
        let already = cache
            .map(|c| c.attachments_checked(account_id, &path, uid))
            .unwrap_or(false);
        if !already {
            if let Ok(raw) = load_raw_retry(session, account, &path, uid).await {
                let attachments = extract_attachments(&raw);
                if let Some(c) = cache {
                    c.save_body(account_id, &path, uid, &extract_body(&raw));
                    c.save_attachments(account_id, &path, uid, &attachments);
                    // Mark as fetched so it's never re-downloaded to re-check,
                    // even if it had no attachments after all.
                    c.mark_attachments_checked(account_id, &path, uid);
                }
                // Correct a false paperclip live: a message flagged as having an
                // attachment but with none once fetched (iCloud multipart/mixed
                // wrapping only inline images).
                if attachments.is_empty() {
                    emit(WorkerEvent::NoAttachments { message_id: uid });
                }
            }
        }
        emit(WorkerEvent::Status(prefetch_status(prefetch.len())));
    } else {
        prefetch.clear();
        emit(WorkerEvent::Status(String::new()));
    }
}

/// Queue the newest messages for background body prefetch, so opening new mail is
/// instant. The prefetch both caches the body on disk *and* pushes it to the UI's
/// in-memory cache (see [`run_one_body_prefetch`]), so a click needs no round-trip.
/// Skips messages already pushed this session. Called after every folder sync.
fn queue_body_prefetch(
    queue: &mut std::collections::VecDeque<(String, u32)>,
    path: &str,
    messages: &[Message],
    emitted: &std::collections::HashSet<(String, u32)>,
) {
    let mut recent: Vec<&Message> = messages.iter().collect();
    recent.sort_by(|a, b| b.uid.cmp(&a.uid)); // newest first
    for m in recent.into_iter().take(PREFETCH_BODY_LIMIT) {
        let key = (path.to_string(), m.uid);
        if emitted.contains(&key) {
            continue;
        }
        if queue.iter().any(|(p, u)| p == path && *u == m.uid) {
            continue;
        }
        queue.push_back(key);
    }
}

/// Queue the newest messages that have attachments (not already cached) for
/// background attachment pre-download, so new mail's attachments are ready too.
/// Whether a folder's attachments feed the gallery (so its mail is worth
/// prefetching): everything except Trash, Junk and Drafts.
fn gallery_folder(kind: crate::models::FolderKind) -> bool {
    use crate::models::FolderKind::*;
    !matches!(kind, Trash | Junk | Drafts)
}

/// Gallery eligibility of a folder by path, looked up from the cached folder list.
/// Unknown folders default to eligible.
fn folder_is_gallery(cache: Option<&Cache>, account_id: u32, path: &str) -> bool {
    cache
        .map(|c| {
            c.load_folders(account_id)
                .iter()
                .find(|f| f.path == path)
                .map(|f| gallery_folder(f.kind))
                .unwrap_or(true)
        })
        .unwrap_or(true)
}

fn queue_attachment_prefetch(
    queue: &mut std::collections::VecDeque<(String, u32)>,
    path: &str,
    messages: &[Message],
    cache: Option<&Cache>,
    account_id: u32,
) {
    let mut recent: Vec<&Message> = messages.iter().collect();
    recent.sort_by(|a, b| b.uid.cmp(&a.uid)); // newest first
    for m in recent.into_iter().take(PREFETCH_LIMIT) {
        if !m.has_attachment {
            continue;
        }
        // Skip messages we've already fetched attachments for — including ones
        // that turned out to have none (so false "has attachment" flags, e.g.
        // iCloud's multipart/mixed, aren't re-downloaded on every sync).
        let checked = cache
            .map(|c| c.attachments_checked(account_id, path, m.uid))
            .unwrap_or(false);
        let queued = queue.iter().any(|(p, u)| p == path && *u == m.uid);
        if !checked && !queued {
            queue.push_back((path.to_string(), m.uid));
        }
    }
}

/// Prefetch one queued message body: serve it from the disk cache if present,
/// otherwise fetch and cache it. Either way, push it to the UI so its in-memory
/// cache is warm and clicking the message renders instantly with no round-trip.
async fn run_one_body_prefetch(
    queue: &mut std::collections::VecDeque<(String, u32)>,
    session: &mut Option<ImapSession>,
    account: &AccountConfig,
    account_id: u32,
    cache: Option<&Cache>,
    emitted: &mut std::collections::HashSet<(String, u32)>,
    emit: &impl Fn(WorkerEvent),
) {
    let Some((path, uid)) = queue.front().cloned() else {
        return;
    };
    // A cached body needs no connection — serve it straight to the UI.
    if let Some(body) = cache.and_then(|c| c.load_body(account_id, &path, uid)) {
        queue.pop_front();
        emit(WorkerEvent::Body { message_id: uid, body });
        emitted.insert((path, uid));
        return;
    }
    if session.is_none() {
        *session = connect_and_list(account_id, account, cache, emit).await;
    }
    if session.is_none() {
        queue.clear();
        return;
    }
    queue.pop_front();
    if let Ok(body) = load_body_retry(session, account, &path, uid).await {
        if let Some(c) = cache {
            c.save_body(account_id, &path, uid, &body);
        }
        emit(WorkerEvent::Body { message_id: uid, body });
        emitted.insert((path, uid));
    }
}

/// Block for the next request (used as the IDLE fallback).
async fn recv_one(rx: &mut mpsc::UnboundedReceiver<MailRequest>) -> IdleOutcome {
    match rx.recv().await {
        Some(req) => IdleOutcome::Request(req),
        None => IdleOutcome::Closed,
    }
}

/// Enter IMAP IDLE on `path` for up to `timeout_secs` and wait for new mail or
/// an incoming request. Returns `Refreshed` (after re-syncing) only when the
/// server actually reports new data; `Quiet` on timeout (so the caller can do a
/// prefetch pass and re-IDLE); `Request` to be handled normally; `Closed` when
/// the channel ends. Any IDLE error falls back to a plain receive.
#[allow(clippy::too_many_arguments)]
async fn idle_wait(
    session: &mut Option<ImapSession>,
    account: &AccountConfig,
    account_id: u32,
    folder_id: u32,
    path: &str,
    rx: &mut mpsc::UnboundedReceiver<MailRequest>,
    cache: Option<&Cache>,
    use_envelope: &mut bool,
    body_prefetch: &mut std::collections::VecDeque<(String, u32)>,
    att_prefetch: &mut std::collections::VecDeque<(String, u32)>,
    body_emitted: &std::collections::HashSet<(String, u32)>,
    emit: &impl Fn(WorkerEvent),
    timeout_secs: u64,
) -> IdleOutcome {
    let Some(mut sess) = session.take() else {
        return recv_one(rx).await;
    };
    if sess.select(path).await.is_err() {
        // Stale connection — drop it so the next request reconnects.
        return recv_one(rx).await;
    }

    let mut handle = sess.idle();
    if handle.init().await.is_err() {
        *session = handle.done().await.ok();
        return recv_one(rx).await;
    }

    enum Wake {
        Idle(async_imap::error::Result<async_imap::extensions::idle::IdleResponse>),
        Request(Option<MailRequest>),
    }
    let wake = {
        let (idle_fut, stop) = handle.wait_with_timeout(Duration::from_secs(timeout_secs));
        tokio::select! {
            r = idle_fut => Wake::Idle(r),
            req = rx.recv() => { drop(stop); Wake::Request(req) }
        }
    };
    *session = handle.done().await.ok();

    match wake {
        Wake::Request(Some(req)) => IdleOutcome::Request(req),
        Wake::Request(None) => IdleOutcome::Closed,
        // Only re-sync on actual new data; a plain timeout is Quiet.
        Wake::Idle(Ok(async_imap::extensions::idle::IdleResponse::NewData(_))) => {
            if session.is_some() {
                if let Ok(messages) = load_messages_retry(
                    account_id, session, account, folder_id, path, use_envelope, cache,
                )
                .await
                {
                    if let Some(c) = cache {
                        c.upsert_messages(account_id, path, &messages);
                    }
                    // Pre-download the new mail's body (and any attachments) so
                    // opening it is instant.
                    queue_body_prefetch(body_prefetch, path, &messages, body_emitted);
                    queue_attachment_prefetch(att_prefetch, path, &messages, cache, account_id);
                    emit(WorkerEvent::Messages { folder_id, messages });
                    // Refresh the true unread count too. IDLE only re-synced the
                    // message list; without this the sidebar chip never moves when
                    // new mail lands in a background (unfocused) inbox — it would
                    // take an explicit reload / "All Inboxes" refresh to appear.
                    if let Some(sess) = session.as_mut() {
                        if let Some(unread) = selected_unseen(sess).await {
                            emit(WorkerEvent::FolderUnread { folder_id, unread });
                        }
                    }
                }
            }
            IdleOutcome::Refreshed
        }
        Wake::Idle(_) => IdleOutcome::Quiet,
    }
}

async fn load_raw_retry(
    session: &mut Option<ImapSession>,
    account: &AccountConfig,
    path: &str,
    uid: u32,
) -> Result<Vec<u8>, async_imap::error::Error> {
    let mut s = session.take().expect("session ensured before call");
    let mut res = load_raw(&mut s, path, uid).await;
    if res.is_err() {
        if let Ok(fresh) = connect(account).await {
            s = fresh;
            res = load_raw(&mut s, path, uid).await;
        }
    }
    if res.is_ok() {
        *session = Some(s);
    }
    res
}

/// Fetch the raw RFC 822 bytes of a message (binary-safe, for attachments).
async fn load_raw(
    session: &mut ImapSession,
    path: &str,
    uid: u32,
) -> Result<Vec<u8>, async_imap::error::Error> {
    session.select(path).await?;
    let fetches: Vec<Fetch> = session
        .uid_fetch(uid.to_string(), "(BODY.PEEK[])")
        .await?
        .try_collect()
        .await?;
    Ok(fetches
        .iter()
        .find_map(|f| f.body())
        .map(|b| b.to_vec())
        .unwrap_or_default())
}

/// Parse attachment parts (name, mime, decoded bytes) out of a raw message.
fn extract_attachments(raw: &[u8]) -> Vec<crate::models::Attachment> {
    use mail_parser::{MessageParser, MimeHeaders};
    let Some(parsed) = MessageParser::default().parse(raw) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, part) in parsed.attachments().enumerate() {
        // Skip `cid:` resources referenced from the HTML body (newsletter logos):
        // they're rendered in place, and `structure_has_attachment` doesn't count
        // them, so listing them here would contradict the paperclip.
        if part.content_id().is_some() {
            continue;
        }
        let name = part
            .attachment_name()
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("attachment-{}", i + 1));
        out.push(crate::models::Attachment {
            name,
            data: part.contents().to_vec(),
        });
    }
    out
}

/// Status text for the attachment pre-download queue (empty = idle).
fn prefetch_status(remaining: usize) -> String {
    if remaining == 0 {
        String::new()
    } else if remaining == 1 {
        "Downloading attachments… 1 remaining".to_string()
    } else {
        format!("Downloading attachments… {remaining} remaining")
    }
}

/// Guess a MIME type from a filename extension (best-effort).
fn guess_mime(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "html" | "htm" => "text/html",
        "csv" => "text/csv",
        "zip" => "application/zip",
        "doc" | "docx" => "application/msword",
        "xls" | "xlsx" => "application/vnd.ms-excel",
        _ => "application/octet-stream",
    }
}

type SmtpError = Box<dyn std::error::Error + Send + Sync>;

/// Record a sent message's recipients so they autocomplete in future composes.
fn record_sent_addresses(cache: Option<&Cache>, msg: &OutgoingMessage) {
    let Some(cache) = cache else {
        return;
    };
    let mut entries = Vec::new();
    for list in [&msg.to, &msg.cc, &msg.bcc] {
        entries.extend(parse_recipients(list));
    }
    cache.record_addresses(&entries);
}

/// Parse a recipient field ("Name <a@b>, c@d") into (name, email) pairs.
fn parse_recipients(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match (part.rfind('<'), part.rfind('>')) {
            (Some(lt), Some(gt)) if lt < gt => {
                let email = part[lt + 1..gt].trim().to_string();
                let name = part[..lt].trim().trim_matches('"').trim().to_string();
                out.push((name, email));
            }
            _ => out.push((String::new(), part.to_string())),
        }
    }
    out
}

/// Send the message and return its raw RFC 822 bytes (for saving to Sent).
/// Build the RFC 822 email (headers + MIME body) from a composed message. Shared
/// by SMTP sending and by saving to Drafts (no network).
fn build_email(account: &AccountConfig, msg: &OutgoingMessage) -> Result<LettreMessage, SmtpError> {
    let from: Mailbox = format!("{} <{}>", account.name, account.email).parse()?;
    let mut builder = LettreMessage::builder().from(from);
    for addr in split_addrs(&msg.to) {
        builder = builder.to(addr.parse()?);
    }
    for addr in split_addrs(&msg.cc) {
        builder = builder.cc(addr.parse()?);
    }
    for addr in split_addrs(&msg.bcc) {
        builder = builder.bcc(addr.parse()?);
    }
    let builder = builder.subject(msg.subject.clone());

    use lettre::message::{header::ContentType, Attachment, MultiPart, SinglePart};
    let has_html = !msg.html.trim().is_empty();
    let email = if msg.attachments.is_empty() {
        if has_html {
            builder.multipart(MultiPart::alternative_plain_html(
                msg.body.clone(),
                msg.html.clone(),
            ))?
        } else {
            builder.body(msg.body.clone())?
        }
    } else {
        // text part (plain, or alternative plain+html) followed by attachments.
        let mut multipart = if has_html {
            MultiPart::mixed().multipart(MultiPart::alternative_plain_html(
                msg.body.clone(),
                msg.html.clone(),
            ))
        } else {
            MultiPart::mixed().singlepart(SinglePart::plain(msg.body.clone()))
        };
        for path in &msg.attachments {
            let bytes = std::fs::read(path)?;
            let name = std::path::Path::new(path)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "attachment".to_string());
            let ct = ContentType::parse(guess_mime(&name))
                .unwrap_or(ContentType::TEXT_PLAIN);
            multipart = multipart.singlepart(Attachment::new(name).body(bytes, ct));
        }
        builder.multipart(multipart)?
    };
    Ok(email)
}

async fn send_smtp(account: &AccountConfig, msg: &OutgoingMessage) -> Result<Vec<u8>, SmtpError> {
    // GOA credentials can change while an IMAP session remains connected.
    // Resolve them again immediately before SMTP authentication.
    let mut resolved = account.clone();
    resolve_credentials(&mut resolved, false).await;
    let account = &resolved;
    let email = build_email(account, msg)?;
    let raw = email.formatted();

    let host = smtp_host(account);
    // Port 465 is implicit TLS; everything else (587, etc.) uses STARTTLS.
    let transport_builder = if account.smtp_uses_implicit_tls() {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&host)?
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&host)?
    };
    let mut builder = transport_builder.port(account.smtp_port);
    if account.smtp_auth && account.oauth {
        // XOAUTH2: the "password" is a fresh OAuth token from GOA.
        let token = fetch_oauth_token(account).await.ok_or_else(|| -> SmtpError {
            "could not get an OAuth token from GNOME Online Accounts".into()
        })?;
        let user = smtp_oauth_user(account);
        builder = builder
            .credentials(Credentials::new(user, token))
            .authentication(vec![lettre::transport::smtp::authentication::Mechanism::Xoauth2]);
    } else if account.smtp_auth {
        // Use the separate SMTP credentials when configured, else the IMAP ones.
        let creds = if account.smtp_separate {
            Credentials::new(account.smtp_username.clone(), account.smtp_password.clone())
        } else {
            Credentials::new(account.username.clone(), account.password.clone())
        };
        builder = builder.credentials(creds);
    }
    let mailer: AsyncSmtpTransport<Tokio1Executor> = builder.build();

    mailer.send(email).await?;
    Ok(raw)
}

/// The SASL identity for XOAUTH2 (the IMAP/SMTP username, else the email).
fn oauth_user(account: &AccountConfig) -> String {
    if account.username.trim().is_empty() {
        account.email.clone()
    } else {
        account.username.clone()
    }
}

/// SMTP may use a different SASL identity even though both services share the
/// same GOA OAuth token.
fn smtp_oauth_user(account: &AccountConfig) -> String {
    if account.smtp_username.trim().is_empty() {
        oauth_user(account)
    } else {
        account.smtp_username.clone()
    }
}

/// Fetch a fresh OAuth access token for this account — from GNOME Online Accounts
/// (imported) or by refreshing a natively-added account's stored refresh token.
async fn fetch_oauth_token(account: &AccountConfig) -> Option<String> {
    if let Some(goa_id) = account.goa_id.clone() {
        return tokio::task::spawn_blocking(move || crate::goa::oauth_token(&goa_id))
            .await
            .ok()
            .flatten();
    }
    // Natively-added OAuth account: refresh with the keyring-stored refresh token.
    // The client credentials saved with the account MUST be used — a refresh token
    // is bound to the OAuth client that issued it, so switching clients requires
    // re-adding the account (a fresh sign-in), not swapping creds here.
    let settings = account.oauth_settings.clone()?;
    let refresh = crate::config::load_oauth_refresh(&account.email)?;
    tokio::task::spawn_blocking(move || crate::oauth::refresh_access_token(&settings, &refresh).ok())
        .await
        .ok()
        .flatten()
}

/// XOAUTH2 SASL authenticator for async-imap.
struct XOAuth2 {
    user: String,
    token: String,
    step: u8,
}

impl async_imap::Authenticator for XOAuth2 {
    type Response = Vec<u8>;
    fn process(&mut self, challenge: &[u8]) -> Self::Response {
        self.step += 1;
        if self.step == 1 {
            // Initial SASL response.
            format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token).into_bytes()
        } else {
            // The server rejected auth and sent an error challenge; XOAUTH2 requires
            // an empty response so the server then sends the tagged error (rather
            // than the exchange deadlocking).
            let _ = challenge;
            Vec::new()
        }
    }
}

async fn append_to_sent(
    session: &mut ImapSession,
    path: &str,
    raw: &[u8],
) -> Result<(), async_imap::error::Error> {
    // Mark the saved copy as already read.
    session.append(path, Some("(\\Seen)"), None, raw).await
}

/// APPEND a draft to the Drafts folder (flagged `\Draft \Seen`), creating the
/// mailbox first if the server doesn't have one yet.
async fn append_draft(
    session: &mut ImapSession,
    path: &str,
    raw: &[u8],
) -> Result<(), async_imap::error::Error> {
    if session
        .append(path, Some("(\\Draft \\Seen)"), None, raw)
        .await
        .is_ok()
    {
        return Ok(());
    }
    // Folder likely doesn't exist — create it and retry.
    let _ = session.create(path).await;
    let _ = session.subscribe(path).await;
    session.append(path, Some("(\\Draft \\Seen)"), None, raw).await
}

/// Delete a superseded draft (the previous version being replaced or sent):
/// flag it `\Deleted` and expunge it from the Drafts folder.
async fn delete_draft(
    session: &mut ImapSession,
    path: &str,
    uid: u32,
) -> Result<(), async_imap::error::Error> {
    session.select(path).await?;
    let _: Result<Vec<Fetch>, _> = session
        .uid_store(uid.to_string(), "+FLAGS (\\Deleted)")
        .await?
        .try_collect()
        .await;
    let _: Vec<u32> = session.expunge().await?.try_collect().await?;
    Ok(())
}

fn split_addrs(s: &str) -> impl Iterator<Item = &str> {
    s.split(',').map(str::trim).filter(|s| !s.is_empty())
}

/// SMTP host: the configured value, or derived from the IMAP host.
fn smtp_host(account: &AccountConfig) -> String {
    let configured = account.smtp_host.trim();
    if !configured.is_empty() {
        configured.to_string()
    } else if let Some(rest) = account.imap_host.strip_prefix("imap") {
        format!("smtp{rest}")
    } else {
        account.imap_host.clone()
    }
}

async fn store_flag(
    session: &mut ImapSession,
    path: &str,
    uid: u32,
    flag: &str,
    add: bool,
) -> Result<(), async_imap::error::Error> {
    session.select(path).await?;
    let op = if add { "+FLAGS" } else { "-FLAGS" };
    let query = format!("{op} ({flag})");
    // Drain the resulting FETCH stream so the command completes.
    let _: Vec<Fetch> = session
        .uid_store(uid.to_string(), query)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// Move a message to `dest`, creating (and subscribing to) the destination
/// mailbox first if it doesn't exist yet. Returns whether a folder was created,
/// so the caller can refresh the folder list.
async fn move_or_create(
    session: &mut ImapSession,
    path: &str,
    uid: u32,
    dest: &str,
) -> Result<bool, async_imap::error::Error> {
    session.select(path).await?;
    if session.uid_mv(uid.to_string(), dest).await.is_ok() {
        return Ok(false);
    }
    // The move failed — most likely the destination mailbox doesn't exist (the
    // account has no Archive/Junk/… folder yet). Create it, subscribe, and retry;
    // if it still fails, surface that error.
    let created = session.create(dest).await.is_ok();
    let _ = session.subscribe(dest).await;
    session.select(path).await?;
    session.uid_mv(uid.to_string(), dest).await?;
    Ok(created)
}

async fn move_message(
    session: &mut ImapSession,
    path: &str,
    uid: u32,
    dest: &str,
) -> Result<bool, async_imap::error::Error> {
    move_or_create(session, path, uid, dest).await
}

/// Move many messages from `path` to `dest` with as few IMAP commands as
/// possible: one SELECT, then a UID MOVE per chunk of the UID set (chunked so a
/// huge selection never overflows the server's command-length limit). Creates and
/// subscribes to `dest` on demand. Returns whether the destination was created.
async fn move_messages(
    session: &mut ImapSession,
    path: &str,
    uids: &[u32],
    dest: &str,
) -> Result<bool, async_imap::error::Error> {
    session.select(path).await?;
    let mut created = false;
    let mut ensured_dest = false;
    for chunk in uids.chunks(300) {
        let set = chunk.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
        if session.uid_mv(&set, dest).await.is_ok() {
            continue;
        }
        // First failure is most likely a missing destination mailbox — create,
        // subscribe, re-select the source, and retry this chunk (then the rest).
        if !ensured_dest {
            created = session.create(dest).await.is_ok();
            let _ = session.subscribe(dest).await;
            ensured_dest = true;
            session.select(path).await?;
        }
        session.uid_mv(&set, dest).await?;
    }
    Ok(created)
}

/// Create (and subscribe to) a new mailbox.
async fn create_folder(
    session: &mut ImapSession,
    path: &str,
) -> Result<(), async_imap::error::Error> {
    session.create(path).await?;
    let _ = session.subscribe(path).await;
    Ok(())
}

/// Delete a mailbox after moving all of its messages to `trash` (creating the
/// trash mailbox if needed). With no trash target the contents are discarded.
async fn delete_folder(
    session: &mut ImapSession,
    path: &str,
    trash: Option<&str>,
) -> Result<(), async_imap::error::Error> {
    let mailbox = session.select(path).await?;
    if mailbox.exists > 0 {
        if let Some(trash) = trash {
            if !trash.eq_ignore_ascii_case(path) {
                // Move everything to Trash; create it first if the move fails.
                if session.uid_mv("1:*", trash).await.is_err() {
                    let _ = session.create(trash).await;
                    let _ = session.subscribe(trash).await;
                    session.select(path).await?;
                    session.uid_mv("1:*", trash).await?;
                }
            }
        }
    }
    // A mailbox can't be deleted while selected — close it first.
    let _ = session.close().await;
    session.delete(path).await?;
    Ok(())
}

/// Mark every message in a folder as read (`\Seen`) in one STORE.
async fn mark_all_read(
    session: &mut ImapSession,
    path: &str,
) -> Result<(), async_imap::error::Error> {
    let mailbox = session.select(path).await?;
    if mailbox.exists == 0 {
        return Ok(());
    }
    let _: Vec<Fetch> = session
        .uid_store("1:*", "+FLAGS (\\Seen)")
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// Mark a message as spam following common conventions: set the `$Junk` keyword
/// and clear `$NotJunk` (so server-side filters / other clients can learn), then
/// move it to the Junk folder. Keyword stores are best-effort — some servers
/// reject custom keywords — but the move is authoritative.
async fn mark_spam(
    session: &mut ImapSession,
    path: &str,
    uid: u32,
    dest: &str,
) -> Result<bool, async_imap::error::Error> {
    session.select(path).await?;
    for query in ["+FLAGS ($Junk)", "-FLAGS ($NotJunk)"] {
        if let Ok(stream) = session.uid_store(uid.to_string(), query).await {
            let _: Result<Vec<Fetch>, _> = stream.try_collect().await;
        }
    }
    move_or_create(session, path, uid, dest).await
}

const IMAP_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn connect(account: &AccountConfig) -> Result<ImapSession, Box<dyn std::error::Error>> {
    // Every retry path eventually comes through here. Refresh GOA/keyring
    // credentials first so reconnect helpers never reuse a stale worker copy.
    let mut resolved = account.clone();
    resolve_credentials(&mut resolved, false).await;
    match tokio::time::timeout(IMAP_CONNECT_TIMEOUT, connect_inner(&resolved)).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "IMAP connection to {}:{} timed out",
            account.imap_host, account.imap_port
        )
        .into()),
    }
}

async fn connect_inner(account: &AccountConfig) -> Result<ImapSession, Box<dyn std::error::Error>> {
    let tcp = TcpStream::connect((account.imap_host.as_str(), account.imap_port)).await?;
    let tls = async_native_tls::TlsConnector::new();
    let client = if account.imap_uses_starttls() {
        // GOA distinguishes implicit TLS (`ImapUseSsl`) from STARTTLS
        // (`ImapUseTls`). Require STARTTLS before sending any credentials.
        let mut plain = async_imap::Client::new(tcp);
        plain
            .read_response()
            .await
            .ok_or("IMAP server closed before its greeting")??;
        // On async-imap 0.10's unauthenticated Client this is the Connection
        // method; its second argument is the optional unsolicited-response sink.
        plain.run_command_and_check_ok("STARTTLS", None).await?;
        let stream = tls
            .connect(account.imap_host.as_str(), plain.into_inner())
            .await?;
        async_imap::Client::new(stream)
    } else {
        let stream = tls.connect(account.imap_host.as_str(), tcp).await?;
        let mut client = async_imap::Client::new(stream);
        // Consume the greeting before AUTHENTICATE; LOGIN happens to tolerate an
        // unread greeting, while the OAuth exchange does not.
        client
            .read_response()
            .await
            .ok_or("IMAP server closed before its greeting")??;
        client
    };
    let session = if account.oauth {
        // XOAUTH2 with a fresh access token (from GOA or a native refresh token).
        let token = fetch_oauth_token(account)
            .await
            .ok_or("could not get an OAuth token")?;
        let auth = XOAuth2 { user: oauth_user(account), token, step: 0 };
        client
            .authenticate("XOAUTH2", auth)
            .await
            .map_err(|(e, _client)| e)?
    } else {
        client
            .login(&account.username, &account.password)
            .await
            .map_err(|(e, _client)| e)?
    };
    Ok(session)
}

/// Outcome of a credential/connection test for the Accounts window.
#[derive(Debug)]
pub struct ConnTest {
    /// Incoming server (IMAP or POP3, per the account's protocol).
    pub incoming: Result<(), String>,
    pub smtp: Result<(), String>,
}

/// Test that the account's servers accept the given credentials (no mail sent).
pub async fn test_connection(account: &AccountConfig) -> ConnTest {
    let incoming = if account.protocol == crate::config::Protocol::Pop3 {
        test_pop3(account).await
    } else {
        test_imap(account).await
    };
    ConnTest {
        incoming,
        smtp: test_smtp(account).await,
    }
}

async fn test_pop3(account: &AccountConfig) -> Result<(), String> {
    let mut pop = Pop3::connect(account).await?;
    pop.login(&account.username, &account.password).await?;
    pop.quit().await;
    Ok(())
}

/// Blocking wrapper around [`test_connection`] that spins up its own Tokio
/// runtime — call it from `spawn_blocking` so the IMAP/SMTP sockets have an I/O
/// reactor regardless of the caller's runtime.
pub fn test_connection_blocking(mut account: AccountConfig) -> ConnTest {
    match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt.block_on(async {
            resolve_credentials(&mut account, false).await;
            test_connection(&account).await
        }),
        Err(e) => ConnTest {
            incoming: Err(e.to_string()),
            smtp: Err(e.to_string()),
        },
    }
}

async fn test_imap(account: &AccountConfig) -> Result<(), String> {
    // Stringify the (non-Send) error before any further await so the returned
    // future stays Send (required by relm4's command runner).
    let mut session = connect(account).await.map_err(|e| e.to_string())?;
    let _ = session.logout().await;
    Ok(())
}

/// Connect to the SMTP server and authenticate (then quit) — verifies the send
/// credentials without delivering anything.
async fn test_smtp(account: &AccountConfig) -> Result<(), String> {
    use lettre::transport::smtp::authentication::{Credentials, Mechanism};
    use lettre::transport::smtp::client::{AsyncSmtpConnection, TlsParameters};
    use lettre::transport::smtp::extension::ClientId;

    let host = smtp_host(account);
    let (creds, mechanisms) = if account.oauth {
        let token = fetch_oauth_token(account)
            .await
            .ok_or_else(|| "could not get an OAuth token from GNOME Online Accounts".to_string())?;
        (
            Credentials::new(smtp_oauth_user(account), token),
            vec![Mechanism::Xoauth2],
        )
    } else {
        let (user, pass) = if account.smtp_separate {
            (account.smtp_username.clone(), account.smtp_password.clone())
        } else {
            (account.username.clone(), account.password.clone())
        };
        (
            Credentials::new(user, pass),
            vec![Mechanism::Plain, Mechanism::Login],
        )
    };
    let hello = ClientId::default();
    let tls = TlsParameters::new(host.clone()).map_err(|e| e.to_string())?;
    // A tuple keeps IPv6 literals unambiguous; formatting `host:port` would
    // require manually restoring brackets stripped while parsing GOA settings.
    let addr = (host.as_str(), account.smtp_port);
    let timeout = Some(std::time::Duration::from_secs(20));

    // Port 465 is implicit TLS; everything else uses STARTTLS.
    let mut conn = if account.smtp_uses_implicit_tls() {
        AsyncSmtpConnection::connect_tokio1(addr, timeout, &hello, Some(tls), None)
            .await
            .map_err(|e| e.to_string())?
    } else {
        let mut conn = AsyncSmtpConnection::connect_tokio1(addr, timeout, &hello, None, None)
            .await
            .map_err(|e| e.to_string())?;
        conn.starttls(tls, &hello).await.map_err(|e| e.to_string())?;
        conn
    };
    let result = if account.smtp_auth {
        conn.auth(&mechanisms, &creds)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    } else {
        Ok(())
    };
    let _ = conn.quit().await;
    result
}

async fn list_folders(
    account_id: u32,
    session: &mut ImapSession,
) -> Result<Vec<Folder>, async_imap::error::Error> {
    let names: Vec<async_imap::types::Name> = session
        .list(Some(""), Some("*"))
        .await?
        .try_collect()
        .await?;

    let mut folders = Vec::new();
    let mut special_use = Vec::new(); // parallel: kind came from a SPECIAL-USE attr
    for name in names.iter() {
        // Skip containers that cannot hold messages.
        if name.attributes().contains(&NameAttribute::NoSelect) {
            continue;
        }
        let path = name.name().to_string();
        let (kind, by_special_use) = classify_with_source(&path, name.attributes());
        special_use.push(by_special_use);
        folders.push(Folder {
            id: 0, // assigned by order below
            account_id,
            name: display_name(&path, name.delimiter()),
            path,
            kind,
            unread: 0,
        });
    }

    // When a role has a real SPECIAL-USE folder, demote any name-matched impostors
    // of that role to Custom — otherwise a stray folder like a plain "Trash" label
    // can shadow the server's actual [Gmail]/Trash and mail "moved" there never
    // really leaves (it just gains a label).
    for role in [
        FolderKind::Sent,
        FolderKind::Drafts,
        FolderKind::Trash,
        FolderKind::Junk,
        FolderKind::Archive,
        FolderKind::Starred,
    ] {
        let has_special = folders
            .iter()
            .zip(&special_use)
            .any(|(f, su)| f.kind == role && *su);
        if has_special {
            for (f, su) in folders.iter_mut().zip(&special_use) {
                if f.kind == role && !*su {
                    f.kind = FolderKind::Custom;
                }
            }
        }
    }

    folders.sort_by_key(|f| folder_order(f.kind));
    for (i, f) in folders.iter_mut().enumerate() {
        f.id = i as u32 + 1;
    }

    // Ask the server for each folder's true unread count. STATUS is cheap and
    // downloads no message content, so this stays fast even for huge mailboxes.
    for f in folders.iter_mut() {
        if let Ok(mb) = session.status(&f.path, "(UNSEEN)").await {
            f.unread = mb.unseen.unwrap_or(0);
        }
    }

    // STATUS is unreliable on some servers (notably iCloud), which leaves stale
    // inbox chips in the sidebar until the folder is opened. The inbox is the only
    // folder whose unread count is shown, so refine just that one up front with the
    // accurate EXAMINE + SEARCH UNSEEN (read-only; leaves the mailbox unselected
    // for the main loop to re-select as needed).
    if let Some(inbox) = folders.iter_mut().find(|f| f.kind == FolderKind::Inbox) {
        if session.examine(&inbox.path).await.is_ok() {
            if let Some(n) = selected_unseen(session).await {
                inbox.unread = n;
            }
        }
    }
    Ok(folders)
}

/// Re-list folders (e.g. after auto-creating one) and push them to the UI.
async fn refresh_folders(
    account_id: u32,
    session: &mut ImapSession,
    cache: Option<&Cache>,
    emit: &impl Fn(WorkerEvent),
) {
    if let Ok(folders) = list_folders(account_id, session).await {
        if let Some(c) = cache {
            c.save_folders(account_id, &folders);
        }
        emit(WorkerEvent::Folders(folders));
    }
}

/// Count unseen messages in the currently-selected mailbox via SEARCH (safe on
/// the selected folder, unlike STATUS on some servers). Downloads only ids.
async fn selected_unseen(session: &mut ImapSession) -> Option<u32> {
    session
        .uid_search("UNSEEN")
        .await
        .ok()
        .map(|uids| uids.len() as u32)
}

/// Load a folder's message index for immediate display.
///
/// Never-synced folder → fetch a fast [`FIRST_PAGE`] of the newest messages so
/// browsing is instant; the background backfill indexes the rest. Already-cached
/// folder → fetch just the recent window and merge it over the existing (possibly
/// whole-mailbox) index, picking up new mail and flag changes without re-pulling
/// thousands of envelopes.
async fn load_messages(
    account_id: u32,
    session: &mut ImapSession,
    folder_id: u32,
    path: &str,
    use_envelope: bool,
    cache: Option<&Cache>,
) -> Result<Vec<Message>, async_imap::error::Error> {
    let mailbox = session.select(path).await?;
    let total = mailbox.exists;
    if total == 0 {
        // Folder emptied on the server — drop any cached copies so they don't linger.
        if let Some(c) = cache {
            for uid in c.cached_uids(account_id, path) {
                c.delete_message(account_id, path, uid);
            }
        }
        return Ok(Vec::new());
    }

    let cached = cache
        .map(|c| c.load_messages(account_id, path, folder_id))
        .unwrap_or_default();
    if cached.is_empty() {
        let mut messages =
            fetch_window(account_id, session, folder_id, total, FIRST_PAGE, use_envelope).await?;
        reconcile_attachment_flags(cache, account_id, path, &mut messages);
        Ok(messages)
    } else {
        let recent =
            fetch_window(account_id, session, folder_id, total, PAGE_SIZE, use_envelope).await?;
        let mut merged = merge_index(cached, recent);
        // Reconcile deletions/moves made on the server or another device: drop any
        // message whose UID the server no longer lists (a plain merge would keep it
        // forever), and prune it from the cache so it doesn't come back. The full
        // UID set is a cheap server-side search even for large mailboxes.
        let server: std::collections::HashSet<u32> = session.uid_search("ALL").await?;
        if let Some(c) = cache {
            for m in merged.iter().filter(|m| !server.contains(&m.uid)) {
                c.delete_message(account_id, path, m.uid);
            }
        }
        merged.retain(|m| server.contains(&m.uid));
        reconcile_attachment_flags(cache, account_id, path, &mut merged);
        Ok(merged)
    }
}

/// Clear the "has attachment" flag on freshly-fetched summaries whose bodies we
/// already downloaded and found to contain no real attachments. Server summary
/// flags (especially iCloud's header-only `multipart/mixed` guess) over-report
/// attachments for HTML mail whose only extra parts are inline `cid:` images.
fn reconcile_attachment_flags(
    cache: Option<&Cache>,
    account_id: u32,
    path: &str,
    messages: &mut [Message],
) {
    let Some(c) = cache else { return };
    if messages.iter().all(|m| !m.has_attachment) {
        return;
    }
    let attachmentless = c.attachmentless_uids(account_id, path);
    if attachmentless.is_empty() {
        return;
    }
    for m in messages.iter_mut() {
        if m.has_attachment && attachmentless.contains(&m.uid) {
            m.has_attachment = false;
        }
    }
}

/// Fetch the most-recent `count` messages' summaries (newest first). Includes
/// BODYSTRUCTURE so the attachment indicator is known for every indexed message
/// — still no bodies/attachments are downloaded.
async fn fetch_window(
    account_id: u32,
    session: &mut ImapSession,
    folder_id: u32,
    total: u32,
    count: u32,
    use_envelope: bool,
) -> Result<Vec<Message>, async_imap::error::Error> {
    let start = total.saturating_sub(count - 1).max(1);
    let range = format!("{start}:{total}");

    // Normally we fetch the structured ENVELOPE + BODYSTRUCTURE (compact, and the
    // latter gives the attachment indicator). But some servers (notably iCloud)
    // emit RFC-noncompliant ENVELOPE/BODYSTRUCTURE (e.g. NIL transfer-encodings,
    // unescaped quotes in the Message-ID) that our IMAP parser rejects. For those
    // the caller retries with `use_envelope = false`, and we instead pull the raw
    // header block — opaque to the IMAP parser — and parse it with mail-parser.
    let mut messages: Vec<Message> = if use_envelope {
        let fetches: Vec<Fetch> = session
            .fetch(&range, "(UID ENVELOPE FLAGS BODYSTRUCTURE INTERNALDATE)")
            .await?
            .try_collect()
            .await?;
        fetches
            .iter()
            .map(|f| build_summary(account_id, f, folder_id))
            .collect()
    } else {
        let fetches: Vec<Fetch> = session
            .fetch(&range, "(UID FLAGS BODY.PEEK[HEADER] INTERNALDATE)")
            .await?
            .try_collect()
            .await?;
        fetches
            .iter()
            .map(|f| summary_from_headers(account_id, f, folder_id))
            .collect()
    };
    messages.reverse(); // IMAP returns oldest-first; show newest at the top.
    Ok(messages)
}

/// Build a message summary from a raw header block (mail-parser), for servers
/// whose structured ENVELOPE our IMAP parser can't handle.
fn summary_from_headers(account_id: u32, fetch: &Fetch, folder_id: u32) -> Message {
    use mail_parser::MessageParser;

    let uid = fetch.uid.unwrap_or(0);
    let flags: Vec<Flag> = fetch.flags().collect();
    let unread = !flags.iter().any(|f| matches!(f, Flag::Seen));
    let starred = flags.iter().any(|f| matches!(f, Flag::Flagged));

    let raw = fetch.header().unwrap_or(&[]);
    let parsed = MessageParser::default().parse(raw);

    let mp_first = |a: Option<&mail_parser::Address>| -> (String, String) {
        a.and_then(|a| a.first())
            .map(|addr| {
                let email = addr.address().unwrap_or_default().to_string();
                let name = addr.name().map(|s| s.to_string()).filter(|s| !s.is_empty());
                (name.unwrap_or_else(|| email.clone()), email)
            })
            .unwrap_or_else(|| ("Unknown".to_string(), String::new()))
    };
    let mp_list = |a: Option<&mail_parser::Address>| -> String {
        a.map(|a| {
            a.iter()
                .filter_map(|addr| addr.address().map(|s| s.to_string()))
                .filter(|e| !e.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
    };

    let (from_name, from_addr) = parsed
        .as_ref()
        .map(|p| mp_first(p.from()))
        .unwrap_or_else(|| ("Unknown".to_string(), String::new()));
    let subject = parsed
        .as_ref()
        .and_then(|p| p.subject())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(no subject)".to_string());
    let (date, timestamp) = parsed
        .as_ref()
        .and_then(|p| p.date())
        .map(|d| {
            let ts = d.to_timestamp();
            (format_timestamp(ts), ts)
        })
        .filter(|(_, ts)| *ts > 0)
        .unwrap_or_else(|| internal_date_summary(fetch));
    let to = parsed.as_ref().map(|p| mp_list(p.to())).unwrap_or_default();
    let cc = parsed.as_ref().map(|p| mp_list(p.cc())).unwrap_or_default();

    // Best-effort attachment hint from the top-level Content-Type (BODYSTRUCTURE
    // isn't available on this path). multipart/mixed is the usual attachment case.
    let has_attachment = parsed
        .as_ref()
        .and_then(|p| p.header("Content-Type"))
        .and_then(|h| h.as_content_type())
        .map(|ct| {
            ct.ctype().eq_ignore_ascii_case("multipart")
                && ct
                    .subtype()
                    .is_some_and(|s| s.eq_ignore_ascii_case("mixed"))
        })
        .unwrap_or(false);

    let (message_id, references) = mp_thread_ids(parsed.as_ref());

    Message {
        id: uid,
        account_id,
        folder_id,
        uid,
        from_name,
        from_addr,
        to,
        cc,
        subject,
        preview: String::new(),
        body: String::new(),
        date,
        timestamp,
        unread,
        starred,
        has_attachment,
        message_id,
        references,
    }
}

/// Extract (message_id, references) from a parsed message for threading. References
/// combines In-Reply-To and References, normalized (no angle brackets, lowercased).
fn mp_thread_ids(parsed: Option<&mail_parser::Message>) -> (String, String) {
    use mail_parser::HeaderValue;
    let norm = |s: &str| {
        s.trim().trim_start_matches('<').trim_end_matches('>').trim().to_ascii_lowercase()
    };
    let collect = |hv: &HeaderValue| -> Vec<String> {
        match hv {
            HeaderValue::Text(t) => vec![norm(t)],
            HeaderValue::TextList(v) => v.iter().map(|t| norm(t)).collect(),
            _ => Vec::new(),
        }
    };
    let Some(p) = parsed else {
        return (String::new(), String::new());
    };
    let message_id = p.message_id().map(norm).unwrap_or_default();
    let mut refs: Vec<String> = Vec::new();
    for id in collect(p.in_reply_to()).into_iter().chain(collect(p.references())) {
        if !id.is_empty() && !refs.contains(&id) {
            refs.push(id);
        }
    }
    (message_id, refs.join(" "))
}

/// Overlay a freshly-fetched recent window onto the cached index: recent rows
/// replace their cached versions (updated flags / new mail), the rest are kept.
/// No size cap — the whole folder is searchable once the background backfill has
/// indexed it.
fn merge_index(cached: Vec<Message>, recent: Vec<Message>) -> Vec<Message> {
    let mut map: std::collections::HashMap<u32, Message> =
        cached.into_iter().map(|m| (m.uid, m)).collect();
    for m in recent {
        map.insert(m.uid, m);
    }
    let mut out: Vec<Message> = map.into_values().collect();
    out.sort_by(|a, b| b.uid.cmp(&a.uid)); // newest first
    out
}

/// A background job to index the rest of a folder (everything past the fast first
/// page) so search covers the whole mailbox. `remaining` is the still-to-fetch
/// UIDs (newest first), computed lazily on the first drain.
struct Backfill {
    folder_id: u32,
    path: String,
    remaining: Option<Vec<u32>>,
    /// Whether this folder feeds the attachments gallery (not Trash/Junk/Drafts);
    /// if so, its backfilled messages' attachments are prefetched too.
    gallery: bool,
}

/// Determine which UIDs still need indexing: everything on the server not already
/// cached. Also reconciles deletions (cached UIDs the server no longer has).
async fn backfill_worklist(
    session: &mut ImapSession,
    account_id: u32,
    path: &str,
    cache: Option<&Cache>,
) -> Result<Vec<u32>, async_imap::error::Error> {
    session.select(path).await?;
    let server: std::collections::HashSet<u32> = session.uid_search("ALL").await?;
    let cached = cache
        .map(|c| c.cached_uids(account_id, path))
        .unwrap_or_default();
    if let Some(c) = cache {
        for uid in cached.iter() {
            if !server.contains(uid) {
                c.delete_message(account_id, path, *uid);
            }
        }
    }
    let mut remaining: Vec<u32> = server.difference(&cached).copied().collect();
    remaining.sort_unstable_by(|a, b| b.cmp(a)); // newest first
    Ok(remaining)
}

/// Fetch message summaries for a specific set of UIDs (used by the backfill).
async fn fetch_summaries_by_uid(
    account_id: u32,
    session: &mut ImapSession,
    folder_id: u32,
    path: &str,
    uids: &[u32],
    use_envelope: bool,
) -> Result<Vec<Message>, async_imap::error::Error> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    session.select(path).await?;
    let set = uids.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
    let items = if use_envelope {
        "(UID ENVELOPE FLAGS BODYSTRUCTURE INTERNALDATE)"
    } else {
        "(UID FLAGS BODY.PEEK[HEADER] INTERNALDATE)"
    };
    let fetches: Vec<Fetch> = session.uid_fetch(set, items).await?.try_collect().await?;
    Ok(fetches
        .iter()
        .map(|f| {
            if use_envelope {
                build_summary(account_id, f, folder_id)
            } else {
                summary_from_headers(account_id, f, folder_id)
            }
        })
        .collect())
}

/// Advance one backfill job by a chunk. Fetches the next `BACKFILL_CHUNK` UIDs,
/// upserts them into the cache, and emits them as an append to the UI's index.
/// Requeues the job (at the back) if more remain. Reconnects and, if needed,
/// disables ENVELOPE parsing (iCloud) on a parse failure.
#[allow(clippy::too_many_arguments)]
async fn run_one_backfill(
    queue: &mut std::collections::VecDeque<Backfill>,
    session: &mut Option<ImapSession>,
    account: &AccountConfig,
    account_id: u32,
    cache: Option<&Cache>,
    prefetch: &mut std::collections::VecDeque<(String, u32)>,
    use_envelope: &mut bool,
    emit: &impl Fn(WorkerEvent),
) {
    let Some(mut job) = queue.pop_front() else {
        return;
    };
    let Some(mut s) = session.take() else {
        queue.push_front(job);
        return;
    };

    // Compute the worklist on first touch (with one reconnect on failure).
    if job.remaining.is_none() {
        match backfill_worklist(&mut s, account_id, &job.path, cache).await {
            Ok(rem) => job.remaining = Some(rem),
            Err(_) => match connect(account).await {
                Ok(fresh) => {
                    s = fresh;
                    match backfill_worklist(&mut s, account_id, &job.path, cache).await {
                        Ok(rem) => job.remaining = Some(rem),
                        Err(_) => {
                            *session = Some(s);
                            queue.push_back(job);
                            return;
                        }
                    }
                }
                Err(_) => {
                    // Stay offline; retry this job on the next connect.
                    queue.push_back(job);
                    return;
                }
            },
        }
    }

    let rem = job.remaining.as_mut().unwrap();
    if rem.is_empty() {
        emit(WorkerEvent::BackfillDone { folder_id: job.folder_id });
        *session = Some(s); // done — don't requeue
        return;
    }
    let take = rem.len().min(BACKFILL_CHUNK);
    let chunk: Vec<u32> = rem.drain(..take).collect();

    match fetch_summaries_by_uid(account_id, &mut s, job.folder_id, &job.path, &chunk, *use_envelope)
        .await
    {
        Ok(msgs) => {
            if let Some(c) = cache {
                c.upsert_messages(account_id, &job.path, &msgs);
            }
            // Gallery folders: queue this chunk's attachments for background
            // download so they appear in the attachments gallery.
            if job.gallery {
                queue_attachment_prefetch(prefetch, &job.path, &msgs, cache, account_id);
            }
            emit(WorkerEvent::MessagesAppend {
                folder_id: job.folder_id,
                messages: msgs,
            });
            *session = Some(s);
        }
        Err(_) => {
            // Put the chunk back and reconnect; a parse error means the server's
            // structured responses are unusable (iCloud) — fall back to headers.
            for uid in chunk.into_iter().rev() {
                rem.insert(0, uid);
            }
            if *use_envelope {
                *use_envelope = false;
            }
            if let Ok(fresh) = connect(account).await {
                *session = Some(fresh);
            }
            // else: session stays None; the main loop reconnects on next request.
        }
    }

    if job.remaining.as_ref().is_some_and(|r| !r.is_empty()) {
        queue.push_back(job);
    } else {
        emit(WorkerEvent::BackfillDone { folder_id: job.folder_id });
    }
}

async fn load_body(
    session: &mut ImapSession,
    path: &str,
    uid: u32,
) -> Result<String, async_imap::error::Error> {
    session.select(path).await?;

    // Fetch the whole message (PEEK so \Seen isn't set) and extract the body with
    // mail-parser. We deliberately avoid a BODYSTRUCTURE-based "text part only"
    // fast path: some servers (iCloud) return structures our IMAP parser rejects,
    // which would fail the fetch and corrupt the session.
    let fetches: Vec<Fetch> = session
        .uid_fetch(uid.to_string(), "(BODY.PEEK[])")
        .await?
        .try_collect()
        .await?;
    let body = fetches
        .iter()
        .find_map(|f| f.body())
        .map(extract_body)
        .unwrap_or_else(|| "(empty message)".to_string());
    Ok(body)
}

/// Fetch BODYSTRUCTURE and return the IMAP section number of the preferred text
/// part (HTML over plain). `None` for non-multipart messages (just fetch whole).
#[allow(dead_code)]
async fn body_section(
    session: &mut ImapSession,
    uid: u32,
) -> Result<Option<String>, async_imap::error::Error> {
    let fetches: Vec<Fetch> = session
        .uid_fetch(uid.to_string(), "BODYSTRUCTURE")
        .await?
        .try_collect()
        .await?;
    Ok(fetches
        .iter()
        .find_map(|f| f.bodystructure())
        .and_then(find_text_section))
}

/// Fetch a single MIME part's headers + body and decode it into display HTML.
#[allow(dead_code)]
async fn fetch_part_body(
    session: &mut ImapSession,
    uid: u32,
    section: &str,
) -> Result<Option<String>, async_imap::error::Error> {
    let mime: Vec<Fetch> = session
        .uid_fetch(uid.to_string(), format!("(BODY.PEEK[{section}.MIME])"))
        .await?
        .try_collect()
        .await?;
    let headers = mime.iter().find_map(|f| f.body()).map(|b| b.to_vec());

    let part: Vec<Fetch> = session
        .uid_fetch(uid.to_string(), format!("(BODY.PEEK[{section}])"))
        .await?
        .try_collect()
        .await?;
    let body = part.iter().find_map(|f| f.body()).map(|b| b.to_vec());

    match (headers, body) {
        (Some(mut msg), Some(b)) => {
            // Reassemble "headers\r\n\r\nbody" so mail-parser decodes it (charset,
            // quoted-printable / base64) using the part's own MIME headers. Trim
            // any trailing newlines first so there's exactly one blank-line
            // separator whether or not the server already included it.
            while msg.ends_with(b"\r\n") {
                msg.truncate(msg.len() - 2);
            }
            while msg.ends_with(b"\n") {
                msg.truncate(msg.len() - 1);
            }
            msg.extend_from_slice(b"\r\n\r\n");
            msg.extend_from_slice(&b);
            Ok(Some(extract_body(&msg)))
        }
        _ => Ok(None),
    }
}

/// Find the IMAP section number of the best text part in a BODYSTRUCTURE.
/// Returns `None` for non-multipart messages (small — fetch the whole thing).
#[allow(dead_code)]
fn find_text_section(bs: &async_imap::imap_proto::types::BodyStructure) -> Option<String> {
    use async_imap::imap_proto::types::BodyStructure as Bs;

    fn walk(
        bs: &Bs,
        prefix: &str,
        html: &mut Option<String>,
        plain: &mut Option<String>,
    ) {
        match bs {
            Bs::Multipart { bodies, .. } => {
                for (i, child) in bodies.iter().enumerate() {
                    let section = if prefix.is_empty() {
                        format!("{}", i + 1)
                    } else {
                        format!("{prefix}.{}", i + 1)
                    };
                    walk(child, &section, html, plain);
                }
            }
            Bs::Text { common, .. } => {
                let is_attachment = common
                    .disposition
                    .as_ref()
                    .is_some_and(|d| d.ty.eq_ignore_ascii_case("attachment"));
                if !is_attachment {
                    if common.ty.subtype.eq_ignore_ascii_case("html") && html.is_none() {
                        *html = Some(prefix.to_string());
                    } else if common.ty.subtype.eq_ignore_ascii_case("plain") && plain.is_none() {
                        *plain = Some(prefix.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    if !matches!(bs, Bs::Multipart { .. }) {
        return None;
    }
    let (mut html, mut plain) = (None, None);
    walk(bs, "", &mut html, &mut plain);
    html.or(plain)
}

fn build_summary(account_id: u32, fetch: &Fetch, folder_id: u32) -> Message {
    let uid = fetch.uid.unwrap_or(0);
    let flags: Vec<Flag> = fetch.flags().collect();
    let unread = !flags.iter().any(|f| matches!(f, Flag::Seen));
    let starred = flags.iter().any(|f| matches!(f, Flag::Flagged));

    let env = fetch.envelope();
    let (from_name, from_addr) = env
        .and_then(|e| e.from.as_ref())
        .and_then(|v| v.first())
        .map(address_parts)
        .unwrap_or_else(|| ("Unknown".to_string(), String::new()));
    let subject = env
        .and_then(|e| e.subject.as_deref())
        .map(decode_header)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(no subject)".to_string());
    let (date, timestamp) = env
        .and_then(|e| e.date.as_deref())
        .map(format_date)
        .filter(|(_, ts)| *ts > 0)
        .unwrap_or_else(|| internal_date_summary(fetch));
    let to = address_list(env.and_then(|e| e.to.as_ref()));
    let cc = address_list(env.and_then(|e| e.cc.as_ref()));

    let has_attachment = fetch
        .bodystructure()
        .map(structure_has_attachment)
        .unwrap_or(false);

    // Message-ID + In-Reply-To drive accurate threading (References isn't in the
    // IMAP ENVELOPE, but In-Reply-To links each reply to its parent).
    let message_id = env
        .and_then(|e| e.message_id.as_deref())
        .map(normalize_msgid)
        .unwrap_or_default();
    let references = env
        .and_then(|e| e.in_reply_to.as_deref())
        .map(normalize_msgids)
        .unwrap_or_default();

    Message {
        id: uid,
        account_id,
        folder_id,
        uid,
        from_name,
        from_addr,
        to,
        cc,
        subject,
        preview: String::new(),
        body: String::new(),
        date,
        timestamp,
        unread,
        starred,
        has_attachment,
        message_id,
        references,
    }
}

/// Normalize a single Message-ID: strip angle brackets/whitespace, lowercase.
fn normalize_msgid(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    s.trim().trim_start_matches('<').trim_end_matches('>').trim().to_ascii_lowercase()
}

/// Normalize a whitespace-separated list of Message-IDs into a canonical string.
fn normalize_msgids(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    s.split_whitespace()
        .map(|tok| tok.trim_start_matches('<').trim_end_matches('>').trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether an IMAP BODYSTRUCTURE contains a part marked as an attachment.
///
/// `Content-Disposition: attachment` is the obvious case, but Apple Mail sends
/// iPhone photos as *inline* parts of a multipart/mixed, so disposition alone
/// misses them. Any non-text part therefore counts, except one carrying a
/// Content-ID — that marks a `cid:` resource referenced from the HTML body
/// (a newsletter logo), which is rendered in place rather than listed.
fn structure_has_attachment(bs: &async_imap::imap_proto::types::BodyStructure) -> bool {
    use async_imap::imap_proto::types::BodyStructure as Bs;

    let is_attachment = |common: &async_imap::imap_proto::types::BodyContentCommon| {
        common
            .disposition
            .as_ref()
            .is_some_and(|d| d.ty.eq_ignore_ascii_case("attachment"))
    };

    match bs {
        Bs::Multipart { bodies, .. } => bodies.iter().any(structure_has_attachment),
        Bs::Text { common, .. } => is_attachment(common),
        Bs::Basic { common, other, .. } | Bs::Message { common, other, .. } => {
            is_attachment(common) || other.id.is_none()
        }
    }
}

// ---------------------------------------------------------------------------
// POP3 path
// ---------------------------------------------------------------------------

/// Cap on how many messages a POP3 sync downloads in full (POP3 has no partial
/// fetch; mailboxes are usually small). Older messages aren't indexed.
const POP3_LIMIT: usize = 200;

/// A minimal async POP3 client over TLS (implicit on 995, STLS otherwise).
struct Pop3 {
    stream: tokio::io::BufReader<async_native_tls::TlsStream<TcpStream>>,
}

impl Pop3 {
    async fn connect(account: &AccountConfig) -> Result<Self, String> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let host = account.imap_host.as_str();
        let port = account.imap_port;
        let tcp = TcpStream::connect((host, port))
            .await
            .map_err(|e| e.to_string())?;
        let tls = async_native_tls::TlsConnector::new();

        let stream = if port == 995 {
            tls.connect(host, tcp).await.map_err(|e| e.to_string())?
        } else {
            // STARTTLS: greet + STLS on the plaintext socket, then upgrade.
            let mut plain = BufReader::new(tcp);
            let mut line = Vec::new();
            plain.read_until(b'\n', &mut line).await.map_err(|e| e.to_string())?;
            plain.write_all(b"STLS\r\n").await.map_err(|e| e.to_string())?;
            plain.flush().await.map_err(|e| e.to_string())?;
            line.clear();
            plain.read_until(b'\n', &mut line).await.map_err(|e| e.to_string())?;
            if !line.starts_with(b"+OK") {
                return Err("server refused STLS".to_string());
            }
            tls.connect(host, plain.into_inner())
                .await
                .map_err(|e| e.to_string())?
        };

        let mut pop = Pop3 { stream: BufReader::new(stream) };
        if port == 995 {
            pop.read_reply().await?; // greeting
        }
        Ok(pop)
    }

    async fn read_reply(&mut self) -> Result<Vec<u8>, String> {
        use tokio::io::AsyncBufReadExt;
        let mut line = Vec::new();
        self.stream
            .read_until(b'\n', &mut line)
            .await
            .map_err(|e| e.to_string())?;
        if line.is_empty() {
            return Err("connection closed".to_string());
        }
        Ok(line)
    }

    async fn send(&mut self, cmd: &str) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        self.stream.write_all(cmd.as_bytes()).await.map_err(|e| e.to_string())?;
        self.stream.write_all(b"\r\n").await.map_err(|e| e.to_string())?;
        self.stream.flush().await.map_err(|e| e.to_string())
    }

    /// Single-line command: returns Err with the server text on `-ERR`.
    async fn command(&mut self, cmd: &str) -> Result<(), String> {
        self.send(cmd).await?;
        let reply = self.read_reply().await?;
        if reply.starts_with(b"+OK") {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&reply).trim().to_string())
        }
    }

    /// Multi-line command: returns the dot-unstuffed body bytes after `+OK`.
    async fn multiline(&mut self, cmd: &str) -> Result<Vec<u8>, String> {
        self.send(cmd).await?;
        let first = self.read_reply().await?;
        if !first.starts_with(b"+OK") {
            return Err(String::from_utf8_lossy(&first).trim().to_string());
        }
        let mut out = Vec::new();
        loop {
            let line = self.read_reply().await?;
            let trimmed = strip_crlf(&line);
            if trimmed == b"." {
                break;
            }
            // Dot-stuffing: a leading '.' is doubled on the wire.
            let content = if trimmed.starts_with(b"..") { &trimmed[1..] } else { trimmed };
            out.extend_from_slice(content);
            out.extend_from_slice(b"\r\n");
        }
        Ok(out)
    }

    async fn login(&mut self, user: &str, pass: &str) -> Result<(), String> {
        self.command(&format!("USER {user}")).await?;
        self.command(&format!("PASS {pass}")).await
    }

    /// Returns (message number, server UID) pairs.
    async fn uidl(&mut self) -> Result<Vec<(u32, String)>, String> {
        let body = self.multiline("UIDL").await?;
        let text = String::from_utf8_lossy(&body);
        let mut out = Vec::new();
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            if let (Some(n), Some(uid)) = (parts.next(), parts.next()) {
                if let Ok(num) = n.parse::<u32>() {
                    out.push((num, uid.to_string()));
                }
            }
        }
        Ok(out)
    }

    async fn retr(&mut self, num: u32) -> Result<Vec<u8>, String> {
        self.multiline(&format!("RETR {num}")).await
    }

    async fn quit(&mut self) {
        let _ = self.command("QUIT").await;
    }
}

fn strip_crlf(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && (line[end - 1] == b'\n' || line[end - 1] == b'\r') {
        end -= 1;
    }
    &line[..end]
}

/// Stable u32 id derived from a POP3 server UID string (which is a string, but
/// the rest of the app keys messages by u32).
fn hash_uid(uid: &str) -> u32 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    uid.hash(&mut h);
    (h.finish() & 0x7fff_ffff) as u32
}

/// Format a unix timestamp the same way [`format_date`] labels mail dates.
fn label_from_timestamp(ts: i64) -> String {
    use chrono::{Datelike, TimeZone};
    let Some(dt) = chrono::Local.timestamp_opt(ts, 0).single() else {
        return String::new();
    };
    let now = chrono::Local::now();
    if dt.date_naive() == now.date_naive() {
        dt.format("%-I:%M %p").to_string()
    } else if dt.year() == now.year() {
        dt.format("%b %-d").to_string()
    } else {
        dt.format("%b %-d, %Y").to_string()
    }
}

/// Build a message summary from a full RFC 822 message (POP3 has no ENVELOPE).
fn summary_from_raw(account_id: u32, folder_id: u32, uid: u32, raw: &[u8]) -> Message {
    use mail_parser::MessageParser;
    let parsed = MessageParser::default().parse(raw);
    let addr = parsed
        .as_ref()
        .and_then(|p| p.from())
        .and_then(|a| a.first());
    let from_addr = addr
        .and_then(|a| a.address())
        .map(|s| s.to_string())
        .unwrap_or_default();
    let from_name = addr
        .and_then(|a| a.name())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            if from_addr.is_empty() {
                "Unknown".to_string()
            } else {
                from_addr.clone()
            }
        });
    let subject = parsed
        .as_ref()
        .and_then(|p| p.subject())
        .filter(|s| !s.is_empty())
        .unwrap_or("(no subject)")
        .to_string();
    let timestamp = parsed
        .as_ref()
        .and_then(|p| p.date())
        .map(|d| d.to_timestamp())
        .unwrap_or(0);
    let date = label_from_timestamp(timestamp);
    let addr_list = |list: Option<&mail_parser::Address>| -> String {
        list.map(|a| {
            a.iter()
                .filter_map(|x| x.address())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
    };
    let to = addr_list(parsed.as_ref().and_then(|p| p.to()));
    let cc = addr_list(parsed.as_ref().and_then(|p| p.cc()));
    let has_attachment = parsed.as_ref().map(|p| p.attachment_count() > 0).unwrap_or(false);
    let (message_id, references) = mp_thread_ids(parsed.as_ref());

    Message {
        id: uid,
        account_id,
        folder_id,
        uid,
        from_name,
        from_addr,
        to,
        cc,
        subject,
        preview: String::new(),
        body: String::new(),
        date,
        timestamp,
        unread: true,
        starred: false,
        has_attachment,
        message_id,
        references,
    }
}

/// The folder list for a POP3 account: just the inbox.
fn pop3_folders(account_id: u32) -> Vec<Folder> {
    vec![Folder {
        id: 1,
        account_id,
        name: "Inbox".to_string(),
        path: "INBOX".to_string(),
        kind: FolderKind::Inbox,
        unread: 0,
    }]
}

async fn run_pop3(
    account_id: u32,
    mut account: AccountConfig,
    mut rx: mpsc::UnboundedReceiver<MailRequest>,
    emit: impl Fn(WorkerEvent),
) {
    resolve_credentials(&mut account, true).await;

    let cache = Cache::open().map_err(|e| tracing::warn!("cache unavailable: {e}")).ok();
    const INBOX: &str = "INBOX";
    let inbox_id = 1u32;

    emit(WorkerEvent::Account(Account {
        id: account_id,
        name: account.name.clone(),
        email: account.email.clone(),
        label: account.display_label(),
        accent: accent_for(account_id).into(),
    }));
    let folders = pop3_folders(account_id);
    if let Some(c) = cache.as_ref() {
        c.save_folders(account_id, &folders);
    }
    emit(WorkerEvent::Folders(folders));

    while let Some(req) = rx.recv().await {
        match req {
            MailRequest::LoadGallery => {
                if let Some(c) = cache.as_ref() {
                    let items = c.gallery_items(account_id, GALLERY_DATA_CAP, GALLERY_LIMIT);
                    emit(WorkerEvent::Gallery { items });
                }
            }
            MailRequest::LoadMessages { folder_id, path } => {
                if path != INBOX {
                    emit(WorkerEvent::Messages { folder_id, messages: Vec::new() });
                    continue;
                }
                // Serve cache first for instant display.
                if let Some(c) = cache.as_ref() {
                    let cached = c.load_messages(account_id, INBOX, inbox_id);
                    if !cached.is_empty() {
                        emit(WorkerEvent::Messages { folder_id, messages: cached });
                    }
                }
                emit(WorkerEvent::Status("Syncing…".into()));
                match pop3_sync(account_id, &account, inbox_id, cache.as_ref()).await {
                    Ok(messages) => {
                        let unread = messages.iter().filter(|m| m.unread).count() as u32;
                        emit(WorkerEvent::Messages { folder_id, messages });
                        emit(WorkerEvent::FolderUnread { folder_id: inbox_id, unread });
                    }
                    Err(e) => emit(WorkerEvent::Error {
                        text: format!("Could not fetch mail: {e}"),
                        connectivity: true,
                    }),
                }
                emit(WorkerEvent::Status(String::new()));
            }

            MailRequest::LoadBody { message_id, path: _, uid } => {
                if let Some(body) = cache.as_ref().and_then(|c| c.load_body(account_id, INBOX, uid)) {
                    emit(WorkerEvent::Body { message_id, body });
                    continue;
                }
                match pop3_fetch_raw(&account, uid).await {
                    Ok(raw) => {
                        let body = extract_body(&raw);
                        if let Some(c) = cache.as_ref() {
                            c.save_body(account_id, INBOX, uid, &body);
                        }
                        emit(WorkerEvent::Body { message_id, body });
                    }
                    Err(e) => emit(WorkerEvent::Error {
                        text: format!("Could not load message: {e}"),
                        connectivity: true,
                    }),
                }
            }

            MailRequest::LoadSource { message_id: _, path: _, uid } => {
                match pop3_fetch_raw(&account, uid).await {
                    Ok(raw) => emit(WorkerEvent::Source {
                        text: String::from_utf8_lossy(&raw).into_owned(),
                    }),
                    Err(e) => emit(WorkerEvent::Error {
                        text: format!("Could not load source: {e}"),
                        connectivity: true,
                    }),
                }
            }

            MailRequest::LoadAttachments { message_id, path: _, uid, download } => {
                if let Some(c) = cache.as_ref() {
                    let items = c.load_attachments(account_id, INBOX, uid);
                    if !items.is_empty() {
                        emit(WorkerEvent::Attachments { message_id, items });
                        continue;
                    }
                }
                if !download {
                    emit(WorkerEvent::AttachmentsPending { message_id });
                    continue;
                }
                match pop3_fetch_raw(&account, uid).await {
                    Ok(raw) => {
                        let items = extract_attachments(&raw);
                        if let Some(c) = cache.as_ref() {
                            c.save_attachments(account_id, INBOX, uid, &items);
                        }
                        emit(WorkerEvent::Attachments { message_id, items });
                    }
                    Err(e) => emit(WorkerEvent::Error {
                        text: format!("Could not load attachments: {e}"),
                        connectivity: true,
                    }),
                }
            }

            MailRequest::SetSeen { uid, seen, .. } => {
                if let Some(c) = cache.as_ref() {
                    c.set_unread(account_id, INBOX, uid, !seen);
                }
            }
            MailRequest::SetFlagged { uid, flagged, .. } => {
                if let Some(c) = cache.as_ref() {
                    c.set_starred(account_id, INBOX, uid, flagged);
                }
            }
            MailRequest::MarkAllRead { folder_id, .. } => {
                if let Some(c) = cache.as_ref() {
                    c.mark_folder_read(account_id, INBOX);
                }
                emit(WorkerEvent::FolderUnread { folder_id, unread: 0 });
            }

            // POP3 has no folders to move between: deleting removes from the
            // server. (Archive/spam have no destination folder, so the UI never
            // reaches here for those.)
            MailRequest::MoveMessage { uid, .. } | MailRequest::MarkSpam { uid, .. } => {
                match pop3_delete(&account, uid).await {
                    Ok(()) => {
                        if let Some(c) = cache.as_ref() {
                            c.delete_message(account_id, INBOX, uid);
                        }
                    }
                    Err(e) => emit(WorkerEvent::Error {
                        text: format!("Could not delete message: {e}"),
                        connectivity: false,
                    }),
                }
            }
            MailRequest::MoveMessages { uids, .. } => {
                for uid in uids {
                    if pop3_delete(&account, uid).await.is_ok() {
                        if let Some(c) = cache.as_ref() {
                            c.delete_message(account_id, INBOX, uid);
                        }
                    }
                }
                emit(WorkerEvent::BulkComplete);
            }

            // POP3 has no folders beyond the inbox.
            MailRequest::CreateFolder { .. }
            | MailRequest::DeleteFolder { .. }
            | MailRequest::SaveDraft { .. } => {
                emit(WorkerEvent::Error {
                    text: "POP3 accounts don't support folders".into(),
                    connectivity: false,
                });
            }

            MailRequest::Send { message, .. } => match send_smtp(&account, &message).await {
                Ok(_) => {
                    record_sent_addresses(cache.as_ref(), &message);
                    emit(WorkerEvent::Sent);
                }
                Err(e) => emit(WorkerEvent::Error {
                    text: format!("Could not send message: {e}"),
                    connectivity: false,
                }),
            },

            MailRequest::Reconnect => {
                emit(WorkerEvent::Folders(pop3_folders(account_id)));
            }
        }
    }
}

/// Connect, download summaries/bodies for new messages, return the inbox list
/// (newest first), merging server UIDs with the local cache (for read state).
async fn pop3_sync(
    account_id: u32,
    account: &AccountConfig,
    inbox_id: u32,
    cache: Option<&Cache>,
) -> Result<Vec<Message>, String> {
    const INBOX: &str = "INBOX";
    let mut pop = Pop3::connect(account).await?;
    pop.login(&account.username, &account.password).await?;
    let mut entries = pop.uidl().await?;
    // Newest first; bound how many we index.
    entries.sort_by(|a, b| b.0.cmp(&a.0));
    entries.truncate(POP3_LIMIT);

    let cached: std::collections::HashMap<u32, Message> = cache
        .map(|c| c.load_messages(account_id, INBOX, inbox_id))
        .unwrap_or_default()
        .into_iter()
        .map(|m| (m.uid, m))
        .collect();

    let mut messages = Vec::with_capacity(entries.len());
    for (num, uid_str) in &entries {
        let uid = hash_uid(uid_str);
        if let Some(existing) = cached.get(&uid) {
            messages.push(existing.clone()); // keep read/star state; already downloaded
            continue;
        }
        // New message: download in full, cache its body + attachments.
        let raw = pop.retr(*num).await?;
        let msg = summary_from_raw(account_id, inbox_id, uid, &raw);
        if let Some(c) = cache {
            c.save_body(account_id, INBOX, uid, &extract_body(&raw));
            let items = extract_attachments(&raw);
            if !items.is_empty() {
                c.save_attachments(account_id, INBOX, uid, &items);
            }
        }
        messages.push(msg);
    }
    pop.quit().await;

    messages.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    if let Some(c) = cache {
        c.save_messages(account_id, INBOX, &messages);
    }
    Ok(messages)
}

/// Fetch one message's raw bytes by its hashed UID (reconnects + maps UID→num).
async fn pop3_fetch_raw(account: &AccountConfig, uid: u32) -> Result<Vec<u8>, String> {
    let mut pop = Pop3::connect(account).await?;
    pop.login(&account.username, &account.password).await?;
    let num = pop
        .uidl()
        .await?
        .into_iter()
        .find(|(_, u)| hash_uid(u) == uid)
        .map(|(n, _)| n)
        .ok_or_else(|| "message no longer on server".to_string())?;
    let raw = pop.retr(num).await?;
    pop.quit().await;
    Ok(raw)
}

/// Delete a message from the POP3 server (DELE, committed on QUIT).
async fn pop3_delete(account: &AccountConfig, uid: u32) -> Result<(), String> {
    let mut pop = Pop3::connect(account).await?;
    pop.login(&account.username, &account.password).await?;
    let num = pop
        .uidl()
        .await?
        .into_iter()
        .find(|(_, u)| hash_uid(u) == uid)
        .map(|(n, _)| n);
    if let Some(num) = num {
        pop.command(&format!("DELE {num}")).await?;
    }
    pop.quit().await; // commits the deletion
    Ok(())
}

// ---------------------------------------------------------------------------
// Mock path (offline fallback)
// ---------------------------------------------------------------------------

async fn run_mock(
    account_id: u32,
    mut rx: mpsc::UnboundedReceiver<MailRequest>,
    emit: impl Fn(WorkerEvent),
) {
    let backend = MockBackend::new();

    if let Some(account) = backend.accounts().into_iter().find(|a| a.id == account_id) {
        emit(WorkerEvent::Account(account));
    }
    emit(WorkerEvent::Folders(backend.folders(account_id)));

    while let Some(req) = rx.recv().await {
        match req {
            // The mock backend has no attachment cache.
            MailRequest::LoadGallery => {
                emit(WorkerEvent::Gallery { items: Vec::new() });
            }
            MailRequest::LoadMessages { folder_id, .. } => {
                emit(WorkerEvent::Messages {
                    folder_id,
                    messages: backend.messages(folder_id),
                });
            }
            MailRequest::LoadBody { message_id, .. } => {
                let body = backend.message(message_id).map(|m| m.body).unwrap_or_default();
                emit(WorkerEvent::Body { message_id, body });
            }
            MailRequest::LoadSource { message_id, .. } => {
                let text = backend.message(message_id).map(|m| m.body).unwrap_or_default();
                emit(WorkerEvent::Source { text });
            }
            MailRequest::LoadAttachments { message_id, .. } => {
                emit(WorkerEvent::Attachments { message_id, items: Vec::new() });
            }
            // Mutations are no-ops offline; the UI updates optimistically.
            MailRequest::SetSeen { .. }
            | MailRequest::SetFlagged { .. }
            | MailRequest::MarkAllRead { .. }
            | MailRequest::MarkSpam { .. }
            | MailRequest::MoveMessage { .. }
            | MailRequest::CreateFolder { .. }
            | MailRequest::DeleteFolder { .. }
            | MailRequest::Reconnect => {}
            // Signal completion so the demo's bulk spinner clears.
            MailRequest::MoveMessages { .. } => emit(WorkerEvent::BulkComplete),
            MailRequest::SaveDraft { .. } => emit(WorkerEvent::DraftSaved),
            // Pretend the send succeeded so the compose flow is demoable offline.
            MailRequest::Send { .. } => emit(WorkerEvent::Sent),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Classify a folder, also reporting whether the kind came from an RFC 6154
/// SPECIAL-USE attribute (`true`) rather than name matching (`false`). This lets
/// `list_folders` prefer the real special-use folder when a server also exposes a
/// stray folder that merely *looks* like it fills the same role — e.g. Gmail's
/// real `[Gmail]/Trash` (\Trash) next to a plain top-level `Trash` label.
fn classify_with_source(path: &str, attrs: &[NameAttribute]) -> (FolderKind, bool) {
    // Prefer RFC 6154 SPECIAL-USE attributes; fall back to name matching.
    for a in attrs {
        match a {
            NameAttribute::Sent => return (FolderKind::Sent, true),
            NameAttribute::Drafts => return (FolderKind::Drafts, true),
            NameAttribute::Trash => return (FolderKind::Trash, true),
            NameAttribute::Junk => return (FolderKind::Junk, true),
            NameAttribute::Archive => return (FolderKind::Archive, true),
            NameAttribute::Flagged => return (FolderKind::Starred, true),
            _ => {}
        }
    }

    let leaf = path.rsplit(['/', '.']).next().unwrap_or(path).to_lowercase();
    let kind = match leaf.as_str() {
        "inbox" => FolderKind::Inbox,
        "sent" | "sent items" | "sent mail" => FolderKind::Sent,
        "drafts" => FolderKind::Drafts,
        "trash" | "deleted" | "deleted items" | "bin" => FolderKind::Trash,
        "junk" | "spam" => FolderKind::Junk,
        "archive" | "all mail" => FolderKind::Archive,
        "starred" | "flagged" => FolderKind::Starred,
        _ => FolderKind::Custom,
    };
    (kind, false)
}

fn folder_order(kind: FolderKind) -> u8 {
    match kind {
        FolderKind::Inbox => 0,
        FolderKind::Starred => 1,
        FolderKind::Drafts => 2,
        FolderKind::Sent => 3,
        FolderKind::Archive => 4,
        FolderKind::Junk => 5,
        FolderKind::Trash => 6,
        FolderKind::Custom => 7,
    }
}

/// Show only the leaf segment of a hierarchical mailbox path, with INBOX
/// special-cased.
fn display_name(path: &str, delimiter: Option<&str>) -> String {
    if path.eq_ignore_ascii_case("inbox") {
        return "Inbox".to_string();
    }
    let leaf = match delimiter {
        Some(d) if !d.is_empty() => path.rsplit(d).next().unwrap_or(path),
        _ => path.rsplit(['/', '.']).next().unwrap_or(path),
    };
    leaf.to_string()
}

/// Join an envelope address list into "a@x.com, b@y.com" (emails only).
fn address_list(addrs: Option<&Vec<async_imap::imap_proto::types::Address>>) -> String {
    addrs
        .map(|v| {
            v.iter()
                .map(|a| address_parts(a).1)
                .filter(|e| !e.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn address_parts(addr: &async_imap::imap_proto::types::Address) -> (String, String) {
    let mailbox = addr.mailbox.as_deref().map(bytes_to_string).unwrap_or_default();
    let host = addr.host.as_deref().map(bytes_to_string).unwrap_or_default();
    let email = if host.is_empty() {
        mailbox.clone()
    } else {
        format!("{mailbox}@{host}")
    };
    let name = addr
        .name
        .as_deref()
        .map(decode_header)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| email.clone());
    (name, email)
}

fn bytes_to_string(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Decode a header value that may be RFC 2047 encoded ("=?UTF-8?…?=").
///
/// Some senders (notably Mailchimp) emit a single encoded-word far longer than
/// RFC 2047's 75-character limit — e.g. The Marginalian's newsletter subjects.
/// The decoder aborts on over-long words by default, which would leave the raw
/// `=?utf-8?Q?…?=` gibberish in the UI; we ask it to decode them anyway, as
/// Apple Mail and Thunderbird do.
pub(crate) fn decode_header(raw: &[u8]) -> String {
    use rfc2047_decoder::{Decoder, RecoverStrategy};
    match Decoder::new()
        .too_long_encoded_word_strategy(RecoverStrategy::Decode)
        .decode(raw)
    {
        Ok(s) => s,
        Err(_) => String::from_utf8_lossy(raw).into_owned(),
    }
}

/// Compact date label from a unix timestamp (same style as [`format_date`]).
fn format_timestamp(ts: i64) -> String {
    use chrono::{Datelike, TimeZone};
    if ts <= 0 {
        return String::new();
    }
    let Some(local) = chrono::Local.timestamp_opt(ts, 0).single() else {
        return String::new();
    };
    let now = chrono::Local::now();
    if local.date_naive() == now.date_naive() {
        local.format("%-I:%M %p").to_string()
    } else if local.year() == now.year() {
        local.format("%b %-d").to_string()
    } else {
        local.format("%b %-d, %Y").to_string()
    }
}

/// Fall back to the server's `INTERNALDATE` (the delivery date) when a message
/// carries no parseable `Date:` header. Some senders omit `Date:` entirely,
/// which would otherwise leave the row with a blank date label and a zero sort
/// timestamp (sinking it to the bottom of the list). Returns `("", 0)` if the
/// server didn't supply an INTERNALDATE either.
fn internal_date_summary(fetch: &Fetch) -> (String, i64) {
    match fetch.internal_date() {
        Some(dt) => {
            let ts = dt.timestamp();
            (format_timestamp(ts), ts)
        }
        None => (String::new(), 0),
    }
}

/// Parse an RFC 2822 date into a compact label and a sortable unix timestamp.
fn format_date(raw: &[u8]) -> (String, i64) {
    let s = String::from_utf8_lossy(raw);
    let s = s.trim();
    let Ok(dt) = chrono::DateTime::parse_from_rfc2822(s) else {
        return (s.to_string(), 0);
    };
    let now = chrono::Local::now();
    let local = dt.with_timezone(&chrono::Local);
    let label = if local.date_naive() == now.date_naive() {
        local.format("%-I:%M %p").to_string()
    } else if local.year() == now.year() {
        local.format("%b %-d").to_string()
    } else {
        local.format("%b %-d, %Y").to_string()
    };
    (label, dt.timestamp())
}

/// Extract a renderable HTML body from a raw RFC 822 message. A lone HTML part is
/// used directly; anything else is composed part by part.
///
/// A message can carry more than one display body. Apple Mail (iPhone) sends photo
/// mail as a multipart/mixed that interleaves text parts with the images, so taking
/// only the first body — as `body_html(0)` does — renders the message blank. Walk
/// every display part in order instead, embedding inline images as `data:` URIs:
/// the reader's CSP permits those even while remote content is blocked, and the
/// bytes arrived with the message, so nothing is fetched from the network.
fn extract_body(raw: &[u8]) -> String {
    use mail_parser::{MessageParser, PartType};

    /// Total decoded bytes of inline images embedded into one message body.
    const INLINE_IMAGE_BUDGET: usize = 16 * 1024 * 1024;

    let Some(parsed) = MessageParser::default().parse(raw) else {
        return wrap_plain(&String::from_utf8_lossy(raw));
    };

    let bodies: Vec<_> = parsed.html_bodies().collect();

    // A single HTML part is already a complete document — pass it through untouched
    // so the sender's own layout and styling survive.
    if let [only] = bodies.as_slice() {
        if let PartType::Html(html) = &only.body {
            return html.to_string();
        }
    }

    let mut inner = String::new();
    let mut budget = INLINE_IMAGE_BUDGET;
    for part in &bodies {
        match &part.body {
            PartType::Html(html) => inner.push_str(html),
            PartType::Text(text) if !text.trim().is_empty() => {
                inner.push_str("<div class=\"vireo-plain\">");
                inner.push_str(&linkify(text));
                inner.push_str("</div>");
            }
            PartType::Binary(bytes) | PartType::InlineBinary(bytes) => {
                // Embedding inflates by ~4/3 and the result is cached on disk, so
                // stop inlining past a budget. Oversized images are still listed
                // as attachments and can be opened from there.
                if let Some(mime) = image_mime(part) {
                    if let Some(left) = budget.checked_sub(bytes.len()) {
                        budget = left;
                        inner.push_str(&format!(
                            "<img class=\"vireo-inline\" src=\"data:{mime};base64,{}\">",
                            crate::oauth::base64_encode(bytes)
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    if inner.trim().is_empty() {
        return match parsed.body_text(0) {
            Some(text) if !text.trim().is_empty() => wrap_plain(&text),
            _ => wrap_plain("(no readable content)"),
        };
    }
    wrap_fragment(&inner)
}

/// The `image/<subtype>` MIME type of a part, if it is an image we can inline.
/// The subtype is validated so it can't break out of the `data:` URI.
fn image_mime(part: &mail_parser::MessagePart) -> Option<String> {
    use mail_parser::MimeHeaders;
    let ty = part.content_type()?;
    if !ty.ctype().eq_ignore_ascii_case("image") {
        return None;
    }
    let subtype = ty.subtype()?;
    let safe = subtype
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    safe.then(|| format!("image/{}", subtype.to_ascii_lowercase()))
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escape a string for use inside a double-quoted HTML attribute (e.g. `href`).
fn escape_attr(text: &str) -> String {
    escape_html(text).replace('"', "&quot;")
}

/// HTML-escape plain text and turn bare URLs into clickable links. Runs on raw
/// (unescaped) text: non-URL spans are escaped as usual; each URL becomes an
/// `<a href>` whose scheme is always http(s) (a bare `www.` host is prefixed with
/// `https://`), so no `javascript:`-style link can be forged. Links open in the
/// external browser via the reader's navigation policy.
fn linkify(text: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    while let Some((start, end, href)) = next_url(text, i) {
        out.push_str(&escape_html(&text[i..start]));
        out.push_str(&format!(
            "<a href=\"{}\">{}</a>",
            escape_attr(&href),
            escape_html(&text[start..end])
        ));
        i = end;
    }
    out.push_str(&escape_html(&text[i..]));
    out
}

/// Find the next bare URL at or after `from`, returning `(start, end, href)`.
fn next_url(text: &str, from: usize) -> Option<(usize, usize, String)> {
    let mut i = from;
    while i < text.len() {
        if !text.is_char_boundary(i) {
            i += 1;
            continue;
        }
        let rest = &text[i..];
        let (prefix, add_https) = if rest.starts_with("https://") {
            ("https://", false)
        } else if rest.starts_with("http://") {
            ("http://", false)
        } else if rest.starts_with("www.") {
            ("www.", true)
        } else {
            i += 1;
            continue;
        };
        // Only match at a boundary — the start, after whitespace, or after an
        // opening bracket/quote — so "shttp://", "awww." and "hi@www.x" (an email)
        // aren't linked.
        let boundary = text[..i]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '<' | '[' | '{' | '"' | '\'' | '|'));
        let end = consume_url(text, i);
        // Reject a scheme with nothing (usable) after it.
        if boundary && end > i + prefix.len() {
            let url = &text[i..end];
            let href = if add_https { format!("https://{url}") } else { url.to_string() };
            return Some((i, end, href));
        }
        i += prefix.len();
    }
    None
}

/// The end index of the URL that begins at `start`: consume non-terminator chars,
/// then trim trailing sentence punctuation (keeping a `)` that balances a `(`).
fn consume_url(text: &str, start: usize) -> usize {
    let mut end = start;
    for (off, ch) in text[start..].char_indices() {
        if ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '`' | '\'' | '\\') {
            break;
        }
        end = start + off + ch.len_utf8();
    }
    let url = &text[start..end];
    let mut trimmed = url;
    loop {
        match trimmed.chars().last() {
            Some(')') => {
                if trimmed.matches(')').count() > trimmed.matches('(').count() {
                    trimmed = &trimmed[..trimmed.len() - 1];
                } else {
                    break; // balanced — part of the URL (e.g. a Wikipedia link)
                }
            }
            Some('.' | ',' | ';' | ':' | '!' | '?' | ']' | '}') => {
                trimmed = &trimmed[..trimmed.len() - 1];
            }
            _ => break,
        }
    }
    start + trimmed.len()
}

/// Wrap plain text in a minimal, readable HTML document. Colours are left to the
/// `color-scheme` the reader injects, so the message follows the light/dark theme.
fn wrap_plain(text: &str) -> String {
    let escaped = linkify(text);
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><style>\
         body{{margin:0;padding:16px;box-sizing:border-box;\
         font:14px/1.5 system-ui,sans-serif;\
         white-space:pre-wrap;word-wrap:break-word}}\
         </style></head><body>{escaped}</body></html>"
    )
}

/// Wrap composed body fragments (text blocks and inline images) in a document.
fn wrap_fragment(inner: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><style>\
         body{{margin:0;padding:16px;box-sizing:border-box;\
         font:14px/1.5 system-ui,sans-serif}}\
         .vireo-plain{{white-space:pre-wrap;word-wrap:break-word}}\
         .vireo-inline{{display:block;max-width:100%;height:auto;\
         margin:12px 0;border-radius:6px}}\
         </style></head><body>{inner}</body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_header_handles_over_long_encoded_word() {
        // The Marginalian (Mailchimp) sends the entire subject as one Q-encoded
        // word ~250 chars long — far past RFC 2047's 75-char limit. Earlier
        // builds aborted and left the raw `=?utf-8?Q?…?=` in the UI; we now
        // decode it as Apple Mail and Thunderbird do.
        let raw = b"=?utf-8?Q?92=2Dyear=2Dold=20artist=20Sheila=20Hicks=20on=20the=20key=20to=20creative=20vitality=2C=20how=20to=20manage=20heartbreak=20like=20Frida=20Kahlo=2C=20the=20elusive=20science=20of=20the=20present=20moment?=";
        assert_eq!(
            decode_header(raw),
            "92-year-old artist Sheila Hicks on the key to creative vitality, \
             how to manage heartbreak like Frida Kahlo, the elusive science of \
             the present moment"
        );
    }

    #[test]
    fn decode_header_leaves_plain_text_untouched() {
        assert_eq!(decode_header(b"Just a normal subject"), "Just a normal subject");
    }

    #[test]
    fn linkify_wraps_http_and_https() {
        assert_eq!(
            linkify("see http://example.com now"),
            "see <a href=\"http://example.com\">http://example.com</a> now"
        );
        assert_eq!(
            linkify("https://a.b/c?d=1"),
            "<a href=\"https://a.b/c?d=1\">https://a.b/c?d=1</a>"
        );
    }

    #[test]
    fn linkify_prefixes_bare_www_with_https() {
        assert_eq!(
            linkify("go to www.example.com today"),
            "go to <a href=\"https://www.example.com\">www.example.com</a> today"
        );
    }

    #[test]
    fn linkify_escapes_and_does_not_double_link() {
        // The surrounding <> are escaped; the URL inside is linked once.
        assert_eq!(
            linkify("<http://x.com>"),
            "&lt;<a href=\"http://x.com\">http://x.com</a>&gt;"
        );
        // A query '&' is escaped in both href and text.
        assert_eq!(
            linkify("http://x.com/?a=1&b=2"),
            "<a href=\"http://x.com/?a=1&amp;b=2\">http://x.com/?a=1&amp;b=2</a>"
        );
    }

    #[test]
    fn linkify_trims_trailing_punctuation_but_keeps_balanced_parens() {
        assert_eq!(
            linkify("visit http://x.com."),
            "visit <a href=\"http://x.com\">http://x.com</a>."
        );
        assert_eq!(
            linkify("(see http://x.com)"),
            "(see <a href=\"http://x.com\">http://x.com</a>)"
        );
        // A balanced ')' belongs to the URL (e.g. a Wikipedia article).
        assert_eq!(
            linkify("http://en.wikipedia.org/wiki/Foo_(bar)"),
            "<a href=\"http://en.wikipedia.org/wiki/Foo_(bar)\">http://en.wikipedia.org/wiki/Foo_(bar)</a>"
        );
    }

    #[test]
    fn linkify_ignores_scheme_inside_a_word() {
        // No word boundary before "http", so it's not a link (just escaped text).
        assert_eq!(linkify("shttp://x.com"), "shttp://x.com");
        assert!(!linkify("email hi@www.x").contains("<a "));
    }

    #[test]
    fn linkify_never_forges_a_dangerous_scheme() {
        // "javascript:" isn't one of our recognized prefixes, so it stays plain text.
        let out = linkify("javascript:alert(1)");
        assert!(!out.contains("<a "), "got: {out}");
    }

    #[test]
    fn extract_body_linkifies_plain_text_mail() {
        let raw = b"Content-Type: text/plain\r\n\r\nRead http://example.com/x for more.";
        let body = extract_body(raw);
        assert!(
            body.contains("<a href=\"http://example.com/x\">http://example.com/x</a>"),
            "body was: {body}"
        );
    }

    /// An iPhone photo mail: multipart/mixed interleaving an empty text part, an
    /// *inline* JPEG, and the signature. Exactly the shape Apple Mail produces.
    const IPHONE_PHOTO: &str = concat!(
        "Content-Type: multipart/mixed; boundary=Apple-Mail-32B9517E\r\n",
        "Content-Transfer-Encoding: 7bit\r\n",
        "From: Alex Doe <alex@example.com>\r\n",
        "Subject: Panda\r\n",
        "X-Mailer: iPhone Mail (23F84)\r\n",
        "\r\n",
        "--Apple-Mail-32B9517E\r\n",
        "Content-Type: text/plain;\r\n\tcharset=us-ascii\r\n",
        "Content-Transfer-Encoding: 7bit\r\n\r\n\r\n\r\n",
        "--Apple-Mail-32B9517E\r\n",
        "Content-Type: image/jpeg;\r\n\tname=C21AA3E8.jpeg;\r\n",
        "\tx-apple-part-url=4F5E768A\r\n",
        "Content-Disposition: inline;\r\n\tfilename=C21AA3E8.jpeg\r\n",
        "Content-Transfer-Encoding: base64\r\n\r\n",
        "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAgGBgcGBQgHBwcJCQgKDBQNDAsLDBk=\r\n",
        "\r\n",
        "--Apple-Mail-32B9517E\r\n",
        "Content-Type: text/plain;\r\n\tcharset=us-ascii\r\n",
        "Content-Transfer-Encoding: 7bit\r\n\r\n",
        "\r\n\r\nAlex Doe\r\nSent from my iPhone\r\n",
        "--Apple-Mail-32B9517E--\r\n",
    );

    /// A newsletter: HTML body plus a `cid:` logo referenced from it.
    const CID_NEWSLETTER: &str = concat!(
        "Content-Type: multipart/related; boundary=R\r\n",
        "Subject: News\r\n\r\n",
        "--R\r\n",
        "Content-Type: text/html; charset=utf-8\r\n\r\n",
        "<p>hi <img src=\"cid:logo\"></p>\r\n",
        "--R\r\n",
        "Content-Type: image/png; name=logo.png\r\n",
        "Content-ID: <logo>\r\n",
        "Content-Disposition: inline; filename=logo.png\r\n",
        "Content-Transfer-Encoding: base64\r\n\r\n",
        "iVBORw0KGgo=\r\n",
        "--R--\r\n",
    );

    #[test]
    fn iphone_photo_body_keeps_text_and_inlines_the_image() {
        let body = extract_body(IPHONE_PHOTO.as_bytes());
        // The signature lives in the *third* part; taking only the first left the
        // message blank.
        assert!(body.contains("Sent from my iPhone"), "body was: {body}");
        // The photo is embedded, not dropped or fetched from the network.
        assert!(body.contains("src=\"data:image/jpeg;base64,/9j/4AAQ"), "body was: {body}");
    }

    #[test]
    fn iphone_photo_is_listed_as_an_attachment() {
        let found = extract_attachments(IPHONE_PHOTO.as_bytes());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "C21AA3E8.jpeg");
        assert!(!found[0].data.is_empty());
    }

    #[test]
    fn cid_resources_are_not_listed_as_attachments() {
        assert!(extract_attachments(CID_NEWSLETTER.as_bytes()).is_empty());
    }

    #[test]
    fn lone_html_part_passes_through_untouched() {
        let raw = b"Content-Type: text/html\r\n\r\n<p>hello</p>";
        assert_eq!(extract_body(raw), "<p>hello</p>");
    }

    #[test]
    fn plain_text_only_message_still_renders() {
        let raw = b"Content-Type: text/plain\r\n\r\nhello <there>";
        let body = extract_body(raw);
        assert!(body.contains("hello &lt;there&gt;"), "body was: {body}");
    }

    /// Parse a real server BODYSTRUCTURE response into the structure our IMAP
    /// summary path sees.
    fn bodystructure(raw: &str) -> async_imap::imap_proto::types::BodyStructure<'_> {
        use async_imap::imap_proto::{parser::parse_response, AttributeValue, Response};
        let (_, resp) = parse_response(raw.as_bytes()).expect("parses");
        match resp {
            Response::Fetch(_, attrs) => attrs
                .into_iter()
                .find_map(|a| match a {
                    AttributeValue::BodyStructure(bs) => Some(bs),
                    _ => None,
                })
                .expect("has BODYSTRUCTURE"),
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn inline_iphone_photo_counts_as_an_attachment() {
        // Apple marks the JPEG `INLINE` with no Content-ID; requiring a disposition
        // of `attachment` missed it, so no paperclip and no download.
        let bs = bodystructure(concat!(
            "* 1 FETCH (BODYSTRUCTURE (",
            "(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"us-ascii\") NIL NIL \"7BIT\" 4 2 NIL NIL NIL NIL)",
            "(\"IMAGE\" \"JPEG\" (\"NAME\" \"a.jpeg\") NIL NIL \"BASE64\" 100 NIL ",
            "(\"INLINE\" (\"FILENAME\" \"a.jpeg\")) NIL NIL)",
            "(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"us-ascii\") NIL NIL \"7BIT\" 40 4 NIL NIL NIL NIL)",
            " \"MIXED\" (\"BOUNDARY\" \"b\") NIL NIL NIL))\r\n",
        ));
        assert!(structure_has_attachment(&bs));
    }

    #[test]
    fn cid_logo_does_not_count_as_an_attachment() {
        // Same inline disposition, but a Content-ID: it's rendered in the body.
        let bs = bodystructure(concat!(
            "* 1 FETCH (BODYSTRUCTURE (",
            "(\"TEXT\" \"HTML\" (\"CHARSET\" \"utf-8\") NIL NIL \"7BIT\" 20 1 NIL NIL NIL NIL)",
            "(\"IMAGE\" \"PNG\" (\"NAME\" \"logo.png\") \"<logo>\" NIL \"BASE64\" 100 NIL ",
            "(\"INLINE\" (\"FILENAME\" \"logo.png\")) NIL NIL)",
            " \"RELATED\" (\"BOUNDARY\" \"r\") NIL NIL NIL))\r\n",
        ));
        assert!(!structure_has_attachment(&bs));
    }

    #[test]
    fn plain_alternative_has_no_attachment() {
        let bs = bodystructure(concat!(
            "* 1 FETCH (BODYSTRUCTURE (",
            "(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"utf-8\") NIL NIL \"7BIT\" 10 1 NIL NIL NIL NIL)",
            "(\"TEXT\" \"HTML\" (\"CHARSET\" \"utf-8\") NIL NIL \"7BIT\" 20 1 NIL NIL NIL NIL)",
            " \"ALTERNATIVE\" (\"BOUNDARY\" \"a\") NIL NIL NIL))\r\n",
        ));
        assert!(!structure_has_attachment(&bs));
    }

    #[test]
    fn image_mime_rejects_a_hostile_subtype() {
        // Guards the `data:` URI against a subtype that would break out of it.
        let raw = b"Content-Type: image/\"onerror=alert(1)\r\n\r\nx";
        let parsed = mail_parser::MessageParser::default().parse(raw.as_slice()).unwrap();
        let part = parsed.html_bodies().next().unwrap();
        assert!(image_mime(part).is_none());
    }
}
