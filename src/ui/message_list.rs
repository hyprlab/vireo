//! Middle pane: the scrollable list of messages in the selected folder,
//! with a search field and live filtering.

use adw::prelude::*;
use relm4::factory::FactoryVecDeque;
use relm4::prelude::*;

use crate::models::Message;

/// Max rows rendered at once. GtkListBox isn't virtualized, so the full folder
/// index is kept in memory for search but only this many rows are built.
const RENDER_CAP: usize = 200;

/// An action chosen from a message's right-click context menu.
#[derive(Debug, Clone, Copy)]
pub enum RowAction {
    Reply,
    ReplyAll,
    Forward,
    ToggleStar,
    ToggleRead,
    Spam,
    Archive,
    Delete,
    ViewSource,
}

/// Init for a row: the message, Gravatar flag, and optional account-ring class
/// (the account colour drawn as a ring around the avatar in the unified view).
pub struct RowInit {
    pub msg: Message,
    pub gravatar: bool,
    pub ring_class: Option<String>,
    /// Shared Actions Palette collapse delay in seconds — how long it stays open
    /// after the cursor leaves it (read live when scheduling).
    pub palette_collapse_secs: std::rc::Rc<std::cell::Cell<u64>>,
    /// Number of messages in this conversation (only set on a thread head; 1 for
    /// a standalone message).
    pub thread_count: usize,
    /// This row is a (newer/older) reply nested under a thread head — indent it.
    pub is_thread_child: bool,
    /// Whether this thread head is currently expanded.
    pub thread_expanded: bool,
    /// The conversation key, set on a thread head so its chevron can toggle it.
    pub thread_key: Option<(u32, String)>,
    /// Any message in this conversation is unread (thread heads only) — keeps
    /// the head marked unread while unread replies are hidden beneath it.
    pub thread_unread: bool,
}

/// One message summary row.
pub struct MessageRow {
    msg: Message,
    gravatar: bool,
    avatar_texture: Option<gtk::gdk::Texture>,
    ring_class: Option<String>,
    /// Whether the pointer is over this row (drives the chevron fade).
    row_hovered: bool,
    /// Whether the Actions Palette is slid open on this row.
    palette_open: bool,
    /// Pending auto-collapse timer (armed when the cursor isn't over the palette;
    /// cancelled while it is, so the palette stays open).
    collapse_timer: Option<gtk::glib::SourceId>,
    /// Shared collapse delay (seconds) after the cursor leaves the palette.
    palette_collapse_secs: std::rc::Rc<std::cell::Cell<u64>>,
    /// Conversation size (only meaningful on a thread head).
    thread_count: usize,
    /// Nested reply under a thread head.
    is_thread_child: bool,
    /// Whether this head's conversation is expanded.
    thread_expanded: bool,
    /// Conversation key for the head's expand/collapse toggle.
    thread_key: Option<(u32, String)>,
    /// Any message in this conversation is unread (heads only).
    thread_unread: bool,
}

#[derive(Debug)]
pub enum MessageRowInput {
    SetRead(bool),
    SetStarred(bool),
    SetHasAttachment(bool),
    /// The pointer entered/left the row — fade the chevron in/out.
    SetRowHover(bool),
    /// The chevron was clicked — slide the Actions Palette open or shut.
    TogglePalette,
    /// The cursor moved onto the palette — keep it open (cancel auto-collapse).
    PaletteEnter,
    /// The cursor left the palette — arm the auto-collapse countdown.
    PaletteLeave,
    /// The auto-collapse countdown elapsed — slide the palette shut.
    CollapsePalette,
    Action(RowAction),
    /// The conversation chevron was clicked — expand/collapse the thread.
    ToggleThreadClicked,
    /// The thread's aggregate unread state changed (a hidden reply was read).
    SetThreadUnread(bool),
}

#[derive(Debug)]
pub enum MessageRowOutput {
    Action { action: RowAction, message: Box<Message> },
    ToggleThread((u32, String)),
}

#[relm4::factory(pub)]
impl FactoryComponent for MessageRow {
    type Init = RowInit;
    type Input = MessageRowInput;
    type Output = MessageRowOutput;
    type CommandOutput = (
        String,
        u64,
        crate::avatar::FetchMode,
        crate::avatar::FetchOutcome,
    );
    type ParentWidget = gtk::ListBox;

    view! {
        gtk::ListBoxRow {
            // Unread rows get a pale accent background (cleared once read);
            // thread replies are indented.
            #[watch]
            set_css_classes: &self.row_css(),

            // Track hover so the Actions Palette chevron can fade in/out.
            add_controller = gtk::EventControllerMotion {
                connect_enter[sender] => move |_, _, _| sender.input(MessageRowInput::SetRowHover(true)),
                connect_leave[sender] => move |_| sender.input(MessageRowInput::SetRowHover(false)),
            },

            // Drag a message onto a sidebar folder to move it there. The payload
            // carries the account, source folder, UID and id.
            add_controller = gtk::DragSource {
                set_actions: gtk::gdk::DragAction::MOVE,
                connect_prepare[aid = self.msg.account_id, fid = self.msg.folder_id, uid = self.msg.uid, id = self.msg.id] => move |_, _, _| {
                    let payload = format!("vireo-move\t{aid}\t{fid}\t{uid}\t{id}");
                    Some(gtk::gdk::ContentProvider::for_value(&payload.to_value()))
                },
            },

            #[wrap(Some)]
            set_child = &gtk::Box {
            set_orientation: gtk::Orientation::Horizontal,
            set_spacing: 10,
            add_css_class: "message-row",

            adw::Avatar {
                set_size: 38,
                set_valign: gtk::Align::Center,
                set_show_initials: true,
                // Account colour ring (unified view only).
                set_css_classes: &self.ring_classes(),
                #[watch]
                set_text: Some(self.msg.from_name.as_str()),
                #[watch]
                set_custom_image: self.avatar_texture.as_ref(),
            },

            gtk::Box {
                add_css_class: "unread-dot",
                set_valign: gtk::Align::Center,
                #[watch]
                set_visible: self.msg.unread || self.thread_unread,
            },

            gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                set_spacing: 2,
                set_hexpand: true,

                gtk::Box {
                    set_spacing: 6,
                    gtk::Label {
                        set_label: &self.msg.from_name,
                        set_halign: gtk::Align::Start,
                        set_hexpand: true,
                        set_ellipsize: gtk::pango::EllipsizeMode::End,
                        #[watch]
                        set_css_classes: &self.sender_classes(),
                    },
                    gtk::Image {
                        set_icon_name: Some("co.hyprlab.Vireo-mail-attachment-symbolic"),
                        #[watch]
                        set_visible: self.msg.has_attachment,
                        add_css_class: "dim-icon",
                    },
                    gtk::Image {
                        set_icon_name: Some("co.hyprlab.Vireo-starred-symbolic"),
                        #[watch]
                        set_visible: self.msg.starred,
                        add_css_class: "star-icon",
                    },
                    gtk::Label {
                        set_label: &self.msg.datetime_list(),
                        set_halign: gtk::Align::End,
                        add_css_class: "message-date",
                    },
                    // Conversation size + expand/collapse (thread heads only).
                    gtk::Label {
                        set_label: &self.thread_count.to_string(),
                        set_visible: self.thread_count > 1,
                        add_css_class: "thread-count",
                        set_valign: gtk::Align::Center,
                    },
                    gtk::Button {
                        set_visible: self.thread_count > 1,
                        set_icon_name: if self.thread_expanded { "co.hyprlab.Vireo-pan-down-symbolic" } else { "co.hyprlab.Vireo-pan-end-symbolic" },
                        set_tooltip_text: Some("Show conversation"),
                        add_css_class: "flat",
                        add_css_class: "thread-toggle",
                        set_valign: gtk::Align::Center,
                        connect_clicked[sender] => move |_| sender.input(MessageRowInput::ToggleThreadClicked),
                    },
                },

                gtk::Label {
                    set_label: &self.msg.subject,
                    set_halign: gtk::Align::Start,
                    set_ellipsize: gtk::pango::EllipsizeMode::End,
                    #[watch]
                    set_css_classes: &self.subject_classes(),
                },

                // Bottom line: a ⋯ button that slides the Actions Palette in from the
                // left, over the preview text (so the row height doesn't change).
                gtk::Box {
                    set_spacing: 2,
                    set_valign: gtk::Align::Center,

                    // Actions toggle (⋯). Clicking it opens/closes the palette but
                    // does NOT select or open the message (it's a button, so the
                    // click is consumed before the row's selection gesture).
                    gtk::Button {
                        set_icon_name: "co.hyprlab.Vireo-view-more-horizontal-symbolic",
                        // Hidden until the row is hovered (or the palette is open);
                        // the .revealed class fades it in via a CSS transition.
                        #[watch]
                        set_css_classes: &self.chevron_classes(),
                        set_tooltip_text: Some("Actions"),
                        set_valign: gtk::Align::Center,
                        connect_clicked[sender] => move |_| sender.input(MessageRowInput::TogglePalette),
                    },

                    gtk::Revealer {
                        set_transition_type: gtk::RevealerTransitionType::SlideRight,
                        set_transition_duration: 180,
                        #[watch]
                        set_reveal_child: self.palette_open,

                        gtk::Box {
                            add_css_class: "actions-palette",
                            set_halign: gtk::Align::Start,
                            set_valign: gtk::Align::Center,
                            set_spacing: 0,

                            // Keep the palette open while the cursor is over it.
                            add_controller = gtk::EventControllerMotion {
                                connect_enter[sender] => move |_, _, _| sender.input(MessageRowInput::PaletteEnter),
                                connect_leave[sender] => move |_| sender.input(MessageRowInput::PaletteLeave),
                            },

                            gtk::Button {
                                set_icon_name: "co.hyprlab.Vireo-mail-reply-sender-symbolic",
                                set_tooltip_text: Some("Reply"),
                                add_css_class: "flat",
                                connect_clicked[sender] => move |_| sender.input(MessageRowInput::Action(RowAction::Reply)),
                            },
                            gtk::Button {
                                set_icon_name: "co.hyprlab.Vireo-mail-reply-all-symbolic",
                                set_tooltip_text: Some("Reply All"),
                                add_css_class: "flat",
                                connect_clicked[sender] => move |_| sender.input(MessageRowInput::Action(RowAction::ReplyAll)),
                            },
                            gtk::Button {
                                set_icon_name: "co.hyprlab.Vireo-mail-forward-symbolic",
                                set_tooltip_text: Some("Forward"),
                                add_css_class: "flat",
                                connect_clicked[sender] => move |_| sender.input(MessageRowInput::Action(RowAction::Forward)),
                            },
                            gtk::Button {
                                #[watch]
                                set_icon_name: if self.msg.starred { "co.hyprlab.Vireo-starred-symbolic" } else { "co.hyprlab.Vireo-non-starred-symbolic" },
                                #[watch]
                                set_tooltip_text: Some(if self.msg.starred { "Remove star" } else { "Star" }),
                                add_css_class: "flat",
                                connect_clicked[sender] => move |_| sender.input(MessageRowInput::Action(RowAction::ToggleStar)),
                            },
                            gtk::Button {
                                #[watch]
                                set_icon_name: if self.msg.unread { "co.hyprlab.Vireo-mail-read-symbolic" } else { "co.hyprlab.Vireo-mail-unread-symbolic" },
                                #[watch]
                                set_tooltip_text: Some(if self.msg.unread { "Mark as read" } else { "Mark as unread" }),
                                add_css_class: "flat",
                                connect_clicked[sender] => move |_| sender.input(MessageRowInput::Action(RowAction::ToggleRead)),
                            },
                            gtk::Button {
                                set_icon_name: "co.hyprlab.Vireo-mail-mark-junk-symbolic",
                                set_tooltip_text: Some("Mark as spam"),
                                add_css_class: "flat",
                                connect_clicked[sender] => move |_| sender.input(MessageRowInput::Action(RowAction::Spam)),
                            },
                            gtk::Button {
                                set_icon_name: "co.hyprlab.Vireo-mail-archive-symbolic",
                                set_tooltip_text: Some("Archive"),
                                add_css_class: "flat",
                                connect_clicked[sender] => move |_| sender.input(MessageRowInput::Action(RowAction::Archive)),
                            },
                            gtk::Button {
                                set_icon_name: "co.hyprlab.Vireo-user-trash-symbolic",
                                set_tooltip_text: Some("Delete"),
                                add_css_class: "flat",
                                connect_clicked[sender] => move |_| sender.input(MessageRowInput::Action(RowAction::Delete)),
                            },
                            gtk::Button {
                                set_icon_name: "co.hyprlab.Vireo-background-app-ghost-symbolic",
                                set_tooltip_text: Some("View source"),
                                add_css_class: "flat",
                                connect_clicked[sender] => move |_| sender.input(MessageRowInput::Action(RowAction::ViewSource)),
                            },
                        },
                    },

                    // After the palette so its collapse (slide back left) isn't
                    // shoved rightward by the preview reappearing. Hidden while open.
                    gtk::Label {
                        #[watch]
                        set_visible: !self.palette_open,
                        set_label: &self.msg.preview,
                        set_halign: gtk::Align::Start,
                        set_hexpand: true,
                        set_valign: gtk::Align::Center,
                        set_ellipsize: gtk::pango::EllipsizeMode::End,
                        set_lines: 1,
                        add_css_class: "message-preview",
                    },
                },
            },
        }
        }
    }

    fn init_model(init: Self::Init, _index: &DynamicIndex, sender: FactorySender<Self>) -> Self {
        let RowInit {
            msg,
            gravatar,
            ring_class,
            palette_collapse_secs,
            thread_count,
            is_thread_child,
            thread_expanded,
            thread_key,
            thread_unread,
        } = init;
        let mut model = Self {
            msg,
            gravatar,
            avatar_texture: None,
            ring_class,
            row_hovered: false,
            palette_open: false,
            collapse_timer: None,
            palette_collapse_secs,
            thread_count,
            is_thread_child,
            thread_expanded,
            thread_key,
            thread_unread,
        };

        let email = model.msg.from_addr.clone();
        if !email.is_empty() {
            match crate::avatar::lookup(&email, model.gravatar) {
                crate::avatar::CacheLookup::Texture(texture) => {
                    model.avatar_texture = Some(texture);
                }
                crate::avatar::CacheLookup::Missing => {}
                crate::avatar::CacheLookup::Fetch { generation, mode } => {
                    let requested_email = email.clone();
                    sender.oneshot_command(async move {
                        let lookup_email = requested_email.clone();
                        let outcome = tokio::task::spawn_blocking(move || {
                            crate::avatar::fetch(&lookup_email, mode)
                        })
                        .await
                        .unwrap_or(crate::avatar::FetchOutcome::Retry);
                        (requested_email, generation, mode, outcome)
                    });
                }
            }
        }

        model
    }

    fn update(&mut self, msg: Self::Input, sender: FactorySender<Self>) {
        match msg {
            MessageRowInput::SetRead(read) => self.msg.unread = !read,
            MessageRowInput::SetStarred(starred) => self.msg.starred = starred,
            MessageRowInput::SetHasAttachment(has) => self.msg.has_attachment = has,
            MessageRowInput::SetRowHover(over) => self.row_hovered = over,
            MessageRowInput::TogglePalette => {
                if self.palette_open {
                    self.palette_open = false;
                    self.cancel_collapse();
                } else {
                    self.palette_open = true;
                    // Persist briefly; moving onto the palette cancels this.
                    self.arm_collapse(&sender);
                }
            }
            MessageRowInput::PaletteEnter => self.cancel_collapse(),
            MessageRowInput::PaletteLeave => {
                if self.palette_open {
                    self.arm_collapse(&sender);
                }
            }
            MessageRowInput::CollapsePalette => {
                self.collapse_timer = None;
                self.palette_open = false;
            }
            MessageRowInput::Action(action) => {
                let _ = sender.output(MessageRowOutput::Action {
                    action,
                    message: Box::new(self.msg.clone()),
                });
            }
            MessageRowInput::ToggleThreadClicked => {
                if let Some(key) = self.thread_key.clone() {
                    let _ = sender.output(MessageRowOutput::ToggleThread(key));
                }
            }
            MessageRowInput::SetThreadUnread(unread) => self.thread_unread = unread,
        }
    }

    fn update_cmd(
        &mut self,
        (email, generation, mode, outcome): Self::CommandOutput,
        sender: FactorySender<Self>,
    ) {
        let retry_stale = crate::avatar::cache_result(&email, generation, mode, outcome);
        if self.msg.from_addr.eq_ignore_ascii_case(&email) {
            match crate::avatar::lookup(&email, self.gravatar) {
                crate::avatar::CacheLookup::Texture(texture) => {
                    self.avatar_texture = Some(texture);
                }
                crate::avatar::CacheLookup::Missing => self.avatar_texture = None,
                crate::avatar::CacheLookup::Fetch { generation, mode } => {
                    self.avatar_texture = None;
                    if retry_stale {
                        let requested_email = email.clone();
                        sender.oneshot_command(async move {
                            let lookup_email = requested_email.clone();
                            let outcome = tokio::task::spawn_blocking(move || {
                                crate::avatar::fetch(&lookup_email, mode)
                            })
                            .await
                            .unwrap_or(crate::avatar::FetchOutcome::Retry);
                            (requested_email, generation, mode, outcome)
                        });
                    }
                }
            }
        }
    }

    fn shutdown(&mut self, _widgets: &mut Self::Widgets, _output: relm4::Sender<Self::Output>) {
        // Cancel a pending collapse timer so it can't fire into this (now dropped)
        // component's shut-down runtime when the row is removed during a rebuild.
        self.cancel_collapse();
    }
}

/// Fire `DayChanged` shortly after the next local midnight (re-armed each time).
fn schedule_midnight_refresh(sender: &ComponentSender<MessageList>) {
    use chrono::Timelike;
    let secs = 86_400u32
        .saturating_sub(chrono::Local::now().num_seconds_from_midnight())
        .saturating_add(2);
    let input = sender.input_sender().clone();
    gtk::glib::timeout_add_seconds_local(secs, move || {
        let _ = input.send(MessageListInput::DayChanged);
        gtk::glib::ControlFlow::Break
    });
}

impl MessageRow {
    /// Cancel any pending auto-collapse (e.g. the cursor is now over the palette).
    fn cancel_collapse(&mut self) {
        if let Some(id) = self.collapse_timer.take() {
            id.remove();
        }
    }

    /// (Re)start the auto-collapse countdown from the shared preference (min 1s).
    fn arm_collapse(&mut self, sender: &FactorySender<Self>) {
        self.cancel_collapse();
        let secs = self.palette_collapse_secs.get().max(1);
        let s = sender.clone();
        self.collapse_timer = Some(gtk::glib::timeout_add_seconds_local_once(
            secs as u32,
            move || s.input(MessageRowInput::CollapsePalette),
        ));
    }

    /// Chevron classes: shown (`revealed`) while the row is hovered or the palette
    /// is open; a CSS opacity transition fades it in/out.
    fn chevron_classes(&self) -> Vec<&'static str> {
        let mut v = vec!["flat", "palette-toggle"];
        if self.row_hovered || self.palette_open {
            v.push("revealed");
        }
        v
    }

    fn ring_classes(&self) -> Vec<&str> {
        match &self.ring_class {
            Some(c) => vec![c.as_str()],
            None => Vec::new(),
        }
    }

    /// Row classes: unread highlight plus a `thread-child` indent for replies.
    /// A thread head with unread messages anywhere in its conversation gets the
    /// heavier `thread-unread` highlight until every one of them is read.
    fn row_css(&self) -> Vec<&'static str> {
        let mut v = row_classes(&self.msg);
        if self.thread_unread {
            v.push("thread-unread");
        }
        if self.is_thread_child {
            v.push("thread-child");
        }
        v
    }

    /// Unread for display purposes: the message itself, or (on a thread head)
    /// any message hidden in its conversation.
    fn display_unread(&self) -> bool {
        self.msg.unread || self.thread_unread
    }

    fn sender_classes(&self) -> Vec<&'static str> {
        if self.display_unread() {
            vec!["message-sender", "unread"]
        } else {
            vec!["message-sender"]
        }
    }

    fn subject_classes(&self) -> Vec<&'static str> {
        if self.display_unread() {
            vec!["message-subject", "unread"]
        } else {
            vec!["message-subject"]
        }
    }
}

/// Sort messages in place by the chosen order (stable; ties fall back to date).
fn sort_messages(msgs: &mut [Message], order: SortOrder) {
    let name_of = |m: &Message| {
        if m.from_name.trim().is_empty() {
            m.from_addr.to_lowercase()
        } else {
            m.from_name.to_lowercase()
        }
    };
    match order {
        SortOrder::DateNewest => msgs.sort_by(|a, b| b.timestamp.cmp(&a.timestamp)),
        SortOrder::DateOldest => msgs.sort_by(|a, b| a.timestamp.cmp(&b.timestamp)),
        SortOrder::Sender => msgs.sort_by(|a, b| {
            name_of(a).cmp(&name_of(b)).then(b.timestamp.cmp(&a.timestamp))
        }),
        SortOrder::Subject => msgs.sort_by(|a, b| {
            normalize_subject(&a.subject)
                .cmp(&normalize_subject(&b.subject))
                .then(b.timestamp.cmp(&a.timestamp))
        }),
        // `true` sorts after `false`, so compare b-vs-a to put unread/flagged first.
        SortOrder::UnreadFirst => {
            msgs.sort_by(|a, b| b.unread.cmp(&a.unread).then(b.timestamp.cmp(&a.timestamp)))
        }
        SortOrder::FlaggedFirst => {
            msgs.sort_by(|a, b| b.starred.cmp(&a.starred).then(b.timestamp.cmp(&a.timestamp)))
        }
    }
}

/// The conversation key a message belongs to: its owning account plus the
/// subject with reply/forward prefixes stripped. Messages with no subject get a
/// per-message key (by UID) so they never group together.
/// Lower-case the subject and strip any leading run of reply/forward prefixes
/// (`Re:`, `Fwd:`, …) so subject-sorting keeps a topic and its replies adjacent.
fn normalize_subject(subject: &str) -> String {
    const PREFIXES: &[&str] = &["re:", "fwd:", "fw:", "aw:", "sv:", "antw:", "wg:"];
    let mut s = subject.trim();
    loop {
        let lower = s.to_ascii_lowercase();
        let mut stripped = false;
        for p in PREFIXES {
            if lower.starts_with(p) {
                s = s[p.len()..].trim_start();
                stripped = true;
                break;
            }
        }
        if !stripped {
            break;
        }
    }
    s.trim().to_ascii_lowercase()
}

/// Group messages into conversations by their reply headers (Message-ID linked
/// via In-Reply-To / References), scoped per account. Returns each message's
/// thread key `(account_id, root)`. Messages with no reply relationship get a
/// unique key (a thread of one) — so unrelated messages that merely share a
/// subject are never threaded together.
fn compute_thread_keys(
    msgs: &[Message],
) -> std::collections::HashMap<(u32, u32), (u32, String)> {
    use std::collections::HashMap;

    // Union-find over message-id nodes (namespaced by account).
    let mut parent: HashMap<String, String> = HashMap::new();
    fn find(parent: &mut HashMap<String, String>, x: &str) -> String {
        let mut cur = x.to_string();
        while let Some(p) = parent.get(&cur) {
            if p == &cur {
                break;
            }
            cur = p.clone();
        }
        cur
    }
    fn union(parent: &mut HashMap<String, String>, a: &str, b: &str) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            parent.insert(ra, rb);
        }
    }
    // A message with its own Message-ID is a real node; one without gets a unique
    // node keyed by uid so it only links through its references (if any).
    let self_node = |m: &Message| -> String {
        if m.message_id.is_empty() {
            format!("{}\u{0}uid{}", m.account_id, m.uid)
        } else {
            format!("{}\u{0}{}", m.account_id, m.message_id)
        }
    };

    for m in msgs {
        let sn = self_node(m);
        parent.entry(sn.clone()).or_insert_with(|| sn.clone());
        for r in m.references.split_whitespace() {
            let rn = format!("{}\u{0}{}", m.account_id, r);
            parent.entry(rn.clone()).or_insert_with(|| rn.clone());
            union(&mut parent, &sn, &rn);
        }
    }

    let mut out = HashMap::new();
    for m in msgs {
        let root = find(&mut parent, &self_node(m));
        out.insert((m.account_id, m.id), (m.account_id, root));
    }
    out
}

/// Style classes for a row: highlight unread messages with a pale accent.
fn row_classes(m: &Message) -> Vec<&'static str> {
    if m.unread {
        vec!["message-unread"]
    } else {
        Vec::new()
    }
}

pub struct MessageList {
    rows: FactoryVecDeque<MessageRow>,
    /// All messages for the current folder (full searchable index).
    all: Vec<Message>,
    /// Every folder's messages (all accounts), supplied by the app while a search
    /// is active, so `AllFolders` scope can filter across the whole mailbox. Empty
    /// when not searching.
    search_pool: Vec<Message>,
    /// Which messages the search field filters over.
    scope: SearchScope,
    /// The search field widget, kept so a folder switch can clear its text.
    search_entry: Option<gtk::SearchEntry>,
    /// Currently displayed (post-filter, capped) messages, aligned with rows.
    shown: Vec<Message>,
    /// Total messages matching the current filter (may exceed what's rendered).
    total_matches: usize,
    query: String,
    title: String,
    gravatar: bool,
    /// Tint each row by its account (used in the unified inbox view).
    colorize: bool,
    /// account_id → avatar colour, for tinting rows.
    account_colors: std::collections::HashMap<u32, String>,
    /// Display-wide provider with each account's pale row-tint rule.
    color_provider: gtk::CssProvider,
    /// Reusable popover for the per-message right-click menu.
    context_popover: gtk::Popover,
    /// Actions Palette collapse delay (seconds), shared with every row.
    palette_collapse_secs: std::rc::Rc<std::cell::Cell<u64>>,
    /// The message currently being viewed, kept selected across list rebuilds.
    /// Keyed by (account_id, id) since UIDs collide across accounts in the
    /// unified "All Inboxes" view.
    selected_id: Option<(u32, u32)>,
    /// Every selected message key, so the whole selection survives list rebuilds
    /// (background syncs) until the user clicks away.
    selected_ids: Vec<(u32, u32)>,
    /// How many rows are currently selected (drives the bulk-action bar).
    selection_count: usize,
    /// Conversation keys the user has toggled away from the default state
    /// (expanded when the default is collapsed, and vice versa).
    expanded_threads: std::collections::HashSet<(u32, String)>,
    /// Whether conversations start expanded (user preference; collapsed default).
    default_expanded: bool,
    /// Rendered thread membership: message key → conversation key, rebuilt with
    /// the rows. Lets a read-state change on a hidden reply refresh its head.
    msg_thread: std::collections::HashMap<(u32, u32), (u32, String)>,
    /// Conversation key → member message keys (multi-message threads only).
    thread_members: std::collections::HashMap<(u32, String), Vec<(u32, u32)>>,
    /// Messages actually rendered (after the render limit), independent of how
    /// many rows are visible once threads are collapsed.
    rendered_count: usize,
    /// How many messages to render — grows by `RENDER_CAP` each time the user
    /// scrolls to the bottom (infinite scroll). Reset on folder switch / search.
    render_limit: usize,
    /// Whether the folder's background index is fully loaded. When false, more
    /// rows may still stream in, so hitting the bottom shows a loading spinner.
    index_complete: bool,
    /// The list's scroller, kept so expand/collapse can preserve scroll position.
    scroller: Option<gtk::ScrolledWindow>,
    /// Current sort order for the list.
    sort: SortOrder,
    /// Group messages into conversation threads (user preference).
    threading: bool,
    /// When `Some`, a spinner + this message overlays the list — shown while a
    /// large bulk action (archive/delete/spam) is applied.
    busy: Option<String>,
}

/// How the message list is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    DateNewest,
    DateOldest,
    Sender,
    Subject,
    UnreadFirst,
    FlaggedFirst,
}

impl SortOrder {
    fn from_key(key: &str) -> Self {
        match key {
            "date_oldest" => SortOrder::DateOldest,
            "sender" => SortOrder::Sender,
            "subject" => SortOrder::Subject,
            "unread" => SortOrder::UnreadFirst,
            "flagged" => SortOrder::FlaggedFirst,
            _ => SortOrder::DateNewest,
        }
    }
    fn key(self) -> &'static str {
        match self {
            SortOrder::DateNewest => "date_newest",
            SortOrder::DateOldest => "date_oldest",
            SortOrder::Sender => "sender",
            SortOrder::Subject => "subject",
            SortOrder::UnreadFirst => "unread",
            SortOrder::FlaggedFirst => "flagged",
        }
    }
}

/// Which messages the search field filters over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    /// Every folder of every account (the merged `search_pool`).
    AllFolders,
    /// Only the folder currently shown (the local `all` index).
    ThisFolder,
}

/// A bulk action applied to every selected message at once.
#[derive(Debug, Clone, Copy)]
pub enum BulkAction {
    MarkRead,
    MarkUnread,
    Flag,
    Archive,
    Spam,
    Delete,
}

#[derive(Debug)]
pub enum MessageListInput {
    SetMessages { title: String, messages: Vec<Message> },
    /// Merge more indexed messages into the current list (background backfill),
    /// preserving the current search query and view.
    AppendMessages { messages: Vec<Message> },
    SetLoading { title: String },
    SetThreading(bool),
    /// Whether conversations start expanded (true) or collapsed (false).
    SetThreadsExpanded(bool),
    SetGravatar(bool),
    ContactPhotosChanged,
    SetColorize(bool),
    /// The local day rolled over — re-render rows so "Today" stays accurate.
    DayChanged,
    SetAccountColors(std::collections::HashMap<u32, String>),
    Search(String),
    /// Change the search scope (all folders vs. the current folder).
    SetScope(SearchScope),
    /// Replace the cross-folder search pool (all folders, all accounts). Sent by
    /// the app when a search begins; cleared to empty when it ends.
    SetSearchPool(Vec<Message>),
    /// The set of selected rows changed (single click, Ctrl/Shift multi-select).
    SelectionChanged,
    /// A row was activated (double-click / Enter): pop it out into its own window.
    RowActivated(i32),
    /// Apply a bulk action to every selected message.
    Bulk(BulkAction),
    /// Deselect everything.
    ClearSelection,
    /// Expand/collapse a conversation thread.
    ToggleThread((u32, String)),
    /// Change the list sort order.
    SetSort(SortOrder),
    MarkRead(u32),
    SetRead { id: u32, read: bool },
    /// A hover-palette action for a specific message (forwarded to the app).
    RowAction { action: RowAction, message: Box<Message> },
    SetStarred { id: u32, starred: bool },
    /// Update a message's attachment indicator (e.g. clearing a false paperclip).
    SetHasAttachment { id: u32, has: bool },
    Remove(u32),
    /// Remove many messages in a single batch (bulk archive/delete/spam), so the
    /// list updates in one render pass instead of one per message.
    RemoveMany(Vec<u32>),
    /// Show (`Some(message)`) or hide (`None`) the busy spinner over the list.
    SetBusy(Option<String>),
    /// Secondary-click at (x, y) in the list: open the context menu.
    ContextMenu { x: f64, y: f64 },
    /// Set the Actions Palette auto-collapse delay (seconds).
    SetPaletteCollapse(u64),
    /// Folder switch: reset infinite-scroll paging back to the first page and
    /// scroll to the top (a plain `SetMessages` now preserves paging for refreshes).
    ResetPaging,
    /// The list was scrolled to the bottom — render the next page of messages.
    LoadMore,
    /// Whether the current folder's background index is fully loaded.
    SetIndexComplete(bool),
    /// Mark which message is being viewed so it stays highlighted across
    /// rebuilds; `None` clears the selection (e.g. on folder switch).
    SetSelected(Option<u32>),
    /// Select a message by `(account_id, id)` AND load it in the reader — used to
    /// advance after the viewed message is removed by a background sync.
    SelectAndLoad((u32, u32)),
}

#[derive(Debug)]
pub enum MessageListOutput {
    /// A message was selected. `thread` holds the whole conversation (newest
    /// first) when the newest/head row was chosen, so the reader can show it as a
    /// scrollable conversation; otherwise it's just `[message]`.
    Selected { message: Message, thread: Vec<Message> },
    /// A row was double-clicked: open the message in its own window.
    Activated(Message),
    /// A context-menu action chosen for a specific message.
    Action { action: RowAction, message: Box<Message> },
    /// A bulk action chosen for every currently-selected message.
    Bulk { action: BulkAction, messages: Vec<Message> },
    /// The viewed message was removed and no row remains to advance to, so the
    /// reader should clear.
    SelectionCleared,
    /// The search field became active (non-empty) or inactive (empty), so the app
    /// can supply or drop the cross-folder search pool.
    SearchActive(bool),
}

#[relm4::component(pub)]
impl SimpleComponent for MessageList {
    type Init = ();
    type Input = MessageListInput;
    type Output = MessageListOutput;

    view! {
        gtk::Box {
            set_orientation: gtk::Orientation::Vertical,
            add_css_class: "message-list-pane",

            gtk::Box {
                add_css_class: "list-toolbar",
                set_orientation: gtk::Orientation::Vertical,
                set_spacing: 8,

                gtk::Box {
                    set_spacing: 6,
                    gtk::Label {
                        #[watch]
                        set_label: &model.title,
                        set_halign: gtk::Align::Start,
                        set_hexpand: true,
                        add_css_class: "list-title",
                    },
                    gtk::Label {
                        #[watch]
                        set_label: &model.count_label(),
                        set_valign: gtk::Align::Center,
                        add_css_class: "list-count",
                    },
                    #[name = "sort_btn"]
                    gtk::MenuButton {
                        set_icon_name: "co.hyprlab.Vireo-view-sort-descending-symbolic",
                        set_tooltip_text: Some("Sort messages"),
                        set_valign: gtk::Align::Center,
                        add_css_class: "flat",
                    },
                },

                gtk::Box {
                    set_spacing: 6,

                    #[name = "search_entry"]
                    gtk::SearchEntry {
                        set_hexpand: true,
                        #[watch]
                        set_placeholder_text: Some(model.search_placeholder()),
                        connect_search_changed[sender] => move |entry| {
                            sender.input(MessageListInput::Search(entry.text().to_string()));
                        },
                    },

                    // Scope picker: 0 = All folders (default), 1 = This folder.
                    #[name = "scope_dropdown"]
                    gtk::DropDown {
                        set_valign: gtk::Align::Center,
                        set_tooltip_text: Some("Choose which folders to search"),
                        set_model: Some(&gtk::StringList::new(&["All folders", "This folder"])),
                        set_selected: 0,
                        connect_selected_notify[sender] => move |dd| {
                            let scope = if dd.selected() == 0 {
                                SearchScope::AllFolders
                            } else {
                                SearchScope::ThisFolder
                            };
                            sender.input(MessageListInput::SetScope(scope));
                        },
                    },
                },
            },

            // Bulk-action bar, revealed while more than one message is selected.
            gtk::Revealer {
                set_transition_type: gtk::RevealerTransitionType::SlideDown,
                #[watch]
                set_reveal_child: model.selection_count > 1,

                gtk::Box {
                    add_css_class: "bulk-bar",
                    set_spacing: 2,

                    gtk::Label {
                        #[watch]
                        set_label: &format!("{} selected", model.selection_count),
                        set_hexpand: true,
                        set_halign: gtk::Align::Start,
                        add_css_class: "bulk-count",
                    },
                    gtk::Button {
                        set_icon_name: "co.hyprlab.Vireo-mail-read-symbolic",
                        set_tooltip_text: Some("Mark as Read"),
                        add_css_class: "flat",
                        connect_clicked => MessageListInput::Bulk(BulkAction::MarkRead),
                    },
                    gtk::Button {
                        set_icon_name: "co.hyprlab.Vireo-mail-unread-symbolic",
                        set_tooltip_text: Some("Mark as Unread"),
                        add_css_class: "flat",
                        connect_clicked => MessageListInput::Bulk(BulkAction::MarkUnread),
                    },
                    gtk::Button {
                        set_icon_name: "co.hyprlab.Vireo-starred-symbolic",
                        set_tooltip_text: Some("Flag"),
                        add_css_class: "flat",
                        connect_clicked => MessageListInput::Bulk(BulkAction::Flag),
                    },
                    gtk::Button {
                        set_icon_name: "co.hyprlab.Vireo-mail-archive-symbolic",
                        set_tooltip_text: Some("Archive"),
                        add_css_class: "flat",
                        connect_clicked => MessageListInput::Bulk(BulkAction::Archive),
                    },
                    gtk::Button {
                        set_icon_name: "co.hyprlab.Vireo-mail-mark-junk-symbolic",
                        set_tooltip_text: Some("Mark as Spam"),
                        add_css_class: "flat",
                        connect_clicked => MessageListInput::Bulk(BulkAction::Spam),
                    },
                    gtk::Button {
                        set_icon_name: "co.hyprlab.Vireo-user-trash-symbolic",
                        set_tooltip_text: Some("Delete"),
                        add_css_class: "flat",
                        connect_clicked => MessageListInput::Bulk(BulkAction::Delete),
                    },
                    gtk::Separator {
                        set_orientation: gtk::Orientation::Vertical,
                    },
                    gtk::Button {
                        set_icon_name: "co.hyprlab.Vireo-edit-clear-symbolic",
                        set_tooltip_text: Some("Clear selection"),
                        add_css_class: "flat",
                        connect_clicked => MessageListInput::ClearSelection,
                    },
                },
            },

            gtk::Overlay {
                #[wrap(Some)]
                #[name = "scroller"]
                set_child = &gtk::ScrolledWindow {
                set_vexpand: true,
                set_hscrollbar_policy: gtk::PolicyType::Never,

                // Reaching the bottom pulls in the next page (and, if the index is
                // still loading, shows the spinner below until more arrive).
                connect_edge_reached[sender] => move |_, pos| {
                    if pos == gtk::PositionType::Bottom {
                        sender.input(MessageListInput::LoadMore);
                    }
                },

                gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,

                    #[local_ref]
                    row_box -> gtk::ListBox {
                        // Multiple selection: plain click selects one (shown in the
                        // reader), Ctrl/Shift extend the selection for bulk actions;
                        // double click (or Enter) pops a message out into its own window.
                        set_selection_mode: gtk::SelectionMode::Multiple,
                        set_activate_on_single_click: false,
                        add_css_class: "message-listbox",
                        connect_selected_rows_changed[sender] => move |_| {
                            sender.input(MessageListInput::SelectionChanged);
                        },
                        connect_row_activated[sender] => move |_, row| {
                            sender.input(MessageListInput::RowActivated(row.index()));
                        },
                    },

                    // Bottom loading indicator while the rest of the folder streams in.
                    gtk::Box {
                        add_css_class: "list-loading",
                        set_halign: gtk::Align::Center,
                        set_spacing: 8,
                        set_margin_top: 10,
                        set_margin_bottom: 14,
                        #[watch]
                        set_visible: model.is_loading_more(),

                        gtk::Spinner {
                            set_spinning: true,
                            set_width_request: 18,
                            set_height_request: 18,
                        },
                        gtk::Label {
                            set_label: "Loading more…",
                            add_css_class: "dim-label",
                        },
                    },
                },
                },

                add_overlay = &gtk::Box {
                    add_css_class: "bulk-busy",
                    set_halign: gtk::Align::Center,
                    set_valign: gtk::Align::Center,
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 12,
                    #[watch]
                    set_visible: model.busy.is_some(),

                    gtk::Spinner {
                        set_spinning: true,
                        set_width_request: 32,
                        set_height_request: 32,
                    },
                    gtk::Label {
                        #[watch]
                        set_label: model.busy.as_deref().unwrap_or(""),
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
        let rows = FactoryVecDeque::builder()
            .launch(gtk::ListBox::new())
            .forward(sender.input_sender(), |out| match out {
                MessageRowOutput::Action { action, message } => {
                    MessageListInput::RowAction { action, message }
                }
                MessageRowOutput::ToggleThread(key) => MessageListInput::ToggleThread(key),
            });

        let color_provider = gtk::CssProvider::new();
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &color_provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        let context_popover = gtk::Popover::new();
        context_popover.set_has_arrow(false);
        context_popover.set_position(gtk::PositionType::Bottom);
        context_popover.add_css_class("menu");

        let mut model = MessageList {
            rows,
            all: Vec::new(),
            search_pool: Vec::new(),
            scope: SearchScope::AllFolders,
            search_entry: None,
            shown: Vec::new(),
            total_matches: 0,
            render_limit: RENDER_CAP,
            index_complete: true,
            query: String::new(),
            title: String::new(),
            gravatar: false,
            colorize: false,
            account_colors: std::collections::HashMap::new(),
            color_provider,
            context_popover,
            palette_collapse_secs: std::rc::Rc::new(std::cell::Cell::new(5)),
            selected_id: None,
            selected_ids: Vec::new(),
            selection_count: 0,
            expanded_threads: std::collections::HashSet::new(),
            default_expanded: false,
            msg_thread: std::collections::HashMap::new(),
            thread_members: std::collections::HashMap::new(),
            rendered_count: 0,
            scroller: None,
            sort: SortOrder::DateNewest,
            threading: true,
            busy: None,
        };

        let row_box = model.rows.widget();
        model.context_popover.set_parent(row_box);

        // Right-click (or long-press) a row to open its context menu.
        let click = gtk::GestureClick::new();
        click.set_button(gtk::gdk::BUTTON_SECONDARY);
        let cs = sender.clone();
        click.connect_pressed(move |_, _, x, y| {
            cs.input(MessageListInput::ContextMenu { x, y });
        });
        row_box.add_controller(click);

        // Delete / Backspace on a focused row deletes the selection (single or
        // multi). Scoped to the list, so typing in the search box is unaffected.
        let key = gtk::EventControllerKey::new();
        let ks = sender.clone();
        key.connect_key_pressed(move |_, keyval, _, _| {
            if matches!(keyval, gtk::gdk::Key::Delete | gtk::gdk::Key::BackSpace) {
                ks.input(MessageListInput::Bulk(BulkAction::Delete));
                gtk::glib::Propagation::Stop
            } else {
                gtk::glib::Propagation::Proceed
            }
        });
        row_box.add_controller(key);

        let widgets = view_output!();
        model.scroller = Some(widgets.scroller.clone());
        model.search_entry = Some(widgets.search_entry.clone());

        // Sort menu: a stateful "order" action drives a radio-style menu, so the
        // active sort shows a checkmark.
        let sort_group = gtk::gio::SimpleActionGroup::new();
        let sort_action = gtk::gio::SimpleAction::new_stateful(
            "order",
            Some(gtk::glib::VariantTy::STRING),
            &model.sort.key().to_variant(),
        );
        let ss = sender.clone();
        sort_action.connect_activate(move |action, param| {
            if let Some(key) = param.and_then(|v| v.str()) {
                action.set_state(&key.to_variant());
                ss.input(MessageListInput::SetSort(SortOrder::from_key(key)));
            }
        });
        sort_group.add_action(&sort_action);
        widgets.sort_btn.insert_action_group("sortmenu", Some(&sort_group));

        let menu = gtk::gio::Menu::new();
        for (label, key) in [
            ("Date (Newest first)", "date_newest"),
            ("Date (Oldest first)", "date_oldest"),
            ("Sender (A–Z)", "sender"),
            ("Subject (A–Z)", "subject"),
            ("Unread first", "unread"),
            ("Flagged first", "flagged"),
        ] {
            menu.append(Some(label), Some(&format!("sortmenu.order::{key}")));
        }
        widgets.sort_btn.set_menu_model(Some(&menu));

        schedule_midnight_refresh(&sender);

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            MessageListInput::SetMessages { title, messages } => {
                self.title = title;
                self.all = messages;
                // Keep any active search query: this also fires for a background
                // re-sync of the folder you're viewing, which shouldn't drop your
                // search. Folder switches clear the query via `ResetPaging` first.
                self.rebuild_preserving_scroll();
            }
            MessageListInput::AppendMessages { messages } => {
                // Grow the searchable index in place. Dedup by (account, uid) since
                // UIDs collide across accounts in the unified inbox.
                let existing: std::collections::HashSet<(u32, u32)> =
                    self.all.iter().map(|m| (m.account_id, m.uid)).collect();
                let before = self.all.len();
                for m in messages {
                    if !existing.contains(&(m.account_id, m.uid)) {
                        self.all.push(m);
                    }
                }
                // Re-render when it could change what's visible: an active search, a
                // sort where older messages can surface at the top, or the user is
                // waiting at the bottom for more rows to fill the raised limit.
                let waiting_for_more = self.render_limit > self.rendered_count;
                if self.all.len() != before
                    && (!self.query.is_empty()
                        || self.sort != SortOrder::DateNewest
                        || waiting_for_more)
                {
                    self.rebuild_preserving_scroll();
                }
            }
            MessageListInput::SetLoading { title } => {
                self.title = title;
                self.all.clear();
                self.clear_search();
                self.render_limit = RENDER_CAP;
                self.rebuild();
            }
            MessageListInput::ResetPaging => {
                // Folder switch: drop any active search, back to the first page,
                // scrolled to the top.
                self.clear_search();
                self.render_limit = RENDER_CAP;
                if let Some(s) = &self.scroller {
                    s.vadjustment().set_value(0.0);
                }
            }
            MessageListInput::LoadMore => {
                // Show more if the index already has more, or if it's still loading
                // (the spinner covers the wait, and appended rows fill in).
                if self.rendered_count < self.total_matches || !self.index_complete {
                    self.render_limit = self.render_limit.saturating_add(RENDER_CAP);
                    self.rebuild_preserving_scroll();
                }
            }
            MessageListInput::SetIndexComplete(complete) => {
                self.index_complete = complete;
            }
            MessageListInput::SetThreading(on) => {
                if self.threading != on {
                    self.threading = on;
                    self.rebuild();
                }
            }
            MessageListInput::SetThreadsExpanded(on) => {
                if self.default_expanded != on {
                    self.default_expanded = on;
                    // Per-thread toggles were exceptions to the old default;
                    // drop them so everything follows the new one.
                    self.expanded_threads.clear();
                    self.rebuild();
                }
            }
            MessageListInput::SetGravatar(on) => {
                if self.gravatar != on {
                    self.gravatar = on;
                    self.rebuild();
                }
            }
            MessageListInput::ContactPhotosChanged => self.rebuild_preserving_scroll(),
            MessageListInput::SetColorize(on) => {
                if self.colorize != on {
                    self.colorize = on;
                    self.rebuild();
                }
            }
            MessageListInput::DayChanged => {
                // Re-render so relative labels like "Today" reflect the new date.
                self.rebuild();
                schedule_midnight_refresh(&sender);
            }
            MessageListInput::SetAccountColors(colors) => {
                self.account_colors = colors;
                self.refresh_tint_css();
                // Existing rows keep their classes; the rule update reaches them.
            }
            MessageListInput::Search(q) => {
                let was_active = self.searching();
                self.query = q;
                let now_active = self.searching();
                // On the empty↔non-empty edge, tell the app to supply or drop the
                // cross-folder pool.
                if was_active != now_active {
                    let _ = sender.output(MessageListOutput::SearchActive(now_active));
                }
                self.render_limit = RENDER_CAP;
                self.rebuild();
            }
            MessageListInput::SetScope(scope) => {
                if self.scope != scope {
                    self.scope = scope;
                    // Scope only affects the view while a query is present.
                    if self.searching() {
                        self.render_limit = RENDER_CAP;
                        self.rebuild();
                    }
                }
            }
            MessageListInput::SetSearchPool(pool) => {
                self.search_pool = pool;
                if self.searching() && self.scope == SearchScope::AllFolders {
                    self.render_limit = RENDER_CAP;
                    self.rebuild_preserving_scroll();
                }
            }
            MessageListInput::SelectionChanged => {
                let keys: Vec<(u32, u32)> = self
                    .rows
                    .widget()
                    .selected_rows()
                    .iter()
                    .filter_map(|r| self.shown.get(r.index() as usize).map(|m| (m.account_id, m.id)))
                    .collect();
                self.selection_count = keys.len();
                match keys.as_slice() {
                    [] => self.selected_id = None,
                    [key] => {
                        // Exactly one selected → show it in the reader. Skip when
                        // it's already the viewed row (e.g. programmatic restore
                        // after a rebuild) so it isn't needlessly reloaded.
                        if self.selected_id != Some(*key) {
                            self.selected_id = Some(*key);
                            if let Some(m) = self
                                .shown
                                .iter()
                                .find(|m| (m.account_id, m.id) == *key)
                                .cloned()
                            {
                                let thread = self.conversation_for(&m);
                                let _ = sender.output(MessageListOutput::Selected {
                                    message: m,
                                    thread,
                                });
                            }
                        }
                    }
                    // Multiple selected → keep the reader on the primary message.
                    _ => {}
                }
                self.selected_ids = keys;
            }
            MessageListInput::Bulk(action) => {
                let messages: Vec<Message> = self
                    .rows
                    .widget()
                    .selected_rows()
                    .iter()
                    .filter_map(|r| self.shown.get(r.index() as usize).cloned())
                    .collect();
                if !messages.is_empty() {
                    let _ = sender.output(MessageListOutput::Bulk { action, messages });
                }
                self.rows.widget().unselect_all();
                self.selected_id = None;
                self.selected_ids.clear();
                self.selection_count = 0;
            }
            MessageListInput::ClearSelection => {
                self.rows.widget().unselect_all();
                self.selected_id = None;
                self.selected_ids.clear();
                self.selection_count = 0;
            }
            MessageListInput::SetSort(order) => {
                if self.sort != order {
                    self.sort = order;
                    self.rebuild();
                }
            }
            MessageListInput::ToggleThread(key) => {
                if !self.expanded_threads.remove(&key) {
                    self.expanded_threads.insert(key);
                }
                // Preserve the scroll position (a plain rebuild jumps to the top).
                self.rebuild_preserving_scroll();
            }
            MessageListInput::RowActivated(index) => {
                if let Some(m) = self.shown.get(index as usize) {
                    let _ = sender.output(MessageListOutput::Activated(m.clone()));
                }
            }
            MessageListInput::MarkRead(id) => {
                if let Some(m) = self.all.iter_mut().find(|m| m.id == id) {
                    m.unread = false;
                }
                if let Some(idx) = self.shown.iter().position(|m| m.id == id) {
                    self.shown[idx].unread = false;
                    self.rows.send(idx, MessageRowInput::SetRead(true));
                }
                self.refresh_thread_unread(id);
            }
            MessageListInput::SetRead { id, read } => {
                if let Some(m) = self.all.iter_mut().find(|m| m.id == id) {
                    m.unread = !read;
                }
                if let Some(idx) = self.shown.iter().position(|m| m.id == id) {
                    self.shown[idx].unread = !read;
                    self.rows.send(idx, MessageRowInput::SetRead(read));
                }
                self.refresh_thread_unread(id);
            }
            MessageListInput::SetStarred { id, starred } => {
                if let Some(m) = self.all.iter_mut().find(|m| m.id == id) {
                    m.starred = starred;
                }
                if let Some(idx) = self.shown.iter().position(|m| m.id == id) {
                    self.shown[idx].starred = starred;
                    self.rows.send(idx, MessageRowInput::SetStarred(starred));
                }
            }
            MessageListInput::SetHasAttachment { id, has } => {
                if let Some(m) = self.all.iter_mut().find(|m| m.id == id) {
                    m.has_attachment = has;
                }
                if let Some(idx) = self.shown.iter().position(|m| m.id == id) {
                    self.shown[idx].has_attachment = has;
                    self.rows.send(idx, MessageRowInput::SetHasAttachment(has));
                }
            }
            MessageListInput::Remove(id) => {
                // Was the removed message the one shown in the reader? If so we'll
                // advance to whatever row slides into its place.
                let was_viewed = self.selected_id.map(|(_, i)| i) == Some(id);
                if was_viewed {
                    self.selected_id = None;
                }
                self.selected_ids.retain(|(_, i)| *i != id);
                self.all.retain(|m| m.id != id);
                let removed_idx = self.shown.iter().position(|m| m.id == id);
                if let Some(idx) = removed_idx {
                    self.shown.remove(idx);
                    self.rows.guard().remove(idx);
                }

                if was_viewed {
                    match removed_idx {
                        // Select the row now at the removed slot (or the new last
                        // row if we deleted the bottom one). Selecting it fires
                        // SelectionChanged, which loads it in the reader.
                        Some(idx) if !self.shown.is_empty() => {
                            let next = idx.min(self.shown.len() - 1);
                            if let Some(row) = self.rows.widget().row_at_index(next as i32) {
                                self.rows.widget().select_row(Some(&row));
                            }
                        }
                        // Nothing left to show → clear the reader.
                        _ => {
                            let _ = sender.output(MessageListOutput::SelectionCleared);
                        }
                    }
                }
            }
            MessageListInput::RemoveMany(ids) => {
                if ids.is_empty() {
                    return;
                }
                let set: std::collections::HashSet<u32> = ids.into_iter().collect();
                let was_viewed = self.selected_id.map(|(_, i)| i).is_some_and(|i| set.contains(&i));
                if was_viewed {
                    self.selected_id = None;
                }
                self.selected_ids.retain(|(_, i)| !set.contains(i));
                self.all.retain(|m| !set.contains(&m.id));
                // Where the first removed row sat, so we can re-select in its place.
                let first_removed = self.shown.iter().position(|m| set.contains(&m.id));
                // Remove all matching rows in one guarded batch (a single widget
                // update) instead of one render cycle per message. Walk back-to-front
                // so indices stay valid.
                {
                    let mut guard = self.rows.guard();
                    let mut idx = self.shown.len();
                    while idx > 0 {
                        idx -= 1;
                        if set.contains(&self.shown[idx].id) {
                            self.shown.remove(idx);
                            guard.remove(idx);
                        }
                    }
                }
                self.selection_count = self.selected_ids.len();
                if was_viewed {
                    match first_removed {
                        Some(idx) if !self.shown.is_empty() => {
                            let next = idx.min(self.shown.len() - 1);
                            if let Some(row) = self.rows.widget().row_at_index(next as i32) {
                                self.rows.widget().select_row(Some(&row));
                            }
                        }
                        _ => {
                            let _ = sender.output(MessageListOutput::SelectionCleared);
                        }
                    }
                }
            }
            MessageListInput::SetBusy(text) => {
                self.busy = text;
            }
            MessageListInput::RowAction { action, message } => {
                let _ = sender.output(MessageListOutput::Action { action, message });
            }
            MessageListInput::SetPaletteCollapse(secs) => self.palette_collapse_secs.set(secs),
            MessageListInput::SetSelected(id) => {
                match id {
                    // Account-less id resolved against the shown list (the app
                    // only sends `None` today; `Some` kept for completeness).
                    Some(i) => {
                        if let Some(m) = self.shown.iter().find(|m| m.id == i) {
                            let key = (m.account_id, m.id);
                            self.selected_id = Some(key);
                            self.selected_ids = vec![key];
                            self.select_current();
                        }
                    }
                    None => {
                        self.selected_id = None;
                        self.selected_ids.clear();
                        self.selection_count = 0;
                        self.rows.widget().unselect_all();
                    }
                }
            }
            MessageListInput::SelectAndLoad(key) => {
                if let Some(m) = self.shown.iter().find(|m| (m.account_id, m.id) == key).cloned() {
                    self.selected_id = Some(key);
                    self.selected_ids = vec![key];
                    self.select_current();
                    let thread = self.conversation_for(&m);
                    let _ = sender.output(MessageListOutput::Selected { message: m, thread });
                }
            }
            MessageListInput::ContextMenu { x, y } => {
                if let Some(row) = self.rows.widget().row_at_y(y as i32) {
                    let list = self.rows.widget();
                    let selected = list.selected_rows();
                    let in_selection = selected.iter().any(|r| r.index() == row.index());
                    if selected.len() > 1 && in_selection {
                        // Right-clicked inside a multi-selection → bulk menu.
                        self.show_bulk_menu(x, y, &sender);
                    } else {
                        // Single-row menu acting on the clicked message. Crucially,
                        // don't select it — selecting would load it in the reader,
                        // and the user may just intend to move/archive/delete it.
                        if let Some(msg) = self.shown.get(row.index() as usize).cloned() {
                            self.show_context_menu(&msg, x, y, &sender);
                        }
                    }
                }
            }
        }
    }
}

impl MessageList {
    /// Build and pop up the right-click menu for `msg` at the click location.
    fn show_context_menu(
        &self,
        msg: &Message,
        x: f64,
        y: f64,
        sender: &ComponentSender<Self>,
    ) {
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 0);
        menu.add_css_class("context-menu");
        let pop = self.context_popover.clone();
        let item = |label: &str, action: RowAction| -> gtk::Button {
            let btn = gtk::Button::with_label(label);
            btn.add_css_class("flat");
            btn.set_halign(gtk::Align::Fill);
            if let Some(lbl) = btn.child().and_downcast::<gtk::Label>() {
                lbl.set_xalign(0.0);
            }
            let s = sender.clone();
            let p = pop.clone();
            let m = msg.clone();
            btn.connect_clicked(move |_| {
                let _ = s.output(MessageListOutput::Action {
                    action,
                    message: Box::new(m.clone()),
                });
                p.popdown();
            });
            btn
        };

        menu.append(&item("Reply", RowAction::Reply));
        menu.append(&item("Reply All", RowAction::ReplyAll));
        menu.append(&item("Forward", RowAction::Forward));
        menu.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        menu.append(&item(
            if msg.starred { "Remove Star" } else { "Star" },
            RowAction::ToggleStar,
        ));
        menu.append(&item(
            if msg.unread { "Mark as Read" } else { "Mark as Unread" },
            RowAction::ToggleRead,
        ));
        menu.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        menu.append(&item("Mark as Spam", RowAction::Spam));
        menu.append(&item("Archive", RowAction::Archive));
        menu.append(&item("Delete", RowAction::Delete));
        menu.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        menu.append(&item("View Source", RowAction::ViewSource));

        self.context_popover.set_child(Some(&menu));
        self.context_popover
            .set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        self.context_popover.popup();
    }

    /// Build and pop up the bulk-action menu for the current multi-selection.
    fn show_bulk_menu(&self, x: f64, y: f64, sender: &ComponentSender<Self>) {
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 0);
        menu.add_css_class("context-menu");
        let pop = self.context_popover.clone();
        let count = self.selection_count;
        let item = |label: &str, action: BulkAction| -> gtk::Button {
            let btn = gtk::Button::with_label(label);
            btn.add_css_class("flat");
            btn.set_halign(gtk::Align::Fill);
            if let Some(lbl) = btn.child().and_downcast::<gtk::Label>() {
                lbl.set_xalign(0.0);
            }
            let s = sender.clone();
            let p = pop.clone();
            btn.connect_clicked(move |_| {
                s.input(MessageListInput::Bulk(action));
                p.popdown();
            });
            btn
        };

        let header = gtk::Label::new(Some(&format!("{count} selected")));
        header.set_xalign(0.0);
        header.add_css_class("dim-label");
        header.add_css_class("caption");
        header.set_margin_start(8);
        header.set_margin_top(4);
        header.set_margin_bottom(2);
        menu.append(&header);

        menu.append(&item("Mark as Read", BulkAction::MarkRead));
        menu.append(&item("Mark as Unread", BulkAction::MarkUnread));
        menu.append(&item("Flag", BulkAction::Flag));
        menu.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        menu.append(&item("Mark as Spam", BulkAction::Spam));
        menu.append(&item("Archive", BulkAction::Archive));
        menu.append(&item("Delete", BulkAction::Delete));

        self.context_popover.set_child(Some(&menu));
        self.context_popover
            .set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        self.context_popover.popup();
    }

    /// Toolbar count: total matches, noting when more exist than are shown.
    fn count_label(&self) -> String {
        if self.total_matches > self.rendered_count {
            format!("{} of {}", self.rendered_count, self.total_matches)
        } else {
            format!("{}", self.total_matches)
        }
    }

    /// Whether the bottom loading spinner should show: the user has scrolled past
    /// what's loaded (`render_limit` exceeds the indexed count) and the folder's
    /// index is still streaming in.
    fn is_loading_more(&self) -> bool {
        self.render_limit > self.total_matches && !self.index_complete
    }

    /// A full rebuild that keeps the current scroll offset (a plain rebuild jumps
    /// to the top). Used when growing the list beneath the user.
    fn rebuild_preserving_scroll(&mut self) {
        let saved = self.scroller.as_ref().map(|s| s.vadjustment().value());
        self.rebuild();
        if let (Some(pos), Some(scroller)) = (saved, self.scroller.clone()) {
            let adj = scroller.vadjustment();
            gtk::glib::idle_add_local_once(move || adj.set_value(pos));
        }
    }

    /// A message's read state changed: recompute its conversation's aggregate
    /// unread flag and push it to the head row, so a collapsed thread's heavy
    /// highlight clears exactly when its last unread message is read.
    fn refresh_thread_unread(&mut self, id: u32) {
        let Some(key) = self
            .all
            .iter()
            .find(|m| m.id == id)
            .map(|m| (m.account_id, m.id))
        else {
            return;
        };
        let Some(tkey) = self.msg_thread.get(&key) else {
            return;
        };
        let Some(members) = self.thread_members.get(tkey).cloned() else {
            return;
        };
        let any_unread = members
            .iter()
            .any(|k| self.all.iter().any(|m| (m.account_id, m.id) == *k && m.unread));
        // The head is the first (and, when collapsed, only) member in `shown`.
        if let Some(idx) = self
            .shown
            .iter()
            .position(|m| members.contains(&(m.account_id, m.id)))
        {
            self.rows.send(idx, MessageRowInput::SetThreadUnread(any_unread));
        }
    }

    /// Whether a search is currently active (the query is non-empty).
    fn searching(&self) -> bool {
        !self.query.trim().is_empty()
    }

    /// The message set the search filters over: the cross-folder pool while an
    /// `AllFolders` search is active (and the pool has arrived), otherwise the
    /// current folder's own index.
    fn active_source(&self) -> &[Message] {
        if self.searching()
            && self.scope == SearchScope::AllFolders
            && !self.search_pool.is_empty()
        {
            &self.search_pool
        } else {
            &self.all
        }
    }

    fn search_placeholder(&self) -> &'static str {
        match self.scope {
            SearchScope::AllFolders => "Search all folders",
            SearchScope::ThisFolder => "Search this folder",
        }
    }

    /// Drop any active search: clear the query and the entry text so a folder
    /// switch doesn't leave a stale term filtering the new folder.
    fn clear_search(&mut self) {
        if self.query.is_empty() {
            return;
        }
        self.query.clear();
        if let Some(e) = &self.search_entry {
            e.set_text("");
        }
    }

    fn rebuild(&mut self) {
        let q = self.query.to_lowercase();
        let mut matches: Vec<Message> = self
            .active_source()
            .iter()
            .filter(|m| {
                q.is_empty()
                    || m.subject.to_lowercase().contains(&q)
                    || m.from_name.to_lowercase().contains(&q)
                    || m.from_addr.to_lowercase().contains(&q)
                    || m.preview.to_lowercase().contains(&q)
            })
            .cloned()
            .collect();
        sort_messages(&mut matches, self.sort);
        self.total_matches = matches.len();
        // Render up to the current limit; the rest stay indexed (for search) until
        // the user scrolls further and `LoadMore` raises the limit.
        let capped: Vec<Message> = matches.into_iter().take(self.render_limit).collect();
        self.rendered_count = capped.len();

        // Group into conversations by reply headers (Message-ID / In-Reply-To /
        // References), preserving newest-first order. Each thread shows its newest
        // message as the head; expanding reveals the older replies beneath it.
        // With threading off, every message is its own group.
        let keys = if self.threading {
            compute_thread_keys(&capped)
        } else {
            std::collections::HashMap::new()
        };
        let key_for = |m: &Message| -> (u32, String) {
            keys.get(&(m.account_id, m.id))
                .cloned()
                .unwrap_or_else(|| (m.account_id, format!("\u{0}uid{}", m.uid)))
        };
        let mut order: Vec<(u32, String)> = Vec::new();
        let mut groups: std::collections::HashMap<(u32, String), Vec<Message>> =
            std::collections::HashMap::new();
        for m in capped {
            let key = key_for(&m);
            if let Some(v) = groups.get_mut(&key) {
                v.push(m);
            } else {
                order.push(key.clone());
                groups.insert(key, vec![m]);
            }
        }

        // Flatten back into display order, recording per-row thread metadata.
        struct RowMeta {
            count: usize,
            is_child: bool,
            expanded: bool,
            key: Option<(u32, String)>,
            unread: bool,
        }
        let mut shown: Vec<Message> = Vec::new();
        let mut metas: Vec<RowMeta> = Vec::new();
        self.msg_thread.clear();
        self.thread_members.clear();
        for key in &order {
            let msgs = groups.remove(key).unwrap();
            let count = msgs.len();
            // `expanded_threads` stores toggles away from the default state.
            let expanded = count > 1 && (self.expanded_threads.contains(key) != self.default_expanded);
            // The head stays marked unread while ANY message in its
            // conversation is unread — hidden replies included.
            let any_unread = count > 1 && msgs.iter().any(|m| m.unread);
            if count > 1 {
                let members: Vec<(u32, u32)> = msgs.iter().map(|m| (m.account_id, m.id)).collect();
                for k in &members {
                    self.msg_thread.insert(*k, key.clone());
                }
                self.thread_members.insert(key.clone(), members);
            }
            let mut it = msgs.into_iter();
            let head = it.next().unwrap();
            shown.push(head);
            metas.push(RowMeta {
                count,
                is_child: false,
                expanded,
                key: if count > 1 { Some(key.clone()) } else { None },
                unread: any_unread,
            });
            if expanded {
                for child in it {
                    shown.push(child);
                    metas.push(RowMeta {
                        count: 0,
                        is_child: true,
                        expanded: false,
                        key: None,
                        unread: false,
                    });
                }
            }
        }
        self.shown = shown;

        {
            let mut guard = self.rows.guard();
            guard.clear();
            for (m, meta) in self.shown.iter().zip(metas.into_iter()) {
                let ring_class = if self.colorize && self.account_colors.contains_key(&m.account_id) {
                    Some(format!("vireo-acct-ring-{}", m.account_id))
                } else {
                    None
                };
                guard.push_back(RowInit {
                    msg: m.clone(),
                    gravatar: self.gravatar,
                    ring_class,
                    palette_collapse_secs: self.palette_collapse_secs.clone(),
                    thread_count: meta.count,
                    is_thread_child: meta.is_child,
                    thread_expanded: meta.expanded,
                    thread_key: meta.key,
                    thread_unread: meta.unread,
                });
            }
        }

        // Restore the highlight on the message being viewed, if it's still shown.
        self.select_current();
    }

    /// The conversation to show for a selected message: when `m` is the newest
    /// (head) of a multi-message thread, every message in it (newest first);
    /// otherwise just `m` (so opening an individual reply shows only that one).
    fn conversation_for(&self, m: &Message) -> Vec<Message> {
        if !self.threading {
            return vec![m.clone()];
        }
        // Thread within whatever set is on screen (the search pool while searching,
        // otherwise the current folder) so the conversation matches the rows shown.
        let source = self.active_source();
        let keys = compute_thread_keys(source);
        let Some(key) = keys.get(&(m.account_id, m.id)).cloned() else {
            return vec![m.clone()];
        };
        let mut members: Vec<Message> = source
            .iter()
            .filter(|x| keys.get(&(x.account_id, x.id)) == Some(&key))
            .cloned()
            .collect();
        if members.len() <= 1 {
            return vec![m.clone()];
        }
        members.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        let is_head = members
            .first()
            .is_some_and(|h| (h.account_id, h.id) == (m.account_id, m.id));
        if is_head {
            members
        } else {
            vec![m.clone()]
        }
    }

    /// Re-apply the whole selection (the viewed message plus any multi-selected
    /// rows) so it persists across rebuilds — background syncs included — until
    /// the user clicks away. Called after a rebuild, when rows are freshly built
    /// and nothing is selected yet.
    fn select_current(&self) {
        let list = self.rows.widget();
        if self.selected_ids.is_empty() {
            list.unselect_all();
            return;
        }
        for key in &self.selected_ids {
            if let Some(idx) = self.shown.iter().position(|m| (m.account_id, m.id) == *key) {
                if let Some(row) = list.row_at_index(idx as i32) {
                    list.select_row(Some(&row));
                }
            }
        }
    }

    /// Update the display-wide CSS that rings each account's avatar with its
    /// colour (used in the unified "All Inboxes" view to identify the account).
    fn refresh_tint_css(&self) {
        let mut css = String::new();
        for (id, color) in &self.account_colors {
            css.push_str(&format!(
                ".vireo-acct-ring-{0} {{ border-radius: 9999px; box-shadow: 0 0 0 3px {1}; }}\n",
                id, color
            ));
        }
        self.color_provider.load_from_data(&css);
    }
}
