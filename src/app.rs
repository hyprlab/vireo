//! Root component: the application window, three-pane adaptive layout, and the
//! routing between the sidebar, list, reader, and the per-account mail workers.

use std::collections::{HashMap, HashSet};

use adw::prelude::*;
use relm4::actions::{RelmAction, RelmActionGroup};
use relm4::prelude::*;
use tokio::sync::mpsc::UnboundedSender;

/// Width of the collapsed, icon-only sidebar rail.
const SIDEBAR_RAIL_WIDTH: f64 = 80.0;

/// Selection size at/above which a bulk archive/delete/spam shows a spinner and
/// applies deferred (smaller batches are fast enough to run inline).
const BULK_SPINNER_MIN: usize = 25;

relm4::new_action_group!(WindowActionGroup, "win");
relm4::new_stateless_action!(AccountsAction, WindowActionGroup, "accounts");
relm4::new_stateless_action!(PreferencesAction, WindowActionGroup, "preferences");
relm4::new_stateless_action!(AboutAction, WindowActionGroup, "about");

use crate::config::{self, AccountConfig};
use crate::models::{Account, Attachment, Folder, FolderKind, Message};
use crate::ui::accounts::{AccountsOutput, AccountsWindow};
use crate::ui::compose::{
    Compose, ComposeAccount, ComposeInit, ComposeInput, ComposeOutput, ComposePrefill,
};
use crate::ui::message_list::{
    BulkAction, MessageList, MessageListInput, MessageListOutput, RowAction,
};
use crate::ui::attachments_gallery::{
    AttachmentsGallery, GalleryInput, GalleryOutput,
};
use crate::ui::attachment_drawer::{AttachmentDrawer, AttachmentDrawerInput};
use crate::ui::message_view::{MessageView, MessageViewInput, MessageViewOutput};
use crate::ui::message_window::{
    MessageWindow, MessageWindowInit, MessageWindowInput, MessageWindowOutput,
};
use crate::ui::notifications::{NotificationCenter, NotifyInput, NotifyOutput};
use crate::ui::preferences::{PrefInit, PrefOutput, Preferences};
use crate::ui::sidebar::{CtxAction, SectionData, Sidebar, SidebarInput, SidebarOutput};
use crate::worker::{self, MailRequest, OutgoingMessage, WorkerEvent};

/// The currently selected mailbox.
#[derive(Clone)]
struct SelectedFolder {
    account_id: u32,
    folder_id: u32,
    name: String,
    path: String,
}

/// A standalone compose window (New Message, compose-to, edit-draft, or a
/// popped-out reply) and its component. Both refs must stay alive: the window
/// holds the content, the controller holds the component root.
struct ComposeHost {
    id: u32,
    /// Held only to keep the component (and its widget tree) alive for the
    /// window's lifetime; dropped when the host is removed.
    #[allow(dead_code)]
    controller: Controller<Compose>,
    window: adw::Window,
}

/// The reader's inline reply/forward composer. `window` is `Some` only while the
/// pane has been promoted to a floating window (else it lives in the reader's
/// drop-down revealer).
struct ReaderCompose {
    id: u32,
    controller: Controller<Compose>,
    window: Option<adw::Window>,
}

pub struct AppModel {
    /// One mail worker per account (account_id → request sender).
    workers: HashMap<u32, UnboundedSender<MailRequest>>,
    config: Vec<AccountConfig>,
    window: adw::ApplicationWindow,
    prefs: Option<Controller<Preferences>>,
    accounts_win: Option<Controller<AccountsWindow>>,
    /// Standalone compose windows (multiple allowed at once). Pruned as they close.
    composers: Vec<ComposeHost>,
    /// The reader's inline reply/forward composer, if open.
    reader_compose: Option<ReaderCompose>,
    /// Superseded inline composers still finishing a save-if-dirty before closing.
    draining_composers: Vec<(u32, Controller<Compose>)>,
    /// SlideDown revealer under the reader toolbar that hosts the inline pane.
    reader_compose_revealer: gtk::Revealer,
    /// Monotonic id source for composers.
    next_compose_id: u32,
    menu: gtk::gio::Menu,
    /// All known accounts, ordered by id.
    accounts: Vec<Account>,
    /// account_id → that account's folders.
    folders: HashMap<u32, Vec<Folder>>,
    /// Preferred sidebar account order (by email).
    account_order: Vec<String>,
    /// Accounts whose folder list is collapsed in the sidebar (by email).
    collapsed: Vec<String>,
    /// Accounts whose custom-folders section is expanded in the sidebar (by email).
    folders_expanded: Vec<String>,
    selected: Option<SelectedFolder>,
    /// Attachments of the currently-open message (for the reader toolbar button).
    attachments: Vec<Attachment>,
    /// True while the current message's attachments are downloading.
    attachments_loading: bool,
    /// The current message has attachments that aren't downloaded yet; offer a
    /// "Load attachments" button instead of fetching automatically.
    attachments_available: bool,
    /// Cache of fetched attachments, keyed by (account_id, message_id), so
    /// revisiting a message doesn't re-download them.
    attachment_cache: HashMap<(u32, u32), Vec<Attachment>>,
    /// Popover content box for the attachments button.
    attach_list: gtk::Box,
    /// True when the unified "All Inboxes" view is active (no single folder).
    unified: bool,
    /// account_id → that account's latest inbox messages (for the unified view).
    unified_by_account: HashMap<u32, Vec<Message>>,
    /// (account_id, folder_id) → last-seen message list, shown instantly on
    /// revisit while a fresh sync runs in the background.
    message_cache: HashMap<(u32, u32), Vec<Message>>,
    /// (account_id, folder_id) whose background backfill has fully finished, so the
    /// message list knows no more rows will stream in for them.
    indexed_folders: HashSet<(u32, u32)>,
    /// (account_id, message_id) → fetched body, so reopening a message renders
    /// instantly with no loading spinner.
    body_cache: HashMap<(u32, u32), String>,
    /// (account_id, folder_id) → server-side unread count, accurate beyond the
    /// loaded window (from IMAP STATUS/SEARCH). Drives the sidebar badges.
    folder_unread: HashMap<(u32, u32), u32>,
    /// The account-list split view, narrowed to icon-only width when collapsed.
    sidebar_split: Option<adw::OverlaySplitView>,
    /// The "Vireo" title label, hidden while the sidebar is collapsed.
    app_title: Option<gtk::Label>,
    /// Sidebar header. In the icon-only rail its window-control buttons are
    /// hidden so the header stops forcing a minimum width wider than the rail.
    sidebar_header: Option<adw::HeaderBar>,
    /// Whether the sidebar is in icon-only (collapsed) mode.
    sidebar_collapsed: bool,
    /// Held so the in-flight collapse/expand width animation isn't dropped.
    sidebar_anim: Option<adw::TimedAnimation>,
    current: Option<Message>,
    /// Sender addresses allowed to auto-load remote content (lowercased).
    allowed_senders: Vec<String>,
    /// Addresses/domains whose incoming inbox mail is auto-deleted (lowercased).
    blacklist: Vec<String>,
    /// Seconds the message-list Actions Palette stays open after the cursor leaves.
    palette_collapse_secs: u64,
    /// Whether to load sender avatars from Gravatar.
    gravatar: bool,
    /// Seconds between automatic mail checks (0 = manual only).
    fetch_interval_secs: u64,
    /// Whether IMAP IDLE push is enabled.
    push: bool,
    /// Whether desktop notifications (new mail, error alerts) are posted.
    notifications_enabled: bool,
    /// Whether messages are grouped into conversation threads.
    threading: bool,
    /// Whether conversation threads start expanded in the message list.
    threads_expanded: bool,
    /// How email content is themed (message content only, not the app UI).
    message_theme: config::MessageTheme,
    /// The repeating auto-fetch timer, if armed.
    auto_fetch_source: Option<gtk::glib::SourceId>,
    notifications: Controller<NotificationCenter>,
    notify_count: usize,
    /// Accounts currently performing network activity (drives the spinner).
    busy: HashSet<u32>,
    sidebar: Controller<Sidebar>,
    message_list: Controller<MessageList>,
    message_view: Controller<MessageView>,
    /// In-message attachment thumbnail drawer, docked below the reader body.
    attachment_drawer: Controller<AttachmentDrawer>,
    gallery: Controller<AttachmentsGallery>,
    /// True when the attachments gallery replaces the mail panes.
    showing_gallery: bool,
    /// Gallery items per account inbox, merged for display.
    gallery_by_account: HashMap<u32, Vec<crate::models::GalleryItem>>,
    /// Messages popped out into their own windows, keyed by (account, message).
    popouts: HashMap<(u32, u32), PopOut>,
    /// The conversation currently shown in the reader (newest first), with bodies
    /// filled in as they arrive. More than one entry = conversation/thread mode.
    current_thread: Vec<Message>,
    /// A draft awaiting its body before opening in the compose editor.
    pending_draft: Option<Message>,
    /// Outstanding bulk MoveMessages requests awaiting a worker `BulkComplete`.
    /// While > 0 (and a large selection triggered it) the list shows a spinner.
    bulk_pending: usize,
    /// A large bulk archive/delete/spam deferred one tick so its spinner paints
    /// before the (blocking) apply runs.
    pending_bulk: Option<(BulkAction, Vec<Message>)>,
}

/// A message displayed in its own top-level window (double-click to pop out).
struct PopOut {
    window: adw::Window,
    controller: Controller<MessageWindow>,
}

#[derive(Debug)]
pub enum AppMsg {
    // User actions
    UnifiedSelected,
    /// Show the attachments gallery (sidebar "Attachments" row).
    ShowAttachments,
    /// Cached gallery attachments for an account inbox arrived.
    GalleryItems { account_id: u32, items: Vec<crate::models::GalleryItem> },
    /// Gallery "Go to Message" — open the attachment's source message.
    OpenAttachmentMessage { account_id: u32, folder_path: String, uid: u32 },
    FolderSelected { account_id: u32, folder_id: u32, name: String, path: String },
    ToggleCollapse(u32),
    ToggleCustomFolders(u32),
    SidebarCollapsed(bool),
    SidebarContext(CtxAction),
    /// A message was dropped on a sidebar folder — move it there.
    DropMoveMessage { account_id: u32, folder_id: u32, uid: u32, id: u32, dest: String },
    /// Create a custom folder under an account (from the right-click menu).
    CreateFolder { account_id: u32, name: String },
    /// Delete a custom folder (its contents are moved to Trash first).
    DeleteFolder { account_id: u32, path: String },
    AccountsReordered(Vec<String>),
    MessageSelected { message: Message, thread: Vec<Message> },
    /// A new-mail desktop notification was clicked — open that message.
    OpenMessageFromNotification { account_id: u32, folder_id: u32, message_id: u32 },
    /// The search field became active/inactive — supply or drop the cross-folder
    /// search pool (every folder's messages, so search can span the mailbox).
    SearchActive(bool),
    /// The message list has no selection to show (e.g. the last message was
    /// removed), so the reader should clear.
    ClearReader,
    /// Double-click: open the message in its own standalone window.
    OpenMessageWindow(Message),
    /// A popped-out message window was closed (remove it from the map).
    PopoutClosed((u32, u32)),
    /// Add a contact from a popout window's sender.
    AddContactFrom { name: String, email: String },
    /// Download a specific message's attachments (from a popout window).
    LoadAttachmentsFor(Box<Message>),
    /// Open a single attachment delivered from a popout window.
    OpenAttachmentItem(Box<Attachment>),
    /// Save attachments delivered from a popout window.
    SaveAttachmentItems(Vec<Attachment>),
    ToggleStar,
    Archive,
    Delete,
    RowAction { action: RowAction, message: Box<Message> },
    /// A bulk action applied to every selected message.
    Bulk { action: BulkAction, messages: Vec<Message> },
    /// Apply the deferred large bulk action (runs after its spinner has painted).
    BulkApply,
    /// A worker finished one bulk MoveMessages request; clears the spinner once
    /// all outstanding bulk moves are done.
    BulkComplete,
    Compose,
    OpenAbout,
    AllowSender(String),
    AddSender(String),
    RemoveSender(String),
    AddBlacklist(String),
    RemoveBlacklist(String),
    MarkSpam,
    SetGravatar(bool),
    ContactPhotosChanged,
    SetThreading(bool),
    SetThreadsExpanded(bool),
    SetFetchInterval(u64),
    SetPush(bool),
    SetNotifications(bool),
    SetPaletteCollapse(u64),
    SetMessageTheme(config::MessageTheme),
    ComposeTo(String),
    Reply,
    ReplyAll,
    Forward,
    AddToContacts,
    ContactAdded(Result<crate::contacts::AddOutcome, String>),
    ViewSource,
    OpenAttachment(usize),
    SaveAllAttachments,
    /// User clicked "Load attachments" for a message whose attachments weren't
    /// pre-downloaded — fetch them from the server now.
    LoadAttachmentsNow,
    SendMessage(Box<OutgoingMessage>),
    SaveDraftMessage(Box<OutgoingMessage>),
    DraftSaved,
    /// A composer (id) finished — tear down its host (window or inline revealer).
    ComposeClosed(u32),
    /// Promote/demote the reader's inline composer (id) between inline and window.
    ComposeToggleWindow(u32),
    Refresh,
    OpenAccounts,
    /// Open the accounts window straight to the "add account" form (empty state).
    AddFirstAccount,
    AccountSaved { original_email: Option<String>, account: Box<AccountConfig> },
    /// Show the keyring / Secret Service setup help. `problem: true` when a save
    /// actually failed to persist; `false` for the proactive one-time tip.
    ShowKeyringHelp { problem: bool },
    AccountRemoved { email: String },
    AccountEnabledChanged { email: String, enabled: bool },
    ImportGoaAccount(Box<AccountConfig>),
    /// GNOME Online Accounts changed on the session bus — re-reconcile and drop
    /// any imported account whose GOA account was removed.
    GoaChanged,
    /// The system resumed from sleep — worker IMAP sockets are stale, so
    /// reconnect every account and reload the visible folder.
    SystemResumed,
    CloseAccounts,
    OpenPreferences,
    ClosePreferences,
    // Worker events (each carries the account it came from)
    SetAccount(Account),
    SetFolders { account_id: u32, folders: Vec<Folder> },
    Messages { account_id: u32, folder_id: u32, messages: Vec<Message> },
    /// Additional indexed summaries from the background backfill (search index).
    MessagesAppend { account_id: u32, folder_id: u32, messages: Vec<Message> },
    /// A folder's background backfill finished — it's fully indexed now.
    BackfillDone { account_id: u32, folder_id: u32 },
    FolderUnread { account_id: u32, folder_id: u32, unread: u32 },
    Body { account_id: u32, message_id: u32, body: String },
    Source { text: String },
    Attachments { account_id: u32, message_id: u32, items: Vec<Attachment> },
    AttachmentsPending { account_id: u32, message_id: u32 },
    /// A flagged message turned out to have no real attachments — drop its paperclip.
    NoAttachments { account_id: u32, message_id: u32 },
    Sent { account_id: u32 },
    Status { account_id: u32, text: String },
    Error { account_id: u32, text: String, connectivity: bool },
    NotifyCount(usize),
    ToggleNotifications,
    OpenContacts,
}

#[relm4::component(pub)]
impl SimpleComponent for AppModel {
    type Init = ();
    type Input = AppMsg;
    type Output = ();

    view! {
        adw::ApplicationWindow {
            set_title: Some("Vireo"),
            set_icon_name: Some(crate::APP_ID),
            add_css_class: "vireo",

            // Persist the window size + maximized state on close. (Position and
            // which monitor can't be restored on Wayland — the compositor owns
            // placement — so only the geometry is saved.)
            connect_close_request => move |w| {
                let maximized = w.is_maximized();
                let (width, height) = if maximized {
                    let (sw, sh, _) = crate::config::load_window_state();
                    (sw, sh)
                } else {
                    (w.width(), w.height())
                };
                crate::config::save_window_state(width, height, maximized);
                // Exit cleanly the moment window state is saved. Letting GTK,
                // WebKit and the per-account worker threads tear down the normal
                // way can abort — a Rust panic fired from a GObject dispose
                // callback becomes SIGABRT, which the Flatpak surfaces as a crash
                // notification. Nothing else needs persisting on quit: accounts
                // and settings are written as they change.
                std::process::exit(0)
            },

            #[wrap(Some)]
            set_content = &gtk::Box {
                set_orientation: gtk::Orientation::Vertical,

                append: model.notifications.widget(),

                #[name = "sidebar_split"]
                adw::OverlaySplitView {
                    set_vexpand: true,
                    set_max_sidebar_width: 280.0,
                    set_min_sidebar_width: 220.0,
                    set_sidebar_width_fraction: 0.2,

                    #[wrap(Some)]
                    set_sidebar = &adw::ToolbarView {
                        #[name = "sidebar_header"]
                        add_top_bar = &adw::HeaderBar {
                            add_css_class: "flat",
                            #[wrap(Some)]
                            #[name = "app_title"]
                            set_title_widget = &gtk::Label {
                                set_label: "Vireo",
                                add_css_class: "app-title",
                            },
                            pack_start = &gtk::Button {
                                set_icon_name: "co.hyprlab.Vireo-mail-message-new-symbolic",
                                set_tooltip_text: Some("Compose"),
                                add_css_class: "suggested-action",
                                connect_clicked[sender] => move |_| sender.input(AppMsg::Compose),
                            },
                            pack_end = &gtk::MenuButton {
                                set_icon_name: "co.hyprlab.Vireo-open-menu-symbolic",
                                set_tooltip_text: Some("Main Menu"),
                                add_css_class: "flat",
                                set_menu_model: Some(&model.menu),
                            },
                        },
                        #[wrap(Some)]
                        set_content = model.sidebar.widget(),
                    },

                    #[wrap(Some)]
                    #[name = "content_stack"]
                    set_content = &gtk::Stack {
                        set_transition_type: gtk::StackTransitionType::Crossfade,
                        // Swap the mail panes for the attachments gallery.
                        #[watch]
                        set_visible_child_name: if model.showing_gallery { "gallery" } else { "mail" },

                    add_named[Some("mail")] = &gtk::Paned {
                        set_orientation: gtk::Orientation::Horizontal,
                        // Thin handle so the panes sit flush (just a 1px divider),
                        // no wide-handle gap between them.
                        set_wide_handle: false,
                        // Launch at the list's minimum width. `shrink_start_child`
                        // is false, so GtkPaned clamps this up to the start child's
                        // natural minimum — exactly wide enough for a row's Actions
                        // Palette to fit — instead of a hardcoded, slightly-too-wide
                        // value. The reader (end child) absorbs the remaining width.
                        set_position: 1,
                        // The list keeps its width as the window resizes (the
                        // reader absorbs the change), and can't be dragged
                        // narrower than its natural minimum — which is exactly the
                        // width needed to show the hover control palette. The reader
                        // may shrink below its natural minimum, though, so the whole
                        // window can be tiled to a screen half (GNOME edge-snapping).
                        set_resize_start_child: false,
                        set_shrink_start_child: false,
                        set_resize_end_child: true,
                        set_shrink_end_child: true,

                        #[wrap(Some)]
                        set_start_child = &adw::ToolbarView {
                            add_top_bar = &adw::HeaderBar {
                                add_css_class: "flat",
                                // Middle pane: no window controls (the reader pane's
                                // header carries the window's close button).
                                set_show_start_title_buttons: false,
                                set_show_end_title_buttons: false,
                                #[wrap(Some)]
                                set_title_widget = &gtk::Label {
                                    #[watch]
                                    set_label: model.pane_title(),
                                    add_css_class: "pane-title",
                                },
                                pack_start = &gtk::Button {
                                    set_tooltip_text: Some("Notifications"),
                                    add_css_class: "flat",
                                    connect_clicked[sender] => move |_| sender.input(AppMsg::ToggleNotifications),
                                    gtk::Box {
                                        set_spacing: 5,
                                        gtk::Image {
                                            #[watch]
                                            set_icon_name: Some(if model.notify_count > 0 {
                                                "co.hyprlab.Vireo-dialog-warning-symbolic"
                                            } else {
                                                "co.hyprlab.Vireo-preferences-system-notifications-symbolic"
                                            }),
                                            #[watch]
                                            set_css_classes: if model.notify_count > 0 {
                                                &["attention-icon"] as &[&str]
                                            } else {
                                                &[] as &[&str]
                                            },
                                        },
                                        gtk::Label {
                                            #[watch]
                                            set_visible: model.notify_count > 0,
                                            #[watch]
                                            set_label: &model.notify_count.to_string(),
                                            add_css_class: "needs-attention",
                                        },
                                    },
                                },
                                pack_start = &gtk::Button {
                                    set_icon_name: "co.hyprlab.Vireo-x-office-address-book-symbolic",
                                    set_tooltip_text: Some("Open Contacts"),
                                    add_css_class: "flat",
                                    connect_clicked[sender] => move |_| sender.input(AppMsg::OpenContacts),
                                },
                                pack_end = &gtk::Button {
                                    set_tooltip_text: Some("Refresh"),
                                    add_css_class: "flat",
                                    connect_clicked[sender] => move |_| sender.input(AppMsg::Refresh),
                                    gtk::Stack {
                                        set_transition_type: gtk::StackTransitionType::Crossfade,
                                        add_named[Some("icon")] = &gtk::Image {
                                            set_icon_name: Some("co.hyprlab.Vireo-view-refresh-symbolic"),
                                        },
                                        add_named[Some("spinner")] = &gtk::Spinner {
                                            #[watch]
                                            set_spinning: !model.busy.is_empty(),
                                        },
                                        #[watch]
                                        set_visible_child_name: if model.busy.is_empty() { "icon" } else { "spinner" },
                                    },
                                },
                            },
                            #[wrap(Some)]
                            set_content = model.message_list.widget(),
                        },

                        #[wrap(Some)]
                        set_end_child = &adw::ToolbarView {
                            add_top_bar = &adw::HeaderBar {
                                add_css_class: "flat",
                                // Empty title so the window's "Vireo" title isn't
                                // shown here; the app title lives above the sidebar.
                                #[wrap(Some)]
                                set_title_widget = &gtk::Label {
                                    set_label: "",
                                },
                                pack_start = &gtk::Button {
                                    set_icon_name: "co.hyprlab.Vireo-mail-reply-sender-symbolic",
                                    set_tooltip_text: Some("Reply"),
                                    add_css_class: "flat",
                                    #[watch]
                                    set_sensitive: model.current.is_some(),
                                    connect_clicked[sender] => move |_| sender.input(AppMsg::Reply),
                                },
                                pack_start = &gtk::Button {
                                    set_icon_name: "co.hyprlab.Vireo-mail-reply-all-symbolic",
                                    set_tooltip_text: Some("Reply All"),
                                    add_css_class: "flat",
                                    #[watch]
                                    set_sensitive: model.current.is_some(),
                                    connect_clicked[sender] => move |_| sender.input(AppMsg::ReplyAll),
                                },
                                pack_start = &gtk::Button {
                                    set_icon_name: "co.hyprlab.Vireo-mail-forward-symbolic",
                                    set_tooltip_text: Some("Forward"),
                                    add_css_class: "flat",
                                    #[watch]
                                    set_sensitive: model.current.is_some(),
                                    connect_clicked[sender] => move |_| sender.input(AppMsg::Forward),
                                },
                                pack_start = &gtk::Button {
                                    set_icon_name: "co.hyprlab.Vireo-contact-new-symbolic",
                                    set_tooltip_text: Some("Add sender to Contacts"),
                                    add_css_class: "flat",
                                    #[watch]
                                    set_sensitive: model.current.is_some(),
                                    connect_clicked[sender] => move |_| sender.input(AppMsg::AddToContacts),
                                },
                                pack_start = &gtk::Button {
                                    set_tooltip_text: Some("Flag"),
                                    add_css_class: "flat",
                                    #[watch]
                                    set_icon_name: if model.current.as_ref().is_some_and(|m| m.starred) {
                                        "co.hyprlab.Vireo-starred-symbolic"
                                    } else {
                                        "co.hyprlab.Vireo-non-starred-symbolic"
                                    },
                                    #[watch]
                                    set_sensitive: model.current.is_some(),
                                    connect_clicked[sender] => move |_| sender.input(AppMsg::ToggleStar),
                                },
                                // pack_end fills right-to-left, so these are declared
                                // in reverse of their visual order. Left to right:
                                // Archive, Delete, Spam, View Source.
                                pack_end = &gtk::Button {
                                    set_icon_name: "co.hyprlab.Vireo-background-app-ghost-symbolic",
                                    set_tooltip_text: Some("View Source"),
                                    add_css_class: "flat",
                                    #[watch]
                                    set_sensitive: model.current.is_some(),
                                    connect_clicked[sender] => move |_| sender.input(AppMsg::ViewSource),
                                },
                                pack_end = &gtk::Button {
                                    set_icon_name: "co.hyprlab.Vireo-mail-mark-junk-symbolic",
                                    set_tooltip_text: Some("Mark as Spam"),
                                    add_css_class: "flat",
                                    #[watch]
                                    set_sensitive: model.current.is_some(),
                                    connect_clicked[sender] => move |_| sender.input(AppMsg::MarkSpam),
                                },
                                pack_end = &gtk::Button {
                                    set_icon_name: "co.hyprlab.Vireo-user-trash-symbolic",
                                    set_tooltip_text: Some("Delete"),
                                    add_css_class: "flat",
                                    #[watch]
                                    set_sensitive: model.current.is_some(),
                                    connect_clicked[sender] => move |_| sender.input(AppMsg::Delete),
                                },
                                pack_end = &gtk::Button {
                                    set_icon_name: "co.hyprlab.Vireo-mail-archive-symbolic",
                                    set_tooltip_text: Some("Archive"),
                                    add_css_class: "flat",
                                    #[watch]
                                    set_sensitive: model.current.is_some(),
                                    connect_clicked[sender] => move |_| sender.input(AppMsg::Archive),
                                },
                                pack_end = &gtk::Spinner {
                                    set_valign: gtk::Align::Center,
                                    set_tooltip_text: Some("Downloading attachments…"),
                                    #[watch]
                                    set_spinning: model.attachments_loading,
                                    #[watch]
                                    set_visible: model.attachments_loading,
                                },
                                // Shown for messages whose attachments weren't
                                // pre-downloaded — load them only when asked.
                                pack_end = &gtk::Button {
                                    set_icon_name: "co.hyprlab.Vireo-folder-download-symbolic",
                                    set_tooltip_text: Some("Load attachments from server"),
                                    add_css_class: "flat",
                                    add_css_class: "attach-present",
                                    #[watch]
                                    set_visible: model.attachments_available && !model.attachments_loading,
                                    connect_clicked[sender] => move |_| sender.input(AppMsg::LoadAttachmentsNow),
                                },
                                pack_end = &gtk::MenuButton {
                                    set_icon_name: "co.hyprlab.Vireo-mail-attachment-symbolic",
                                    set_tooltip_text: Some("Attachments"),
                                    add_css_class: "flat",
                                    add_css_class: "attach-present",
                                    #[watch]
                                    set_visible: !model.attachments.is_empty(),
                                    #[wrap(Some)]
                                    set_popover = &gtk::Popover {
                                        #[local_ref]
                                        attach_list -> gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            set_spacing: 4,
                                            set_width_request: 340,
                                        },
                                    },
                                },
                            },
                            // Reader content: the inline reply/forward pane drops
                            // down (SlideDown revealer) above the message body,
                            // pushing it down to make room. The revealer is
                            // prepended in `init`. The drawer's widget is a Paned
                            // that holds the reader body + attachment footer.
                            #[wrap(Some)]
                            #[name = "reader_content_box"]
                            set_content = &gtk::Box {
                                set_orientation: gtk::Orientation::Vertical,
                                append: model.attachment_drawer.widget(),
                            },
                        },
                    },
                    },
                },
            },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        relm4::set_global_css(include_str!("styles.css"));
        register_icons();

        let mut sidebar_state = config::load_sidebar_state();
        let icon_only = sidebar_state.icon_only;

        // Load accounts, then drop any imported GOA account that GNOME Online
        // Accounts no longer has (removed or Mail-disabled there). Reconciliation
        // is skipped when GOA is unreachable, so a momentary outage never wipes
        // imported accounts. Live changes are handled by the watcher below.
        let mut config = config::load().unwrap_or_default();
        let goa_removed = reconcile_goa(&mut config);
        if !goa_removed.is_empty() {
            for email in &goa_removed {
                config::delete_password(email);
            }
            sidebar_state.order.retain(|e| !goa_removed.contains(e));
            sidebar_state.collapsed.retain(|e| !goa_removed.contains(e));
            sidebar_state.folders_expanded.retain(|e| !goa_removed.contains(e));
            let _ = config::save(&config);
            config::save_sidebar_state(&sidebar_state);
        }
        let order = sidebar_state.order;
        let collapsed = sidebar_state.collapsed;
        let folders_expanded = sidebar_state.folders_expanded;

        let sidebar = Sidebar::builder()
            .launch(icon_only)
            .forward(sender.input_sender(), |out| match out {
                SidebarOutput::UnifiedSelected => AppMsg::UnifiedSelected,
                SidebarOutput::AttachmentsSelected => AppMsg::ShowAttachments,
                SidebarOutput::FolderSelected { account_id, folder_id, name, path } => {
                    AppMsg::FolderSelected { account_id, folder_id, name, path }
                }
                SidebarOutput::ToggleCollapse(id) => AppMsg::ToggleCollapse(id),
                SidebarOutput::ToggleCustomFolders(id) => AppMsg::ToggleCustomFolders(id),
                SidebarOutput::CollapsedChanged(collapsed) => AppMsg::SidebarCollapsed(collapsed),
                SidebarOutput::AddAccount => AppMsg::AddFirstAccount,
                SidebarOutput::Context(action) => AppMsg::SidebarContext(action),
                SidebarOutput::MoveMessage { account_id, folder_id, uid, id, dest } => {
                    AppMsg::DropMoveMessage { account_id, folder_id, uid, id, dest }
                }
            });

        let message_list =
            MessageList::builder()
                .launch(())
                .forward(sender.input_sender(), |out| match out {
                    MessageListOutput::Selected { message, thread } => {
                        AppMsg::MessageSelected { message, thread }
                    }
                    MessageListOutput::Activated(m) => AppMsg::OpenMessageWindow(m),
                    MessageListOutput::Action { action, message } => {
                        AppMsg::RowAction { action, message }
                    }
                    MessageListOutput::Bulk { action, messages } => {
                        AppMsg::Bulk { action, messages }
                    }
                    MessageListOutput::SelectionCleared => AppMsg::ClearReader,
                    MessageListOutput::SearchActive(active) => AppMsg::SearchActive(active),
                });

        let message_view =
            MessageView::builder()
                .launch(())
                .forward(sender.input_sender(), |out| match out {
                    MessageViewOutput::AllowSender(addr) => AppMsg::AllowSender(addr),
                    MessageViewOutput::ComposeTo(addr) => AppMsg::ComposeTo(addr),
                    MessageViewOutput::OpenWindow(m) => AppMsg::OpenMessageWindow(*m),
                });

        // The drawer owns a Paned whose top pane is the reader body, so hand it
        // the message-view widget to dock beneath.
        let attachment_drawer = AttachmentDrawer::builder()
            .launch(crate::ui::attachment_drawer::DrawerInit {
                state: config::load_drawer_state(),
                reader: message_view.widget().clone().upcast(),
            })
            .detach();

        let gallery =
            AttachmentsGallery::builder()
                .launch(())
                .forward(sender.input_sender(), |out| match out {
                    GalleryOutput::OpenMessage { account_id, folder_path, uid } => {
                        AppMsg::OpenAttachmentMessage { account_id, folder_path, uid }
                    }
                });

        let notifications = NotificationCenter::builder().launch(()).forward(
            sender.input_sender(),
            |out| match out {
                NotifyOutput::CountChanged(n) => AppMsg::NotifyCount(n),
            },
        );

        let menu = gtk::gio::Menu::new();
        menu.append(Some("Accounts"), Some("win.accounts"));
        menu.append(Some("Preferences"), Some("win.preferences"));
        menu.append(Some("About Vireo"), Some("win.about"));

        let mut model = AppModel {
            workers: HashMap::new(),
            config,
            window: root.clone(),
            prefs: None,
            accounts_win: None,
            composers: Vec::new(),
            reader_compose: None,
            draining_composers: Vec::new(),
            reader_compose_revealer: {
                let r = gtk::Revealer::new();
                r.set_transition_type(gtk::RevealerTransitionType::SlideDown);
                r.set_transition_duration(200);
                r.set_reveal_child(false);
                r
            },
            next_compose_id: 1,
            menu,
            accounts: Vec::new(),
            folders: HashMap::new(),
            account_order: order,
            collapsed,
            folders_expanded,
            selected: None,
            attachments: Vec::new(),
            attachments_loading: false,
            attachments_available: false,
            attachment_cache: HashMap::new(),
            attach_list: gtk::Box::new(gtk::Orientation::Vertical, 0),
            unified: false,
            unified_by_account: HashMap::new(),
            message_cache: HashMap::new(),
            indexed_folders: HashSet::new(),
            body_cache: HashMap::new(),
            pending_draft: None,
            popouts: HashMap::new(),
            current_thread: Vec::new(),
            bulk_pending: 0,
            pending_bulk: None,
            folder_unread: HashMap::new(),
            sidebar_split: None,
            app_title: None,
            sidebar_header: None,
            sidebar_collapsed: icon_only,
            sidebar_anim: None,
            current: None,
            allowed_senders: config::load_allowed_senders(),
            blacklist: config::load_blacklist(),
            palette_collapse_secs: config::load_palette_collapse(),
            gravatar: config::load_gravatar(),
            fetch_interval_secs: config::load_fetch_interval(),
            push: config::load_push(),
            notifications_enabled: config::load_notifications(),
            threading: config::load_threading(),
            threads_expanded: config::load_threads_expanded(),
            message_theme: config::load_message_theme(),
            auto_fetch_source: None,
            notifications,
            notify_count: 0,
            busy: HashSet::new(),
            sidebar,
            message_list,
            message_view,
            attachment_drawer,
            gallery,
            showing_gallery: false,
            gallery_by_account: HashMap::new(),
        };
        model.spawn_workers(&sender);
        crate::contacts::watch_photo_changes({
            let input = sender.input_sender().clone();
            move || {
                let _ = input.send(AppMsg::ContactPhotosChanged);
            }
        });
        // Watch GNOME Online Accounts so an account removed there disappears from
        // Vireo live (no restart needed); reconciliation happens on GoaChanged.
        crate::goa::watch_removals({
            let s = sender.input_sender().clone();
            move || {
                let _ = s.send(AppMsg::GoaChanged);
            }
        });
        // Watch for resume-from-sleep: suspended IMAP sockets die silently, so
        // on wake we reconnect every worker and refresh, otherwise no new mail
        // arrives until the app is restarted.
        crate::power::watch_resume({
            let s = sender.input_sender().clone();
            move || {
                let _ = s.send(AppMsg::SystemResumed);
            }
        });
        // With no accounts, no worker events will populate the sidebar, so render
        // its empty state (the "Add first account" prompt) up front.
        if model.config.is_empty() {
            model.rebuild_sidebar();
        }
        model
            .message_list
            .emit(MessageListInput::SetGravatar(model.gravatar));
        model
            .message_list
            .emit(MessageListInput::SetThreading(model.threading));
        model
            .message_list
            .emit(MessageListInput::SetThreadsExpanded(model.threads_expanded));
        model
            .message_list
            .emit(MessageListInput::SetPaletteCollapse(model.palette_collapse_secs));
        model
            .message_view
            .emit(MessageViewInput::SetContentTheme(model.message_theme.dark_override()));
        model.arm_auto_fetch(&sender);

        let attach_list = &model.attach_list;
        let widgets = view_output!();
        // The inline reply/forward pane sits above the reader body (top of the
        // content box), sliding down over it when revealed.
        widgets
            .reader_content_box
            .prepend(&model.reader_compose_revealer);
        // The attachments gallery is the content stack's second page. Wrap it in a
        // ToolbarView + HeaderBar so it keeps the window controls (close/minimize)
        // that otherwise live only on the reader pane's header.
        {
            use gtk::prelude::*;
            let gallery_tv = adw::ToolbarView::new();
            let gallery_hb = adw::HeaderBar::new();
            gallery_hb.add_css_class("flat");
            let title = gtk::Label::new(Some("Attachments"));
            title.add_css_class("pane-title");
            gallery_hb.set_title_widget(Some(&title));
            gallery_tv.add_top_bar(&gallery_hb);
            gallery_tv.set_content(Some(model.gallery.widget()));
            widgets.content_stack.add_named(&gallery_tv, Some("gallery"));
        }
        // Desktop-notification click actions: raise the window (error alerts) and
        // raise + open a specific message (new-mail alerts). Registered here rather
        // than in `notify` because opening a message needs the app's channel.
        {
            use gtk::prelude::*;
            let app = relm4::main_application();
            let present = gtk::gio::SimpleAction::new(crate::notify::PRESENT_ACTION, None);
            let win = model.window.clone();
            present.connect_activate(move |_, _| {
                win.set_visible(true);
                win.present();
            });
            app.add_action(&present);

            let ty = gtk::glib::VariantTy::new("(uuu)").unwrap();
            let open = gtk::gio::SimpleAction::new(crate::notify::OPEN_MESSAGE_ACTION, Some(ty));
            let win = model.window.clone();
            let osender = sender.clone();
            open.connect_activate(move |_, param| {
                win.set_visible(true);
                win.present();
                if let Some((account_id, folder_id, message_id)) =
                    param.and_then(|v| v.get::<(u32, u32, u32)>())
                {
                    osender.input(AppMsg::OpenMessageFromNotification {
                        account_id,
                        folder_id,
                        message_id,
                    });
                }
            });
            app.add_action(&open);
        }
        // Restore the last window size + maximized state (Wayland can't restore
        // position/monitor).
        let (win_w, win_h, win_max) = config::load_window_state();
        root.set_default_size(win_w, win_h);
        if win_max {
            root.maximize();
        }
        model.sidebar_split = Some(widgets.sidebar_split.clone());
        model.app_title = Some(widgets.app_title.clone());
        model.sidebar_header = Some(widgets.sidebar_header.clone());
        if model.sidebar_collapsed {
            widgets.sidebar_split.set_min_sidebar_width(SIDEBAR_RAIL_WIDTH);
            widgets.sidebar_split.set_max_sidebar_width(SIDEBAR_RAIL_WIDTH);
            widgets.app_title.set_visible(false);
            set_sidebar_header_compact(&widgets.sidebar_header, true);
        }

        let mut group = RelmActionGroup::<WindowActionGroup>::new();
        let accounts_sender = sender.clone();
        group.add_action(RelmAction::<AccountsAction>::new_stateless(move |_| {
            accounts_sender.input(AppMsg::OpenAccounts);
        }));
        let prefs_sender = sender.clone();
        group.add_action(RelmAction::<PreferencesAction>::new_stateless(move |_| {
            prefs_sender.input(AppMsg::OpenPreferences);
        }));
        let about_sender = sender.clone();
        group.add_action(RelmAction::<AboutAction>::new_stateless(move |_| {
            about_sender.input(AppMsg::OpenAbout);
        }));
        group.register_for_widget(&root);

        // One-time, dismissible keyring setup tip for Linux Mint / Cinnamon, where
        // the Secret Service often needs configuring so passwords persist and the
        // keyring auto-unlocks at login. Only shown once the user actually has an
        // account (so it isn't the very first thing a new user sees), and never
        // again after "Don't show again".
        if !model.config.is_empty()
            && crate::platform::is_mint_cinnamon()
            && !config::mint_keyring_help_dismissed()
        {
            sender.input(AppMsg::ShowKeyringHelp { problem: false });
        }

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            AppMsg::ShowAttachments => {
                self.showing_gallery = true;
                self.gallery_by_account.clear();
                self.gallery.emit(GalleryInput::SetLoading(true));
                self.gallery.emit(GalleryInput::SetItems(Vec::new()));
                // Load each account's attachments (across all gallery folders)
                // from the cache.
                let ids: Vec<u32> = self.accounts.iter().map(|a| a.id).collect();
                for account_id in ids {
                    self.send_to(account_id, MailRequest::LoadGallery);
                }
            }

            AppMsg::GalleryItems { account_id, items } => {
                self.gallery_by_account.insert(account_id, items);
                let mut merged: Vec<crate::models::GalleryItem> = self
                    .gallery_by_account
                    .values()
                    .flatten()
                    .cloned()
                    .collect();
                merged.sort_by_key(|i| std::cmp::Reverse(i.timestamp));
                self.gallery.emit(GalleryInput::SetItems(merged));
            }

            AppMsg::OpenAttachmentMessage { account_id, folder_path, uid } => {
                self.showing_gallery = false;
                if let Some(folder) = self
                    .folders
                    .get(&account_id)
                    .and_then(|fs| fs.iter().find(|f| f.path == folder_path))
                    .cloned()
                {
                    self.select_folder(account_id, folder.id, folder.name.clone(), folder.path.clone());
                    // Messages use their UID as id, so select by (account, uid).
                    self.message_list
                        .emit(MessageListInput::SelectAndLoad((account_id, uid)));
                }
            }

            AppMsg::UnifiedSelected => {
                self.showing_gallery = false;
                self.unified = true;
                self.selected = None;
                self.current = None;
                self.current_thread.clear();
                self.attachments.clear();
                self.attachments_loading = false;
                self.attachments_available = false;
                self.sync_attachment_drawer();
                self.show_message(None, false);
                self.unified_by_account.clear();
                self.message_list.emit(MessageListInput::SetSelected(None));
                self.message_list.emit(MessageListInput::SetColorize(true));
                self.message_list.emit(MessageListInput::ResetPaging);
                self.message_list
                    .emit(MessageListInput::SetLoading { title: "All Inboxes".into() });
                // Request every account's inbox; results are merged as they arrive.
                let reqs: Vec<(u32, u32, String)> = self
                    .accounts
                    .iter()
                    .filter_map(|a| self.inbox_of(a.id).map(|f| (a.id, f.id, f.path.clone())))
                    .collect();
                for (account_id, folder_id, path) in reqs {
                    self.send_to(account_id, MailRequest::LoadMessages { folder_id, path });
                }
                self.push_index_complete();
            }

            AppMsg::FolderSelected { account_id, folder_id, name, path } => {
                self.select_folder(account_id, folder_id, name, path);
            }

            AppMsg::OpenMessageFromNotification { account_id, folder_id, message_id } => {
                // The user clicked a new-mail notification: they've engaged with
                // that account's mail, so clear its toast, then navigate to the
                // message's folder and open it in the reader.
                crate::notify::withdraw_mail(account_id);
                if let Some((name, path)) = self
                    .folders
                    .get(&account_id)
                    .and_then(|fs| fs.iter().find(|f| f.id == folder_id))
                    .map(|f| (f.name.clone(), f.path.clone()))
                {
                    // select_folder emits the (cached) list synchronously, so the
                    // subsequent SelectAndLoad finds the row and opens it.
                    self.select_folder(account_id, folder_id, name, path);
                    self.message_list
                        .emit(MessageListInput::SelectAndLoad((account_id, message_id)));
                }
            }

            AppMsg::ToggleCollapse(account_id) => {
                // The sidebar already animated the toggle locally; just record
                // the new state (a rebuild here would interrupt the animation).
                if let Some(email) = self.email_of(account_id) {
                    if let Some(pos) = self.collapsed.iter().position(|e| *e == email) {
                        self.collapsed.remove(pos);
                    } else {
                        self.collapsed.push(email);
                    }
                    self.save_sidebar_state();
                }
            }

            AppMsg::ToggleCustomFolders(account_id) => {
                // The sidebar animated the toggle locally; record the new state
                // (the "folders_expanded" list holds accounts whose custom
                // folders are revealed; absence means hidden, the default).
                if let Some(email) = self.email_of(account_id) {
                    if let Some(pos) = self.folders_expanded.iter().position(|e| *e == email) {
                        self.folders_expanded.remove(pos);
                    } else {
                        self.folders_expanded.push(email);
                    }
                    self.save_sidebar_state();
                }
            }

            AppMsg::SidebarCollapsed(collapsed) => {
                self.sidebar_collapsed = collapsed;
                self.animate_sidebar(collapsed);
                if let Some(title) = &self.app_title {
                    title.set_visible(!collapsed);
                }
                if let Some(header) = &self.sidebar_header {
                    set_sidebar_header_compact(header, collapsed);
                }
                self.save_sidebar_state();
            }

            AppMsg::SidebarContext(action) => match action {
                CtxAction::MarkFolderRead { account_id, folder_id } => {
                    self.mark_folder_read(account_id, folder_id);
                }
                CtxAction::RefreshFolder { account_id, folder_id } => {
                    if let Some(path) = self
                        .folders
                        .get(&account_id)
                        .and_then(|fs| fs.iter().find(|f| f.id == folder_id))
                        .map(|f| f.path.clone())
                    {
                        self.send_to(account_id, MailRequest::LoadMessages { folder_id, path });
                    }
                }
                CtxAction::MarkAllInboxesRead => {
                    let inboxes: Vec<(u32, u32)> = self
                        .accounts
                        .iter()
                        .filter_map(|a| self.inbox_of(a.id).map(|f| (a.id, f.id)))
                        .collect();
                    for (account_id, folder_id) in inboxes {
                        self.mark_folder_read(account_id, folder_id);
                    }
                }
                CtxAction::RefreshAllInboxes => {
                    let reqs: Vec<(u32, u32, String)> = self
                        .accounts
                        .iter()
                        .filter_map(|a| self.inbox_of(a.id).map(|f| (a.id, f.id, f.path.clone())))
                        .collect();
                    for (account_id, folder_id, path) in reqs {
                        self.send_to(account_id, MailRequest::LoadMessages { folder_id, path });
                    }
                }
                CtxAction::OpenAccountSettings => sender.input(AppMsg::OpenAccounts),
                CtxAction::RemoveAccount(account_id) => {
                    self.confirm_remove_account(account_id, &sender);
                }
                CtxAction::NewFolder(account_id) => {
                    self.prompt_new_folder(account_id, &sender);
                }
                CtxAction::DeleteFolder { account_id, name, path } => {
                    self.confirm_delete_folder(account_id, name, path, &sender);
                }
            },

            AppMsg::DropMoveMessage { account_id, folder_id, uid, id, dest } => {
                if let Some(m) = self.find_cached_message(account_id, id) {
                    self.move_to_path(m, dest);
                } else if let Some(src) = self
                    .folders
                    .get(&account_id)
                    .and_then(|fs| fs.iter().find(|f| f.id == folder_id))
                    .map(|f| f.path.clone())
                {
                    if src != dest {
                        self.send_to(account_id, MailRequest::MoveMessage { path: src, uid, dest });
                        self.message_list.emit(MessageListInput::Remove(id));
                    }
                }
            }

            AppMsg::CreateFolder { account_id, name } => {
                let name = name.trim();
                if !name.is_empty() {
                    let path = format!("{}{}", self.folder_namespace(account_id), name);
                    self.send_to(account_id, MailRequest::CreateFolder { path });
                }
            }

            AppMsg::DeleteFolder { account_id, path } => {
                let trash = self
                    .folders
                    .get(&account_id)
                    .and_then(|fs| fs.iter().find(|f| f.kind == FolderKind::Trash))
                    .map(|f| f.path.clone())
                    .or_else(|| self.default_folder_path(account_id, FolderKind::Trash));
                // If the deleted folder is currently open, clear the view.
                if self.selected.as_ref().is_some_and(|s| s.account_id == account_id && s.path == path) {
                    self.current = None;
                    self.current_thread.clear();
                    self.show_message(None, false);
                    self.message_list.emit(MessageListInput::SetLoading { title: String::new() });
                }
                self.send_to(account_id, MailRequest::DeleteFolder { path, trash });
            }

            AppMsg::AccountsReordered(emails) => {
                // Display order only (by email) — no reconnect needed.
                if !emails.is_empty() {
                    self.account_order = emails;
                    self.save_sidebar_state();
                    self.rebuild_sidebar();
                }
            }

            AppMsg::ClearReader => {
                self.current = None;
                self.current_thread.clear();
                self.attachments.clear();
                self.attachments_loading = false;
                self.attachments_available = false;
                self.sync_attachment_drawer();
                self.show_message(None, false);
            }
            AppMsg::SearchActive(active) => {
                if active {
                    // Snapshot every folder's indexed messages so the search can
                    // span the whole mailbox.
                    self.message_list.emit(MessageListInput::SetSearchPool(
                        build_search_pool(&self.message_cache),
                    ));
                    // Results span accounts; tint rows by account (as in the unified
                    // inbox) so their origin is legible.
                    if self.accounts.len() > 1 {
                        self.message_list.emit(MessageListInput::SetColorize(true));
                    }
                } else {
                    self.message_list
                        .emit(MessageListInput::SetSearchPool(Vec::new()));
                    // Restore the tint state the underlying view wants.
                    self.message_list
                        .emit(MessageListInput::SetColorize(self.unified));
                }
            }
            AppMsg::MessageSelected { message: m, thread } => {
                // Navigating away releases any inline reply (save-if-dirty, or keep
                // it as an independent window if it was popped out).
                self.release_reader_compose();
                // Clicking a draft opens it in the compose editor, not the reader.
                if self.is_drafts_folder(m.account_id, m.folder_id) {
                    self.open_draft(m, &sender);
                    return;
                }
                self.attachments.clear();
                self.attachments_loading = false;
                self.attachments_available = false;
                self.sync_attachment_drawer();
                let account_id = m.account_id;
                let folder_path = self.resolve_folder_path(&m);
                // Use an already-fetched body if we have one (on the message or in
                // our cache) so reopening renders instantly without a spinner.
                let cached_body = if !m.body.is_empty() {
                    Some(m.body.clone())
                } else {
                    self.body_cache.get(&(account_id, m.id)).cloned()
                };
                let needs_body = cached_body.is_none();

                if m.unread {
                    if let Some(path) = folder_path.clone() {
                        self.send_to(account_id, MailRequest::SetSeen { path, uid: m.uid, seen: true });
                    }
                    // Reading new mail clears that account's new-mail notification.
                    crate::notify::withdraw_mail(account_id);
                    self.message_list.emit(MessageListInput::MarkRead(m.id));
                    self.mark_cached_read(account_id, m.id);
                    // Optimistically drop the badge by one; the next server count
                    // (after the sync below) reconciles any drift.
                    if let Some(n) = self.folder_unread.get_mut(&(account_id, m.folder_id)) {
                        *n = n.saturating_sub(1);
                    }
                    self.push_unread_counts();
                }

                let mut current = m.clone();
                current.unread = false;
                if let Some(body) = cached_body {
                    current.body = body;
                }
                self.current = Some(current.clone());

                if thread.len() > 1 {
                    // Conversation: assemble the thread with any cached bodies,
                    // request the rest, and render it as a scrollable conversation.
                    let mut conv: Vec<Message> = Vec::with_capacity(thread.len());
                    for tm in &thread {
                        let mut tm = tm.clone();
                        tm.unread = false;
                        if tm.id == m.id && tm.account_id == account_id {
                            tm.body = current.body.clone();
                        } else if tm.body.is_empty() {
                            if let Some(b) = self.body_cache.get(&(tm.account_id, tm.id)) {
                                tm.body = b.clone();
                            }
                        }
                        conv.push(tm);
                    }
                    self.current_thread = conv;
                    // Fetch the bodies we don't have yet (primary first via order).
                    let to_load: Vec<(u32, u32, u32, String)> = self
                        .current_thread
                        .iter()
                        .filter(|tm| tm.body.is_empty())
                        .filter_map(|tm| {
                            self.resolve_folder_path(tm)
                                .map(|p| (tm.account_id, tm.id, tm.uid, p))
                        })
                        .collect();
                    for (aid, mid, uid, path) in to_load {
                        self.send_to(aid, MailRequest::LoadBody { message_id: mid, path, uid });
                    }
                    self.show_thread();
                } else {
                    self.current_thread.clear();
                    let display = current;
                    // Request the body FIRST so it renders before attachments — the
                    // worker processes requests in order, so the body must come first.
                    if needs_body {
                        if let Some(path) = folder_path.clone() {
                            self.send_to(account_id, MailRequest::LoadBody {
                                message_id: m.id,
                                path,
                                uid: m.uid,
                            });
                        }
                    }
                    self.show_message(Some(display), needs_body);
                }

                // Attachments: use the in-memory cache if present; otherwise ask
                // the worker to serve only from its disk cache (download = false).
                // Pre-downloaded (recent) attachments come back immediately; for
                // others the worker replies AttachmentsPending and we offer a
                // "Load attachments" button rather than fetching automatically.
                if m.has_attachment {
                    if let Some(cached) = self.attachment_cache.get(&(account_id, m.id)).cloned() {
                        self.attachments = cached;
                        self.rebuild_attach_popover(&sender);
                    } else if let Some(path) = folder_path {
                        self.send_to(account_id, MailRequest::LoadAttachments {
                            message_id: m.id,
                            path,
                            uid: m.uid,
                            download: false,
                        });
                    }
                }
            }

            AppMsg::OpenMessageWindow(m) => {
                // Drafts open in the editor rather than a read-only window.
                if self.is_drafts_folder(m.account_id, m.folder_id) {
                    self.open_draft(m, &sender);
                } else {
                    self.open_message_window(m, &sender);
                }
            }

            AppMsg::PopoutClosed(key) => {
                self.popouts.remove(&key);
            }

            AppMsg::AddContactFrom { name, email } => {
                self.show_add_contact_dialog(&name, &email, &sender);
            }

            AppMsg::LoadAttachmentsFor(m) => {
                let m = *m;
                if let Some(path) = self.resolve_folder_path(&m) {
                    self.send_to(m.account_id, MailRequest::LoadAttachments {
                        message_id: m.id,
                        path,
                        uid: m.uid,
                        download: true,
                    });
                }
            }

            AppMsg::OpenAttachmentItem(att) => {
                open_attachment(&att);
            }

            AppMsg::SaveAttachmentItems(items) => {
                save_all_attachments(items, Some(self.window.clone()));
            }

            AppMsg::ToggleStar => {
                if let Some(m) = self.current.clone() {
                    self.set_star(&m, !m.starred);
                }
            }

            AppMsg::Archive => {
                if let Some(m) = self.current.clone() {
                    self.move_to(m, FolderKind::Archive);
                }
            }
            AppMsg::Delete => {
                if let Some(m) = self.current.clone() {
                    self.move_to(m, FolderKind::Trash);
                }
            }

            AppMsg::RowAction { action, message } => {
                let m = *message;
                match action {
                    RowAction::Reply => {
                        let m = self.with_cached_body(m);
                        self.open_compose(m.account_id, reply_prefill(&m), &sender);
                    }
                    RowAction::ReplyAll => {
                        let m = self.with_cached_body(m);
                        let self_email = self.email_of(m.account_id).unwrap_or_default();
                        self.open_compose(
                            m.account_id,
                            reply_all_prefill(&m, &self_email),
                            &sender,
                        );
                    }
                    RowAction::Forward => {
                        let m = self.with_cached_body(m);
                        self.open_compose(m.account_id, forward_prefill(&m), &sender);
                    }
                    RowAction::ToggleStar => self.set_star(&m, !m.starred),
                    RowAction::ToggleRead => self.set_read(&m, m.unread),
                    RowAction::Spam => self.mark_spam_msg(m),
                    RowAction::Archive => self.move_to(m, FolderKind::Archive),
                    RowAction::Delete => self.move_to(m, FolderKind::Trash),
                    RowAction::ViewSource => {
                        if let Some(path) = self.resolve_folder_path(&m) {
                            self.send_to(m.account_id, MailRequest::LoadSource {
                                message_id: m.id,
                                path,
                                uid: m.uid,
                            });
                        }
                    }
                }
            }

            AppMsg::Bulk { action, messages } => {
                match action {
                    // Flag/read changes update rows in place (no removal).
                    BulkAction::MarkRead => for m in &messages { self.set_read(m, true); },
                    BulkAction::MarkUnread => for m in &messages { self.set_read(m, false); },
                    BulkAction::Flag => for m in &messages { self.set_star(m, true); },
                    // Archive/Delete/Spam remove every selected row. Doing that one
                    // at a time blocks the UI thread (a render cycle per message) and
                    // trips GTK's "app is not responding" dialog for large selections.
                    // Batch it; for big selections show a spinner and defer the apply
                    // one tick so the spinner paints before the blocking work runs.
                    BulkAction::Archive | BulkAction::Spam | BulkAction::Delete => {
                        if messages.len() >= BULK_SPINNER_MIN {
                            self.message_list.emit(MessageListInput::SetBusy(Some(
                                bulk_busy_label(action, messages.len()),
                            )));
                            self.pending_bulk = Some((action, messages));
                            let s = sender.clone();
                            gtk::glib::timeout_add_local_once(
                                std::time::Duration::from_millis(16),
                                move || s.input(AppMsg::BulkApply),
                            );
                        } else {
                            self.apply_bulk_move(action, messages);
                        }
                    }
                }
            }

            AppMsg::BulkApply => {
                if let Some((action, messages)) = self.pending_bulk.take() {
                    self.apply_bulk_move(action, messages);
                    // The optimistic removal is done; keep the spinner up until the
                    // server-side moves finish (BulkComplete). If nothing was sent,
                    // clear it now.
                    if self.bulk_pending == 0 {
                        self.message_list.emit(MessageListInput::SetBusy(None));
                    }
                }
            }

            AppMsg::BulkComplete => {
                self.bulk_pending = self.bulk_pending.saturating_sub(1);
                if self.bulk_pending == 0 {
                    self.message_list.emit(MessageListInput::SetBusy(None));
                }
            }

            AppMsg::Refresh => {
                if self.unified {
                    let reqs: Vec<(u32, u32, String)> = self
                        .accounts
                        .iter()
                        .filter_map(|a| self.inbox_of(a.id).map(|f| (a.id, f.id, f.path.clone())))
                        .collect();
                    for (account_id, folder_id, path) in reqs {
                        self.send_to(account_id, MailRequest::LoadMessages { folder_id, path });
                    }
                } else {
                    match self.selected.clone() {
                        Some(sel) => self.send_to(sel.account_id, MailRequest::LoadMessages {
                            folder_id: sel.folder_id,
                            path: sel.path,
                        }),
                        None => {
                            for w in self.workers.values() {
                                let _ = w.send(MailRequest::Reconnect);
                            }
                        }
                    }
                }
            }

            AppMsg::Compose => {
                let account = self.active_account();
                self.open_compose(account, ComposePrefill::default(), &sender);
            }

            AppMsg::Reply => {
                if let Some(m) = self.current.clone() {
                    self.open_inline_reply(m.account_id, reply_prefill(&m), &sender);
                }
            }

            AppMsg::ReplyAll => {
                if let Some(m) = self.current.clone() {
                    let self_email = self.email_of(m.account_id).unwrap_or_default();
                    self.open_inline_reply(
                        m.account_id,
                        reply_all_prefill(&m, &self_email),
                        &sender,
                    );
                }
            }

            AppMsg::Forward => {
                if let Some(m) = self.current.clone() {
                    self.open_inline_reply(m.account_id, forward_prefill(&m), &sender);
                }
            }

            AppMsg::AddToContacts => {
                if let Some(m) = self.current.clone() {
                    self.show_add_contact_dialog(&m.from_name, &m.from_addr, &sender);
                }
            }

            AppMsg::ContactAdded(result) => {
                use crate::contacts::AddOutcome;
                let (text, error) = match result {
                    Ok(AddOutcome::Created) => ("Added to Contacts".to_string(), false),
                    Ok(AddOutcome::Merged(name)) => (format!("Added email to {name}"), false),
                    Ok(AddOutcome::AlreadyPresent(name)) => {
                        (format!("Already in Contacts ({name})"), false)
                    }
                    Err(e) => (format!("Could not add contact: {e}"), true),
                };
                self.notifications.emit(NotifyInput::Push { text, error, connectivity: false });
            }

            AppMsg::ViewSource => {
                if let Some(m) = self.current.clone() {
                    if let Some(path) = self.resolve_folder_path(&m) {
                        self.send_to(m.account_id, MailRequest::LoadSource {
                            message_id: m.id,
                            path,
                            uid: m.uid,
                        });
                    }
                }
            }

            AppMsg::OpenAbout => {
                self.open_about(&sender);
            }

            AppMsg::AllowSender(addr) | AppMsg::AddSender(addr) => {
                let addr = addr.trim().to_lowercase();
                if !addr.is_empty() && !self.allowed_senders.contains(&addr) {
                    self.allowed_senders.push(addr);
                    self.save_settings();
                }
            }

            AppMsg::RemoveSender(addr) => {
                let addr = addr.to_lowercase();
                self.allowed_senders.retain(|s| *s != addr);
                self.save_settings();
            }

            AppMsg::AddBlacklist(addr) => {
                let addr = addr.trim().trim_start_matches('@').to_lowercase();
                if !addr.is_empty() && !self.blacklist.contains(&addr) {
                    self.blacklist.push(addr);
                    self.save_settings();
                    // Sweep mail already in view from the newly-blocked sender.
                    self.sweep_blacklisted();
                }
            }

            AppMsg::RemoveBlacklist(addr) => {
                let addr = addr.to_lowercase();
                self.blacklist.retain(|s| *s != addr);
                self.save_settings();
            }

            AppMsg::MarkSpam => self.mark_spam(),

            AppMsg::SetGravatar(on) => {
                if self.gravatar != on {
                    self.gravatar = on;
                    self.save_settings();
                    self.message_list.emit(MessageListInput::SetGravatar(on));
                    // Refresh the reader's avatar for the open message.
                    let current = self.current.clone();
                    self.show_message(current, false);
                }
            }

            AppMsg::ContactPhotosChanged => {
                self.message_list.emit(MessageListInput::ContactPhotosChanged);
                self.message_view.emit(MessageViewInput::ContactPhotosChanged);
            }

            AppMsg::SetFetchInterval(secs) => {
                if self.fetch_interval_secs != secs {
                    self.fetch_interval_secs = secs;
                    self.save_settings();
                    self.arm_auto_fetch(&sender);
                }
            }

            AppMsg::SetPush(on) => {
                if self.push != on {
                    self.push = on;
                    self.save_settings();
                    // Workers read the push setting at startup; restart to apply.
                    self.reconnect_all(&sender);
                }
            }

            AppMsg::SetNotifications(on) => {
                if self.notifications_enabled != on {
                    self.notifications_enabled = on;
                    self.save_settings();
                }
            }

            AppMsg::SetThreading(on) => {
                if self.threading != on {
                    self.threading = on;
                    self.save_settings();
                    self.message_list.emit(MessageListInput::SetThreading(on));
                }
            }

            AppMsg::SetThreadsExpanded(on) => {
                if self.threads_expanded != on {
                    self.threads_expanded = on;
                    self.save_settings();
                    self.message_list.emit(MessageListInput::SetThreadsExpanded(on));
                }
            }

            AppMsg::SetPaletteCollapse(secs) => {
                if self.palette_collapse_secs != secs {
                    self.palette_collapse_secs = secs;
                    self.save_settings();
                    self.message_list.emit(MessageListInput::SetPaletteCollapse(secs));
                }
            }

            AppMsg::SetMessageTheme(theme) => {
                if self.message_theme != theme {
                    self.message_theme = theme;
                    self.save_settings();
                    let dark = theme.dark_override();
                    // Message content only — the reader and any popped-out windows.
                    self.message_view.emit(MessageViewInput::SetContentTheme(dark));
                    for p in self.popouts.values() {
                        p.controller.emit(MessageWindowInput::SetContentTheme(dark));
                    }
                }
            }

            AppMsg::ComposeTo(addr) => {
                let account = self
                    .current
                    .as_ref()
                    .map(|m| m.account_id)
                    .unwrap_or_else(|| self.active_account());
                let prefill = ComposePrefill {
                    to: addr,
                    ..Default::default()
                };
                self.open_compose(account, prefill, &sender);
            }

            AppMsg::SendMessage(out) => {
                let account_id = out.from_account_id;
                let sent_path = self
                    .folders
                    .get(&account_id)
                    .and_then(|fs| fs.iter().find(|f| f.kind == FolderKind::Sent))
                    .map(|f| f.path.clone());
                self.send_to(account_id, MailRequest::Send { message: out, sent_path });
            }

            AppMsg::SaveDraftMessage(out) => {
                let account_id = out.from_account_id;
                // Existing Drafts folder, else a default path (worker creates it).
                let drafts = self
                    .folders
                    .get(&account_id)
                    .and_then(|fs| fs.iter().find(|f| f.kind == FolderKind::Drafts))
                    .map(|f| (f.id, f.path.clone()))
                    .or_else(|| {
                        self.default_folder_path(account_id, FolderKind::Drafts)
                            .map(|p| (0, p))
                    });
                let Some((folder_id, path)) = drafts else {
                    self.notifications.emit(NotifyInput::Push {
                        text: "No Drafts folder available for this account".to_string(),
                        error: true,
                        connectivity: false,
                    });
                    return;
                };
                self.send_to(account_id, MailRequest::SaveDraft { message: out, folder_id, path });
            }

            AppMsg::DraftSaved => {
                // The Drafts folder reload already reflects the saved draft; the
                // compose window has closed. No notification (mirrors silent send).
            }

            AppMsg::ComposeClosed(id) => self.close_compose(id),

            AppMsg::ComposeToggleWindow(id) => self.toggle_compose_window(id, &sender),

            AppMsg::Sent { account_id } => {
                // No success notification — only send failures are surfaced (via
                // WorkerEvent::Error). Just refresh the Sent folder if it's open.
                // Reload Sent if it's the open folder for that account.
                if let Some(sel) = self.selected.clone() {
                    let viewing_sent = sel.account_id == account_id
                        && self
                            .folders
                            .get(&account_id)
                            .is_some_and(|fs| fs.iter().any(|f| f.id == sel.folder_id && f.kind == FolderKind::Sent));
                    if viewing_sent {
                        self.send_to(account_id, MailRequest::LoadMessages {
                            folder_id: sel.folder_id,
                            path: sel.path,
                        });
                    }
                }
            }

            AppMsg::OpenAccounts => self.open_accounts_window(&sender, false),

            AppMsg::AddFirstAccount => self.open_accounts_window(&sender, true),

            AppMsg::AccountSaved { original_email, account } => {
                let new_email = account.email.clone();
                // Remember the secret we expect to persist, so we can verify the
                // keyring actually stored it (a silent keyring failure would
                // otherwise leave the account unable to log in after a restart).
                let expected_secret = (!account.password.is_empty())
                    .then(|| account.password.clone());
                match original_email {
                    // Editing an existing account (matched by its previous email).
                    Some(orig) => {
                        if let Some(slot) = self.config.iter_mut().find(|c| c.email == orig) {
                            *slot = *account;
                        } else {
                            self.config.push(*account);
                        }
                        // Track an email change in the display-order/collapsed lists.
                        if orig != new_email {
                            for e in self.account_order.iter_mut().chain(self.collapsed.iter_mut()) {
                                if *e == orig {
                                    *e = new_email.clone();
                                }
                            }
                        }
                    }
                    // Adding a new account.
                    None => self.config.push(*account),
                }
                match config::save(&self.config) {
                    Ok(()) => {
                        // config::save() only logs keyring errors, so confirm the
                        // password can actually be read back. If not, the Secret
                        // Service isn't persisting it — tell the user how to fix it
                        // instead of silently "saving" an account that won't stay
                        // logged in.
                        if let Some(secret) = expected_secret {
                            if config::load_password(&new_email).as_deref() != Some(secret.as_str()) {
                                sender.input(AppMsg::ShowKeyringHelp { problem: true });
                            }
                        }
                        self.save_sidebar_state();
                        self.reconnect_all(&sender);
                    }
                    Err(e) => self.notifications.emit(NotifyInput::Push {
                        text: format!("Could not save account: {e}"),
                        error: true,
                        connectivity: false,
                    }),
                }
            }

            AppMsg::ShowKeyringHelp { problem } => self.show_keyring_help(problem),

            AppMsg::AccountEnabledChanged { email, enabled } => {
                if let Some(slot) = self.config.iter_mut().find(|c| c.email == email) {
                    if slot.enabled != enabled {
                        slot.enabled = enabled;
                        if let Err(e) = config::save(&self.config) {
                            self.notifications.emit(NotifyInput::Push {
                                text: format!("Could not save account: {e}"),
                                error: true,
                                connectivity: false,
                            });
                        }
                        // Respawn workers so the account starts/stops syncing.
                        self.reconnect_all(&sender);
                    }
                }
            }

            AppMsg::ImportGoaAccount(account) => {
                // Enable a GNOME Online Account in Vireo (or re-enable if already
                // imported). Its password came from GOA and is stored in the keyring.
                let email = account.email.clone();
                if let Some(slot) = self.config.iter_mut().find(|c| c.email == email) {
                    slot.enabled = true;
                } else {
                    self.config.push(*account);
                }
                match config::save(&self.config) {
                    Ok(()) => self.reconnect_all(&sender),
                    Err(e) => self.notifications.emit(NotifyInput::Push {
                        text: format!("Could not import account: {e}"),
                        error: true,
                        connectivity: false,
                    }),
                }
            }

            AppMsg::AccountRemoved { email } => {
                self.config.retain(|c| c.email != email);
                config::delete_password(&email);
                self.account_order.retain(|e| *e != email);
                self.collapsed.retain(|e| *e != email);
                if let Err(e) = config::save(&self.config) {
                    tracing::error!("could not save config: {e}");
                }
                self.save_sidebar_state();
                self.reconnect_all(&sender);
            }

            AppMsg::GoaChanged => {
                // A GOA account was removed/disabled in GNOME Settings. Drop any
                // imported account that no longer exists there. (Adding an account
                // to GOA never auto-imports — that stays a manual choice.)
                let removed = reconcile_goa(&mut self.config);
                if !removed.is_empty() {
                    for email in &removed {
                        config::delete_password(email);
                        self.account_order.retain(|e| e != email);
                        self.collapsed.retain(|e| e != email);
                        self.folders_expanded.retain(|e| e != email);
                    }
                    if let Err(e) = config::save(&self.config) {
                        tracing::error!("could not save config after GOA change: {e}");
                    }
                    self.save_sidebar_state();
                    self.reconnect_all(&sender);
                }
            }

            AppMsg::SystemResumed => {
                // Sockets left open across suspend are dead. Reconnect drops the
                // stale session, logs in fresh and re-arms IMAP IDLE — and it
                // unsticks any worker parked inside an IDLE wait, since the
                // request breaks its select loop. Then reload the visible folder
                // so new mail appears without waiting for the next auto-fetch.
                for w in self.workers.values() {
                    let _ = w.send(MailRequest::Reconnect);
                }
                sender.input(AppMsg::Refresh);
                // Realign the auto-fetch timer to now; its monotonic countdown
                // did not advance during sleep.
                self.arm_auto_fetch(&sender);
            }

            AppMsg::CloseAccounts => self.accounts_win = None,

            AppMsg::OpenPreferences => {
                // Already open? Bring it forward instead of opening another.
                if let Some(p) = self.prefs.as_ref().filter(|p| p.widget().is_visible()) {
                    p.widget().present();
                    return;
                }
                let init = PrefInit {
                    allowed_senders: self.allowed_senders.clone(),
                    gravatar: self.gravatar,
                    fetch_interval_secs: self.fetch_interval_secs,
                    push: self.push,
                    blacklist: self.blacklist.clone(),
                    palette_collapse_secs: self.palette_collapse_secs,
                    threading: self.threading,
                    threads_expanded: self.threads_expanded,
                    message_theme: self.message_theme,
                    notifications: self.notifications_enabled,
                };
                let prefs = Preferences::builder()
                    .transient_for(&self.window)
                    .launch(init)
                    .forward(sender.input_sender(), |out| match out {
                        PrefOutput::AddSender(addr) => AppMsg::AddSender(addr),
                        PrefOutput::RemoveSender(addr) => AppMsg::RemoveSender(addr),
                        PrefOutput::AddBlacklist(addr) => AppMsg::AddBlacklist(addr),
                        PrefOutput::RemoveBlacklist(addr) => AppMsg::RemoveBlacklist(addr),
                        PrefOutput::SetGravatar(on) => AppMsg::SetGravatar(on),
                        PrefOutput::SetThreading(on) => AppMsg::SetThreading(on),
                        PrefOutput::SetThreadsExpanded(on) => AppMsg::SetThreadsExpanded(on),
                        PrefOutput::SetFetchInterval(secs) => AppMsg::SetFetchInterval(secs),
                        PrefOutput::SetPush(on) => AppMsg::SetPush(on),
                        PrefOutput::SetNotifications(on) => AppMsg::SetNotifications(on),
                        PrefOutput::SetPaletteCollapse(secs) => AppMsg::SetPaletteCollapse(secs),
                        PrefOutput::SetMessageTheme(t) => AppMsg::SetMessageTheme(t),
                        PrefOutput::Closed => AppMsg::ClosePreferences,
                    });
                prefs.widget().present();
                self.prefs = Some(prefs);
            }

            AppMsg::ClosePreferences => self.prefs = None,

            AppMsg::SetAccount(account) => {
                if let Some(existing) = self.accounts.iter_mut().find(|a| a.id == account.id) {
                    *existing = account;
                } else {
                    self.accounts.push(account);
                    self.accounts.sort_by_key(|a| a.id);
                }
                self.rebuild_sidebar();
            }

            AppMsg::SetFolders { account_id, folders } => {
                self.notifications.emit(NotifyInput::ClearConnectivity);
                for f in &folders {
                    self.folder_unread.insert((account_id, f.id), f.unread);
                }
                self.folders.insert(account_id, folders);
                self.rebuild_sidebar();
            }

            AppMsg::FolderUnread { account_id, folder_id, unread } => {
                self.folder_unread.insert((account_id, folder_id), unread);
                self.push_unread_counts();
            }

            AppMsg::Messages { account_id, folder_id, messages } => {
                self.notifications.emit(NotifyInput::ClearConnectivity);
                // Auto-delete blacklisted senders from the inbox before anything
                // else sees them.
                let messages = self.apply_blacklist(account_id, folder_id, messages);
                // Did this sync remove the message currently open in the reader
                // (deleted/moved on another device)? Scope the check to the reader's
                // own folder so a folder switch or another folder's sync doesn't
                // count. Capture where it sat so we can advance to that slot.
                let vanished = self.current.as_ref().is_some_and(|c| {
                    c.account_id == account_id
                        && c.folder_id == folder_id
                        && !messages.iter().any(|m| m.uid == c.uid)
                });
                let next_after_vanish = if vanished {
                    let cur_uid = self.current.as_ref().unwrap().uid;
                    let old_idx = self
                        .message_cache
                        .get(&(account_id, folder_id))
                        .and_then(|old| old.iter().position(|m| m.uid == cur_uid));
                    old_idx
                        .and_then(|i| messages.get(i.min(messages.len().saturating_sub(1))))
                        .or_else(|| messages.first())
                        .map(|m| (m.account_id, m.id))
                } else {
                    None
                };
                // Desktop-notify for genuinely new inbox mail. Only when Vireo
                // isn't the active window (no point notifying about mail you're
                // watching arrive), only for the Inbox, and never on the first load
                // of a folder (no prior cache) — that would fire for every existing
                // message on startup. "New" = unread and not in the previous sync.
                if self.notifications_enabled && !self.window.is_active() {
                    let is_inbox = self.folder_kind(account_id, folder_id) == Some(FolderKind::Inbox);
                    if let (true, Some(old)) =
                        (is_inbox, self.message_cache.get(&(account_id, folder_id)))
                    {
                        let old_uids: std::collections::HashSet<u32> =
                            old.iter().map(|m| m.uid).collect();
                        let fresh: Vec<&Message> = messages
                            .iter()
                            .filter(|m| m.unread && !old_uids.contains(&m.uid))
                            .collect();
                        if let Some(newest) = fresh.iter().max_by_key(|m| m.timestamp) {
                            crate::notify::new_mail(
                                account_id,
                                folder_id,
                                newest.id,
                                &newest.from_name,
                                &newest.subject,
                                fresh.len() - 1,
                            );
                        }
                    }
                }
                // Cache for instant display when revisiting this folder.
                self.message_cache
                    .insert((account_id, folder_id), messages.clone());
                if self.unified {
                    // Accept only each account's inbox; merge all by recency.
                    if self.inbox_of(account_id).map(|f| f.id) == Some(folder_id) {
                        self.unified_by_account.insert(account_id, messages);
                        let mut merged: Vec<Message> = self
                            .unified_by_account
                            .values()
                            .flatten()
                            .cloned()
                            .collect();
                        merged.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
                        self.message_list.emit(MessageListInput::SetMessages {
                            title: "All Inboxes".into(),
                            messages: merged,
                        });
                    }
                } else if let Some(sel) = self.selected.as_ref() {
                    if sel.account_id == account_id && sel.folder_id == folder_id {
                        let title = sel.name.clone();
                        self.message_list
                            .emit(MessageListInput::SetMessages { title, messages });
                    }
                }
                // The reader's message was removed by this sync (deleted/moved on
                // another device). Clear it right away so nothing stale lingers, then
                // advance to whatever now sits in its place if there is one — the
                // SelectAndLoad runs after SetMessages, so the target row exists.
                if vanished {
                    sender.input(AppMsg::ClearReader);
                    if let Some(key) = next_after_vanish {
                        self.message_list.emit(MessageListInput::SelectAndLoad(key));
                    }
                }
                // Refresh unread badges with the freshly-synced counts.
                self.push_unread_counts();
            }

            AppMsg::MessagesAppend { account_id, folder_id, messages } => {
                // Background backfill: grow the folder's search index without
                // disturbing the current view (no title/query reset).
                let messages = self.apply_blacklist(account_id, folder_id, messages);
                let entry = self.message_cache.entry((account_id, folder_id)).or_default();
                let existing: std::collections::HashSet<u32> =
                    entry.iter().map(|m| m.uid).collect();
                let fresh: Vec<Message> = messages
                    .into_iter()
                    .filter(|m| !existing.contains(&m.uid))
                    .collect();
                if fresh.is_empty() {
                    return;
                }
                entry.extend(fresh.iter().cloned());
                // Feed the visible list so search covers the new messages live.
                if self.unified {
                    if self.inbox_of(account_id).map(|f| f.id) == Some(folder_id) {
                        self.unified_by_account
                            .entry(account_id)
                            .or_default()
                            .extend(fresh.iter().cloned());
                        self.message_list
                            .emit(MessageListInput::AppendMessages { messages: fresh });
                    }
                } else if let Some(sel) = self.selected.as_ref() {
                    if sel.account_id == account_id && sel.folder_id == folder_id {
                        self.message_list
                            .emit(MessageListInput::AppendMessages { messages: fresh });
                    }
                }
            }

            AppMsg::BackfillDone { account_id, folder_id } => {
                self.indexed_folders.insert((account_id, folder_id));
                self.push_index_complete();
            }

            AppMsg::Body { account_id, message_id, body } => {
                self.body_cache
                    .insert((account_id, message_id), body.clone());
                // If this body was fetched to open a draft, open the editor now.
                if let Some(pd) = self.pending_draft.take() {
                    if pd.account_id == account_id && pd.id == message_id {
                        self.compose_from_draft(pd, body, &sender);
                        return;
                    }
                    self.pending_draft = Some(pd);
                }
                // Keep the primary's body up to date in either mode.
                if let Some(current) = self.current.as_mut() {
                    if current.id == message_id && current.account_id == account_id {
                        current.body = body.clone();
                    }
                }
                if self.current_thread.len() > 1 {
                    // Conversation mode: fill the matching message's body and
                    // re-render the whole thread (placeholders fill in as they load).
                    let mut changed = false;
                    for tm in self.current_thread.iter_mut() {
                        if tm.id == message_id && tm.account_id == account_id {
                            tm.body = body.clone();
                            changed = true;
                        }
                    }
                    if changed {
                        self.show_thread();
                    }
                } else if self
                    .current
                    .as_ref()
                    .is_some_and(|c| c.id == message_id && c.account_id == account_id)
                {
                    let current = self.current.clone();
                    self.show_message(current, false);
                }
                // Re-render any popped-out window showing this message.
                if let Some(p) = self.popouts.get(&(account_id, message_id)) {
                    p.controller.emit(MessageWindowInput::SetBody(body));
                }
            }

            AppMsg::Source { text } => {
                // Source is only fetched on explicit request (toolbar or context
                // menu), so always show it — even for a message that isn't open.
                self.show_source_window(&text);
            }

            AppMsg::Attachments { account_id, message_id, items } => {
                self.attachment_cache
                    .insert((account_id, message_id), items.clone());
                if self
                    .current
                    .as_ref()
                    .is_some_and(|c| c.id == message_id && c.account_id == account_id)
                {
                    self.attachments_loading = false;
                    self.attachments_available = false;
                    self.attachments = items.clone();
                    self.rebuild_attach_popover(&sender);
                }
                if let Some(p) = self.popouts.get(&(account_id, message_id)) {
                    p.controller.emit(MessageWindowInput::SetAttachments(items));
                }
            }

            AppMsg::AttachmentsPending { account_id, message_id } => {
                // Attachments exist but aren't downloaded; offer the load button.
                if self
                    .current
                    .as_ref()
                    .is_some_and(|c| c.id == message_id && c.account_id == account_id)
                {
                    self.attachments_loading = false;
                    self.attachments_available = true;
                }
                if let Some(p) = self.popouts.get(&(account_id, message_id)) {
                    p.controller.emit(MessageWindowInput::AttachmentsPending);
                }
            }

            AppMsg::NoAttachments { account_id, message_id } => {
                // Clear a false paperclip live. Update every cached folder for the
                // account (a UID is per-folder, but the same message copied across
                // folders shares its attachment status) and the visible row.
                for ((aid, _), msgs) in self.message_cache.iter_mut() {
                    if *aid == account_id {
                        for m in msgs.iter_mut().filter(|m| m.id == message_id) {
                            m.has_attachment = false;
                        }
                    }
                }
                if let Some(c) = self.current.as_mut() {
                    if c.id == message_id && c.account_id == account_id {
                        c.has_attachment = false;
                    }
                }
                self.message_list
                    .emit(MessageListInput::SetHasAttachment { id: message_id, has: false });
            }

            AppMsg::LoadAttachmentsNow => {
                if let Some(m) = self.current.clone() {
                    if let Some(path) = self.resolve_folder_path(&m) {
                        self.attachments_available = false;
                        self.attachments_loading = true;
                        self.send_to(m.account_id, MailRequest::LoadAttachments {
                            message_id: m.id,
                            path,
                            uid: m.uid,
                            download: true,
                        });
                    }
                }
            }

            AppMsg::OpenAttachment(i) => {
                if let Some(att) = self.attachments.get(i) {
                    open_attachment(att);
                }
            }

            AppMsg::SaveAllAttachments => {
                save_all_attachments(self.attachments.clone(), Some(self.window.clone()));
            }

            AppMsg::Status { account_id, text } => {
                if text.is_empty() {
                    self.busy.remove(&account_id);
                } else {
                    self.busy.insert(account_id);
                }
                self.notifications.emit(NotifyInput::SetStatus(text));
            }

            AppMsg::Error { account_id, text, connectivity } => {
                tracing::error!("[account {account_id}] {text}");
                let label = self.account_label(account_id);
                // Desktop-notify only genuine failures (not transient connectivity
                // blips that auto-recover), and only when unfocused — the in-app bar
                // already surfaces it while you're looking.
                if self.notifications_enabled && !connectivity && !self.window.is_active() {
                    crate::notify::error(account_id, &format!("{label}: mail error"), &text);
                }
                self.notifications.emit(NotifyInput::Push {
                    text: format!("{label}: {text}"),
                    error: true,
                    connectivity,
                });
            }

            AppMsg::NotifyCount(n) => self.notify_count = n,
            AppMsg::ToggleNotifications => self.notifications.emit(NotifyInput::TogglePanel),

            AppMsg::OpenContacts => self.show_contacts_window(&sender),
        }
    }
}

impl AppModel {
    /// Persist all app settings together.
    fn save_settings(&self) {
        config::save_privacy(
            &self.allowed_senders,
            self.gravatar,
            self.fetch_interval_secs,
            self.push,
            &self.blacklist,
            self.palette_collapse_secs,
            self.threading,
            self.threads_expanded,
            self.message_theme,
            self.notifications_enabled,
        );
    }

    /// Tell the message list whether the folder(s) currently shown are fully
    /// indexed, so it knows whether to expect more rows while scrolling.
    fn push_index_complete(&self) {
        self.message_list
            .emit(MessageListInput::SetIndexComplete(self.current_index_complete()));
    }

    fn current_index_complete(&self) -> bool {
        if self.unified {
            self.accounts.iter().all(|a| {
                self.inbox_of(a.id)
                    .map_or(true, |f| self.indexed_folders.contains(&(a.id, f.id)))
            })
        } else if let Some(sel) = &self.selected {
            self.indexed_folders.contains(&(sel.account_id, sel.folder_id))
        } else {
            true
        }
    }

    /// (Re)arm the repeating auto-fetch timer to the current interval.
    fn arm_auto_fetch(&mut self, sender: &ComponentSender<Self>) {
        if let Some(id) = self.auto_fetch_source.take() {
            id.remove();
        }
        if self.fetch_interval_secs > 0 {
            let input = sender.input_sender().clone();
            let secs = self.fetch_interval_secs.min(u32::MAX as u64) as u32;
            let id = gtk::glib::timeout_add_seconds_local(secs, move || {
                let _ = input.send(AppMsg::Refresh);
                gtk::glib::ControlFlow::Continue
            });
            self.auto_fetch_source = Some(id);
        }
    }

    /// Send a request to a specific account's worker.
    fn send_to(&self, account_id: u32, req: MailRequest) {
        if let Some(worker) = self.workers.get(&account_id) {
            let _ = worker.send(req);
        }
    }

    /// The account to act on by default (selected folder's account, else first).
    fn active_account(&self) -> u32 {
        self.selected
            .as_ref()
            .map(|s| s.account_id)
            .or_else(|| self.accounts.first().map(|a| a.id))
            .unwrap_or(1)
    }

    /// Spawn one worker per configured account (or a single mock worker when no
    /// account is configured).
    fn spawn_workers(&mut self, sender: &ComponentSender<Self>) {
        self.workers.clear();
        // account_id is the config index + 1 (a load-bearing invariant), so we keep
        // every account's slot but only spawn a worker for enabled ones — disabled
        // accounts simply have no worker (no sync, no sidebar presence). With no
        // accounts configured, the app is blank — the sample/demo data only appears
        // when explicitly requested via VIREO_DEMO (so removing all real accounts
        // doesn't fall back to fake content).
        if self.config.is_empty() {
            if demo_mode() {
                for account_id in [1, 2] {
                    self.workers.insert(account_id, Self::spawn_worker(account_id, None, sender));
                }
            }
        } else {
            for (i, account) in self.config.iter().enumerate() {
                if !account.enabled {
                    continue;
                }
                let account_id = i as u32 + 1;
                let worker = Self::spawn_worker(account_id, Some(account.clone()), sender);
                self.workers.insert(account_id, worker);
            }
        }
    }

    fn spawn_worker(
        account_id: u32,
        account: Option<AccountConfig>,
        sender: &ComponentSender<Self>,
    ) -> UnboundedSender<MailRequest> {
        let input = sender.input_sender().clone();
        worker::spawn(account_id, account, move |event| {
            let _ = input.send(map_event(account_id, event));
        })
    }

    /// Tear down all workers and reconnect from the current config.
    fn reconnect_all(&mut self, sender: &ComponentSender<Self>) {
        self.accounts.clear();
        self.folders.clear();
        self.selected = None;
        self.unified = false;
        self.unified_by_account.clear();
        self.message_cache.clear();
        self.body_cache.clear();
        self.attachments.clear();
        self.sync_attachment_drawer();
        self.attachments_loading = false;
        self.attachments_available = false;
        self.attachment_cache.clear();
        self.current = None;
        self.busy.clear();
        self.show_message(None, false);
        self.message_list.emit(MessageListInput::SetLoading { title: String::new() });
        self.rebuild_sidebar();
        self.spawn_workers(sender);
    }

    /// Resolved avatar/accent colour for an account (custom, else auto accent).
    fn account_color(&self, account_id: u32) -> String {
        self.config
            .get(account_id.saturating_sub(1) as usize)
            .and_then(|c| c.color.clone())
            .or_else(|| {
                self.accounts
                    .iter()
                    .find(|a| a.id == account_id)
                    .map(|a| a.accent.clone())
            })
            .unwrap_or_else(|| "#3584e4".to_string())
    }

    /// Custom avatar emoji for an account, if set.
    fn account_emoji(&self, account_id: u32) -> Option<String> {
        // Demo mode only: showcase the emoji-avatar feature on the sample accounts.
        if self.config.is_empty() && demo_mode() {
            return match account_id {
                1 => Some("🚀".into()),
                2 => Some("🦀".into()),
                _ => None,
            };
        }
        self.config
            .get(account_id.saturating_sub(1) as usize)
            .and_then(|c| c.emoji.clone())
    }

    /// A label for an account in messages (name, else email, else "Account N").
    /// Uses config (available even before the account connects), then live data.
    fn account_label(&self, account_id: u32) -> String {
        let pick = |name: &str, email: &str| -> Option<String> {
            if !name.trim().is_empty() {
                Some(name.to_string())
            } else if !email.trim().is_empty() {
                Some(email.to_string())
            } else {
                None
            }
        };
        self.config
            .get(account_id.saturating_sub(1) as usize)
            .and_then(|c| pick(&c.name, &c.email))
            .or_else(|| {
                self.accounts
                    .iter()
                    .find(|a| a.id == account_id)
                    .and_then(|a| pick(&a.name, &a.email))
            })
            .unwrap_or_else(|| format!("Account {account_id}"))
    }

    /// Display name for an account (name, else email).
    fn account_name(&self, account_id: u32) -> String {
        // The account's UI label (how it's shown in All Inboxes / the reader chip).
        self.accounts
            .iter()
            .find(|a| a.id == account_id)
            .map(|a| a.label.clone())
            .unwrap_or_default()
    }

    /// The account email for an id, if known.
    fn email_of(&self, account_id: u32) -> Option<String> {
        self.accounts
            .iter()
            .find(|a| a.id == account_id)
            .map(|a| a.email.clone())
    }

    /// Persist the sidebar's per-account state (order, collapse, custom-folders
    /// expansion, and icon-only mode).
    fn save_sidebar_state(&self) {
        config::save_sidebar_state(&config::SidebarState {
            order: self.account_order.clone(),
            collapsed: self.collapsed.clone(),
            folders_expanded: self.folders_expanded.clone(),
            icon_only: self.sidebar_collapsed,
        });
    }

    /// Account emails in display order: those listed in `account_order` first
    /// (in that order), then any remaining accounts by id.
    fn ordered_emails(&self) -> Vec<String> {
        let mut result: Vec<String> = Vec::new();
        for email in &self.account_order {
            if self.accounts.iter().any(|a| &a.email == email) && !result.contains(email) {
                result.push(email.clone());
            }
        }
        let mut rest: Vec<&Account> = self
            .accounts
            .iter()
            .filter(|a| !result.contains(&a.email))
            .collect();
        rest.sort_by_key(|a| a.id);
        for a in rest {
            result.push(a.email.clone());
        }
        result
    }

    /// Smoothly animate the sidebar rail between its expanded width and the
    /// narrow icon-only width by interpolating the split view's pinned width.
    fn animate_sidebar(&mut self, collapsing: bool) {
        let Some(split) = self.sidebar_split.clone() else {
            return;
        };
        // Start from the current on-screen width; fall back to sensible defaults.
        let from = split
            .sidebar()
            .map(|w| w.width() as f64)
            .filter(|w| *w > 1.0)
            .unwrap_or(if collapsing { 256.0 } else { SIDEBAR_RAIL_WIDTH });
        let expanded = (0.2 * self.window.width() as f64).clamp(220.0, 280.0);
        let to = if collapsing { SIDEBAR_RAIL_WIDTH } else { expanded };

        let s = split.clone();
        let target = adw::CallbackAnimationTarget::new(move |v| {
            s.set_min_sidebar_width(v);
            s.set_max_sidebar_width(v);
        });
        let anim = adw::TimedAnimation::new(&split, from, to, 200, target);
        anim.set_easing(adw::Easing::EaseOutCubic);
        if !collapsing {
            // Restore responsive sizing once expanded, so window resizes track again.
            let s2 = split.clone();
            anim.connect_done(move |_| {
                s2.set_min_sidebar_width(220.0);
                s2.set_max_sidebar_width(280.0);
                s2.set_sidebar_width_fraction(0.2);
            });
        }
        anim.play();
        self.sidebar_anim = Some(anim);
    }

    /// Update the sidebar's unread badges in place (no rebuild), derived from the
    /// loaded message lists. Cheap enough to call on every read/sync.
    fn push_unread_counts(&self) {
        let folders = self.folder_unread.clone();
        let unified = self.accounts.iter().map(|a| self.inbox_unread(a.id)).sum();
        self.sidebar
            .emit(SidebarInput::SetUnread { folders, unified });
    }

    /// Push the current accounts + folders to the sidebar, in the user's chosen
    /// order and with each account's collapsed state.
    fn rebuild_sidebar(&self) {
        let order = self.ordered_emails();
        let sections: Vec<SectionData> = order
            .iter()
            .filter_map(|email| {
                let account = self.accounts.iter().find(|a| &a.email == email)?.clone();
                // Use the server-side unread count (accurate beyond the loaded
                // window) for each folder's badge.
                let folders = self
                    .folders
                    .get(&account.id)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|mut f| {
                        if let Some(n) = self.folder_unread.get(&(account.id, f.id)) {
                            f.unread = *n;
                        }
                        f
                    })
                    .collect();
                let color = self.account_color(account.id);
                let emoji = self.account_emoji(account.id);
                Some(SectionData {
                    collapsed: self.collapsed.contains(email),
                    custom_expanded: self.folders_expanded.contains(email),
                    color,
                    emoji,
                    account,
                    folders,
                })
            })
            .collect();
        let show_unified = self.accounts.len() > 1;
        let unified_unread = self.accounts.iter().map(|a| self.inbox_unread(a.id)).sum();
        self.sidebar.emit(SidebarInput::SetContents {
            sections,
            show_unified,
            unified_unread,
        });

        // Keep the list's per-account tint colours in sync.
        let colors: std::collections::HashMap<u32, String> = self
            .accounts
            .iter()
            .map(|a| (a.id, self.account_color(a.id)))
            .collect();
        self.message_list
            .emit(MessageListInput::SetAccountColors(colors));
    }

    fn remote_allowed(&self, m: &Message) -> bool {
        let addr = m.from_addr.to_lowercase();
        self.allowed_senders.iter().any(|s| *s == addr)
    }

    /// Rebuild the attachments popover (a row per attachment + "Save All").
    fn rebuild_attach_popover(&self, sender: &ComponentSender<Self>) {
        use crate::models::is_image_name;
        use crate::ui::attachments_gallery::{icon_color_class, icon_for, texture_from};

        while let Some(child) = self.attach_list.first_child() {
            self.attach_list.remove(&child);
        }

        // So the action buttons can dismiss the popover before opening a dialog
        // or the lightbox.
        let popover = self
            .attach_list
            .ancestor(gtk::Popover::static_type())
            .and_downcast::<gtk::Popover>();

        for (i, att) in self.attachments.iter().enumerate() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.add_css_class("attach-row");

            // Image attachments show a thumbnail; everything else a type icon.
            let thumb = is_image_name(&att.name)
                .then(|| texture_from(&att.data))
                .flatten();
            match &thumb {
                Some(tex) => {
                    let img = gtk::Image::from_paintable(Some(tex));
                    img.set_pixel_size(36);
                    img.add_css_class("attach-thumb");
                    row.append(&img);
                }
                None => {
                    let img = gtk::Image::from_icon_name(icon_for(&att.name));
                    img.set_pixel_size(28);
                    img.add_css_class("gallery-file-icon");
                    img.add_css_class(icon_color_class(&att.name));
                    row.append(&img);
                }
            }

            let info = gtk::Box::new(gtk::Orientation::Vertical, 0);
            info.set_hexpand(true);
            info.set_valign(gtk::Align::Center);
            let name = gtk::Label::new(Some(&att.name));
            name.set_halign(gtk::Align::Start);
            name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            name.set_max_width_chars(22);
            let size = gtk::Label::new(Some(&att.human_size()));
            size.set_halign(gtk::Align::Start);
            size.add_css_class("dim-label");
            size.add_css_class("caption");
            info.append(&name);
            info.append(&size);
            row.append(&info);

            let action = |icon: &str, tip: &str| {
                let b = gtk::Button::from_icon_name(icon);
                b.add_css_class("flat");
                b.set_valign(gtk::Align::Center);
                b.set_tooltip_text(Some(tip));
                b
            };

            // Preview (images only) reuses the drawer's lightbox; Download reuses
            // its file chooser; Open launches the default app.
            if thumb.is_some() {
                let preview = action("co.hyprlab.Vireo-system-search-symbolic", "Preview");
                let d = self.attachment_drawer.sender().clone();
                let pop = popover.clone();
                preview.connect_clicked(move |_| {
                    if let Some(p) = &pop {
                        p.popdown();
                    }
                    let _ = d.send(AttachmentDrawerInput::Activate(i));
                });
                row.append(&preview);
            }

            let open = action("co.hyprlab.Vireo-document-open-symbolic", "Open");
            let s = sender.input_sender().clone();
            let pop = popover.clone();
            open.connect_clicked(move |_| {
                if let Some(p) = &pop {
                    p.popdown();
                }
                let _ = s.send(AppMsg::OpenAttachment(i));
            });
            row.append(&open);

            let download = action("co.hyprlab.Vireo-folder-download-symbolic", "Download");
            let d = self.attachment_drawer.sender().clone();
            let pop = popover.clone();
            download.connect_clicked(move |_| {
                if let Some(p) = &pop {
                    p.popdown();
                }
                let _ = d.send(AttachmentDrawerInput::Download(i));
            });
            row.append(&download);

            self.attach_list.append(&row);
        }
        if !self.attachments.is_empty() {
            self.attach_list
                .append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            let save = gtk::Button::with_label("Save All…");
            save.add_css_class("flat");
            let s = sender.input_sender().clone();
            save.connect_clicked(move |_| {
                let _ = s.send(AppMsg::SaveAllAttachments);
            });
            self.attach_list.append(&save);
        }
        self.sync_attachment_drawer();
    }

    /// Push the current attachments into the in-message thumbnail drawer (which
    /// hides itself when the list is empty). Called wherever `self.attachments`
    /// changes so the drawer always mirrors the open message.
    fn sync_attachment_drawer(&self) {
        self.attachment_drawer
            .emit(AttachmentDrawerInput::SetItems(self.attachments.clone()));
    }

    /// Present a read-only window showing raw message source (monospace).
    fn show_source_window(&self, text: &str) {
        let buffer = gtk::TextBuffer::new(None);
        buffer.set_text(text);
        let view = gtk::TextView::with_buffer(&buffer);
        view.set_editable(false);
        view.set_monospace(true);
        view.set_wrap_mode(gtk::WrapMode::WordChar);
        view.set_left_margin(12);
        view.set_right_margin(12);
        view.set_top_margin(8);
        view.set_bottom_margin(8);

        let scroller = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hexpand(true)
            .child(&view)
            .build();

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&scroller));

        let window = adw::Window::builder()
            .transient_for(&self.window)
            .title("Message Source")
            .default_width(720)
            .default_height(620)
            .content(&toolbar)
            .build();
        window.present();
    }

    /// Switch the message list to a folder: reset the view, show its cached
    /// messages instantly (if any), and kick off a background sync. Shared by the
    /// sidebar selection and the "open message from notification" flow.
    fn select_folder(&mut self, account_id: u32, folder_id: u32, name: String, path: String) {
        self.showing_gallery = false;
        self.unified = false;
        self.attachments.clear();
        self.sync_attachment_drawer();
        self.attachments_loading = false;
        self.attachments_available = false;
        self.message_list.emit(MessageListInput::SetSelected(None));
        self.message_list.emit(MessageListInput::SetColorize(false));
        self.message_list.emit(MessageListInput::ResetPaging);
        self.selected = Some(SelectedFolder {
            account_id,
            folder_id,
            name: name.clone(),
            path: path.clone(),
        });
        self.current = None;
        self.current_thread.clear();
        self.show_message(None, false);
        match self.message_cache.get(&(account_id, folder_id)) {
            Some(cached) => self.message_list.emit(MessageListInput::SetMessages {
                title: name,
                messages: cached.clone(),
            }),
            None => self.message_list.emit(MessageListInput::SetLoading { title: name }),
        }
        self.push_index_complete();
        self.send_to(account_id, MailRequest::LoadMessages { folder_id, path });
    }

    fn show_message(&self, message: Option<Message>, loading: bool) {
        let allow_remote = message.as_ref().is_some_and(|m| self.remote_allowed(m));
        let (account_name, account_color) = match message.as_ref() {
            Some(m) => (
                Some(self.account_name(m.account_id)),
                Some(self.account_color(m.account_id)),
            ),
            None => (None, None),
        };
        self.message_view.emit(MessageViewInput::Show {
            thread: message.into_iter().collect(),
            allow_remote,
            gravatar: self.gravatar,
            account_name,
            account_color,
            loading,
        });
    }

    /// Render the current conversation (thread) in the reader, newest first.
    fn show_thread(&self) {
        let Some(primary) = self.current_thread.first() else {
            return;
        };
        let account_id = primary.account_id;
        let allow_remote = self.remote_allowed(primary);
        let loading = primary.body.is_empty();
        self.message_view.emit(MessageViewInput::Show {
            thread: self.current_thread.clone(),
            allow_remote,
            gravatar: self.gravatar,
            account_name: Some(self.account_name(account_id)),
            account_color: Some(self.account_color(account_id)),
            loading,
        });
    }

    /// Pop a message out into its own standalone window with a dedicated reader.
    fn open_message_window(&mut self, m: Message, sender: &ComponentSender<Self>) {
        let key = (m.account_id, m.id);
        // Already open? Just bring it forward.
        if let Some(p) = self.popouts.get(&key) {
            p.window.present();
            return;
        }
        let account_id = m.account_id;

        // Reuse an already-fetched body so the window renders instantly.
        let cached_body = if !m.body.is_empty() {
            Some(m.body.clone())
        } else if self
            .current
            .as_ref()
            .is_some_and(|c| c.id == m.id && c.account_id == account_id && !c.body.is_empty())
        {
            self.current.as_ref().map(|c| c.body.clone())
        } else {
            self.body_cache.get(&key).cloned()
        };
        let needs_body = cached_body.is_none();

        let mut display = m.clone();
        display.unread = false;
        if let Some(body) = cached_body {
            display.body = body;
        }

        // Fetch the body unless the in-flight selection request already will
        // (single click precedes the double click), to avoid a duplicate fetch.
        let already_loading = self
            .current
            .as_ref()
            .is_some_and(|c| c.id == m.id && c.account_id == account_id);
        if needs_body && !already_loading {
            if let Some(path) = self.resolve_folder_path(&m) {
                self.send_to(account_id, MailRequest::LoadBody {
                    message_id: m.id,
                    path,
                    uid: m.uid,
                });
            }
        }

        // Attachments: use the in-memory cache if present; otherwise ask the
        // worker for them (cache-only) and route the reply to this window, just
        // like the main reader does.
        let mut atts: Vec<Attachment> = Vec::new();
        if display.has_attachment {
            if let Some(cached) = self.attachment_cache.get(&key).cloned() {
                atts = cached;
            } else if let Some(path) = self.resolve_folder_path(&m) {
                self.send_to(account_id, MailRequest::LoadAttachments {
                    message_id: m.id,
                    path,
                    uid: m.uid,
                    download: false,
                });
            }
        }

        let allow_remote = self.remote_allowed(&display);
        let init = MessageWindowInit {
            message: display,
            gravatar: self.gravatar,
            account_name: Some(self.account_name(account_id)),
            account_color: Some(self.account_color(account_id)),
            allow_remote,
            loading: needs_body,
            attachments: atts,
            attachments_available: false,
            attachments_loading: false,
            content_dark: self.message_theme.dark_override(),
        };

        let controller = MessageWindow::builder()
            .launch(init)
            .forward(sender.input_sender(), move |out| match out {
                MessageWindowOutput::Action { action, message } => {
                    AppMsg::RowAction { action, message }
                }
                MessageWindowOutput::AddToContacts { name, email } => {
                    AppMsg::AddContactFrom { name, email }
                }
                MessageWindowOutput::LoadAttachments(message) => AppMsg::LoadAttachmentsFor(message),
                MessageWindowOutput::OpenAttachment(att) => AppMsg::OpenAttachmentItem(att),
                MessageWindowOutput::SaveAllAttachments(items) => AppMsg::SaveAttachmentItems(items),
                MessageWindowOutput::AllowSender(addr) => AppMsg::AllowSender(addr),
                MessageWindowOutput::ComposeTo(addr) => AppMsg::ComposeTo(addr),
                MessageWindowOutput::Closed => AppMsg::PopoutClosed(key),
            });

        let window = controller.widget().clone();
        window.set_transient_for(Some(&self.window));
        window.present();

        self.popouts.insert(key, PopOut { window, controller });
    }

    /// Whether the given folder is the account's Drafts folder.
    fn is_drafts_folder(&self, account_id: u32, folder_id: u32) -> bool {
        self.folder_kind(account_id, folder_id) == Some(FolderKind::Drafts)
    }

    /// The kind of a folder by id, if known.
    fn folder_kind(&self, account_id: u32, folder_id: u32) -> Option<FolderKind> {
        self.folders
            .get(&account_id)?
            .iter()
            .find(|f| f.id == folder_id)
            .map(|f| f.kind)
    }

    /// Open a draft for editing: reuse a cached body if we have one, otherwise
    /// fetch it and open the editor once it arrives (see the `Body` handler).
    fn open_draft(&mut self, m: Message, sender: &ComponentSender<Self>) {
        let body = if !m.body.is_empty() {
            Some(m.body.clone())
        } else {
            self.body_cache.get(&(m.account_id, m.id)).cloned()
        };
        match body {
            Some(html) => self.compose_from_draft(m, html, sender),
            None => {
                if let Some(path) = self.resolve_folder_path(&m) {
                    self.send_to(
                        m.account_id,
                        MailRequest::LoadBody { message_id: m.id, path, uid: m.uid },
                    );
                }
                self.pending_draft = Some(m);
            }
        }
    }

    /// Open the compose editor pre-filled from a draft, remembering its origin so
    /// saving/sending replaces it.
    fn compose_from_draft(&mut self, m: Message, body_html: String, sender: &ComponentSender<Self>) {
        let path = self.resolve_folder_path(&m).unwrap_or_default();
        let prefill = ComposePrefill {
            to: m.to.clone(),
            cc: m.cc.clone(),
            subject: m.subject.clone(),
            body_html,
            draft_origin: Some(crate::models::DraftOrigin {
                account_id: m.account_id,
                folder_id: m.folder_id,
                path,
                uid: m.uid,
            }),
        };
        self.open_compose(m.account_id, prefill, sender);
    }

    /// Assemble the `ComposeInit` for a composer (from-accounts + signatures,
    /// autocomplete suggestions, a fresh id, host mode).
    fn build_compose_init(
        &mut self,
        account_id: u32,
        prefill: ComposePrefill,
        windowed: bool,
        can_toggle: bool,
    ) -> (u32, ComposeInit) {
        // Selectable "from" accounts, in display order, with their signatures.
        let accounts: Vec<ComposeAccount> = self
            .ordered_emails()
            .iter()
            .filter_map(|email| {
                let a = self.accounts.iter().find(|a| &a.email == email)?;
                let label = if a.name.trim().is_empty() {
                    a.email.clone()
                } else {
                    format!("{} <{}>", a.name, a.email)
                };
                let signature = self
                    .config
                    .get(a.id.saturating_sub(1) as usize)
                    .and_then(|c| c.signature.clone())
                    .unwrap_or_default();
                Some(ComposeAccount { id: a.id, label, signature })
            })
            .collect();
        let selected = accounts.iter().position(|c| c.id == account_id).unwrap_or(0);

        // Exclude the user's own addresses from recipient suggestions.
        let own: Vec<String> = self.accounts.iter().map(|a| a.email.clone()).collect();
        let id = self.next_compose_id;
        self.next_compose_id += 1;
        let init = ComposeInit {
            compose_id: id,
            prefill,
            accounts,
            selected,
            suggestions: crate::contacts::suggestions(&own),
            windowed,
            can_toggle,
        };
        (id, init)
    }

    /// Launch a `Compose` component, forwarding its outputs into `AppMsg`.
    fn spawn_compose(
        &self,
        init: ComposeInit,
        sender: &ComponentSender<Self>,
    ) -> Controller<Compose> {
        Compose::builder()
            .launch(init)
            .forward(sender.input_sender(), |out| match out {
                ComposeOutput::Send(msg) => AppMsg::SendMessage(msg),
                ComposeOutput::SaveDraft(msg) => AppMsg::SaveDraftMessage(msg),
                ComposeOutput::ToggleWindow(id) => AppMsg::ComposeToggleWindow(id),
                ComposeOutput::Close(id) => AppMsg::ComposeClosed(id),
            })
    }

    /// Host a compose pane in a fresh standalone window, transient for the app.
    fn compose_window_host(
        &self,
        content: &impl IsA<gtk::Widget>,
        id: u32,
        sender: &ComponentSender<Self>,
    ) -> adw::Window {
        let win = adw::Window::builder()
            .modal(false)
            .default_width(660)
            .default_height(760)
            .title("New Message")
            .transient_for(&self.window)
            .build();
        win.set_content(Some(content));
        let s = sender.input_sender().clone();
        win.connect_close_request(move |_| {
            let _ = s.send(AppMsg::ComposeClosed(id));
            gtk::glib::Propagation::Proceed
        });
        win.present();
        win
    }

    /// Open a standalone compose window (New Message, compose-to, edit-draft).
    fn open_compose(
        &mut self,
        account_id: u32,
        prefill: ComposePrefill,
        sender: &ComponentSender<Self>,
    ) {
        let (id, init) = self.build_compose_init(account_id, prefill, true, false);
        let controller = self.spawn_compose(init, sender);
        let window = self.compose_window_host(controller.widget(), id, sender);
        self.composers.push(ComposeHost { id, controller, window });
    }

    /// Open (or replace) the reader's inline reply/forward drop-down pane.
    fn open_inline_reply(
        &mut self,
        account_id: u32,
        prefill: ComposePrefill,
        sender: &ComponentSender<Self>,
    ) {
        // Supersede any composer already in the reader slot first.
        self.release_reader_compose();
        let (id, init) = self.build_compose_init(account_id, prefill, false, true);
        let controller = self.spawn_compose(init, sender);
        let widget = controller.widget();
        self.reader_compose_revealer.set_child(Some(widget));
        self.reader_compose_revealer.set_reveal_child(true);
        controller.emit(ComposeInput::FocusEditor);
        self.reader_compose = Some(ReaderCompose { id, controller, window: None });
    }

    /// Detach the reader's inline composer from the reader slot. If it was popped
    /// out to a window it lives on independently; if inline, ask it to save-if-
    /// dirty and let it drain closed.
    fn release_reader_compose(&mut self) {
        let Some(r) = self.reader_compose.take() else {
            return;
        };
        match r.window {
            Some(window) => {
                self.composers.push(ComposeHost { id: r.id, controller: r.controller, window });
            }
            None => {
                self.reader_compose_revealer.set_reveal_child(false);
                self.reader_compose_revealer.set_child(None::<&gtk::Widget>);
                r.controller.emit(ComposeInput::SaveDraftIfDirty);
                self.draining_composers.push((r.id, r.controller));
            }
        }
    }

    /// Promote the reader's inline pane to a window, or collapse a window back
    /// inline — reparenting the live pane so the editor state survives the move.
    fn toggle_compose_window(&mut self, id: u32, sender: &ComponentSender<Self>) {
        let Some(mut r) = self.reader_compose.take() else {
            return;
        };
        if r.id != id {
            self.reader_compose = Some(r);
            return;
        }
        let widget = r.controller.widget().clone();
        match r.window.take() {
            None => {
                // inline → window: unparent from the revealer, then host in a window.
                self.reader_compose_revealer.set_reveal_child(false);
                self.reader_compose_revealer.set_child(None::<&gtk::Widget>);
                let window = self.compose_window_host(&widget, id, sender);
                r.window = Some(window);
                r.controller.emit(ComposeInput::SetWindowed(true));
            }
            Some(window) => {
                // window → inline: unparent from the window, drop it back in place.
                window.set_content(None::<&gtk::Widget>);
                window.destroy();
                self.reader_compose_revealer.set_child(Some(&widget));
                self.reader_compose_revealer.set_reveal_child(true);
                r.controller.emit(ComposeInput::SetWindowed(false));
            }
        }
        r.controller.emit(ComposeInput::FocusEditor);
        self.reader_compose = Some(r);
    }

    /// Tear down a composer by id (from a Close output or a window's close-request).
    fn close_compose(&mut self, id: u32) {
        if let Some(pos) = self.composers.iter().position(|h| h.id == id) {
            let host = self.composers.remove(pos);
            host.window.set_content(None::<&gtk::Widget>);
            host.window.destroy();
            return;
        }
        if self.reader_compose.as_ref().is_some_and(|r| r.id == id) {
            let r = self.reader_compose.take().unwrap();
            match r.window {
                Some(window) => {
                    window.set_content(None::<&gtk::Widget>);
                    window.destroy();
                }
                None => {
                    self.reader_compose_revealer.set_reveal_child(false);
                    self.reader_compose_revealer.set_child(None::<&gtk::Widget>);
                }
            }
            return;
        }
        self.draining_composers.retain(|(cid, _)| *cid != id);
    }

    /// Move a message to its account's folder of `kind` (archive/delete).
    /// Destination path for `kind` on an account: its existing folder of that kind,
    /// else a sensible default (the worker creates it on the server on first move).
    fn folder_path_for(&self, account_id: u32, kind: FolderKind) -> Option<String> {
        self.folders
            .get(&account_id)
            .and_then(|fs| fs.iter().find(|f| f.kind == kind))
            .map(|f| f.path.clone())
            .or_else(|| self.default_folder_path(account_id, kind))
    }

    fn move_to(&mut self, m: Message, kind: FolderKind) {
        let Some(dest) = self.folder_path_for(m.account_id, kind) else {
            self.notifications.emit(NotifyInput::Push {
                text: format!("No {} folder available", kind_label(kind)),
                error: true,
                connectivity: false,
            });
            return;
        };
        self.move_to_path(m, dest);
    }

    /// Move a message to an explicit destination folder path (used by both the
    /// kind-based actions and drag-and-drop onto a folder).
    fn move_to_path(&mut self, m: Message, dest: String) {
        let Some(src) = self.resolve_folder_path(&m) else {
            return;
        };
        if src == dest {
            return; // already in that folder
        }
        self.send_to(m.account_id, MailRequest::MoveMessage { path: src, uid: m.uid, dest });
        self.discard_message(&m);
    }

    /// The mailbox namespace prefix for an account (e.g. "INBOX." if its folders
    /// nest under INBOX, otherwise ""), derived from an existing sub-folder.
    fn folder_namespace(&self, account_id: u32) -> String {
        self.folders
            .get(&account_id)
            .map(|folders| {
                folders
                    .iter()
                    .filter(|f| f.kind != FolderKind::Inbox && !f.name.is_empty())
                    .find_map(|f| {
                        f.path
                            .strip_suffix(&f.name)
                            .filter(|p| !p.is_empty())
                            .map(|p| p.to_string())
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    /// A sensible destination path for a standard folder the account doesn't have
    /// yet (Archive/Trash/Junk), matching the account's folder namespace so the
    /// server creates it in the right place (e.g. "INBOX.Archive").
    fn default_folder_path(&self, account_id: u32, kind: FolderKind) -> Option<String> {
        let leaf = match kind {
            FolderKind::Archive => "Archive",
            FolderKind::Trash => "Trash",
            FolderKind::Junk => "Junk",
            FolderKind::Drafts => "Drafts",
            _ => return None,
        };
        self.folders.get(&account_id)?; // require folders to be loaded
        Some(format!("{}{leaf}", self.folder_namespace(account_id)))
    }

    /// Find a cached message by (account, id) for drag-and-drop moves.
    fn find_cached_message(&self, account_id: u32, id: u32) -> Option<Message> {
        for ((aid, _), msgs) in self.message_cache.iter() {
            if *aid == account_id {
                if let Some(m) = msgs.iter().find(|m| m.id == id) {
                    return Some(m.clone());
                }
            }
        }
        self.unified_by_account
            .get(&account_id)
            .and_then(|msgs| msgs.iter().find(|m| m.id == id).cloned())
    }

    /// Explain how to set up the system keyring (Secret Service) so passwords
    /// persist across restarts, and — on Linux Mint / Cinnamon — how to stop the
    /// keyring asking for an unlock password at every login.
    ///
    /// `problem` is true when this is shown because a save actually failed;
    /// false for the proactive one-time tip (which offers "Don't show again").
    fn show_keyring_help(&self, problem: bool) {
        let mint = crate::platform::is_mint_cinnamon();

        let heading = if problem {
            "Vireo couldn’t save your password"
        } else {
            "Keyring setup on Linux Mint"
        };

        let mut body = String::new();
        if problem {
            body.push_str(
                "Vireo stores account passwords in the system keyring (the Secret \
                 Service), never on disk. The keyring didn’t accept the password, so \
                 this account won’t stay signed in after you close Vireo.\n\n",
            );
        } else {
            body.push_str(
                "Vireo keeps your account passwords in the system keyring (the Secret \
                 Service) rather than on disk. On Linux Mint with Cinnamon the keyring \
                 sometimes needs a one-time setup so passwords persist — and so it \
                 doesn’t ask you to unlock it at every login.\n\n",
            );
        }

        if mint {
            body.push_str(
                "Set it up:\n\
                 1. Install the keyring tools if needed:\n\
                 \u{2003}sudo apt install gnome-keyring seahorse\n\
                 2. Open “Passwords and Keys” (Seahorse) and make sure a keyring named \
                 “Login” exists and is set as Default (right-click → Set as Default).\n\n\
                 Stop it asking for a password at each login — pick one:\n\
                 • Recommended: set the Login keyring’s password to match your user \
                 login password (right-click the Login keyring → Change Password), and \
                 log in with your password rather than using automatic login. The \
                 keyring then unlocks automatically when you log in.\n\
                 • Or, to remove the prompt entirely even with automatic login: set the \
                 Login keyring’s password to blank (Change Password → leave the new \
                 password empty). This is convenient, but your saved passwords are then \
                 stored unencrypted at rest — only do this on a machine you trust.",
            );
            if crate::platform::is_flatpak() {
                body.push_str(
                    "\n\nNote: run these steps on the host system (not inside the \
                     Flatpak) — Vireo uses whatever keyring your desktop provides.",
                );
            }
        } else {
            body.push_str(
                "Make sure a Secret Service keyring is installed, running, and \
                 unlocked — for example install “gnome-keyring” and “seahorse” \
                 (Passwords and Keys), then create a default “Login” keyring and set \
                 its password to your login password so it unlocks automatically.",
            );
        }

        let dialog = adw::MessageDialog::new(Some(&self.window), Some(heading), Some(&body));
        dialog.add_response("ok", "Got it");
        dialog.set_default_response(Some("ok"));
        // The proactive tip is a one-time thing: mark it seen once dismissed (by
        // any means) so it never nags again. A real save failure always shows.
        if !problem {
            dialog.connect_response(None, |_, _| config::dismiss_mint_keyring_help());
        }
        dialog.present();
    }

    /// Prompt for a new custom folder name and create it under `account_id`.
    fn prompt_new_folder(&self, account_id: u32, sender: &ComponentSender<Self>) {
        let dialog = adw::MessageDialog::new(
            Some(&self.window),
            Some("New Folder"),
            Some("Create a new folder for this account."),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("ok", "Create");
        dialog.set_default_response(Some("ok"));
        dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
        let entry = gtk::Entry::new();
        entry.set_placeholder_text(Some("Folder name"));
        entry.set_activates_default(true);
        dialog.set_extra_child(Some(&entry));
        let s = sender.clone();
        dialog.connect_response(None, move |_, resp| {
            if resp == "ok" {
                let name = entry.text().to_string();
                if !name.trim().is_empty() {
                    s.input(AppMsg::CreateFolder { account_id, name });
                }
            }
        });
        dialog.present();
    }

    /// Confirm deleting a custom folder (contents moved to Trash).
    fn confirm_delete_folder(
        &self,
        account_id: u32,
        name: String,
        path: String,
        sender: &ComponentSender<Self>,
    ) {
        let dialog = adw::MessageDialog::new(
            Some(&self.window),
            Some(&format!("Delete “{name}”?")),
            Some("Its messages are moved to Trash and the folder is removed."),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("delete", "Delete");
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        let s = sender.clone();
        dialog.connect_response(None, move |_, resp| {
            if resp == "delete" {
                s.input(AppMsg::DeleteFolder {
                    account_id,
                    path: path.clone(),
                });
            }
        });
        dialog.present();
    }

    /// Mark `m` as spam: tag `$Junk` and move it to the account's Junk folder
    /// (so the server's spam filter can learn from it).
    fn mark_spam_msg(&mut self, m: Message) {
        let Some(src) = self.resolve_folder_path(&m) else {
            return;
        };
        let dest = self
            .folders
            .get(&m.account_id)
            .and_then(|fs| fs.iter().find(|f| f.kind == FolderKind::Junk))
            .map(|f| f.path.clone())
            .or_else(|| self.default_folder_path(m.account_id, FolderKind::Junk));
        let Some(dest) = dest else {
            self.notifications.emit(NotifyInput::Push {
                text: "No Junk folder available for this account".to_string(),
                error: true,
                connectivity: false,
            });
            return;
        };
        self.send_to(m.account_id, MailRequest::MarkSpam { path: src, uid: m.uid, dest });
        self.discard_message(&m);
    }

    fn mark_spam(&mut self) {
        if let Some(m) = self.current.clone() {
            self.mark_spam_msg(m);
        }
    }

    /// Mark every message in a folder as read: update server, caches, badges,
    /// and the displayed list.
    fn mark_folder_read(&mut self, account_id: u32, folder_id: u32) {
        let Some(path) = self
            .folders
            .get(&account_id)
            .and_then(|fs| fs.iter().find(|f| f.id == folder_id))
            .map(|f| f.path.clone())
        else {
            return;
        };
        // Optimistic in-memory update so the UI reacts instantly.
        if let Some(msgs) = self.message_cache.get_mut(&(account_id, folder_id)) {
            for m in msgs {
                m.unread = false;
            }
        }
        if self.inbox_of(account_id).map(|f| f.id) == Some(folder_id) {
            if let Some(msgs) = self.unified_by_account.get_mut(&account_id) {
                for m in msgs {
                    m.unread = false;
                }
            }
        }
        self.folder_unread.insert((account_id, folder_id), 0);
        self.send_to(account_id, MailRequest::MarkAllRead { folder_id, path });
        self.refresh_list_display();
        self.push_unread_counts();
    }

    /// Re-emit the currently-visible folder/unified list from the caches, so
    /// in-place changes (e.g. mark-all-read) show without a server round-trip.
    fn refresh_list_display(&self) {
        if self.unified {
            let mut merged: Vec<Message> = self
                .unified_by_account
                .values()
                .flatten()
                .cloned()
                .collect();
            merged.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
            self.message_list.emit(MessageListInput::SetMessages {
                title: "All Inboxes".into(),
                messages: merged,
            });
        } else if let Some(sel) = self.selected.as_ref() {
            if let Some(msgs) = self.message_cache.get(&(sel.account_id, sel.folder_id)) {
                self.message_list.emit(MessageListInput::SetMessages {
                    title: sel.name.clone(),
                    messages: msgs.clone(),
                });
            }
        }
    }

    /// Modal contacts browser: pick a contact to start a new message to them.
    fn show_contacts_window(&self, sender: &ComponentSender<Self>) {
        let input = sender.input_sender().clone();
        crate::ui::contacts_browser::present(&self.window, move |contact| {
            let _ = input.send(AppMsg::ComposeTo(contact.email));
        });
    }

    /// Dialog to add an email to GNOME Contacts (choosing the address book).
    fn show_add_contact_dialog(&self, name: &str, email: &str, sender: &ComponentSender<Self>) {
        let books = crate::contacts::writable_books();
        if books.is_empty() || email.trim().is_empty() {
            self.notifications.emit(NotifyInput::Push {
                text: "No address book available to add contacts".to_string(),
                error: true,
                connectivity: false,
            });
            return;
        }

        let dialog = adw::MessageDialog::new(
            Some(&self.window),
            Some("Add to Contacts"),
            None,
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("add", "Add");
        dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("add"));
        dialog.set_close_response("cancel");

        let form = gtk::ListBox::new();
        form.add_css_class("boxed-list");
        form.set_selection_mode(gtk::SelectionMode::None);
        let name_row = adw::EntryRow::new();
        name_row.set_title("Name");
        name_row.set_text(name);
        let email_row = adw::EntryRow::new();
        email_row.set_title("Email");
        email_row.set_text(email);
        let book_row = adw::ComboRow::new();
        book_row.set_title("Address book");
        let labels: Vec<&str> = books.iter().map(|b| b.name.as_str()).collect();
        book_row.set_model(Some(&gtk::StringList::new(&labels)));
        form.append(&name_row);
        form.append(&email_row);
        form.append(&book_row);
        dialog.set_extra_child(Some(&form));

        let input = sender.input_sender().clone();
        dialog.connect_response(None, move |_, resp| {
            if resp != "add" {
                return;
            }
            let name = name_row.text().trim().to_string();
            let email = email_row.text().trim().to_string();
            let idx = book_row.selected() as usize;
            let Some(book) = books.get(idx).cloned() else {
                return;
            };
            if email.is_empty() {
                return;
            }
            // Writing talks to EDS over D-Bus (blocking) — do it off the UI thread.
            let input = input.clone();
            std::thread::spawn(move || {
                let result = crate::contacts::add_or_merge(&book.uid, &name, &email);
                let _ = input.send(AppMsg::ContactAdded(result));
            });
        });
        dialog.present();
    }

    /// Confirm and remove an account (drops its keyring password too).
    /// Open (or focus) the accounts window. When `add_new`, jump straight to the
    /// "add account" form — used by the empty-state "Add first account" button.
    fn open_accounts_window(&mut self, sender: &ComponentSender<Self>, add_new: bool) {
        // Already open? Bring it forward instead of opening another.
        if let Some(w) = self.accounts_win.as_ref().filter(|w| w.widget().is_visible()) {
            w.widget().present();
            if add_new {
                w.emit(crate::ui::accounts::AccountsInput::AddAccount);
            }
            return;
        }
        // Pass accounts in display order, with passwords prefilled from the keyring
        // so the editor shows them when editing.
        let order = self.ordered_emails();
        let mut accounts: Vec<AccountConfig> = Vec::new();
        for email in &order {
            if let Some(a) = self.config.iter().find(|c| &c.email == email) {
                accounts.push(a.clone());
            }
        }
        for c in &self.config {
            if !accounts.iter().any(|a| a.email == c.email) {
                accounts.push(c.clone());
            }
        }
        for a in &mut accounts {
            if a.password.is_empty() {
                a.password = config::load_password(&a.email).unwrap_or_default();
            }
            if a.smtp_separate && a.smtp_password.is_empty() {
                a.smtp_password = config::load_smtp_password(&a.email).unwrap_or_default();
            }
        }
        let win = AccountsWindow::builder()
            .transient_for(&self.window)
            .launch(accounts)
            .forward(sender.input_sender(), |out| match out {
                AccountsOutput::Saved { original_email, account } => {
                    AppMsg::AccountSaved { original_email, account }
                }
                AccountsOutput::Removed { email } => AppMsg::AccountRemoved { email },
                AccountsOutput::Reordered(emails) => AppMsg::AccountsReordered(emails),
                AccountsOutput::EnabledChanged { email, enabled } => {
                    AppMsg::AccountEnabledChanged { email, enabled }
                }
                AccountsOutput::ImportGoa(account) => AppMsg::ImportGoaAccount(account),
                AccountsOutput::Closed => AppMsg::CloseAccounts,
            });
        if add_new {
            win.emit(crate::ui::accounts::AccountsInput::AddAccount);
        }
        win.widget().present();
        self.accounts_win = Some(win);
    }

    fn confirm_remove_account(&self, account_id: u32, sender: &ComponentSender<Self>) {
        let Some(email) = self.email_of(account_id) else {
            return;
        };
        let label = self.account_label(account_id);
        let dialog = adw::MessageDialog::new(
            Some(&self.window),
            Some("Remove Account?"),
            Some(&format!(
                "Remove {label} from Vireo? Its saved password is deleted. \
                 Mail on the server is not affected."
            )),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("remove", "Remove");
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let s = sender.clone();
        dialog.connect_response(None, move |_, resp| {
            if resp == "remove" {
                s.input(AppMsg::AccountRemoved { email: email.clone() });
            }
        });
        dialog.present();
    }

    /// A custom, scrollable About window: app identity up top (icon, name, a
    /// version chip, and a one-line description under it), then the feature
    /// sections laid out on the page itself, and project links.
    fn open_about(&self, sender: &ComponentSender<Self>) {
        let win = adw::Window::builder()
            .transient_for(&self.window)
            .modal(false)
            .title("About Vireo")
            .default_width(460)
            .default_height(640)
            .build();

        // A navigation stack so Release Notes / Changelog slide in (and back out)
        // within the same window instead of spawning separate ones.
        let nav = adw::NavigationView::new();

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        let clamp = adw::Clamp::builder().maximum_size(420).build();
        let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page.set_margin_top(18);
        page.set_margin_bottom(12);

        // Identity block.
        let icon = gtk::Image::from_icon_name(crate::APP_ID);
        icon.set_pixel_size(96);
        icon.set_margin_bottom(10);
        page.append(&icon);

        let name = gtk::Label::new(Some("Vireo"));
        name.add_css_class("title-1");
        page.append(&name);

        let version = gtk::Label::new(Some(env!("CARGO_PKG_VERSION")));
        version.add_css_class("about-version-chip");
        version.set_halign(gtk::Align::Center);
        version.set_margin_top(8);
        page.append(&version);

        // One-sentence description, directly under the version chip.
        let desc = gtk::Label::new(Some("A clean, fast, GNOME-native email client."));
        desc.set_wrap(true);
        desc.set_justify(gtk::Justification::Center);
        desc.add_css_class("dim-label");
        desc.set_margin_top(12);
        page.append(&desc);

        // Release notes: slide in as a sub-page of this window.
        let info = gtk::ListBox::new();
        info.add_css_class("boxed-list");
        info.set_selection_mode(gtk::SelectionMode::None);
        info.set_margin_top(20);

        let notes_row = adw::ActionRow::builder()
            .title("Release Notes")
            .subtitle(format!("What's new in {}", env!("CARGO_PKG_VERSION")))
            .activatable(true)
            .build();
        notes_row.add_suffix(&gtk::Image::from_icon_name("co.hyprlab.Vireo-go-next-symbolic"));
        {
            let nav = nav.clone();
            notes_row.connect_activated(move |_| nav.push_by_tag("notes"));
        }
        info.append(&notes_row);

        let changelog_row = adw::ActionRow::builder()
            .title("Changelog")
            .subtitle("Full version history")
            .activatable(true)
            .build();
        changelog_row.add_suffix(&gtk::Image::from_icon_name("co.hyprlab.Vireo-go-next-symbolic"));
        {
            let nav = nav.clone();
            changelog_row.connect_activated(move |_| nav.push_by_tag("changelog"));
        }
        info.append(&changelog_row);

        // Linux Mint (Cinnamon): re-open the one-time keyring setup tip. Shown only
        // where that tip applies, so Mint users who dismissed it can find it again.
        if crate::platform::is_mint_cinnamon() {
            let keyring_row = adw::ActionRow::builder()
                .title("Keyring Setup Help")
                .subtitle("Make account passwords persist on Linux Mint")
                .activatable(true)
                .build();
            keyring_row.add_suffix(&gtk::Image::from_icon_name("co.hyprlab.Vireo-go-next-symbolic"));
            let sender = sender.clone();
            keyring_row.connect_activated(move |_| {
                sender.input(AppMsg::ShowKeyringHelp { problem: false });
            });
            info.append(&keyring_row);
        }

        page.append(&info);

        // Project links. Each row shows its URL as a hover tooltip.
        let links_title = gtk::Label::new(Some("Project"));
        links_title.add_css_class("heading");
        links_title.set_halign(gtk::Align::Start);
        links_title.set_margin_top(20);
        links_title.set_margin_bottom(6);
        page.append(&links_title);

        let links = gtk::ListBox::new();
        links.add_css_class("boxed-list");
        links.set_selection_mode(gtk::SelectionMode::None);
        let mk_row = |title: &str, url: &str| -> adw::ActionRow {
            let row = adw::ActionRow::builder().title(title).activatable(true).build();
            row.set_tooltip_text(Some(url));
            row.add_suffix(&gtk::Image::from_icon_name("co.hyprlab.Vireo-adw-external-link-symbolic"));
            let u = url.to_string();
            row.connect_activated(move |_| crate::oauth::open_uri(&u));
            row
        };
        links.append(&mk_row("Website", "https://vireo.hyprlab.co"));
        links.append(&mk_row("Contact — hyprlab@proton.me", "mailto:hyprlab@proton.me"));
        links.append(&mk_row("Source Code", "https://github.com/hyprlab/vireo"));
        links.append(&mk_row("License (GNU AGPL v3)", "https://www.gnu.org/licenses/agpl-3.0.html"));

        // Buy Me a Coffee — with a coffee-cup glyph as its leading icon.
        let coffee = adw::ActionRow::builder()
            .title("Buy Me a Coffee")
            .activatable(true)
            .build();
        coffee.set_tooltip_text(Some("https://buymeacoffee.com/hyprlab"));
        let cup = gtk::Label::new(Some("☕"));
        cup.add_css_class("about-coffee");
        coffee.add_prefix(&cup);
        coffee.add_suffix(&gtk::Image::from_icon_name("co.hyprlab.Vireo-adw-external-link-symbolic"));
        coffee.connect_activated(move |_| crate::oauth::open_uri("https://buymeacoffee.com/hyprlab"));
        links.append(&coffee);
        page.append(&links);

        // Footer.
        let footer = gtk::Label::new(Some("© 2026 Hyprlab"));
        footer.add_css_class("dim-label");
        footer.add_css_class("caption");
        footer.set_wrap(true);
        footer.set_justify(gtk::Justification::Center);
        footer.set_margin_top(20);
        page.append(&footer);

        clamp.set_child(Some(&page));
        scroller.set_child(Some(&clamp));

        // The root page holds the identity + links; the sub-pages slide over it.
        let main_tv = adw::ToolbarView::new();
        let main_header = adw::HeaderBar::new();
        main_header.add_css_class("flat");
        main_tv.add_top_bar(&main_header);
        main_tv.set_content(Some(&scroller));
        nav.add(
            &adw::NavigationPage::builder()
                .title("About Vireo")
                .tag("main")
                .child(&main_tv)
                .build(),
        );
        nav.add(&notes_page("Release Notes", "notes", &release_notes_pango()));
        nav.add(&notes_page("Changelog", "changelog", &changelog_pango()));

        win.set_content(Some(&nav));
        win.present();
    }

    /// Star/unstar a message, updating the server, the list, and the reader.
    fn set_star(&mut self, m: &Message, starred: bool) {
        let Some(path) = self.resolve_folder_path(m) else {
            return;
        };
        self.send_to(m.account_id, MailRequest::SetFlagged { path, uid: m.uid, flagged: starred });
        self.message_list
            .emit(MessageListInput::SetStarred { id: m.id, starred });
        if let Some(cur) = self.current.as_mut() {
            if cur.id == m.id && cur.account_id == m.account_id {
                cur.starred = starred;
            }
        }
        if self.current.as_ref().is_some_and(|c| c.id == m.id && c.account_id == m.account_id) {
            let current = self.current.clone();
            self.show_message(current, false);
        }
        if let Some(p) = self.popouts.get(&(m.account_id, m.id)) {
            p.controller.emit(MessageWindowInput::SetStarred(starred));
        }
    }

    /// Mark a message read/unread, updating the server, list, badges, and reader.
    fn set_read(&mut self, m: &Message, read: bool) {
        // No-op if it's already in the requested state.
        if read != m.unread {
            return;
        }
        let Some(path) = self.resolve_folder_path(m) else {
            return;
        };
        self.send_to(m.account_id, MailRequest::SetSeen { path, uid: m.uid, seen: read });
        self.message_list
            .emit(MessageListInput::SetRead { id: m.id, read });
        self.set_cached_unread(m.account_id, m.id, !read);
        if let Some(n) = self.folder_unread.get_mut(&(m.account_id, m.folder_id)) {
            if read {
                *n = n.saturating_sub(1);
            } else {
                *n += 1;
            }
        }
        if let Some(cur) = self.current.as_mut() {
            if cur.id == m.id && cur.account_id == m.account_id {
                cur.unread = !read;
            }
        }
        self.push_unread_counts();
    }

    /// Fill a message's body from the cache if it isn't already loaded, so
    /// reply/forward from the context menu can quote it when available.
    fn with_cached_body(&self, mut m: Message) -> Message {
        if m.body.is_empty() {
            if let Some(b) = self.body_cache.get(&(m.account_id, m.id)) {
                m.body = b.clone();
            }
        }
        m
    }

    /// Remove a handled message from the list, caches, and badges. Clears the
    /// reader only if that message was the one open. Shared by archive/delete/spam.
    /// Apply a removing bulk action (archive/delete/spam) to many messages at once.
    /// Messages are grouped by (account, source folder) and each group is moved in a
    /// SINGLE `MoveMessages` request (one server-side UID MOVE) — far faster and more
    /// reliable than one request per message, which on a huge mailbox (e.g. Gmail's
    /// All Mail) is slow and drops moves when the connection blips. The list is
    /// updated once (`RemoveMany`); the spinner clears when every group's worker
    /// reports `BulkComplete`.
    fn apply_bulk_move(&mut self, action: BulkAction, messages: Vec<Message>) {
        let kind = match action {
            BulkAction::Archive => FolderKind::Archive,
            BulkAction::Delete => FolderKind::Trash,
            BulkAction::Spam => FolderKind::Junk,
            // Non-removing actions never reach here (handled inline).
            BulkAction::MarkRead | BulkAction::MarkUnread | BulkAction::Flag => return,
        };
        // (account, source path) → (dest path, uids). dest is per-account.
        let mut groups: HashMap<(u32, String), (String, Vec<u32>)> = HashMap::new();
        let mut removed_ids = Vec::with_capacity(messages.len());
        let mut missing_dest = false;
        for m in &messages {
            let Some(src) = self.resolve_folder_path(m) else { continue };
            let Some(dest) = self.folder_path_for(m.account_id, kind) else {
                missing_dest = true;
                continue;
            };
            if src == dest {
                continue;
            }
            groups
                .entry((m.account_id, src))
                .or_insert_with(|| (dest, Vec::new()))
                .1
                .push(m.uid);
            self.discard_message_local(m);
            removed_ids.push(m.id);
        }
        if missing_dest {
            self.notifications.emit(NotifyInput::Push {
                text: format!("No {} folder available for some messages", kind_label(kind)),
                error: true,
                connectivity: false,
            });
        }
        self.bulk_pending += groups.len();
        for ((account_id, src), (dest, uids)) in groups {
            self.send_to(account_id, MailRequest::MoveMessages { path: src, uids, dest });
        }
        self.message_list.emit(MessageListInput::RemoveMany(removed_ids));
        self.push_unread_counts();
    }

    /// Optimistic local cleanup when a message leaves the current folder: close its
    /// popout, drop it from the in-memory caches and the unread count, and clear the
    /// reader if it was the viewed message. Does NOT touch the list widget or push
    /// unread counts — the caller does that (single delete via `discard_message`;
    /// bulk via one `RemoveMany` + one push in `apply_bulk_move`).
    fn discard_message_local(&mut self, m: &Message) {
        if let Some(p) = self.popouts.get(&(m.account_id, m.id)) {
            p.window.close();
        }
        if let Some(msgs) = self.unified_by_account.get_mut(&m.account_id) {
            msgs.retain(|x| x.uid != m.uid);
        }
        if let Some(msgs) = self.message_cache.get_mut(&(m.account_id, m.folder_id)) {
            msgs.retain(|x| x.uid != m.uid);
        }
        if m.unread {
            if let Some(n) = self.folder_unread.get_mut(&(m.account_id, m.folder_id)) {
                *n = n.saturating_sub(1);
            }
        }
        if self.current.as_ref().is_some_and(|c| c.id == m.id && c.account_id == m.account_id) {
            // Drop the reader's view state, but DON'T blank it here: the list's
            // Remove handler advances to the next message (or emits SelectionCleared
            // when the folder is empty), which drives the reader. Blanking now would
            // flash "No message selected" before the next one loads.
            self.current = None;
            self.current_thread.clear();
            self.attachments.clear();
            self.sync_attachment_drawer();
            self.attachments_loading = false;
            self.attachments_available = false;
        }
    }

    fn discard_message(&mut self, m: &Message) {
        self.discard_message_local(m);
        self.message_list.emit(MessageListInput::Remove(m.id));
        self.push_unread_counts();
    }

    /// Whether a sender address matches the blacklist (exact address, or a bare
    /// domain entry matching the sender's domain or any subdomain of it).
    fn is_blacklisted(&self, addr: &str) -> bool {
        let a = addr.trim().to_lowercase();
        if a.is_empty() {
            return false;
        }
        let domain = a.rsplit('@').next().unwrap_or("");
        self.blacklist.iter().any(|entry| {
            if entry.contains('@') {
                a == *entry
            } else {
                domain == entry.as_str() || domain.ends_with(&format!(".{entry}"))
            }
        })
    }

    /// Move any blacklisted senders in an inbox sync to Trash, returning the rest.
    fn apply_blacklist(
        &self,
        account_id: u32,
        folder_id: u32,
        messages: Vec<Message>,
    ) -> Vec<Message> {
        if self.blacklist.is_empty()
            || self.inbox_of(account_id).map(|f| f.id) != Some(folder_id)
        {
            return messages;
        }
        let folders = self.folders.get(&account_id);
        let trash = folders
            .and_then(|fs| fs.iter().find(|f| f.kind == FolderKind::Trash))
            .map(|f| f.path.clone());
        let src = folders
            .and_then(|fs| fs.iter().find(|f| f.id == folder_id))
            .map(|f| f.path.clone());
        let mut kept = Vec::with_capacity(messages.len());
        for m in messages {
            if self.is_blacklisted(&m.from_addr) {
                if let (Some(trash), Some(src)) = (&trash, &src) {
                    self.send_to(account_id, MailRequest::MoveMessage {
                        path: src.clone(),
                        uid: m.uid,
                        dest: trash.clone(),
                    });
                }
            } else {
                kept.push(m);
            }
        }
        kept
    }

    /// Re-sync every inbox so a newly-blacklisted sender's existing mail is
    /// caught and deleted by [`apply_blacklist`].
    fn sweep_blacklisted(&self) {
        let reqs: Vec<(u32, u32, String)> = self
            .accounts
            .iter()
            .filter_map(|a| self.inbox_of(a.id).map(|f| (a.id, f.id, f.path.clone())))
            .collect();
        for (account_id, folder_id, path) in reqs {
            self.send_to(account_id, MailRequest::LoadMessages { folder_id, path });
        }
    }

    /// The IMAP folder path a message lives in (its account's folder by id).
    fn resolve_folder_path(&self, m: &Message) -> Option<String> {
        self.folders
            .get(&m.account_id)?
            .iter()
            .find(|f| f.id == m.folder_id)
            .map(|f| f.path.clone())
    }

    /// An account's Inbox folder, if known.
    fn inbox_of(&self, account_id: u32) -> Option<&Folder> {
        self.folders
            .get(&account_id)?
            .iter()
            .find(|f| f.kind == FolderKind::Inbox)
    }

    /// Server-side unread count for an account's inbox.
    fn inbox_unread(&self, account_id: u32) -> u32 {
        self.inbox_of(account_id)
            .and_then(|inbox| self.folder_unread.get(&(account_id, inbox.id)))
            .copied()
            .unwrap_or(0)
    }

    /// Mark a cached message read in every list that holds it, so unread badges
    /// update immediately without waiting for the next server sync.
    fn mark_cached_read(&mut self, account_id: u32, message_id: u32) {
        self.set_cached_unread(account_id, message_id, false);
    }

    /// Set a cached message's unread flag in every list that holds it.
    fn set_cached_unread(&mut self, account_id: u32, message_id: u32, unread: bool) {
        for ((aid, _), msgs) in self.message_cache.iter_mut() {
            if *aid == account_id {
                if let Some(m) = msgs.iter_mut().find(|m| m.id == message_id) {
                    m.unread = unread;
                }
            }
        }
        if let Some(msgs) = self.unified_by_account.get_mut(&account_id) {
            if let Some(m) = msgs.iter_mut().find(|m| m.id == message_id) {
                m.unread = unread;
            }
        }
    }

    /// Title for the message-list pane header.
    fn pane_title(&self) -> &str {
        if self.unified {
            "All Inboxes"
        } else {
            self.selected.as_ref().map(|s| s.name.as_str()).unwrap_or("Mailbox")
        }
    }
}

/// Render the single source of truth — `RELEASE_NOTES.md` at the repo root — as
/// Pango markup for the About window's "Release Notes" page. The same
/// `RELEASE_NOTES.md` is used verbatim for the GitHub release, so the notes stay
/// identical everywhere.
fn release_notes_pango() -> String {
    md_to_pango(include_str!("../RELEASE_NOTES.md"))
}

/// Pango markup for the About window's "Changelog" page, from the centralized
/// `CHANGELOG.md` — so the version history updates everywhere from one file.
fn changelog_pango() -> String {
    md_to_pango(include_str!("../CHANGELOG.md"))
}

/// Minimal Markdown → Pango markup (headings, bullets) for the About sub-pages.
fn md_to_pango(md: &str) -> String {
    let mut out = String::new();
    for raw in md.lines() {
        let line = raw.trim_end();
        let rendered = if let Some(rest) = line.strip_prefix("## ") {
            format!("<b>{}</b>", gtk::glib::markup_escape_text(rest))
        } else if let Some(rest) = line.strip_prefix("# ") {
            format!("<big><b>{}</b></big>", gtk::glib::markup_escape_text(rest))
        } else if let Some(rest) = line.strip_prefix("- ") {
            format!("•  {}", gtk::glib::markup_escape_text(rest))
        } else if line.is_empty() {
            String::new()
        } else {
            gtk::glib::markup_escape_text(line).to_string()
        };
        out.push_str(&rendered);
        out.push('\n');
    }
    out
}

/// Build a scrollable About sub-page (Pango `markup`) for the navigation stack,
/// reachable by `tag`. Pushed pages get a back button and slide animation from
/// the parent `NavigationView`.
fn notes_page(title: &str, tag: &str, markup: &str) -> adw::NavigationPage {
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    let clamp = adw::Clamp::builder().maximum_size(460).build();

    let label = gtk::Label::new(None);
    label.set_markup(markup);
    label.set_wrap(true);
    label.set_xalign(0.0);
    label.set_yalign(0.0);
    label.set_margin_top(18);
    label.set_margin_bottom(24);
    label.set_margin_start(18);
    label.set_margin_end(18);

    clamp.set_child(Some(&label));
    scroller.set_child(Some(&clamp));

    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&adw::HeaderBar::new());
    tv.set_content(Some(&scroller));

    adw::NavigationPage::builder()
        .title(title)
        .tag(tag)
        .child(&tv)
        .build()
}

/// Whether to serve the built-in sample/demo data (for screenshots). Off unless
/// `VIREO_DEMO` is set, so removing all real accounts leaves the app blank.
fn demo_mode() -> bool {
    std::env::var_os("VIREO_DEMO").is_some()
}

/// Drop imported accounts whose GNOME Online Account no longer exists (removed or
/// Mail-disabled in GNOME Settings), returning the emails dropped. Keeps every
/// account — a no-op — when GOA can't be reached, so a momentarily-unavailable GOA
/// never wipes imported accounts.
fn reconcile_goa(config: &mut Vec<AccountConfig>) -> Vec<String> {
    let Some(live) = crate::goa::live_account_ids() else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    config.retain(|c| match &c.goa_id {
        Some(id) if !live.contains(id) => {
            removed.push(c.email.clone());
            false
        }
        _ => true,
    });
    removed
}

/// Label for the spinner shown while a large bulk action is applied.
fn bulk_busy_label(action: BulkAction, n: usize) -> String {
    let verb = match action {
        BulkAction::Archive => "Archiving",
        BulkAction::Delete => "Deleting",
        BulkAction::Spam => "Moving to Spam",
        BulkAction::MarkRead | BulkAction::MarkUnread | BulkAction::Flag => "Updating",
    };
    format!("{verb} {n} messages…")
}

/// Trim the sidebar header in the icon-only rail so it no longer forces a minimum
/// width wider than the rail: hide the (redundant) window-control buttons and tag
/// the header so its Compose/Menu buttons shrink to fit (see `.rail-header` in the
/// stylesheet). The reader pane's header still carries the window's close button,
/// so nothing becomes unreachable.
fn set_sidebar_header_compact(header: &adw::HeaderBar, compact: bool) {
    header.set_show_start_title_buttons(!compact);
    header.set_show_end_title_buttons(!compact);
    if compact {
        header.add_css_class("rail-header");
    } else {
        header.remove_css_class("rail-header");
    }
}

fn open_attachment(att: &Attachment) {
    let dir = std::env::temp_dir().join("vireo-attachments");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let safe = att.name.replace(['/', '\\'], "_");
    let path = dir.join(&safe);
    if std::fs::write(&path, &att.data).is_ok() {
        let uri = format!("file://{}", path.display());
        let _ = gtk::gio::AppInfo::launch_default_for_uri(
            &uri,
            None::<&gtk::gio::AppLaunchContext>,
        );
    }
}

/// Ask for a folder and write every attachment into it.
fn save_all_attachments(atts: Vec<Attachment>, parent: Option<adw::ApplicationWindow>) {
    let dialog = gtk::FileDialog::new();
    dialog.set_title("Save All Attachments");
    dialog.select_folder(parent.as_ref(), gtk::gio::Cancellable::NONE, move |res| {
        if let Ok(folder) = res {
            if let Some(dir) = folder.path() {
                for att in &atts {
                    let safe = att.name.replace(['/', '\\'], "_");
                    let _ = std::fs::write(dir.join(&safe), &att.data);
                }
            }
        }
    });
}

/// Register the app icon so windows and dialogs can find it by name.
///
/// Vireo's toolbar/list icons are shipped inside the binary as a GResource
/// (registered in `main`), so they no longer depend on the host icon theme.
/// GTK auto-adds the bundle's resource path (`/co/hyprlab/Vireo/icons`) to the
/// default theme; we add it explicitly too, so lookups work even if that
/// convention ever changes.
fn register_icons() {
    if let Some(display) = gtk::gdk::Display::default() {
        let theme = gtk::IconTheme::for_display(&display);
        theme.add_resource_path("/co/hyprlab/Vireo/icons");
        // Dev-only: lets the window/about app icon resolve when running from the
        // source tree (uninstalled). Silently ignored on installed systems.
        theme.add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons"));
    }
    gtk::Window::set_default_icon_name(crate::APP_ID);
}

fn map_event(account_id: u32, event: WorkerEvent) -> AppMsg {
    match event {
        WorkerEvent::BulkComplete => AppMsg::BulkComplete,
        WorkerEvent::Account(a) => AppMsg::SetAccount(a),
        WorkerEvent::Folders(folders) => AppMsg::SetFolders { account_id, folders },
        WorkerEvent::Messages { folder_id, messages } => {
            AppMsg::Messages { account_id, folder_id, messages }
        }
        WorkerEvent::MessagesAppend { folder_id, messages } => {
            AppMsg::MessagesAppend { account_id, folder_id, messages }
        }
        WorkerEvent::Gallery { items } => AppMsg::GalleryItems { account_id, items },
        WorkerEvent::BackfillDone { folder_id } => AppMsg::BackfillDone { account_id, folder_id },
        WorkerEvent::FolderUnread { folder_id, unread } => {
            AppMsg::FolderUnread { account_id, folder_id, unread }
        }
        WorkerEvent::Body { message_id, body } => AppMsg::Body { account_id, message_id, body },
        WorkerEvent::Source { text, .. } => AppMsg::Source { text },
        WorkerEvent::Attachments { message_id, items } => {
            AppMsg::Attachments { account_id, message_id, items }
        }
        WorkerEvent::AttachmentsPending { message_id } => {
            AppMsg::AttachmentsPending { account_id, message_id }
        }
        WorkerEvent::NoAttachments { message_id } => {
            AppMsg::NoAttachments { account_id, message_id }
        }
        WorkerEvent::Sent => AppMsg::Sent { account_id },
        WorkerEvent::DraftSaved => AppMsg::DraftSaved,
        WorkerEvent::Status(text) => AppMsg::Status { account_id, text },
        WorkerEvent::Error { text, connectivity } => {
            AppMsg::Error { account_id, text, connectivity }
        }
    }
}

fn reply_prefill(m: &Message) -> ComposePrefill {
    let subject = if m.subject.to_lowercase().starts_with("re:") {
        m.subject.clone()
    } else {
        format!("Re: {}", m.subject)
    };
    let text = message_text(&m.body);
    let attribution = format!("On {}, {} wrote:", m.date, m.from_name);
    ComposePrefill {
        to: m.from_addr.clone(),
        cc: String::new(),
        subject,
        body_html: quote_block(&attribution, &text),
        draft_origin: None,
    }
}

/// Reply-all: To = original sender; Cc = every other recipient (original To +
/// Cc) minus the sender and our own address, de-duplicated.
fn reply_all_prefill(m: &Message, self_email: &str) -> ComposePrefill {
    let mut prefill = reply_prefill(m);
    let self_l = self_email.to_lowercase();
    let from_l = m.from_addr.to_lowercase();
    let mut cc: Vec<String> = Vec::new();
    for list in [m.to.as_str(), m.cc.as_str()] {
        for addr in list.split(',') {
            let a = addr.trim();
            let al = a.to_lowercase();
            if a.is_empty() || al == self_l || al == from_l {
                continue;
            }
            if !cc.iter().any(|x| x.eq_ignore_ascii_case(a)) {
                cc.push(a.to_string());
            }
        }
    }
    prefill.cc = cc.join(", ");
    prefill
}

fn forward_prefill(m: &Message) -> ComposePrefill {
    let subject = if m.subject.to_lowercase().starts_with("fwd:") {
        m.subject.clone()
    } else {
        format!("Fwd: {}", m.subject)
    };
    let text = message_text(&m.body);
    let header = format!(
        "---------- Forwarded message ----------\nFrom: {} <{}>\nDate: {}\nSubject: {}",
        m.from_name, m.from_addr, m.date, m.subject
    );
    ComposePrefill {
        to: String::new(),
        cc: String::new(),
        subject,
        body_html: quote_block(&header, &text),
        draft_origin: None,
    }
}

/// Build the HTML quoted block (attribution line + blockquote) for a reply or
/// forward, from plain text so no scripts/remote content leak into the editor.
fn quote_block(attribution: &str, text: &str) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('\n', "<br>")
    };
    format!(
        "<p class=\"vireo-quote-attr\">{}</p><blockquote>{}</blockquote>",
        esc(attribution),
        esc(text)
    )
}

/// A readable plain-text rendering of a message body, which may be HTML. Used to
/// build safe quoted replies/forwards (no scripts, styles or remote content).
fn message_text(body: &str) -> String {
    if !body.contains('<') {
        return body.trim().to_string();
    }
    let mut s = strip_block(body, "script");
    s = strip_block(&s, "style");
    s = strip_block(&s, "head");
    // Turn common block/line elements into newlines.
    for (tag, nl) in [
        ("<br>", "\n"), ("<br/>", "\n"), ("<br />", "\n"),
        ("</p>", "\n\n"), ("</div>", "\n"), ("</li>", "\n"),
        ("</tr>", "\n"), ("</h1>", "\n"), ("</h2>", "\n"), ("</h3>", "\n"),
    ] {
        s = s.replace(tag, nl);
        s = s.replace(&tag.to_uppercase(), nl);
    }
    // Strip remaining tags.
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    // Decode the handful of entities that matter for plain text.
    let out = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    // Collapse runs of blank lines.
    let mut result = String::new();
    let mut blanks = 0;
    for line in out.lines() {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks <= 1 {
                result.push('\n');
            }
        } else {
            blanks = 0;
            result.push_str(line.trim_end());
            result.push('\n');
        }
    }
    result.trim().to_string()
}

/// Remove `<tag>…</tag>` blocks (case-insensitive) from HTML.
fn strip_block(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::new();
    let mut i = 0;
    while i < html.len() {
        if lower[i..].starts_with(&open) {
            if let Some(rel) = lower[i..].find(&close) {
                i += rel + close.len();
                continue;
            } else {
                break; // unterminated — drop the rest
            }
        }
        let ch = html[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn kind_label(kind: FolderKind) -> &'static str {
    match kind {
        FolderKind::Archive => "archive",
        FolderKind::Trash => "trash",
        _ => "destination",
    }
}

/// Flatten every folder's indexed messages into one pool for cross-folder search.
/// The map is keyed by `(account_id, folder_id)`, so a flat concatenation already
/// spans every folder of every account with no duplicates.
fn build_search_pool(cache: &HashMap<(u32, u32), Vec<Message>>) -> Vec<Message> {
    cache.values().flatten().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(account_id: u32, folder_id: u32, uid: u32) -> Message {
        Message {
            id: uid,
            account_id,
            folder_id,
            uid,
            from_name: String::new(),
            from_addr: String::new(),
            to: String::new(),
            cc: String::new(),
            subject: String::new(),
            preview: String::new(),
            body: String::new(),
            date: String::new(),
            timestamp: 0,
            unread: false,
            starred: false,
            has_attachment: false,
            message_id: String::new(),
            references: String::new(),
        }
    }

    #[test]
    fn search_pool_spans_every_folder_and_account() {
        let mut cache: HashMap<(u32, u32), Vec<Message>> = HashMap::new();
        cache.insert((1, 10), vec![msg(1, 10, 1), msg(1, 10, 2)]); // acct 1, inbox
        cache.insert((1, 11), vec![msg(1, 11, 3)]); // acct 1, archive
        cache.insert((2, 20), vec![msg(2, 20, 4), msg(2, 20, 5)]); // acct 2, inbox

        let pool = build_search_pool(&cache);
        assert_eq!(pool.len(), 5, "pool must include every folder's messages");

        let folders: std::collections::HashSet<(u32, u32)> =
            pool.iter().map(|m| (m.account_id, m.folder_id)).collect();
        assert_eq!(folders.len(), 3, "pool must span all three folders");
    }

    #[test]
    fn search_pool_is_empty_when_nothing_indexed() {
        assert!(build_search_pool(&HashMap::new()).is_empty());
    }
}
