//! Right pane: the reading view for a single message.
//!
//! Bodies are rendered in a sandboxed WebKit view: JavaScript is disabled and a
//! Content-Security-Policy blocks remote content. When remote content is
//! withheld, its URLs are also stripped from the HTML so nothing is requested;
//! the originals are only used once the user (or a trusted sender) allows them.
//! Link clicks open in the browser.

use adw::prelude::*;
use relm4::prelude::*;
use webkit6::prelude::{PolicyDecisionExt, WebViewExt};

use crate::models::Message;
use crate::i18n::{i18n, i18n_f, ni18n_f};

pub struct MessageView {
    /// Render a lone message as an inset card, same as a conversation's
    /// messages (#57, preference; off keeps the full-bleed view).
    single_message_card: bool,
    /// Always show the recipients line under the sender (preference) — the
    /// chip then only appears for multi-recipient mail, as a collapse toggle.
    always_show_recipients: bool,
    /// The newest message in the thread — drives the header, avatar and actions.
    current: Option<Message>,
    /// The whole conversation, newest first. One entry = a single message (shown
    /// exactly as before); more = a scrollable conversation in the body view.
    thread: Vec<Message>,
    /// Which of the conversation's messages came from another folder, and what to
    /// call that folder.
    folder_labels: std::collections::HashMap<(u32, u32), String>,
    /// The tags (#71): what a message's keywords are drawn as.
    tags: Vec<crate::config::Tag>,
    /// The keywords the GTK header's chips were last built for (a lone
    /// full-bleed message; cards carry their chips in the document).
    header_tags_rendered: std::cell::RefCell<Option<Vec<String>>>,
    /// Remote content was detected and is currently withheld — this drives the
    /// "blocked" banner, and nothing else.
    blocked: bool,
    /// Messages the user deliberately marked unread while this conversation was
    /// on screen. They keep their mark but get no scroll sentinel, so simply
    /// having them in view can't undo the thing the user just asked for. Cleared
    /// whenever a conversation is opened afresh.
    no_autoread: std::collections::HashSet<(u32, u32)>,
    /// The reader's own fonts and colours laid over every message (#56);
    /// `NONE` shows each message as its sender formatted it.
    reader_style: crate::config::ReaderStyle,
    /// Messages the user asked to see with the sender's own formatting
    /// despite `reader_style` (the card's "sender's formatting" toggle).
    /// Kept for the session, so a thread reopened keeps the choice.
    sender_style: std::collections::HashSet<(u32, u32)>,
    /// Read-marking policy (#100), stamped on the document for the
    /// viewport observer.
    read_mark: crate::config::ReadMark,
    /// Whether the blocked-content banner is shown at all. It gates only the notice: `blocked`
    /// still governs what is withheld, so hiding it never loads anything.
    show_banner: bool,
    /// Whether the user has actually permitted remote content for what is on
    /// screen (settings, "Load once", or "Always allow sender").
    ///
    /// Kept separate from `blocked` on purpose. `blocked` depends on a detector
    /// guessing whether a message references remote resources; this does not.
    /// Stripping and the CSP key off *this*, so a detector miss costs a banner
    /// rather than the protection itself.
    remote_allowed: bool,
    /// Owning account's display name (header chip).
    account_name: Option<String>,
    /// Provider holding the header chip's per-account colours.
    chip_provider: gtk::CssProvider,
    /// Paints the reader's spinner and its inter-document cover in the *message*
    /// theme rather than the app's. Reading a light message in a dark app used
    /// to mean a dark spinner giving way to a white page.
    cover_provider: gtk::CssProvider,
    /// Fingerprint of what the WebView is currently showing. Re-selecting a
    /// message that renders to the same document skips the load entirely —
    /// every load blanks the view for an instant, however briefly.
    shown_fingerprint: Option<u64>,
    /// Conversation card actions hide until the card is hovered (expanded via
    /// their ⋯ toggle). A preference, applied by stamping the document (see
    /// `document_html`).
    card_actions_hover: bool,
    /// With the toggle off: show the actions automatically while the card is
    /// hovered (vs always).
    card_actions_auto: bool,
    /// Seconds an opened card palette lingers after the pointer leaves —
    /// the same "Actions Palette timeout" the list uses.
    palette_collapse_secs: u64,
    /// The open conversation has already auto-scrolled to its first unread
    /// message: later renders of the same thread (bodies streaming in, a theme
    /// flip) carry a no-scroll stamp so the reader's place is kept.
    did_autoscroll: bool,
    /// The wrapper document's last reported scroll anchor: the topmost card at
    /// the viewport top and the offset into it. A re-render replaces the whole
    /// document (scroll resets to 0) and can reflow everything above — an
    /// element anchor survives that where a raw pixel offset lands short.
    saved_anchor: Option<(u32, u32, u32)>,
    /// What each message's frame measured last time it was shown, so reopening a
    /// conversation lays out right away instead of settling into place.
    frame_heights: std::collections::HashMap<(u32, u32), u32>,
    /// This conversation came ready-made, so there is nothing to cover.
    instant: bool,
    /// The messages the list has selected, outlined in the reader.
    selected_cards: Vec<(u32, u32)>,
    /// Bumped by FocusCard/BlurCard so a blur cancels a focus whose 380ms
    /// post-slide scroll hasn't fired yet (reply cancelled immediately).
    focus_gen: std::rc::Rc<std::cell::Cell<u64>>,
    /// True while the body is being fetched (show a spinner instead).
    loading: bool,
    /// False from when a render starts until the WebView reports it finished
    /// loading — a themed cover hides the WebView's white inter-document gap.
    webview_ready: bool,
    /// In-message find (#103).
    find_open: bool,
    find_matches: Option<(u32, u32)>,
    find_entry: Option<gtk::SearchEntry>,
    /// When find last closed itself (empty entry blur) — the toolbar click
    /// that caused the blur must not instantly reopen it.
    find_closed_at: Option<std::time::Instant>,
    /// Which message the document currently on screen was rendered for, so an
    webview: webkit6::WebView,
    /// Bumped per render: each load gets a unique base URI so WebKit treats it
    /// as a fresh document and re-fetches resources (reusing `about:blank` does
    /// not). An https base also lets https images load without mixed-content.
    seq: std::cell::Cell<u64>,
    /// Forced dark flag for message content, or `None` to follow the system UI.
    /// This themes email content only, not the app chrome.
    content_dark: Option<bool>,
    /// Whether the current message's From: address survived its provider's
    /// authentication checks. `None` until the verdict arrives with the body.
    sender_check: Option<crate::models::SenderCheck>,
    /// Per-member verdicts for the conversation on screen, behind each card's
    /// header seal (#88). Keyed (account_id, id); cleared on Show.
    member_checks: std::collections::HashMap<(u32, u32), crate::models::SenderCheck>,
    /// Full URL of the link under the pointer, shown in a corner overlay so a
    /// link's real destination is visible before it is clicked.
    link_preview: gtk::Label,
}

impl MessageView {
    /// Light the header seal for one member with its verdict class + tooltip.
    fn patch_verify_badge(&self, account_id: u32, id: u32) {
        let Some(check) = self.member_checks.get(&(account_id, id)) else { return };
        // The OpenPGP chip (#133) rides the same patch: shown with its
        // lock/shield and colour once a verdict exists.
        let pgp = match &check.pgp {
            Some(p) => format!(
                "var p=document.querySelector('.vireo-pgp[data-key=\"{account_id}:{id}\"]');\
                 if(p){{p.className='vireo-pgp on{enc}{sig} {cls}';p.title={title:?};}}",
                enc = if p.encrypted { " enc" } else { "" },
                sig = if p.signed() { " sig" } else { "" },
                cls = p.css_class(),
                title = p.summary(),
            ),
            None => String::new(),
        };
        let js = format!(
            "(function(){{{pgp}\
             var b=document.querySelector('.vireo-verify[data-key=\"{account_id}:{id}\"]');\
             if(!b)return;b.className='vireo-verify on {cls}';b.title={title:?};}})()",
            cls = check.trust.css_class(),
            title = check.trust.label(),
        );
        self.webview
            .evaluate_javascript(&js, None, None, None::<&gtk::gio::Cancellable>, |_| {});
    }

    /// Run one of the in-message find engine's calls (#103), logging failures.
    fn eval_find(&self, js: &str) {
        self.webview
            .evaluate_javascript(js, None, None, None::<&gtk::gio::Cancellable>, |r| {
                if let Err(e) = r {
                    tracing::warn!("find js failed: {e}");
                }
            });
    }

    /// The verdict-details popover, anchored where the seal was clicked.
    /// The old toolbar button's popover, reparented.
    ///
    /// The rect arrives in the page's CSS pixels with `page_width` as the
    /// page's innerWidth. With a GNOME text scaling factor ≠ 1.0, WebKit
    /// zooms the whole page, so CSS pixels differ from widget pixels by
    /// widget-width / innerWidth — scaling by the measured ratio anchors the
    /// popover correctly under any scale (and is exactly 1.0 without one).
    fn show_sender_popover(
        &self,
        account_id: u32,
        id: u32,
        rect: (f64, f64, f64, f64),
        page_width: f64,
        sender: &ComponentSender<Self>,
    ) {
        let Some(check) = self.member_checks.get(&(account_id, id)) else { return };
        let popover = gtk::Popover::new();
        let widget_width = self.webview.width() as f64;
        let ratio = if page_width > 0.0 && widget_width > 0.0 {
            widget_width / page_width
        } else {
            1.0
        };
        let rect = (rect.0 * ratio, rect.1 * ratio, rect.2 * ratio, rect.3 * ratio);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
        content.add_css_class("sender-detail");
        // The OpenPGP verdict (#133) leads when there is one: what the
        // encryption and signature say, then the sender check below it.
        if let Some(pgp) = &check.pgp {
            let head = gtk::Label::new(Some(&i18n("OpenPGP")));
            head.set_halign(gtk::Align::Start);
            head.add_css_class("heading");
            content.append(&head);
            let line = gtk::Label::new(Some(&pgp.summary()));
            line.set_halign(gtk::Align::Start);
            line.set_wrap(true);
            line.set_xalign(0.0);
            line.set_max_width_chars(44);
            content.append(&line);
            if !pgp.notes.is_empty() {
                let notes = gtk::Label::new(Some(&pgp.notes.join("\n")));
                notes.set_halign(gtk::Align::Start);
                notes.set_wrap(true);
                notes.set_xalign(0.0);
                notes.set_max_width_chars(44);
                notes.add_css_class("dim-label");
                content.append(&notes);
            }
            // The way out of the two doubts, without a terminal (#133).
            let action = if pgp.key_missing() {
                Some((i18n("Fetch the sender's key"), MessageViewInput::PgpFetchKey { account_id, id }))
            } else if pgp.key_untrusted() {
                Some((i18n("Trust this key…"), MessageViewInput::PgpTrustKey { account_id, id }))
            } else {
                None
            };
            if let Some((label, input)) = action {
                let button = gtk::Button::with_label(&label);
                button.set_halign(gtk::Align::Start);
                button.add_css_class("pill");
                let s = sender.clone();
                let p = popover.clone();
                let input = std::cell::RefCell::new(Some(input));
                button.connect_clicked(move |_| {
                    if let Some(i) = input.borrow_mut().take() {
                        s.input(i);
                    }
                    p.popdown();
                });
                content.append(&button);
            }
            content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        }
        let heading = gtk::Label::new(Some(&check.trust.label()));
        heading.set_halign(gtk::Align::Start);
        heading.add_css_class("heading");
        content.append(&heading);
        let summary = gtk::Label::new(Some(&check.summary));
        summary.set_halign(gtk::Align::Start);
        summary.set_wrap(true);
        summary.set_xalign(0.0);
        summary.set_max_width_chars(44);
        content.append(&summary);
        if !check.findings.is_empty() {
            content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            let findings = gtk::Label::new(Some(&check.findings.join("\n")));
            findings.set_halign(gtk::Align::Start);
            findings.set_wrap(true);
            findings.set_xalign(0.0);
            findings.set_max_width_chars(44);
            findings.add_css_class("dim-label");
            content.append(&findings);
        }
        let footnote = gtk::Label::new(Some(
            i18n("A pass proves the address wasn't forged — not that the message is safe.").as_str(),
        ));
        footnote.set_halign(gtk::Align::Start);
        footnote.set_wrap(true);
        footnote.set_xalign(0.0);
        footnote.set_max_width_chars(44);
        footnote.add_css_class("dim-label");
        footnote.add_css_class("caption");
        content.append(&footnote);

        popover.set_child(Some(&content));
        popover.set_parent(&self.webview);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(
            rect.0 as i32,
            rect.1 as i32,
            (rect.2 as i32).max(1),
            (rect.3 as i32).max(1),
        )));
        popover.connect_closed(|p| {
            let p = p.clone();
            gtk::glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    /// The current verdict, defaulting to "unverified" before one arrives.
    fn trust(&self) -> crate::models::SenderTrust {
        self.sender_check
            .as_ref()
            .map(|c| c.trust)
            .unwrap_or(crate::models::SenderTrust::Unverified)
    }
}

#[derive(Debug)]
pub enum MessageViewInput {
    /// Whether the blocked-remote-content banner is shown. It doesn't change
    /// what is blocked — only what the reader says about it.
    SetBannerShown(bool),
    /// The "always show recipients" preference changed (re-render follows).
    SetAlwaysShowRecipients(bool),
    /// The "single messages as cards" preference changed (re-render follows).
    SetSingleMessageCard(bool),
    Show {
        /// The conversation, newest first. A single message for a normal open;
        /// several for a threaded conversation.
        thread: Vec<Message>,
        /// The sender is trusted, so remote content may auto-load.
        allow_remote: bool,
        /// Owning account's display name and colour, for the header chip.
        account_name: Option<String>,
        account_color: Option<String>,
        /// The body is still being fetched — show a spinner.
        loading: bool,
        /// The message the user actually selected, which drives the header,
        /// avatar and sender check. Without it the newest message would, and a
        /// conversation can now open with a reply of your own on top (#21).
        /// Boxed: a `Message` inline here would double the size of every message
        /// this component sends.
        primary: Option<Box<Message>>,
        /// The app already had this conversation assembled, so the reader has
        /// nothing to wait for: it swaps the document in place rather than
        /// covering the view, which is what put a spinner on every return to a
        /// thread it had shown minutes before.
        instant: bool,
        /// Labels for conversation messages that live in another folder, keyed by
        /// (account, message id) — "Sent" beside a reply of yours read from the
        /// Inbox. Messages from the folder on screen aren't in here.
        folder_labels: std::collections::HashMap<(u32, u32), String>,
    },
    LoadRemoteOnce,
    AllowSenderAlways,
    /// The system/app light-dark preference changed; re-render to match.
    ThemeChanged,
    /// Print the message on screen (issue #16).
    Print,
    /// Render the message to a PDF and open it, so the layout can be checked
    /// before any paper is used.
    PrintPreview,
    /// Set the message-content theme: `None` follows the system, `Some(dark)`
    /// forces light/dark for email content only (not the app UI).
    SetContentTheme(Option<bool>),
    /// The reader's own fonts and colours over the senders' (#56).
    SetReaderStyle(crate::config::ReaderStyle),
    /// The card's "sender's formatting" toggle: show that one message as its
    /// sender formatted it, or back under the reader's style.
    ToggleSenderStyle { account_id: u32, id: u32 },
    /// The popover's "Fetch the sender's key" (#133): the Autocrypt key in
    /// the message first, then WKD and the keyservers.
    PgpFetchKey { account_id: u32, id: u32 },
    /// The popover's "Trust this key": vouch for the signing key.
    PgpTrustKey { account_id: u32, id: u32 },
    /// A key action finished: what to say, and whether to re-verify.
    PgpDone { account_id: u32, id: u32, result: Result<String, String> },
    /// The WebView finished loading the current document — reveal it.
    Rendered,
    /// The sender-authentication verdict for the message now on screen.
    SetSenderCheck(Box<crate::models::SenderCheck>),
    /// A member's sender verdict, for its header seal (#88). Patched into the
    /// live document; queued until the document reports ready.
    SenderCheckFor { account_id: u32, id: u32, check: Box<crate::models::SenderCheck> },
    /// The header seal was clicked — show the verdict details anchored on the
    /// seal's own rect (x, y, w, h in document coordinates).
    SenderInfoAt { account_id: u32, id: u32, rect: (f64, f64, f64, f64), page_width: f64 },
    /// A conversation message header was double-clicked — open that message in
    /// its own window.
    OpenHeader { account_id: u32, id: u32 },
    /// The user marked this conversation message unread: keep the mark until the
    /// conversation is opened again, rather than clearing it the moment the
    /// message happens to be in view.
    SuppressAutoRead { account_id: u32, id: u32 },
    /// The page around the cards was clicked — nothing is selected any more.
    ClearCards,
    /// A card was clicked, with whatever modifier was held.
    CardClicked { account_id: u32, id: u32, mode: SelectMode },
    /// Ctrl+A in the conversation — select every card.
    SelectAllCards,
    /// Ctrl+C reached the window rather than this view: copy the selection
    /// made in a message body, if there is one.
    CopySelection,
    /// An email address in a card header was clicked — compose to it.
    ComposeTo(String),
    /// "Add to Contacts" from an address's right-click menu.
    AddContact(String),
    /// Which messages the list has selected. Drawn as an accent outline on the
    /// matching cards, applied to the live document rather than by rendering it
    /// again — a re-render would lose the reader's scroll position.
    SetSelectedCards(Vec<(u32, u32)>),
    /// A message's frame measured this tall, so reopening it can lay out at that
    /// height instead of settling into it.
    FrameSized { account_id: u32, id: u32, height: u32 },
    /// One conversation message has been scrolled all the way through, so it
    /// has been read.
    MarkSeen { account_id: u32, id: u32 },
    /// Reply / Reply all / Forward chosen on one card's header, so the action
    /// applies to that message rather than to whichever the reader calls primary.
    CardAction {
        action: crate::ui::message_list::RowAction,
        account_id: u32,
        id: u32,
    },
    /// The card's "Add sender to Contacts" button.
    CardContact { account_id: u32, id: u32 },
    /// Preferences: how card actions show — behind the ⋯ toggle, automatically
    /// on hover, or always.
    SetCardActionsMode { hover_toggle: bool, hover_auto: bool },
    /// Preferences: seconds an opened card palette lingers after the pointer
    /// leaves (shared with the list's Actions Palette timeout).
    SetPaletteCollapse(u64),
    /// A conversation message was read: clear its card's unread dot in place,
    /// without reloading the document.
    ClearDot { account_id: u32, id: u32 },
    /// A message's star changed outside the card (list row, toolbar): sync
    /// the card's star button without a re-render.
    SetCardStar { account_id: u32, id: u32, starred: bool },
    /// A message's keywords changed (#71): the card's chips are patched in
    /// place, like the star.
    SetCardKeywords { account_id: u32, id: u32, keywords: Vec<String> },
    /// The tag definitions changed: re-render with the new names and colours.
    SetTags(Vec<crate::config::Tag>),
    /// Read-marking policy changed (#100).
    SetReadMark(crate::config::ReadMark),
    /// In-message find (#103): open/close the bar, run/step the search.
    OpenFind,
    CloseFind,
    FindChanged(String),
    FindNext,
    FindPrev,
    FindCounted { current: u32, total: u32 },
    /// A split reply opened for this message (#86): scroll its card to the
    /// top of the (now shorter) reader and give it the selection outline, so
    /// the message being answered is the one in view.
    FocusCard { account_id: u32, id: u32 },
    /// Drop the reply-target outline (the composer closed without sending).
    BlurCard,
    /// The wrapper document reported its scroll anchor (throttled): the card at
    /// the viewport top and the offset into it — kept so a re-render can put
    /// the reader back where they were.
    ScrollAnchor { account_id: u32, id: u32, offset: u32 },
}

/// How a click on a conversation card changes the selection, mirroring what the
/// message list does with the same modifiers.
#[derive(Debug, Clone, Copy)]
pub enum SelectMode {
    /// Plain click: this message alone.
    Plain,
    /// Ctrl: add or remove this one, leaving the rest.
    Toggle,
    /// Shift: everything between the last selection and this one.
    Range,
}

#[derive(Debug)]
pub enum MessageViewOutput {
    /// Add this sender address to the remote-content allowlist.
    AllowSender(String),
    /// Open a conversation message in its own window (header double-clicked).
    OpenWindow(Box<Message>),
    /// The reader's selection changed. It owns this: a conversation can hold
    /// messages the list has no row for — a reply of yours read in from Sent —
    /// and those must still be selectable. The list mirrors what it can.
    SelectCards(Vec<(u32, u32)>),
    /// A conversation message has been read (scrolled through). The reader has
    /// already dropped its mark; the app makes it stick.
    MarkSeen { account_id: u32, id: u32 },
    /// An action chosen on one message's card in a conversation.
    CardAction {
        action: crate::ui::message_list::RowAction,
        message: Box<Message>,
    },
    /// A card's "Add sender to Contacts" button — add this message's sender.
    ContactSender(Box<Message>),
    /// An email address in a card header was clicked — open a composer to it.
    ComposeTo(String),
    /// "Add to Contacts" picked on an address's right-click menu.
    AddContactAddr(String),
    /// Fetch this message's body again (its OpenPGP verdict changed, #133).
    ReloadBody(Box<Message>),
    /// Something to tell the user in a toast.
    Notice(String),
}

impl MessageView {
    /// Whether the GTK header carries tag chips: a lone message shown
    /// full-bleed (not as a card) that has a tag to show.
    fn header_tags_shown(&self) -> bool {
        self.thread.len() == 1
            && !self.single_message_card
            && self
                .current
                .as_ref()
                .is_some_and(|c| self.tags.iter().any(|t| c.has_keyword(&t.keyword)))
    }

    /// Rebuild the GTK header's chips when the shown message's keywords (or
    /// the tags) changed since they were last built.
    fn sync_header_tags(&self, header_tags: &gtk::Box) {
        let keywords = self.current.as_ref().map(|c| c.keywords.clone()).unwrap_or_default();
        if self.header_tags_rendered.borrow().as_ref() == Some(&keywords) {
            return;
        }
        while let Some(child) = header_tags.first_child() {
            header_tags.remove(&child);
        }
        for t in self.tags.iter().filter(|t| keywords.iter().any(|k| k.eq_ignore_ascii_case(&t.keyword))) {
            let chip = gtk::Label::new(Some(&t.name));
            chip.add_css_class("tag-chip");
            chip.add_css_class(&t.css_class());
            chip.set_ellipsize(gtk::pango::EllipsizeMode::End);
            chip.set_max_width_chars(20);
            header_tags.append(&chip);
        }
        *self.header_tags_rendered.borrow_mut() = Some(keywords);
    }
}

/// The tag chips of a card header (#71): one pill per keyword naming a tag,
/// coloured inline (the document has no access to the app's stylesheet).
fn tag_chips_html(tags: &[crate::config::Tag], keywords: &[String]) -> String {
    let mut out = String::new();
    for t in tags.iter().filter(|t| keywords.iter().any(|k| k.eq_ignore_ascii_case(&t.keyword))) {
        out.push_str(&format!(
            "<span class=\"vireo-tag\" style=\"background:{bg};color:{fg}\">{name}</span>",
            bg = gtk::glib::markup_escape_text(&t.color),
            fg = crate::color::readable_text(&t.color),
            name = gtk::glib::markup_escape_text(&t.name),
        ));
    }
    out
}

#[relm4::component(pub)]
impl Component for MessageView {
    type Init = ();
    type Input = MessageViewInput;
    type Output = MessageViewOutput;
    type CommandOutput = ();

    view! {
        gtk::Stack {
            set_transition_type: gtk::StackTransitionType::Crossfade,

            add_named[Some("empty")] = &adw::StatusPage {
                set_icon_name: Some("co.hyprlab.Vireo-mail-read-symbolic"),
                set_title: &i18n("No message selected"),
                set_description: Some(i18n("Choose a message from the list to read it here.").as_str()),
            },

            add_named[Some("message")] = &gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                add_css_class: "reader-pane",

                gtk::Revealer {
                    set_transition_type: gtk::RevealerTransitionType::SlideDown,
                    #[watch]
                    set_reveal_child: model.trust().is_alarming(),

                    gtk::Box {
                        add_css_class: "spoof-alert",
                        set_spacing: 8,

                        gtk::Image { set_icon_name: Some("co.hyprlab.Vireo-dialog-warning-symbolic") },
                        gtk::Label {
                            #[watch]
                            set_label: model
                                .sender_check
                                .as_ref()
                                .map(|c| c.summary.as_str())
                                .unwrap_or_default(),
                            set_hexpand: true,
                            set_halign: gtk::Align::Start,
                            set_wrap: true,
                            set_xalign: 0.0,
                        },
                    },
                },

                gtk::Revealer {
                    set_transition_type: gtk::RevealerTransitionType::SlideDown,
                    #[watch]
                    set_reveal_child: model.blocked && model.show_banner,

                    gtk::Box {
                        add_css_class: "remote-alert",
                        set_spacing: 8,

                        gtk::Image { set_icon_name: Some("co.hyprlab.Vireo-security-high-symbolic") },
                        gtk::Label {
                            set_label: &i18n("Remote content (images, trackers) is blocked to protect your privacy."),
                            set_hexpand: true,
                            set_halign: gtk::Align::Start,
                            set_wrap: true,
                            // Ask for the full single-line width and wrap only
                            // when the pane really is too narrow — without this
                            // a wrapping label requests minimal width and folds
                            // to two lines even with room to spare.
                            set_natural_wrap_mode: gtk::NaturalWrapMode::None,
                            set_xalign: 0.0,
                        },
                        gtk::Button {
                            set_label: &i18n("Load"),
                            set_valign: gtk::Align::Center,
                            connect_clicked => MessageViewInput::LoadRemoteOnce,
                        },
                        gtk::Button {
                            set_label: &i18n("Always allow sender"),
                            set_valign: gtk::Align::Center,
                            #[watch]
                            set_tooltip_text: model.current.as_ref().map(|m| m.from_addr.as_str()),
                            connect_clicked => MessageViewInput::AllowSenderAlways,
                        },
                    },
                },

                // In-message find (#103): WebKit's FindController does the
                // matching and highlighting; this bar just drives it.
                gtk::Revealer {
                    set_transition_type: gtk::RevealerTransitionType::SlideDown,
                    #[watch]
                    set_reveal_child: model.find_open,

                    gtk::Box {
                        add_css_class: "reader-find",
                        set_spacing: 6,

                        #[name = "find_entry"]
                        gtk::SearchEntry {
                            set_hexpand: true,
                            set_placeholder_text: Some(i18n("Find in message").as_str()),
                            connect_search_changed[sender] => move |entry| {
                                sender.input(MessageViewInput::FindChanged(entry.text().to_string()));
                            },
                            connect_activate[sender] => move |_| {
                                sender.input(MessageViewInput::FindNext);
                            },
                            connect_stop_search[sender] => move |_| {
                                sender.input(MessageViewInput::CloseFind);
                            },
                            add_controller = gtk::EventControllerFocus {
                                connect_leave[sender] => move |ctl| {
                                    let empty = ctl
                                        .widget()
                                        .and_downcast_ref::<gtk::SearchEntry>()
                                        .is_some_and(|e| e.text().trim().is_empty());
                                    if empty {
                                        sender.input(MessageViewInput::CloseFind);
                                    }
                                },
                            },
                        },

                        gtk::Label {
                            add_css_class: "dim-label",
                            #[watch]
                            set_label: &match model.find_matches {
                                Some((_, 0)) => i18n("No matches"),
                                Some((cur, total)) => i18n_f("{cur} of {total}", &[("cur", &cur.to_string()), ("total", &total.to_string())]),
                                None => String::new(),
                            },
                        },

                        gtk::Button {
                            set_icon_name: "co.hyprlab.Vireo-pan-up-symbolic",
                            set_tooltip_text: Some(i18n("Previous match").as_str()),
                            add_css_class: "flat",
                            connect_clicked => MessageViewInput::FindPrev,
                        },
                        gtk::Button {
                            set_icon_name: "co.hyprlab.Vireo-pan-down-symbolic",
                            set_tooltip_text: Some(i18n("Next match").as_str()),
                            add_css_class: "flat",
                            connect_clicked => MessageViewInput::FindNext,
                        },
                        gtk::Button {
                            set_icon_name: "co.hyprlab.Vireo-window-close-symbolic",
                            set_tooltip_text: Some(i18n("Close find").as_str()),
                            add_css_class: "flat",
                            connect_clicked => MessageViewInput::CloseFind,
                        },
                    },
                },

                gtk::Box {
                    add_css_class: "reader-header",
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 12,

                    gtk::Box {
                        set_halign: gtk::Align::Start,
                        #[watch]
                        set_visible: model.account_name.is_some(),
                        gtk::Label {
                            #[watch]
                            set_label: model.account_name.as_deref().unwrap_or_default(),
                            add_css_class: "account-chip",
                            add_css_class: "vireo-account-chip",
                        },
                    },

                    gtk::Label {
                        #[watch]
                        set_label: model.current.as_ref().map(|m| m.subject.as_str()).unwrap_or_default(),
                        set_halign: gtk::Align::Start,
                        set_wrap: true,
                        // Break mid-word for unbreakable tokens (e.g. an
                        // undecodable subject or a long URL) so an extreme
                        // subject can never force the pane — and with it the
                        // window controls — wider than the screen.
                        set_wrap_mode: gtk::pango::WrapMode::WordChar,
                        set_xalign: 0.0,
                        set_selectable: true,
                        add_css_class: "reader-subject",
                    },

                    // Tag chips (#71) for a lone full-bleed message, whose
                    // header is these widgets; cards draw theirs in the
                    // document. Filled by post_view.
                    #[local_ref]
                    header_tags -> gtk::Box {
                        set_spacing: 4,
                        set_halign: gtk::Align::Start,
                        #[watch]
                        set_visible: model.header_tags_shown(),
                    },
                },


                #[name = "body_stack"]
                gtk::Stack {
                    // The message's ground, painted on the stack itself so every
                    // page sits on it — the spinner box is centred and paints
                    // only its own few square inches.
                    add_css_class: "reader-cover",
                    // Both pages sit on that same ground, so a short dissolve
                    // between them reads as the message arriving rather than as
                    // the hard cut a stack does by default.
                    set_transition_type: gtk::StackTransitionType::Crossfade,
                    set_transition_duration: 120,
                    set_vexpand: true,
                    #[watch]
                    set_visible_child_name: model.body_page(),

                    add_named[Some("loading")] = &gtk::Box {
                        add_css_class: "reader-loading",
                        set_orientation: gtk::Orientation::Vertical,
                        set_halign: gtk::Align::Center,
                        set_valign: gtk::Align::Center,
                        set_spacing: 14,

                        gtk::Spinner {
                            set_spinning: true,
                            set_width_request: 36,
                            set_height_request: 36,
                        },
                        gtk::Label {
                            // Dimming comes from the cover's own foreground, which
                            // is picked for the message theme — `dim-label` would
                            // fade it against the app's instead.
                            set_label: &i18n("Loading…"),
                        },
                    },
                },
            },

            #[watch]
            set_visible_child_name: if model.current.is_some() { "message" } else { "empty" },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let webview = new_webview();
        // Browser-style link preview: a small plaque in the bottom-left corner of
        // the body showing exactly where the link under the pointer goes. GTK
        // tooltips are unreliable over a WebView (WebKit handles motion events
        // itself, so GTK's hover timer often never starts), and a phishing check
        // that only sometimes appears is worse than none.
        let link_preview = gtk::Label::new(None);
        link_preview.add_css_class("link-preview");
        link_preview.set_halign(gtk::Align::Start);
        link_preview.set_valign(gtk::Align::End);
        link_preview.set_visible(false);
        link_preview.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        link_preview.set_max_width_chars(90);
        link_preview.set_can_target(false); // never intercept clicks meant for the page
        webview.connect_mouse_target_changed({
            let label = link_preview.clone();
            move |_view, hit, _modifiers| {
                let uri = hit.context_is_link().then(|| hit.link_uri()).flatten();
                match uri {
                    Some(uri) => {
                        label.set_text(&link_destination(&uri, hit.link_label().as_deref()));
                        label.set_visible(true);
                    }
                    None => label.set_visible(false),
                }
            }
        });
        let chip_provider = gtk::CssProvider::new();
        let cover_provider = gtk::CssProvider::new();
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &chip_provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
            gtk::style_context_add_provider_for_display(
                &display,
                &cover_provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
        let mut model = MessageView {
            always_show_recipients: false,
            single_message_card: false,
            current: None,
            thread: Vec::new(),
            tags: Vec::new(),
            header_tags_rendered: std::cell::RefCell::new(None),
            folder_labels: std::collections::HashMap::new(),
            blocked: false,
            no_autoread: std::collections::HashSet::new(),
            reader_style: crate::config::ReaderStyle::NONE,
            sender_style: std::collections::HashSet::new(),
            read_mark: crate::config::ReadMark::default(),
            show_banner: crate::config::load_show_remote_banner(),
            card_actions_hover: crate::config::load_card_actions_hover(),
            card_actions_auto: crate::config::load_card_actions_auto(),
            palette_collapse_secs: crate::config::load_palette_collapse(),
            remote_allowed: false,
            account_name: None,
            chip_provider,
            cover_provider,
            shown_fingerprint: None,
            did_autoscroll: false,
            saved_anchor: None,
            frame_heights: std::collections::HashMap::new(),
            instant: false,
            selected_cards: Vec::new(),
            focus_gen: std::rc::Rc::new(std::cell::Cell::new(0)),
            loading: false,
            webview_ready: false,
            find_open: false,
            find_matches: None,
            find_entry: None,
            find_closed_at: None,
            webview,
            sender_check: None,
            member_checks: std::collections::HashMap::new(),
            link_preview: link_preview.clone(),
            seq: std::cell::Cell::new(0),
            content_dark: None,
        };

        // The document being loaded is not the same as it being ready to look at:
        // each message body is a frame that loads afterwards and is then sized
        // from its placeholder height, so revealing here shows every card at the
        // wrong size and then jumping. The page says when its frames have
        // settled; this is only the backstop for a page that never does.
        let ready_sender = sender.clone();
        model.webview.connect_load_changed(move |_view, event| {
            if event == webkit6::LoadEvent::Finished {
                let s = ready_sender.clone();
                gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
                    s.input(MessageViewInput::Rendered);
                });
            }
        });

        // Double-click on a conversation header → open that message's window.
        if let Some(ucm) = model.webview.user_content_manager() {
            let open_sender = sender.clone();
            ucm.connect_script_message_received(Some("vireo"), move |_ucm, value| {
                // "verb:account:id" — the page only ever posts what this document
                // put there, but it is still parsed strictly.
                use crate::ui::message_list::RowAction;
                let msg = value.to_str().to_string();
                let mut parts = msg.splitn(4, ':');
                let (Some(verb), Some(a), Some(i)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    return;
                };
                let extra = parts.next();
                let (Ok(account_id), Ok(id)) = (a.parse::<u32>(), i.parse::<u32>()) else {
                    return;
                };
                match verb {
                    // Every message frame has loaded and been sized: the layout
                    // has settled, so there is something worth revealing.
                    "ready" => open_sender.input(MessageViewInput::Rendered),
                    "desel" => open_sender.input(MessageViewInput::ClearCards),
                    "selall" => open_sender.input(MessageViewInput::SelectAllCards),
                    // Ctrl+C over a body frame the engine would not copy from
                    // itself: the selected text, for the clipboard.
                    "copy" => {
                        if let Some(text) = extra.filter(|t| !t.is_empty()) {
                            if let Some(display) = gtk::gdk::Display::default() {
                                display.clipboard().set_text(text);
                            }
                        }
                    }
                    // An address was clicked (or "New Message" picked from its
                    // little menu) — compose to it. `extra` is the bare address.
                    "composeto" => {
                        if let Some(addr) = extra.map(str::trim).filter(|a| !a.is_empty()) {
                            open_sender.input(MessageViewInput::ComposeTo(addr.to_string()));
                        }
                    }
                    // "Add to Contacts" from the address menu.
                    "addcontact" => {
                        if let Some(addr) = extra.map(str::trim).filter(|a| !a.is_empty()) {
                            open_sender.input(MessageViewInput::AddContact(addr.to_string()));
                        }
                    }
                    // "Copy Address" from the address menu.
                    "copyaddr" => {
                        if let Some(addr) = extra.map(str::trim).filter(|a| !a.is_empty()) {
                            if let Some(display) = gtk::gdk::Display::default() {
                                display.clipboard().set_text(addr);
                            }
                        }
                    }
                    // The header's verification seal was clicked; extra is
                    // the seal's own rect plus the page's innerWidth. The
                    // rect is in CSS pixels — under a GNOME text scaling
                    // factor ≠ 1.0 WebKit zooms the page, so CSS pixels no
                    // longer match widget pixels; innerWidth lets the
                    // receiver recover the real ratio.
                    "senderinfo" => {
                        let nums: Vec<f64> = extra
                            .map(|e| e.split(',').filter_map(|n| n.parse().ok()).collect())
                            .unwrap_or_default();
                        if let [x, y, w, h, vw] = nums[..] {
                            open_sender.input(MessageViewInput::SenderInfoAt {
                                account_id,
                                id,
                                rect: (x, y, w, h),
                                page_width: vw,
                            });
                        }
                    }
                    "sel" => {
                        let mode = match extra {
                            Some("t") => SelectMode::Toggle,
                            Some("r") => SelectMode::Range,
                            _ => SelectMode::Plain,
                        };
                        open_sender.input(MessageViewInput::CardClicked {
                            account_id,
                            id,
                            mode,
                        });
                    }
                    "size" => {
                        if let Some(Ok(height)) = extra.map(|h| h.parse::<u32>()) {
                            open_sender.input(MessageViewInput::FrameSized {
                                account_id,
                                id,
                                height,
                            });
                        }
                    }
                    "open" => open_sender.input(MessageViewInput::OpenHeader { account_id, id }),
                    "seen" => open_sender.input(MessageViewInput::MarkSeen { account_id, id }),
                    "found" => {
                        let (cur, total) = extra
                            .and_then(|e| e.split_once(','))
                            .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
                            .unwrap_or((0, 0));
                        open_sender
                            .input(MessageViewInput::FindCounted { current: cur, total });
                    }
                    "reply" => open_sender.input(MessageViewInput::CardAction {
                        action: RowAction::Reply,
                        account_id,
                        id,
                    }),
                    "replyall" => open_sender.input(MessageViewInput::CardAction {
                        action: RowAction::ReplyAll,
                        account_id,
                        id,
                    }),
                    "forward" => open_sender.input(MessageViewInput::CardAction {
                        action: RowAction::Forward,
                        account_id,
                        id,
                    }),
                    "star" => open_sender.input(MessageViewInput::CardAction {
                        action: RowAction::ToggleStar,
                        account_id,
                        id,
                    }),
                    "spam" => open_sender.input(MessageViewInput::CardAction {
                        action: RowAction::Spam,
                        account_id,
                        id,
                    }),
                    "archive" => open_sender.input(MessageViewInput::CardAction {
                        action: RowAction::Archive,
                        account_id,
                        id,
                    }),
                    "delete" => open_sender.input(MessageViewInput::CardAction {
                        action: RowAction::Delete,
                        account_id,
                        id,
                    }),
                    "toggleread" => open_sender.input(MessageViewInput::CardAction {
                        action: RowAction::ToggleRead,
                        account_id,
                        id,
                    }),
                    "viewsource" => open_sender.input(MessageViewInput::CardAction {
                        action: RowAction::ViewSource,
                        account_id,
                        id,
                    }),
                    "contact" => {
                        open_sender.input(MessageViewInput::CardContact { account_id, id })
                    }
                    "senderfmt" => {
                        open_sender.input(MessageViewInput::ToggleSenderStyle { account_id, id })
                    }
                    // "scrollat:<aid>:<id>:<offset>" — the topmost visible card.
                    "scrollat" => {
                        if let Some(Ok(offset)) = extra.map(|o| o.parse::<u32>()) {
                            open_sender.input(MessageViewInput::ScrollAnchor {
                                account_id,
                                id,
                                offset,
                            });
                        }
                    }
                    _ => {}
                }
            });
        }

        // Re-render the body when the light/dark preference changes so unstyled
        // content tracks the theme live.
        let style_manager = adw::StyleManager::default();
        model.apply_webview_bg(model.effective_dark());
        let theme_sender = sender.clone();
        style_manager.connect_dark_notify(move |_| {
            theme_sender.input(MessageViewInput::ThemeChanged);
        });

        let header_tags = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let widgets = view_output!();
        model.find_entry = Some(widgets.find_entry.clone());
        let body_overlay = gtk::Overlay::new();
        body_overlay.set_child(Some(&model.webview));
        body_overlay.add_overlay(&link_preview);
        widgets.body_stack.add_named(&body_overlay, Some("body"));
        widgets.body_stack.set_visible_child_name("body");
        ComponentParts { model, widgets }
    }

    fn post_view() {
        self.sync_header_tags(&widgets.header_tags);
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            MessageViewInput::Show {
                thread,
                allow_remote,
                account_name,
                account_color,
                loading,
                primary,
                folder_labels,
                instant,
            } => {
                let shown = primary.map(|p| *p).or_else(|| thread.first().cloned());
                // A new message: the previous message's verdict must not linger
                // on screen while this one's is still being fetched.
                let same_message = self.current.as_ref().zip(shown.as_ref()).is_some_and(
                    |(a, b)| a.id == b.id && a.account_id == b.account_id,
                );
                if !same_message {
                    self.sender_check = None;
                    self.member_checks.clear();
                }
                self.link_preview.set_visible(false);
                self.current = shown;
                // A fresh open: anything the user marked unread earlier can be
                // read by scrolling again — and the first render may auto-scroll
                // to the unread mark again.
                if !same_message {
                    self.no_autoread.clear();
                    self.did_autoscroll = false;
                    self.saved_anchor = None;
                }
                self.thread = thread;
                self.folder_labels = folder_labels;
                self.account_name = account_name;
                self.loading = loading;
                self.instant = instant;
                if let Some(color) = &account_color {
                    let css = format!(
                        ".vireo-account-chip {{ background-color: {}; color: {}; }}",
                        crate::color::pale(color, 0.18),
                        color
                    );
                    self.chip_provider.load_from_data(&css);
                }
                self.remote_allowed = allow_remote;
                let has_remote = self
                    .thread
                    .iter()
                    .any(|m| has_remote_resources(&m.body));
                self.blocked = has_remote && !allow_remote;
                // Repaint the cover for what is arriving, even when the spinner
                // is about to be shown instead of a document: the whole point is
                // that the spinner already sits on the right colour.
                self.apply_webview_bg(self.effective_dark());
                // While loading, the spinner page is shown; rendering the (empty)
                // body would just flash blank, so wait for the real body.
                if !self.loading {
                    self.render();
                }
            }
            MessageViewInput::LoadRemoteOnce => {
                self.remote_allowed = true;
                self.blocked = false;
                self.render();
            }
            MessageViewInput::AllowSenderAlways => {
                if let Some(m) = &self.current {
                    let _ = sender.output(MessageViewOutput::AllowSender(m.from_addr.clone()));
                }
                self.remote_allowed = true;
                self.blocked = false;
                self.render();
            }
            MessageViewInput::Print => {
                crate::ui::print_preview::print_html(
                    &self.print_document_html(),
                    &sanitize_filename(&self.job_name()),
                    self.webview.root().and_downcast::<gtk::Window>(),
                );
            }

            MessageViewInput::PrintPreview => {
                // Shown inside Vireo rather than exported to a PDF and handed to
                // whatever the desktop opens PDFs with: that route is a temporary
                // file, a URI, the document portal and an external viewer, each
                // able to fail without saying anything — and it did.
                let Some(parent) = self
                    .webview
                    .root()
                    .and_downcast::<adw::ApplicationWindow>()
                else {
                    tracing::warn!("no window to attach the preview to");
                    return;
                };
                let html = self.preview_html();
                crate::ui::print_preview::open(
                    &parent,
                    &html,
                    &sanitize_filename(&self.job_name()),
                );
            }

            MessageViewInput::ThemeChanged => {
                let dark = self.effective_dark();
                self.apply_webview_bg(dark);
                if self.current.is_some() && !self.loading {
                    self.render();
                }
            }
            MessageViewInput::SetAlwaysShowRecipients(on) => {
                self.always_show_recipients = on;
            }
            MessageViewInput::SetSingleMessageCard(on) => {
                self.single_message_card = on;
            }
            MessageViewInput::SetBannerShown(show) => {
                self.show_banner = show;
            }
            MessageViewInput::SetContentTheme(o) => {
                if self.content_dark != o {
                    self.content_dark = o;
                    let dark = self.effective_dark();
                    self.apply_webview_bg(dark);
                    if self.current.is_some() && !self.loading {
                        self.render();
                    }
                }
            }
            MessageViewInput::SetReaderStyle(style) => {
                if self.reader_style != style {
                    self.reader_style = style;
                    if self.current.is_some() && !self.loading {
                        self.render();
                    }
                }
            }
            MessageViewInput::ToggleSenderStyle { account_id, id } => {
                let key = (account_id, id);
                if !self.sender_style.remove(&key) {
                    self.sender_style.insert(key);
                }
                if self.current.is_some() && !self.loading {
                    self.render();
                }
            }
            MessageViewInput::SetSenderCheck(check) => {
                self.sender_check = Some(*check);
            }

            MessageViewInput::SenderCheckFor { account_id, id, check } => {
                self.member_checks.insert((account_id, id), *check);
                if self.webview_ready {
                    self.patch_verify_badge(account_id, id);
                }
            }

            MessageViewInput::SenderInfoAt { account_id, id, rect, page_width } => {
                self.show_sender_popover(account_id, id, rect, page_width, &sender);
            }
            MessageViewInput::PgpFetchKey { account_id, id } => {
                let Some(pgp) = self.member_checks.get(&(account_id, id)).and_then(|c| c.pgp.clone()) else {
                    return;
                };
                let input = sender.input_sender().clone();
                sender.oneshot_command(async move {
                    let result = tokio::task::spawn_blocking(move || fetch_sender_key(&pgp))
                        .await
                        .unwrap_or_else(|_| Err("task failed".into()));
                    let _ = input.send(MessageViewInput::PgpDone { account_id, id, result });
                });
            }
            MessageViewInput::PgpTrustKey { account_id, id } => {
                let Some(pgp) = self.member_checks.get(&(account_id, id)).and_then(|c| c.pgp.clone()) else {
                    return;
                };
                let crate::models::PgpSignature::Good { signer, key_id, .. } = &pgp.signature else {
                    return;
                };
                let gpg = crate::pgp::Gpg::system();
                let Some(key) = crate::pgp::key_by_fingerprint(&gpg, key_id) else {
                    let _ = sender.output(MessageViewOutput::Notice(i18n("That key is no longer in your keyring.")));
                    return;
                };
                let parent = self.webview.root().and_downcast::<gtk::Window>();
                let dialog = adw::MessageDialog::new(
                    parent.as_ref(),
                    Some(&i18n("Trust this key?")),
                    Some(&i18n_f(
                        "Compare the fingerprint with the one {uid} gives you in person or over another channel. \
                         Trusting a key you have not checked lets an impostor's signature pass as theirs.",
                        &[("uid", signer)],
                    )),
                );
                let fpr_label = gtk::Label::new(Some(&key.fingerprint_display()));
                fpr_label.add_css_class("monospace");
                fpr_label.set_wrap(true);
                fpr_label.set_selectable(true);
                fpr_label.set_justify(gtk::Justification::Center);
                dialog.set_extra_child(Some(&fpr_label));
                dialog.add_response("cancel", &i18n("Cancel"));
                dialog.add_response("trust", &i18n("Trust"));
                dialog.set_response_appearance("trust", adw::ResponseAppearance::Suggested);
                dialog.set_default_response(Some("cancel"));
                let s = sender.clone();
                let fpr = key.fingerprint.clone();
                dialog.connect_response(None, move |_, resp| {
                    if resp != "trust" {
                        return;
                    }
                    let result = crate::pgp::trust_key(&crate::pgp::Gpg::system(), &fpr, None)
                        .map(|()| i18n("Key trusted."))
                        .map_err(|e| {
                            if e.contains("secret key") || e.contains("default") {
                                i18n("Generate a key of your own first (Settings, OpenPGP): trusting a key means signing it with yours.")
                            } else {
                                e
                            }
                        });
                    s.input(MessageViewInput::PgpDone { account_id, id, result });
                });
                dialog.present();
            }
            MessageViewInput::PgpDone { account_id, id, result } => {
                match result {
                    Ok(text) => {
                        let _ = sender.output(MessageViewOutput::Notice(text));
                        if let Some(m) = self.thread.iter().find(|m| m.account_id == account_id && m.id == id) {
                            let _ = sender.output(MessageViewOutput::ReloadBody(Box::new(m.clone())));
                        }
                    }
                    Err(e) => {
                        let _ = sender.output(MessageViewOutput::Notice(e));
                    }
                }
            }

            MessageViewInput::Rendered => {
                self.webview_ready = true;
                // Verdicts that arrived while the document was still loading.
                for (aid, id) in self.member_checks.keys().copied().collect::<Vec<_>>() {
                    self.patch_verify_badge(aid, id);
                }
            }
            MessageViewInput::SuppressAutoRead { account_id, id } => {
                self.no_autoread.insert((account_id, id));
                for m in self.thread.iter_mut() {
                    if m.account_id == account_id && m.id == id {
                        m.unread = true;
                    }
                }
                // Put the card's dot back in place — reloading the document
                // for one mark made every card visibly resettle (the same
                // blip ClearDot avoids in the other direction).
                let js = format!(
                    "(function(){{\
                     var s=document.querySelector('.vireo-msg[data-key=\"{account_id}:{id}\"]');\
                     if(s)s.classList.add('unread');\
                     var h=document.querySelector('.vireo-msg-hdr[data-key=\"{account_id}:{id}\"]');\
                     if(!h||h.querySelector('.vireo-dot'))return;\
                     var d=document.createElement('span');d.className='vireo-dot';\
                     d.setAttribute('data-key','{account_id}:{id}');\
                     var f=h.querySelector('.vireo-from');\
                     if(f)f.parentNode.insertBefore(d,f);else h.appendChild(d);}})()"
                );
                self.webview
                    .evaluate_javascript(&js, None, None, None::<&gtk::gio::Cancellable>, |r| {
                        if let Err(e) = r {
                            tracing::warn!("unread-dot patch failed: {e}");
                        }
                    });
            }
            MessageViewInput::ClearCards => {
                if !self.selected_cards.is_empty() {
                    self.selected_cards.clear();
                    self.apply_card_selection();
                    let _ = sender.output(MessageViewOutput::SelectCards(Vec::new()));
                }
            }
            MessageViewInput::CardClicked { account_id, id, mode } => {
                // Only meaningful in a conversation: a lone message is already
                // the selection.
                if self.thread.len() <= 1 {
                    return;
                }
                let key = (account_id, id);
                let order: Vec<(u32, u32)> =
                    self.thread.iter().map(|m| (m.account_id, m.id)).collect();
                let mut keys = match mode {
                    SelectMode::Plain => vec![key],
                    SelectMode::Toggle => {
                        let mut k = self.selected_cards.clone();
                        if let Some(pos) = k.iter().position(|x| *x == key) {
                            k.remove(pos);
                        } else {
                            k.push(key);
                        }
                        k
                    }
                    SelectMode::Range => {
                        // From the first thing already selected to this one, in
                        // the order the conversation is shown.
                        let anchor = self
                            .selected_cards
                            .first()
                            .and_then(|a| order.iter().position(|x| x == a))
                            .unwrap_or_else(|| {
                                order.iter().position(|x| *x == key).unwrap_or(0)
                            });
                        let here = order.iter().position(|x| *x == key).unwrap_or(anchor);
                        let (lo, hi) = (anchor.min(here), anchor.max(here));
                        order[lo..=hi].to_vec()
                    }
                };
                keys.retain(|k| order.contains(k));
                self.selected_cards = keys.clone();
                self.apply_card_selection();
                let _ = sender.output(MessageViewOutput::SelectCards(keys));
            }
            MessageViewInput::CopySelection => {
                self.webview.evaluate_javascript(
                    "window.__vireoCopySel && window.__vireoCopySel()",
                    None,
                    None,
                    gtk::gio::Cancellable::NONE,
                    |_| {},
                );
            }
            MessageViewInput::SelectAllCards => {
                if self.thread.len() <= 1 {
                    return;
                }
                let keys: Vec<(u32, u32)> =
                    self.thread.iter().map(|m| (m.account_id, m.id)).collect();
                if self.selected_cards != keys {
                    self.selected_cards = keys.clone();
                    self.apply_card_selection();
                    let _ = sender.output(MessageViewOutput::SelectCards(keys));
                }
            }
            MessageViewInput::ComposeTo(addr) => {
                let _ = sender.output(MessageViewOutput::ComposeTo(addr));
            }
            MessageViewInput::AddContact(addr) => {
                let _ = sender.output(MessageViewOutput::AddContactAddr(addr));
            }
            MessageViewInput::SetSelectedCards(keys) => {
                // The list mirrors every selection change here — including the
                // plain single "this row is open" state that comes with merely
                // opening a message or thread, which must not outline anything.
                // A lone mirrored key is therefore dropped: the accent border
                // appears only for a deliberate selection — a card header
                // clicked in the reader (CardClicked, which never routes
                // through here), or a multi-selection made in the list.
                let keys = if keys.len() == 1 { Vec::new() } else { keys };
                if self.selected_cards != keys {
                    self.selected_cards = keys;
                    self.apply_card_selection();
                }
            }
            MessageViewInput::FrameSized { account_id, id, height } => {
                // A few hundred numbers at most, and only for messages actually
                // opened; the store is trimmed rather than allowed to creep.
                if self.frame_heights.len() > 512 {
                    self.frame_heights.clear();
                }
                self.frame_heights.insert((account_id, id), height);
            }
            MessageViewInput::MarkSeen { account_id, id } => {
                // A deliberate unread mark stands until the conversation is
                // reopened — the viewport observer must not undo it.
                if self.no_autoread.contains(&(account_id, id)) {
                    return;
                }
                // Keep the local copy in step so a later re-render doesn't put
                // the mark back on a message already read.
                let mut found = false;
                for m in self.thread.iter_mut() {
                    if m.account_id == account_id && m.id == id && m.unread {
                        m.unread = false;
                        found = true;
                    }
                }
                if found {
                    let _ = sender.output(MessageViewOutput::MarkSeen { account_id, id });
                }
            }
            MessageViewInput::CardAction { action, account_id, id } => {
                if let Some(m) = self
                    .thread
                    .iter()
                    .find(|m| m.account_id == account_id && m.id == id)
                {
                    let _ = sender.output(MessageViewOutput::CardAction {
                        action,
                        message: Box::new(m.clone()),
                    });
                }
            }
            MessageViewInput::SetCardActionsMode { hover_toggle, hover_auto } => {
                if self.card_actions_hover != hover_toggle
                    || self.card_actions_auto != hover_auto
                {
                    self.card_actions_hover = hover_toggle;
                    self.card_actions_auto = hover_auto;
                    if self.current.is_some() && !self.loading {
                        self.render();
                    }
                }
            }
            MessageViewInput::SetPaletteCollapse(secs) => {
                if self.palette_collapse_secs != secs {
                    self.palette_collapse_secs = secs;
                    if self.current.is_some() && !self.loading {
                        self.render();
                    }
                }
            }
            MessageViewInput::ClearDot { account_id, id } => {
                for m in self.thread.iter_mut() {
                    if m.account_id == account_id && m.id == id {
                        m.unread = false;
                    }
                }
                let js = format!(
                    "(function(){{\
                     var d=document.querySelector('.vireo-dot[data-key=\"{account_id}:{id}\"]');\
                     if(d)d.remove();\
                     var s=document.querySelector('.vireo-msg[data-key=\"{account_id}:{id}\"]');\
                     if(s)s.classList.remove('unread');}})()"
                );
                self.webview
                    .evaluate_javascript(&js, None, None, None::<&gtk::gio::Cancellable>, |r| {
                        if let Err(e) = r {
                            tracing::warn!("clear-dot patch failed: {e}");
                        }
                    });
            }
            MessageViewInput::SetReadMark(policy) => {
                if self.read_mark != policy {
                    self.read_mark = policy;
                    if self.current.is_some() && !self.loading {
                        self.render();
                    }
                }
            }
            MessageViewInput::OpenFind => {
                // The toolbar button toggles: a second press collapses the bar.
                if self.find_open {
                    sender.input(MessageViewInput::CloseFind);
                    return;
                }
                if self
                    .find_closed_at
                    .take()
                    .is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(300))
                {
                    return;
                }
                self.find_open = true;
                if let Some(entry) = self.find_entry.clone() {
                    gtk::glib::idle_add_local_once(move || {
                        entry.grab_focus();
                    });
                }
            }
            MessageViewInput::CloseFind => {
                self.find_open = false;
                self.find_closed_at = Some(std::time::Instant::now());
                self.find_matches = None;
                if let Some(entry) = &self.find_entry {
                    entry.set_text("");
                }
                self.eval_find("vireoFindClear()");
            }
            MessageViewInput::FindChanged(text) => {
                let text = text.trim();
                if text.is_empty() {
                    self.find_matches = None;
                    self.eval_find("vireoFindClear()");
                    return;
                }
                // Into a JS single-quoted string: backslashes and quotes only.
                let quoted = text.replace('\\', "\\\\").replace('\'', "\\'");
                self.eval_find(&format!("vireoFind('{quoted}')"));
            }
            MessageViewInput::FindNext => self.eval_find("vireoFindStep(1)"),
            MessageViewInput::FindPrev => self.eval_find("vireoFindStep(-1)"),
            MessageViewInput::FindCounted { current, total } => {
                self.find_matches = Some((current, total));
            }
            MessageViewInput::SetTags(tags) => {
                if self.tags != tags {
                    self.tags = tags;
                    *self.header_tags_rendered.borrow_mut() = None;
                    if !self.thread.is_empty() {
                        // The chips are baked into the document: draw it again.
                        self.shown_fingerprint = None;
                        self.render();
                    }
                }
            }
            MessageViewInput::SetCardKeywords { account_id, id, keywords } => {
                for m in self.thread.iter_mut() {
                    if m.account_id == account_id && m.id == id {
                        m.keywords = keywords.clone();
                    }
                }
                if let Some(c) = self.current.as_mut() {
                    if c.account_id == account_id && c.id == id {
                        c.keywords = keywords.clone();
                    }
                }
                let chips = tag_chips_html(&self.tags, &keywords);
                let js = format!(
                    "(function(){{\
                     var t=document.querySelector('.vireo-tags[data-key=\"{account_id}:{id}\"]');\
                     if(t)t.innerHTML={chips};}})()",
                    chips = serde_json::to_string(&chips).unwrap_or_else(|_| "''".into()),
                );
                self.webview
                    .evaluate_javascript(&js, None, None, None::<&gtk::gio::Cancellable>, |r| {
                        if let Err(e) = r {
                            tracing::warn!("card tag patch failed: {e}");
                        }
                    });
            }
            MessageViewInput::SetCardStar { account_id, id, starred } => {
                for m in self.thread.iter_mut() {
                    if m.account_id == account_id && m.id == id {
                        m.starred = starred;
                    }
                }
                let js = format!(
                    "(function(){{\
                     var b=document.querySelector('.vireo-act[data-act=\"star\"][data-key=\"{account_id}:{id}\"]');\
                     if(b)b.classList.{}('on');}})()",
                    if starred { "add" } else { "remove" },
                );
                self.webview
                    .evaluate_javascript(&js, None, None, None::<&gtk::gio::Cancellable>, |r| {
                        if let Err(e) = r {
                            tracing::warn!("card star patch failed: {e}");
                        }
                    });
            }
            MessageViewInput::FocusCard { account_id, id } => {
                let webview = self.webview.clone();
                let gen = self.focus_gen.clone();
                gen.set(gen.get() + 1);
                let my_gen = gen.get();
                // Let the split's 300ms slide finish first — scrolling while
                // the viewport is still shrinking lands somewhere else.
                gtk::glib::timeout_add_local_once(
                    std::time::Duration::from_millis(380),
                    move || {
                        if gen.get() != my_gen {
                            // A BlurCard beat us here: the reply was cancelled
                            // before the slide finished. Adding the outline
                            // now would strand it.
                            return;
                        }
                        let js = format!(
                            "(function(){{\
                             if(!document.body.classList.contains('vireo-conv'))return;\
                             var s=document.querySelector('.vireo-msg[data-key=\"{account_id}:{id}\"]');\
                             if(!s)return;\
                             document.querySelectorAll('.vireo-msg.selected').forEach(function(e){{e.classList.remove('selected');}});\
                             s.classList.add('selected');\
                             s.style.scrollMarginTop='14px';\
                             s.scrollIntoView({{behavior:'smooth',block:'start'}});}})()"
                        );
                        webview.evaluate_javascript(
                            &js,
                            None,
                            None,
                            None::<&gtk::gio::Cancellable>,
                            |r| {
                                if let Err(e) = r {
                                    tracing::warn!("focus-card scroll failed: {e}");
                                }
                            },
                        );
                    },
                );
            }
            MessageViewInput::BlurCard => {
                self.focus_gen.set(self.focus_gen.get() + 1);
                self.webview.evaluate_javascript(
                    "(function(){document.querySelectorAll('.vireo-msg.selected')\
                     .forEach(function(e){e.classList.remove('selected');});})()",
                    None,
                    None,
                    None::<&gtk::gio::Cancellable>,
                    |r| {
                        if let Err(e) = r {
                            tracing::warn!("card blur failed: {e}");
                        }
                    },
                );
                // Cards the list still has selected keep their outline.
                self.apply_card_selection();
            }
            MessageViewInput::ScrollAnchor { account_id, id, offset } => {
                self.saved_anchor = Some((account_id, id, offset));
            }
            MessageViewInput::CardContact { account_id, id } => {
                if let Some(m) = self
                    .thread
                    .iter()
                    .find(|m| m.account_id == account_id && m.id == id)
                {
                    let _ =
                        sender.output(MessageViewOutput::ContactSender(Box::new(m.clone())));
                }
            }
            MessageViewInput::OpenHeader { account_id, id } => {
                if let Some(m) = self
                    .thread
                    .iter()
                    .find(|m| m.account_id == account_id && m.id == id)
                {
                    let _ = sender.output(MessageViewOutput::OpenWindow(Box::new(m.clone())));
                }
            }
        }
    }

}

impl MessageView {
    /// The desktop accent colour, as the document can use it. Read from the
    /// widget's style so it follows the user's choice; GNOME's own blue is the
    /// fallback when the theme doesn't define it.
    fn accent_hex(&self) -> String {
        #[allow(deprecated)]
        self.webview
            .style_context()
            .lookup_color("accent_bg_color")
            .map(|c| crate::color::to_hex(&c))
            .unwrap_or_else(|| "#3584e4".to_string())
    }

    /// Outline the selected cards in the document already on screen. Rendering
    /// it again would reload every frame and lose the reader's place.
    fn apply_card_selection(&self) {
        // A lone card never wears the outline (see `conversation_document`).
        if self.thread.len() <= 1 {
            return;
        }
        let keys: Vec<String> = self
            .selected_cards
            .iter()
            .map(|(a, i)| format!("'{a}:{i}'"))
            .collect();
        let js = format!(
            "(function(){{var s=[{}];             var els=document.querySelectorAll('.vireo-msg');             for(var i=0;i<els.length;i++){{var k=els[i].dataset.key;             els[i].classList.toggle('selected', s.indexOf(k)>=0);}}}})()",
            keys.join(",")
        );
        self.webview
            .evaluate_javascript(&js, None, None, None::<&gtk::gio::Cancellable>, |_| {});
    }

    fn effective_dark(&self) -> bool {
        self.content_dark
            .unwrap_or_else(|| adw::StyleManager::default().is_dark())
    }

    fn render(&mut self) {
        let dark = self.effective_dark();
        self.apply_webview_bg(dark);
        // Already showing exactly this — returning to a conversation, or a
        // re-render nothing changed. Loading it again would only blank the view
        // and paint the same pixels back.
        let fingerprint = self.render_fingerprint(dark);
        if self.webview_ready && self.shown_fingerprint == Some(fingerprint) {
            return;
        }
        self.shown_fingerprint = Some(fingerprint);
        // A conversation is covered until its new document has painted: the
        // spinner is already up from the moment the thread was opened, so it
        // simply stays until there is something to replace it — one transition,
        // not three.
        //
        // A single message is not. It is one small frame that paints in a few
        // milliseconds, and covering that is how a plain message ended up
        // showing a spinner every time it was opened; the view keeps what it has
        // until the new document replaces it.
        if self.thread.len() > 1 && !self.instant {
            self.webview_ready = false;
        }
        let html = self.document_html(dark);
        // Only a conversation's first document may auto-scroll to the unread
        // mark; every later render of the same thread (bodies streaming in, a
        // theme change) is stamped no-scroll so the reader's place is kept.
        let noscroll = if self.did_autoscroll {
            let anchor = self
                .saved_anchor
                .map(|(a, i, o)| format!(" data-vireo-anchor=\"{a}:{i}:{o}\""))
                .unwrap_or_default();
            format!(" data-vireo-noscroll=\"1\"{anchor}")
        } else {
            String::new()
        };
        // How card actions show is a body stamp too, so the static document
        // builder (and its tests) need no extra parameter: "toggle" hides them
        // behind the ⋯, "hover" shows them while the card is hovered, and no
        // stamp means always shown.
        let hover = if self.card_actions_hover {
            " data-vireo-acts=\"toggle\""
        } else if self.card_actions_auto {
            " data-vireo-acts=\"hover\""
        } else {
            ""
        };
        // The palette timeout rides along in ms; the script reads it for the
        // toggle mode's collapse timer and the hover mode's fade-out delay.
        let delay = format!(
            " data-vireo-actsdelay=\"{}\"",
            self.palette_collapse_secs.max(1) * 1000
        );
        // The newest member, for the open-scroll fallback (#101): with no
        // unread mail the reader lands on the newest message rather than
        // wherever the document happens to start.
        let newest = self
            .thread
            .iter()
            .max_by_key(|m| m.timestamp)
            .filter(|_| self.thread.len() > 1)
            .map(|m| format!(" data-vireo-newest=\"{}:{}\"", m.account_id, m.id))
            .unwrap_or_default();
        // Read-marking policy (#100): the observer marks conversation members
        // as they come into view; "manual" installs no observer at all.
        let readmark = match self.read_mark {
            crate::config::ReadMark::Shown => " data-vireo-readmark=\"250\"",
            crate::config::ReadMark::Delay => " data-vireo-readmark=\"2000\"",
            crate::config::ReadMark::Manual => "",
        };
        let copied = format!(" data-vireo-copied=\"{}\"", i18n("Copied").replace('"', "&quot;"));
        let html = html.replacen(
            "<body",
            &format!("<body{noscroll}{hover}{delay}{newest}{readmark}{copied}"),
            1,
        );
        self.did_autoscroll = true;
        let n = self.seq.get().wrapping_add(1);
        self.seq.set(n);
        self.webview
            .load_html(&html, Some(&format!("https://vireo.localhost/message/{n}")));
    }

    /// Which body-stack page to show: spinner while fetching, themed cover while
    /// the WebView loads, then the rendered message(s).
    fn body_page(&self) -> &'static str {
        // A document that has been handed over but hasn't painted yet is still
        // loading as far as the reader is concerned. Showing the previous
        // message in the meantime, or a bare cover between two spinners, is the
        // flash this avoids.
        if self.loading || !self.webview_ready {
            "loading"
        } else {
            "body"
        }
    }

    /// The wrapper document for what is currently on screen.
    /// Everything the rendered document depends on, in one number. Ordered by
    /// the thread so it is stable — the sets it consults are read through it
    /// rather than iterated, whose order is not.
    fn render_fingerprint(&self, dark: bool) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        dark.hash(&mut h);
        // A theme change can move the grounds without flipping `dark` (a
        // different GTK theme variant, say) — the document must follow.
        self.theme_grounds(dark).hash(&mut h);
        self.remote_allowed.hash(&mut h);
        self.reader_style.hash(&mut h);
        self.card_actions_hover.hash(&mut h);
        self.card_actions_auto.hash(&mut h);
        self.palette_collapse_secs.hash(&mut h);
        self.thread.len().hash(&mut h);
        for m in &self.thread {
            let key = (m.account_id, m.id);
            key.hash(&mut h);
            m.unread.hash(&mut h);
            m.from_name.hash(&mut h);
            m.from_addr.hash(&mut h);
            m.to.hash(&mut h);
            m.cc.hash(&mut h);
            m.body.hash(&mut h);
            m.datetime_full().hash(&mut h);
            self.no_autoread.contains(&key).hash(&mut h);
            self.sender_style.contains(&key).hash(&mut h);
            self.folder_labels.get(&key).hash(&mut h);
        }
        h.finish()
    }

    fn document_html(&self, dark: bool) -> String {
        // Hand the live theme grounds to the (display-free, testable) document
        // builder without widening its signature — see LIVE_GROUNDS.
        LIVE_GROUNDS.with(|g| *g.borrow_mut() = Some(self.theme_grounds(dark)));
        LIVE_TAGS.with(|t| *t.borrow_mut() = self.tags.clone());
        Self::conversation_document(
            &self.thread,
            &self.folder_labels,
            &self.no_autoread,
            &self.frame_heights,
            &self.selected_cards,
            &self.accent_hex(),
            !self.remote_allowed,
            dark,
            self.always_show_recipients,
            self.single_message_card,
            &self.reader_style,
            &self.sender_style,
        )
    }

    /// The wrapper document: one sandboxed iframe per message (so each email's
    /// CSS is fully isolated and its scripts can't run), with per-message
    /// headers in conversation mode. A small script sizes each iframe to its
    /// content.
    ///
    /// Takes the thread rather than `&self` so it can be exercised without a GTK
    /// display: what it emits is a security boundary, and one that needs
    /// regression cover.
    fn conversation_document(
        thread: &[Message],
        folder_labels: &std::collections::HashMap<(u32, u32), String>,
        _no_autoread: &std::collections::HashSet<(u32, u32)>,
        heights: &std::collections::HashMap<(u32, u32), u32>,
        selected: &[(u32, u32)],
        accent: &str,
        restrict: bool,
        dark: bool,
        always_show_recipients: bool,
        single_message_card: bool,
        style: &crate::config::ReaderStyle,
        sender_style: &std::collections::HashSet<(u32, u32)>,
    ) -> String {
        // Every message renders with the conversation chrome — a thread of one
        // gets the same in-document header. But only a real conversation is
        // *carded*: inset rounded cards on the deeper page. A lone message
        // goes full-bleed — no gutter, no radius, its ground filling the whole
        // view — so it reads as a message rather than a card in a margin.
        let conversation = !thread.is_empty();
        // A lone message cards up too when the preference asks (#57);
        // otherwise it keeps the full-bleed treatment below.
        let carded = thread.len() > 1 || (conversation && single_message_card);
        // Card selection only means something between cards: a lone message is
        // already the selection, so it never wears the accent outline.
        let mark_selection = thread.len() > 1;
        let mut sections = String::new();
        for m in thread {
            let body = if m.body.trim().is_empty() {
                "<div class=\"vireo-loading\">Loading…</div>".to_string()
            } else {
                message_frame(
                    &m.body,
                    restrict,
                    dark,
                    (m.account_id, m.id),
                    heights.get(&(m.account_id, m.id)).copied(),
                    // The card's toggle wins over the preference.
                    if sender_style.contains(&(m.account_id, m.id)) {
                        &crate::config::ReaderStyle::NONE
                    } else {
                        style
                    },
                    accent,
                )
            };
            if conversation {
                sections.push_str(&format!(
                    "<section class=\"vireo-msg{sel}{unread_cls}\" data-key=\"{aid}:{id}\">\
                       <header class=\"vireo-msg-hdr\" data-key=\"{aid}:{id}\" \
                         title=\"{hdr_title}\">\
                         <div class=\"vireo-hdr-line\">\
                           {ava}{dot}<span class=\"vireo-from\">{from}</span>{verify}{addr}\
                           <span class=\"vireo-tags\" data-key=\"{aid}:{id}\">{tags}</span>\
                           <span class=\"vireo-hdr-meta\">{folder}{rcpt_toggle}\
                             <span class=\"vireo-date\">{date}</span></span>\
                           {acts_toggle}{acts}\
                         </div>{rcpt}\
                       </header>{body}</section>",
                    aid = m.account_id,
                    id = m.id,
                    hdr_title = gtk::glib::markup_escape_text(
                        &i18n("Double-click to open in a new window")
                    ),
                    tags = LIVE_TAGS.with(|t| tag_chips_html(&t.borrow(), &m.keywords)),
                    // The ⋯ toggle that expands/collapses the action row when
                    // the hidden-until-hover preference is on; CSS keeps it
                    // display:none otherwise, so it costs nothing when off.
                    acts_toggle = if !thread.is_empty() {
                        format!(
                            "<button type=\"button\" class=\"vireo-acts-toggle\" \
                             title=\"{acts_title}\" data-key=\"{aid}:{id}\">{svg}</button>",
                            aid = m.account_id,
                            id = m.id,
                            acts_title = gtk::glib::markup_escape_text(&i18n("Actions")),
                            // The same ⋯ the list's Actions Palette toggle uses.
                            svg = inline_icon_svg("view-more-horizontal-symbolic"),
                        )
                    } else {
                        String::new()
                    },
                    // Per-card actions, on every message — single messages
                    // included: the reader toolbar carries no per-message
                    // icons any more, so the card's own row (and the overflow
                    // menu) is where a message is acted on.
                    acts = if !thread.is_empty() {
                        let key = (m.account_id, m.id);
                        format!(
                            "<span class=\"vireo-acts\">{}{}{}{}{}{}{}{}{}{}{}</span>",
                            // Same order as the reader toolbar and the list's
                            // Actions Palette, View Source closing the line.
                            card_action_button(key, "reply", "mail-reply-sender-symbolic", &i18n("Reply to this message")),
                            card_action_button(key, "replyall", "mail-reply-all-symbolic", &i18n("Reply to everyone on this message")),
                            card_action_button(key, "forward", "mail-forward-symbolic", &i18n("Forward this message")),
                            // Action-showing icon (read envelope = "mark as
                            // read"), like the menus and toolbar. Both icons
                            // are baked in; the section's `unread` class picks
                            // one, so marking read/unread flips in place.
                            format!(
                                "<button type=\"button\" class=\"vireo-act\" data-act=\"toggleread\" \
                                 data-key=\"{aid}:{id}\" title=\"{title}\">\
                                 <span class=\"tr-when-unread\">{read_svg}</span>\
                                 <span class=\"tr-when-read\">{unread_svg}</span></button>",
                                aid = key.0,
                                id = key.1,
                                title = gtk::glib::markup_escape_text(&i18n("Mark as read or unread")),
                                read_svg = inline_icon_svg("mail-read-symbolic"),
                                unread_svg = inline_icon_svg("mail-unread-symbolic"),
                            ),
                            // The star keeps one glyph; the flagged state is
                            // colour alone (`.on`, toggled optimistically on
                            // click too).
                            format!(
                                "<button type=\"button\" class=\"vireo-act{on}\" data-act=\"star\" \
                                 data-key=\"{aid}:{id}\" title=\"{title}\">{svg}</button>",
                                on = if m.starred { " on" } else { "" },
                                title = gtk::glib::markup_escape_text(&i18n("Flag this message")),
                                aid = key.0,
                                id = key.1,
                                svg = inline_icon_svg("non-starred-symbolic"),
                            ),
                            card_action_button(key, "archive", "mail-archive-symbolic", &i18n("Archive this message")),
                            card_action_button(key, "delete", "user-trash-symbolic", &i18n("Delete this message")),
                            card_action_button(key, "spam", "mail-mark-junk-symbolic", &i18n("Mark as Spam")),
                            card_action_button(key, "contact", "contact-new-symbolic", &i18n("Add sender to Contacts")),
                            card_action_button(key, "viewsource", "code-symbolic", &i18n("View source")),
                            // The escape from the reader's own fonts and
                            // colours (#56): only offered while an override
                            // is on, lit while this card shows the sender's.
                            if style.active() {
                                let on = sender_style.contains(&key);
                                format!(
                                    "<button type=\"button\" class=\"vireo-act{on_cls}\" data-act=\"senderfmt\" \
                                     data-key=\"{aid}:{id}\" title=\"{title}\">{svg}</button>",
                                    on_cls = if on { " on" } else { "" },
                                    aid = key.0,
                                    id = key.1,
                                    title = gtk::glib::markup_escape_text(&if on {
                                        i18n("Back to my fonts and colours")
                                    } else {
                                        i18n("Show the sender's fonts and colours")
                                    }),
                                    svg = inline_icon_svg("format-text-rich-symbolic"),
                                )
                            } else {
                                String::new()
                            },
                        )
                    } else {
                        String::new()
                    },
                    sel = if mark_selection && selected.contains(&(m.account_id, m.id)) {
                        " selected"
                    } else {
                        ""
                    },
                    unread_cls = if m.unread { " unread" } else { "" },
                    // `escape_text`, not `attr_escape`: these land in element
                    // text content, where `<` and `>` are structural. A `From:`
                    // display name is attacker-controlled (and RFC 2047-decoded,
                    // so any byte sequence can be delivered), and this document
                    // is the trusted wrapper — not a sandboxed message frame.
                    from = escape_text(&m.from_name),
                    // The sender-authentication seal (#88): hidden until a
                    // verdict arrives (SenderCheckFor patches it live), then
                    // tinted by trust — Bazaar's fixed blue for a pass.
                    verify = format!(
                        "<button type=\"button\" class=\"vireo-verify\" data-key=\"{aid}:{id}\" \
                         title=\"\">{svg}</button>\
                         <button type=\"button\" class=\"vireo-pgp\" data-key=\"{aid}:{id}\" \
                         title=\"\">{lock}{sig}</button>",
                        aid = m.account_id,
                        id = m.id,
                        svg = inline_icon_svg("verified-checkmark-symbolic"),
                        // The OpenPGP chip (#133): a lock for an encrypted
                        // message, a shield for a signed one, both when both;
                        // hidden until the verdict is patched in.
                        lock = inline_icon_svg("channel-secure-symbolic"),
                        sig = inline_icon_svg("security-high-symbolic"),
                    ),
                    // An initials circle, tinted per sender address, so who
                    // wrote each card — and which cards are your own replies —
                    // reads at a glance (#22). Pure markup: no texture crosses
                    // into this document, and the initial is escaped like every
                    // other header field.
                    ava = {
                        let initial = m
                            .from_name
                            .trim()
                            .chars()
                            .next()
                            .or_else(|| m.from_addr.trim().chars().next())
                            .map(|c| c.to_uppercase().to_string())
                            .unwrap_or_else(|| "?".to_string());
                        let hue = m
                            .from_addr
                            .to_ascii_lowercase()
                            .bytes()
                            .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32))
                            % 360;
                        let l = if dark { 38 } else { 45 };
                        // Drawn ink-centred by ui::initials and embedded as
                        // a PNG (the same tint); the markup span stands in
                        // only before a window exists to render with.
                        let bg = crate::ui::initials::hsl(f64::from(hue), 0.52, f64::from(l) / 100.0);
                        match crate::ui::initials::png_data_uri(&initial, bg, 26) {
                            Some(uri) => format!("<img class=\"vireo-ava\" src=\"{uri}\" alt=\"\">"),
                            None => format!(
                                "<span class=\"vireo-ava\" style=\"background:hsl({hue},52%,{l}%)\">{}</span>",
                                escape_text(&initial),
                            ),
                        }
                    },
                    addr = if m.from_addr.is_empty() {
                        String::new()
                    } else {
                        // `data-addr` feeds the hover reveal's ::after overlay;
                        // `data-mail` (the bare address) drives click-to-compose
                        // and the copy menu. Fully entity-escaped (&<>"), so an
                        // attacker-controlled address can't smuggle markup
                        // through the attributes.
                        format!(
                            "<span class=\"vireo-addr vireo-mail\" data-mail=\"{0}\" \
                             data-addr=\"&lt;{0}&gt;\">&lt;{0}&gt;</span>",
                            escape_text(&m.from_addr).replace('"', "&quot;")
                        )
                    },
                    date = escape_text(&m.datetime_full()),
                    // Everyone the message went to, tucked behind a small chip so
                    // a card's header stays one line tall until asked. Escaped
                    // like the sender: recipient headers are attacker-controlled.
                    rcpt_toggle = match recipient_count(m) {
                        0 => String::new(),
                        // With the recipients line always visible, a
                        // single-recipient chip would only repeat it (#40).
                        1 if always_show_recipients => String::new(),
                        n => format!(
                            "<button type=\"button\" class=\"vireo-rcpt-toggle{open}\" \
                             title=\"{title}\">{label}</button>",
                            label = gtk::glib::markup_escape_text(&ni18n_f(
                                "{n} recipient",
                                "{n} recipients",
                                n as u32,
                                &[("n", &n.to_string())],
                            )),
                            open = if always_show_recipients { " open" } else { "" },
                            title = if always_show_recipients {
                                i18n("Hide recipients")
                            } else {
                                i18n("Show recipients")
                            },
                        ),
                    },
                    rcpt = recipients_html(m, always_show_recipients),
                    // Where this message was read from, when that isn't the
                    // folder on screen — the reply you sent, pulled in from Sent.
                    folder = match folder_labels.get(&(m.account_id, m.id)) {
                        Some(label) => format!(
                            "<span class=\"vireo-folder\">{}</span>",
                            escape_text(label)
                        ),
                        None => String::new(),
                    },
                    body = body,
                    // Unread messages in a conversation are marked; the mark
                    // clears when the user clicks the card to read it —
                    // scrolling past no longer counts as reading.
                    dot = if m.unread {
                        format!("<span class=\"vireo-dot\" data-key=\"{}:{}\"></span>", m.account_id, m.id)
                    } else {
                        String::new()
                    },
                ));
            } else {
                sections.push_str(&body);
            }
        }
        let scheme = if dark { "dark" } else { "light" };
        // The ⋯ actions toggle: plain white in dark mode — the inherited text
        // colour reads too pale there.
        let toggle_color = if dark { "#ffffff" } else { "inherit" };
        // Paint the wrapper and the (still-loading) iframes in the theme colour so
        // there's no white flash before each message's content renders. The live
        // theme's grounds when the reader set them (issue #62); the stock GNOME
        // values otherwise (tests).
        let (bg, deep, chrome) = LIVE_GROUNDS.with(|g| g.borrow().clone()).unwrap_or_else(|| {
            let (g, p, c) = if dark {
                (GROUND.1, PAGE.1, CHROME.1)
            } else {
                (GROUND.0, PAGE.0, CHROME.0)
            };
            (g.to_string(), p.to_string(), c.to_string())
        });
        // Each message card only reads as a card against a slightly deeper
        // ground than its own; a full-bleed single message sits on its own
        // ground — the chrome ground when it paints no background (see
        // `plain_css` below) — so the whole view is one colour.
        let single_ground = if !carded && thread.len() == 1
            && !paints_own_background(&thread[0].body)
        {
            chrome.clone()
        } else {
            bg.clone()
        };
        let page = if carded { deep } else { single_ground };
        let body_class = if carded { " class=\"vireo-conv\"" } else { "" };
        // Defence in depth for the wrapper: the only script allowed to run is the
        // one carrying this render's nonce, which is ours. Anything a message
        // manages to smuggle into this document — an injected `<script>`, an
        // `onerror=` handler — is refused by the engine even if the escaping
        // above is ever wrong again.
        //
        // Deliberately *only* `script-src`/`object-src`/`base-uri`: no
        // `default-src`. A wrapper policy is inherited by the `srcdoc` frames, so
        // restricting images or styles here would silently re-block the remote
        // content the user has chosen to load. Each frame carries its own
        // `default-src 'none'` policy for that.
        let nonce = crate::rng::nonce(24).ok();
        let csp = match &nonce {
            Some(n) => format!(
                "<meta http-equiv=\"Content-Security-Policy\" content=\"\
                 script-src 'nonce-{n}'; object-src 'none'; base-uri 'none'\">"
            ),
            // No entropy, no nonce we can trust — allow no script at all. The
            // iframes render at their default height instead of resizing, which
            // is a visual degradation, not a security one.
            None => "<meta http-equiv=\"Content-Security-Policy\" content=\"\
                     script-src 'none'; object-src 'none'; base-uri 'none'\">"
                .to_string(),
        };
        let sizer = match &nonce {
            Some(n) => format!("<script nonce=\"{n}\">{SIZE_SCRIPT}</script>"),
            None => String::new(),
        };
        // A single message that paints no background of its own (plain mail,
        // and much styled mail too — Apple Mail styles fonts and wrapping but
        // sets no background) takes the chrome ground for its *whole* frame,
        // matching the GTK subject block and the header above it: one
        // unbroken pane from subject to the end of the message. A message
        // that declares a background keeps the plain ground so its design
        // renders as intended.
        let single_plain =
            !carded && thread.len() == 1 && !paints_own_background(&thread[0].body);
        let plain_css = if single_plain {
            format!(
                "body:not(.vireo-conv) .vireo-msg,\
                 body:not(.vireo-conv) iframe.vireo-frame{{background:{chrome};}}"
            )
        } else {
            String::new()
        };
        format!(
            "<!doctype html><html><head><meta charset=\"utf-8\">{csp}\
             <meta name=\"color-scheme\" content=\"{scheme}\">\
             <style>\
               :root{{color-scheme:{scheme};}}\
               body{{margin:0;padding:0;background:{page};font:14px/1.55 system-ui,sans-serif;}}\
               body.vireo-conv{{padding:14px;}}\
               iframe.vireo-frame{{width:100%;border:0;display:block;background:{bg};}}\
               .vireo-pan{{overflow-x:auto;}}\
               iframe.vireo-frame.anim{{transition:height 240ms cubic-bezier(0.4,0,0.2,1);}}\
               @media (prefers-reduced-motion:reduce){{iframe.vireo-frame.anim{{transition:none;}}}}\
               .vireo-msg{{background:{bg};\
                 border-radius:12px;overflow:hidden;margin:0 0 14px;}}\
               .vireo-msg:last-child{{margin-bottom:0;}}\
               .vireo-msg{{user-select:none;}}\
               body:not(.vireo-conv) .vireo-msg{{border-radius:0;margin:0;}}\
               .vireo-msg.selected{{box-shadow:0 0 0 2px {accent};}}\
               .vireo-msg-hdr{{cursor:pointer;}}\
               .vireo-msg-hdr{{padding:12px 16px;cursor:default;user-select:none;\
                 position:sticky;top:0;z-index:1;background-color:{bg};}}\
               /* 8px between items on a line, but only 2px between wrapped\
                  lines — the flex gap otherwise pads rows apart too. */\
               .vireo-hdr-line{{display:flex;column-gap:8px;row-gap:2px;\
                 align-items:baseline;flex-wrap:nowrap;\
                 min-width:0;position:relative;}}\
               .vireo-hdr-meta{{display:flex;gap:8px;align-items:baseline;flex:none;\
                 margin-left:auto;}}\
               /* Narrow pane: the meta group (folder chip, recipients chip, date)\
                  drops to its own line as one unit — no mid-text wrapping — while\
                  the sender and the action palette hold the first line. */\
               @media (max-width:620px){{\
                 /* Tighten the right-hand insets (card gutter + header padding)\
                    so the palette toggle sits nearer the pane's edge — but the\
                    combined inset never drops below 20px. */\
                 body.vireo-conv{{padding:10px;}}\
                 body.vireo-conv .vireo-msg-hdr{{padding:12px 10px;}}\
                 .vireo-hdr-line{{flex-wrap:wrap;}}\
                 .vireo-hdr-meta{{order:10;flex-basis:100%;margin-left:34px;\
                   justify-content:flex-start;}}\
                 /* Below the wrapped meta line the recipients need a touch\
                    more air than the wide layout's 2px. */\
                 .vireo-rcpt{{margin-top:6px;}}\
                 /* With the meta group gone from the first line, the palette\
                    pins itself to the corner (the \u{22ef} is already absolute). */\
                 body:not([data-vireo-acts=\"toggle\"]) .vireo-acts{{margin-left:auto;}}\
               }}\
               body:not(.vireo-conv) .vireo-msg-hdr{{background-color:{chrome};}}\
               {plain_css}\
               .vireo-ava{{width:26px;height:26px;border-radius:50%;flex:none;align-self:center;\
                 display:flex;align-items:center;justify-content:center;color:#fff;\
                 font-size:0.8em;font-weight:700;}}\
               /* Zero flex-basis: the name and address only absorb free space\
                  (up to their natural width), so their length can never force\
                  the palette onto another line. Under pressure the address\
                  gives way first (tiny grow share), fading out at its right\
                  edge; the name ellipsizes only after that. */\
               .vireo-from{{font-weight:700;flex:99 1 0;max-width:max-content;min-width:0;\
                 white-space:nowrap;overflow:hidden;text-overflow:ellipsis;}}\
               .vireo-tags{{display:inline-flex;gap:4px;align-items:center;flex:0 0 auto;}}\
               .vireo-tags:empty{{display:none;}}\
               .vireo-tag{{font-size:0.72em;font-weight:700;line-height:1.4;padding:1px 7px;\
                 border-radius:9999px;white-space:nowrap;}}\
               .vireo-addr{{opacity:0.55;font-size:0.9em;flex:1 1 0;max-width:max-content;\
                 min-width:0;white-space:nowrap;overflow:hidden;text-overflow:clip;\
                 position:relative;}}\
               /* The fade exists only while something actually collides with\
                  the address (`clipped` is set by measurement, below). */\
               .vireo-addr.clipped{{\
                 -webkit-mask-image:linear-gradient(to right,#000 calc(100% - 18px),transparent);\
                 mask-image:linear-gradient(to right,#000 calc(100% - 18px),transparent);}}\
               /* Hovering a clipped address reveals it in full: an overlay\
                  pinned to the span's spot, floating over whatever crowded it\
                  out, gone when the cursor leaves. */\
               .vireo-addr.clipped:hover{{-webkit-mask-image:none;mask-image:none;\
                 z-index:6;}}\
               /* Addresses act as links: click composes to them, right-click\
                  offers Copy / New Message. */\
               /* Sender-authentication seal (#88): invisible until a\
                  verdict arrives; a pass wears Bazaar's fixed blue (never the\
                  system accent), problems the warning/error reds.\
                  Sized in em and anchored to the text baseline, then eased\
                  down to the name's optical centre with an em transform\
                  (margins can't move it: WebKit synthesizes the missing\
                  baseline before they apply) — a fixed centred pixel box sat\
                  visibly low whenever a GNOME text scaling factor shrank the\
                  type around it. */\
               .vireo-verify{{display:none;background:none;border:none;\
                 padding:0 2px;margin-left:2px;cursor:pointer;line-height:0;\
                 align-self:baseline;transform:translateY(0.18em);flex:none;}}\
               .vireo-verify.on{{display:inline-flex;}}\
               .vireo-verify svg{{width:0.95em;height:0.95em;display:block;}}\
               .vireo-verify svg,.vireo-verify svg *{{fill:currentColor;}}\
               .vireo-verify.trust-pass{{color:#3584e4;}}\
               .vireo-verify.trust-unverified{{color:currentColor;opacity:0.4;}}\
               .vireo-verify.trust-suspicious{{color:#cd9309;}}\
               .vireo-verify.trust-fail{{color:#c01c28;}}\
               .vireo-pgp{{display:none;background:none;border:none;gap:1px;\
                 padding:0 2px;margin-left:2px;cursor:pointer;line-height:0;\
                 align-self:baseline;transform:translateY(0.18em);flex:none;}}\
               .vireo-pgp.on{{display:inline-flex;}}\
               .vireo-pgp svg{{width:0.95em;height:0.95em;display:none;}}\
               .vireo-pgp svg,.vireo-pgp svg *{{fill:currentColor;}}\
               .vireo-pgp.enc svg:first-child{{display:block;}}\
               .vireo-pgp.sig svg:last-child{{display:block;}}\
               .vireo-pgp.pgp-good{{color:#26a269;}}\
               .vireo-pgp.pgp-warn{{color:#cd9309;}}\
               .vireo-pgp.pgp-bad{{color:#c01c28;}}\
               .vireo-mail{{cursor:pointer;}}\
               .vireo-mail:hover{{text-decoration:underline;}}\
               /* Styled after the app's own context menus (context_menu.rs +\
                  .context-menu-list in styles.css): same padding, min width,\
                  row shape and weight, so both menu families read as one. */\
               .vireo-mailmenu-scrim{{position:fixed;left:0;top:0;right:0;bottom:0;\
                 z-index:98;}}\
               .vireo-mailmenu{{position:fixed;z-index:99;background:{bg};\
                 border:1px solid rgba(128,128,128,0.2);border-radius:12px;\
                 box-shadow:0 6px 24px rgba(0,0,0,0.22);padding:6px;\
                 min-width:190px;font-size:0.95em;}}\
               .vireo-mailmenu button{{display:block;width:100%;text-align:left;\
                 background:none;border:none;padding:8px 10px;border-radius:6px;\
                 color:inherit;cursor:pointer;font:inherit;font-weight:normal;}}\
               .vireo-mailmenu button:hover{{background:rgba(128,128,128,0.14);}}\
               .vireo-addr.clipped:hover::after{{content:attr(data-addr);\
                 position:absolute;left:-6px;top:50%;transform:translateY(-50%);\
                 background:{bg};border-radius:6px;padding:1px 6px;\
                 white-space:nowrap;text-decoration:underline;}}\
               body:not(.vireo-conv) .vireo-addr.clipped:hover::after{{\
                 background:{chrome};}}\
               .vireo-date{{opacity:0.55;font-size:0.85em;flex:none;white-space:nowrap;}}\
               .vireo-dot{{width:8px;height:8px;border-radius:50%;background:#3584e4;\
                 flex:none;align-self:center;}}\
               .vireo-end{{height:1px;}}\
               .vireo-quote{{display:block;margin:0 16px 12px;padding:0 7px;\
                 font:inherit;font-size:0.7em;line-height:1.45;letter-spacing:0.06em;\
                 color:inherit;opacity:0.6;background:rgba(128,128,128,0.16);\
                 border:0;border-radius:999px;cursor:pointer;}}\
               .vireo-quote:hover{{opacity:0.95;background:rgba(128,128,128,0.28);}}\
               .vireo-quote.open{{opacity:0.95;}}\
               .vireo-acts{{display:flex;gap:2px;flex:none;align-self:center;margin-left:4px;}}\
               /* Read-toggle: the icon showing is the ACTION (read envelope\
                  means mark-as-read); the section's unread class decides. */\
               .vireo-act .tr-when-unread,.vireo-act .tr-when-read{{display:none;line-height:0;}}\
               .vireo-msg.unread .vireo-act[data-act=\"toggleread\"] .tr-when-unread{{display:inline-flex;}}\
               .vireo-msg:not(.unread) .vireo-act[data-act=\"toggleread\"] .tr-when-read{{display:inline-flex;}}\
               .vireo-act{{color:inherit;background:none;border:none;border-radius:6px;\
                 padding:5px 8px;cursor:pointer;opacity:0.7;\
                 transition:opacity 120ms ease,background 120ms ease;}}\
               .vireo-act:hover{{opacity:1;background:rgba(128,128,128,0.18);}}\
               .vireo-act:active{{background:rgba(128,128,128,0.3);}}\
               .vireo-act svg{{width:14px;height:14px;display:block;}}\
               .vireo-act svg,.vireo-act svg *{{fill:currentColor;}}\
               /* A set star: the one state that carries colour. */\
               .vireo-act.on{{color:#e5a50a;opacity:1;}}\
               .vireo-acts-toggle{{display:none;}}\
               /* Behind-the-\u{22ef} mode: the palette overlays the header line as an\
                  absolute strip anchored at the toggle, expanding leftward over\
                  the text beneath (never pushing it), its left edge fading in\
                  from transparent. The \u{22ef} itself never moves. */\
               body[data-vireo-acts=\"toggle\"] .vireo-acts{{position:absolute;\
                 right:34px;top:13px;transform:translateY(-50%) scaleX(0.6);\
                 transform-origin:right center;margin:0;\
                 padding:2px 6px 2px 30px;border-radius:8px;\
                 background:linear-gradient(to right,transparent,{bg} 26px);\
                 opacity:0;pointer-events:none;\
                 transition:opacity 140ms ease,transform 180ms ease;}}\
               body[data-vireo-acts=\"toggle\"] .vireo-acts.open{{opacity:1;\
                 pointer-events:auto;transform:translateY(-50%) scaleX(1);}}\
               body:not(.vireo-conv)[data-vireo-acts=\"toggle\"] .vireo-acts{{\
                 background:linear-gradient(to right,transparent,{chrome} 26px);}}\
               /* Always visible (not just on card hover), and OUT OF FLOW:\
                  absolutely pinned to the header's right edge, so no amount of\
                  header content can push or wrap it. The line reserves the\
                  button's footprint as fixed right padding instead. */\
               body[data-vireo-acts=\"toggle\"] .vireo-hdr-line{{padding-right:38px;}}\
               /* Anchored to the FIRST row's centre (the 26px avatar/sender\
                  line), not the line box's — when the meta row wraps beneath,\
                  the box grows downward but the button must not move. */\
               body[data-vireo-acts=\"toggle\"] .vireo-acts-toggle{{display:block;\
                 position:absolute;right:0;top:13px;transform:translateY(-50%);\
                 color:{toggle_color};background:none;border:none;border-radius:6px;\
                 padding:4px 8px;margin:0;cursor:pointer;\
                 opacity:0.65;transition:opacity 120ms ease,background 120ms ease;}}\
               /* `svg *` too: the embedded symbolic paths carry their own fill\
                  attribute, which a rule on the svg element alone can't beat. */\
               .vireo-acts-toggle svg{{width:14px;height:14px;display:block;}}\
               .vireo-acts-toggle svg,.vireo-acts-toggle svg *{{fill:currentColor;}}\
               body[data-vireo-acts=\"toggle\"] .vireo-acts-toggle:hover{{opacity:1;\
                 background:rgba(128,128,128,0.18);}}\
               body[data-vireo-acts=\"toggle\"] .vireo-acts-toggle.open{{opacity:1;}}\
               body[data-vireo-acts=\"hover\"] .vireo-acts{{opacity:0;pointer-events:none;\
                 transition:opacity 300ms ease var(--acts-delay,0ms);}}\
               body[data-vireo-acts=\"hover\"] .vireo-msg:hover .vireo-acts{{opacity:1;\
                 pointer-events:auto;transition:opacity 300ms ease 0ms;}}\
               .vireo-folder{{padding:0.05em 0.45em;border-radius:0.7em;\
                 font-size:0.78em;opacity:0.75;border:1px solid currentColor;\
                 white-space:nowrap;flex:none;}}\
               .vireo-rcpt-toggle{{font:inherit;font-size:0.78em;color:inherit;background:none;\
                 border:1px solid rgba(128,128,128,0.45);border-radius:999px;\
                 padding:0.05em 0.6em;cursor:pointer;opacity:0.7;\
                 white-space:nowrap;flex:none;\
                 transition:opacity 120ms ease,background 120ms ease;}}\
               .vireo-rcpt-toggle:hover{{opacity:1;background:rgba(128,128,128,0.18);}}\
               .vireo-rcpt-toggle.open{{opacity:1;background:rgba(128,128,128,0.18);}}\
               /* Indented past the avatar (26px + 8px gap), so the recipients\
                  align with the sender's name — as does the wrapped meta line. */\
               .vireo-rcpt{{font-size:0.85em;opacity:0.75;\
                 user-select:text;overflow-wrap:anywhere;margin:2px 0 0 34px;}}\
               .vireo-rcpt div{{margin-top:2px;}}\
               .vireo-loading{{opacity:0.5;padding:16px;}}\
               /* The Copied pill after Ctrl+C: a toast the page draws itself,\
                  since the reader has no toast overlay of its own. */\
               .vireo-copied{{position:fixed;left:50%;bottom:22px;transform:translateX(-50%) translateY(8px);\
                 padding:8px 18px;border-radius:999px;background:rgba(40,40,40,0.92);color:#fff;\
                 font:inherit;font-size:0.9em;box-shadow:0 2px 10px rgba(0,0,0,0.25);\
                 opacity:0;pointer-events:none;transition:opacity 180ms ease,transform 180ms ease;z-index:50;}}\
               .vireo-copied.on{{opacity:1;transform:translateX(-50%) translateY(0);}}\
             </style>{sizer}\
             </head><body{body_class}>{sections}</body></html>"
        )
    }

    /// What to call the print job — the subject, or something rather than nothing.
    fn job_name(&self) -> String {
        self.current
            .as_ref()
            .map(|m| m.subject.clone())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "Message".to_string())
    }

    /// The document that gets printed: the header, then each message inlined
    /// into the page.
    ///
    /// Not the reader's document. That one puts every message in a sandboxed
    /// iframe, which is right on screen — an email's CSS cannot escape it — but
    /// wrong on paper, where a print engine draws the frame at its on-screen size
    /// with its scrollbars and clips the rest.
    fn print_document_html(&self) -> String {
        // In a conversation the top header describes the first message only, so
        // each one says who sent it and when — as the reader's own per-message
        // headers do on screen.
        let conversation = self.thread.len() > 1;
        let messages: Vec<(String, String)> = self
            .thread
            .iter()
            .enumerate()
            .map(|(n, m)| {
                let doc = body_html(&m.body);
                let doc = if self.remote_allowed { doc } else { strip_remote(&doc) };
                // The reader's own fonts and colours (#56) reach paper too,
                // scoped to this message's block; a card shown with the
                // sender's formatting prints that way as well.
                let style = if self.sender_style.contains(&(m.account_id, m.id)) {
                    &crate::config::ReaderStyle::NONE
                } else {
                    &self.reader_style
                };
                let scope = format!(".{}", print_block_class(n));
                let doc = inject_reader_style(
                    &doc,
                    &reader_style_css(style, false, &self.accent_hex(), &scope),
                );
                let head = if conversation {
                    print_message_header_html(m)
                } else {
                    String::new()
                };
                (head, doc)
            })
            .collect();
        print_document(&self.print_header_html(), &messages, self.remote_allowed)
    }

    /// That same document, dressed as a page for the preview window.
    fn preview_html(&self) -> String {
        let doc = self.print_document_html();
        let extra = format!(
            "<style>{}</style></head>",
            crate::ui::print_preview::PREVIEW_STYLES
        );
        let doc = doc.replacen("</head>", &extra, 1);
        // Wrap the content in the sheet the styles above draw.
        doc.replacen("<body>", "<body><div class=\"vireo-print-sheet\">", 1)
            .replacen("</body>", "</div></body>", 1)
    }

    /// The header block that only appears on paper (see [`print_header_html`]).
    fn print_header_html(&self) -> String {
        print_header_html(self.thread.first().or(self.current.as_ref()))
    }

    /// The reader's two grounds for `dark` (see [`theme_grounds_for`]).
    fn theme_grounds(&self, dark: bool) -> (String, String, String) {
        theme_grounds_for(&self.webview, dark)
    }

    /// Paint the WebView canvas in the theme colour so unstyled bodies (and the
    /// gap before a load) match light/dark mode instead of flashing white.
    fn apply_webview_bg(&self, dark: bool) {
        // Whatever is about to be shown: a conversation's cards sit on the
        // deeper page ground, a full-bleed single message on the plain ground
        // — or the chrome ground when it paints no background of its own.
        // The cover matches it so the spinner gives way to the document
        // without a change of colour.
        let (ground, page, chrome) = self.theme_grounds(dark);
        let ground = if self.thread.len() > 1 || (!self.thread.is_empty() && self.single_message_card) {
            page
        } else if self.thread.len() == 1 && !paints_own_background(&self.thread[0].body) {
            chrome
        } else {
            ground
        };
        self.webview.set_background_color(&ground_rgba(&ground));
        let bg = ground;
        // The spinner and the cover stand in for the message, so they answer to
        // the message's theme: #1e1e1e matches the document's own dark ground,
        // white its light one. The label and spinner take a dimmed foreground
        // from the same side, so neither disappears into the ground.
        let fg = if dark {
            "rgba(255,255,255,0.55)"
        } else {
            "rgba(0,0,0,0.45)"
        };
        self.cover_provider.load_from_data(&format!(
            ".reader-cover {{ background-color: {bg}; }}\
             .reader-loading label, .reader-loading spinner {{ color: {fg}; }}"
        ));
    }
}

/// Create a sandboxed WebView: no JavaScript or dev tools, smooth scrolling, and
/// links routed to the external browser.
/// A web view for the print preview: same sandboxing as the reader's, since it
/// shows the same message.
pub fn new_preview_webview() -> webkit6::WebView {
    new_webview()
}

thread_local! {
    /// One web context for every view the app creates (reader, pop-outs, print
    /// preview, rich editors), configured for showing documents rather than
    /// browsing (issue #106). The default context runs WebKit's browser cache
    /// model — big in-memory resource cache, back/forward page cache — which
    /// for a mail reader only hoards dead documents: every render loads a fresh
    /// unique URI, so nothing cached is ever revisited, and the web process
    /// grew by hundreds of megabytes per reading session and never shrank.
    /// `DocumentViewer` turns those caches off, and the memory-pressure limit
    /// makes the web process actively release memory instead of waiting for
    /// system-wide pressure that arrives only after the damage is done.
    static WEB_CONTEXT: webkit6::WebContext = {
        let mut pressure = webkit6::MemoryPressureSettings::new();
        // In MB. Trimming starts at a fraction of this (~1/3), so the web
        // process settles in the low hundreds of MB instead of the gigabytes
        // reported in #106.
        pressure.set_memory_limit(512);
        let ctx = webkit6::WebContext::builder()
            .memory_pressure_settings(&pressure)
            .build();
        ctx.set_cache_model(webkit6::CacheModel::DocumentViewer);
        ctx
    };
}

/// The shared document-viewer web context (see [`WEB_CONTEXT`]).
pub fn shared_web_context() -> webkit6::WebContext {
    WEB_CONTEXT.with(|c| c.clone())
}

fn new_webview() -> webkit6::WebView {
    // A user-content manager with a script message handler lets the wrapper
    // document notify us (e.g. a double-clicked conversation header).
    let ucm = webkit6::UserContentManager::new();
    ucm.register_script_message_handler("vireo", None);
    let webview = webkit6::WebView::builder()
        .web_context(&shared_web_context())
        .user_content_manager(&ucm)
        .build();

    let settings = webkit6::Settings::new();
    // JavaScript runs only in our own (trusted) wrapper document — it sizes each
    // message's iframe to its content. Every email body is embedded in a
    // `sandbox`ed iframe WITHOUT `allow-scripts`, so message scripts never run.
    settings.set_enable_javascript(true);
    settings.set_enable_developer_extras(false);
    // JS console output lands on stdout, where the tracing/console-mode
    // pipeline can pick it up — a silent script error in the wrapper document
    // otherwise kills card interactions with no trace at all.
    settings.set_enable_write_console_messages_to_stdout(true);
    webview.set_settings(&settings);

    // "Save Image As…" on a right-clicked image. WebKit's own item hands the
    // image to a download, which needs a `WebKitNetworkSession` destination
    // handler we don't have — so it silently did nothing. Every image the reader
    // shows inline (attached photos, and `cid:` images since 1.7.1) is a `data:`
    // URI whose bytes are already in the document, so swap in our own item that
    // decodes them and opens a real save dialog.
    webview.connect_context_menu(|view, menu, hit| {
        if !hit.context_is_image() {
            return false; // not an image — leave the default menu alone
        }
        let Some(uri) = hit.image_uri() else {
            return false;
        };
        let Some((mime, data)) = decode_data_uri(&uri) else {
            // A remote image: WebKit's own download-backed item is the only way
            // to save it, so leave the menu untouched.
            return false;
        };

        // Replace the stock item in place so the menu keeps its familiar order.
        let Some(stock) = menu
            .items()
            .into_iter()
            .find(|i| i.stock_action() == webkit6::ContextMenuAction::DownloadImageToDisk)
        else {
            return false;
        };
        let position = menu.items().iter().position(|i| i == &stock).unwrap_or(0);

        let action = gtk::gio::SimpleAction::new("vireo-save-image", None);
        let window = view.root().and_downcast::<gtk::Window>();
        action.connect_activate(move |_, _| {
            let dialog = gtk::FileDialog::builder()
                .initial_name(default_image_name(&mime))
                .title(&i18n("Save Image"))
                .build();
            let data = data.clone();
            dialog.save(window.as_ref(), gtk::gio::Cancellable::NONE, move |res| {
                if let Ok(file) = res {
                    if let Some(path) = file.path() {
                        let _ = std::fs::write(path, &data);
                    }
                }
            });
        });
        menu.remove(&stock);
        menu.insert(
            &webkit6::ContextMenuItem::from_gaction(&action, "Save Image As…", None),
            position as i32,
        );
        false // show the (edited) menu
    });

    webview.connect_decide_policy(|_view, decision, decision_type| {
        // Links (including ones inside sandboxed message iframes, and `_blank`
        // links that request a new window) open in the external browser.
        let is_nav = decision_type == webkit6::PolicyDecisionType::NavigationAction;
        let is_new_window = decision_type == webkit6::PolicyDecisionType::NewWindowAction;
        if is_nav || is_new_window {
            if let Some(nav) = decision.downcast_ref::<webkit6::NavigationPolicyDecision>() {
                if let Some(mut action) = nav.navigation_action() {
                    let clicked = is_new_window
                        || action.navigation_type() == webkit6::NavigationType::LinkClicked;
                    if clicked {
                        if let Some(uri) = action.request().and_then(|r| r.uri()) {
                            // Only web and mail links reach the desktop's handlers.
                            // An HTML body keeps its own `href` values verbatim, so
                            // without this a message could hand `file://`, `smb://`
                            // or any scheme a third-party app has registered to that
                            // app on a single click.
                            if is_launchable_uri(&uri) {
                                let _ = gtk::gio::AppInfo::launch_default_for_uri(
                                    &uri,
                                    None::<&gtk::gio::AppLaunchContext>,
                                );
                            } else {
                                tracing::warn!(
                                    "refused to open a link with an unsupported scheme: {}",
                                    uri.split(':').next().unwrap_or("?")
                                );
                            }
                        }
                        decision.ignore();
                        return true;
                    }
                }
            }
        }
        false
    });

    // Show the target URL as a tooltip while hovering a link. WebKit doesn't do
    // this itself; we track the hovered link and answer GTK's query-tooltip,
    // re-querying whenever the hovered link changes so it updates immediately.
    let hovered: std::rc::Rc<std::cell::RefCell<Option<String>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    webview.set_has_tooltip(true);

    let hq = hovered.clone();
    webview.connect_query_tooltip(move |_view, _x, _y, _keyboard, tooltip| {
        match hq.borrow().as_deref() {
            Some(uri) => {
                tooltip.set_text(Some(uri));
                true
            }
            None => false,
        }
    });

    let hm = hovered.clone();
    webview.connect_mouse_target_changed(move |view, hit, _modifiers| {
        let uri = if hit.context_is_link() {
            hit.link_uri().map(|s| s.to_string())
        } else {
            None
        };
        if *hm.borrow() != uri {
            *hm.borrow_mut() = uri;
            view.trigger_tooltip_query();
        }
    });

    webview
}

/// Whether a clicked link may be handed to the desktop's default handler.
///
/// An allowlist, not a blocklist: every scheme a desktop registers is a program
/// that would be started with a sender-controlled argument, and there is no way
/// to enumerate the dangerous ones ahead of time.
fn is_launchable_uri(uri: &str) -> bool {
    match uri.split_once(':') {
        Some((scheme, rest)) => {
            matches!(
                scheme.to_ascii_lowercase().as_str(),
                "http" | "https" | "mailto"
            ) && !rest.is_empty()
        }
        None => false,
    }
}

/// What to show in the link preview: the destination, plus an explicit warning
/// when the link's visible text claims a different site than it goes to — the
/// oldest phishing trick there is (`click here: paypal.com` pointing elsewhere).
fn link_destination(uri: &str, label: Option<&str>) -> String {
    match label.and_then(|l| mismatched_host(uri, l)) {
        Some(claimed) => format!("{uri}  ⚠ looks like \"{claimed}\" but goes to {}", host_of(uri).unwrap_or_default()),
        None => uri.to_string(),
    }
}

/// The host a link's visible text claims, when that text is itself a URL or bare
/// hostname pointing somewhere other than the link's real target.
fn mismatched_host(uri: &str, label: &str) -> Option<String> {
    let claimed = host_of(label.trim())?;
    let actual = host_of(uri)?;
    // Compare from the right so `mail.example.com` matches `example.com`.
    let same = actual == claimed
        || actual.ends_with(&format!(".{claimed}"))
        || claimed.ends_with(&format!(".{actual}"));
    (!same).then_some(claimed)
}

/// The hostname of a URL, or of a bare hostname like `paypal.com/login`.
fn host_of(text: &str) -> Option<String> {
    let text = text.trim();
    // A scheme has no dots, which is what separates `mailto:` from a bare
    // `example.com:8080`. Only web links have a host worth comparing.
    let rest = match text.split_once(':') {
        Some((scheme, rest))
            if !scheme.is_empty()
                && !scheme.contains('.')
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-')) =>
        {
            if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
                return None;
            }
            rest.trim_start_matches("//")
        }
        _ => text,
    };
    let host = rest
        .split(['/', '?', '#'])
        .next()?
        .rsplit('@') // strip any userinfo
        .next()?
        .split(':')
        .next()?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let looks_like_host = host.contains('.')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    looks_like_host.then_some(host)
}

/// Split a `data:<mime>;base64,<payload>` URI into its MIME type and bytes.
/// Returns `None` for any other scheme, or for a `data:` URI that isn't base64
/// (the reader only ever emits base64 ones).
fn decode_data_uri(uri: &str) -> Option<(String, Vec<u8>)> {
    let rest = uri.strip_prefix("data:").or_else(|| uri.strip_prefix("DATA:"))?;
    let (meta, payload) = rest.split_once(',')?;
    let mime = meta.strip_suffix(";base64")?;
    let data = gtk::glib::base64_decode(payload);
    (!data.is_empty()).then(|| (mime.to_ascii_lowercase(), data))
}

/// A sensible filename to pre-fill the save dialog with. The original name isn't
/// recoverable from a `data:` URI — the properly named copy is in the attachment
/// drawer — so offer `image.<ext>` derived from the MIME type.
fn default_image_name(mime: &str) -> String {
    let ext = match mime {
        "image/jpeg" => "jpg",
        "image/svg+xml" => "svg",
        other => other.rsplit('/').next().unwrap_or("img"),
    };
    format!("image.{ext}")
}

/// Get the sender's key into the keyring (#133): the Autocrypt key the
/// message carried first, then WKD and the keyservers by address and by the
/// key id the signature named. Runs off the main thread (network).
fn fetch_sender_key(pgp: &crate::models::PgpStatus) -> Result<String, String> {
    let gpg = crate::pgp::Gpg::system();
    if let Some(b64) = &pgp.autocrypt {
        if let Some(bytes) = crate::oauth::base64_decode(b64) {
            if let Ok(s) = crate::pgp::import_keys(&gpg, &bytes) {
                if !s.fingerprints.is_empty() {
                    return Ok(i18n("The sender's key was imported from the message."));
                }
            }
        }
    }
    let summary = crate::pgp::fetch_key(&gpg, &pgp.sender_addr, pgp.signing_key_id())?;
    Ok(if summary.imported > 0 {
        i18n("The sender's key was fetched and imported.")
    } else {
        i18n("The sender's key was already in your keyring.")
    })
}

/// One icon button on a conversation card's action line. The icon is the same
/// embedded symbolic SVG the toolbar draws, inlined (its paths carry no fill,
/// so the document's `fill:currentColor` recolours it); when the resource
/// bundle isn't registered (tests), the button simply has no glyph.
fn card_action_button(key: (u32, u32), act: &str, icon: &str, title: &str) -> String {
    let svg = inline_icon_svg(icon);
    format!(
        "<button type=\"button\" class=\"vireo-act\" data-act=\"{act}\" \
         data-key=\"{aid}:{id}\" title=\"{title}\">{svg}</button>",
        aid = key.0,
        id = key.1,
    )
}

/// The card's "sender's formatting" toggle icon (#56), named in full here
/// so `tools/gen-icon-gresource.sh` (which scans for the prefixed literal)
/// bundles it; the card draws it inline through `inline_icon_svg`.
#[allow(dead_code)]
const SENDER_STYLE_ICON: &str = "co.hyprlab.Vireo-format-text-rich-symbolic";
/// The OpenPGP chip's lock (#133), named in full for the same reason.
#[allow(dead_code)]
const PGP_LOCK_ICON: &str = "co.hyprlab.Vireo-channel-secure-symbolic";

/// An embedded symbolic icon's SVG, inlined for the wrapper document (its
/// paths carry no fill, so the document's `fill:currentColor` recolours it);
/// empty when the resource bundle isn't registered (tests).
fn inline_icon_svg(icon: &str) -> String {
    let path =
        format!("/co/hyprlab/Vireo/icons/scalable/actions/co.hyprlab.Vireo-{icon}.svg");
    gtk::gio::resources_lookup_data(&path, gtk::gio::ResourceLookupFlags::NONE)
        .ok()
        .and_then(|b| String::from_utf8(b.to_vec()).ok())
        .map(|s| {
            s.replacen("<?xml version=\"1.0\" encoding=\"UTF-8\"?>", "", 1)
                .trim()
                .to_string()
        })
        .unwrap_or_default()
}

/// Whether a message body paints a background of its own. The colon/equals
/// forms keep prose that merely mentions "background" from counting.
fn paints_own_background(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    ["background:", "background-color:", "background-image:", "bgcolor="]
        .iter()
        .any(|needle| lower.contains(needle))
}

/// How many addresses a message's To + Cc headers name.
fn recipient_count(m: &Message) -> usize {
    [m.to.as_str(), m.cc.as_str()]
        .iter()
        .flat_map(|s| s.split(','))
        .filter(|a| !a.trim().is_empty())
        .count()
}

/// The collapsible To/Cc block inside a conversation card's header. Starts
/// hidden; the header's recipients chip toggles it. Empty when the message
/// names no recipients at all.
fn recipients_html(m: &Message, always_shown: bool) -> String {
    let mut lines = String::new();
    for (label, list) in [("To", m.to.trim()), ("Cc", m.cc.trim())] {
        if list.is_empty() {
            continue;
        }
        // Each recipient becomes a `.vireo-mail` span (click composes to it,
        // right-click offers copy/new-message), keyed by its bare address.
        let mut parts = String::new();
        for entry in split_addr_list(list) {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let mail = entry
                .rfind('<')
                .and_then(|s| entry[s + 1..].find('>').map(|e| &entry[s + 1..s + 1 + e]))
                .unwrap_or(entry)
                .trim();
            if !parts.is_empty() {
                parts.push_str(", ");
            }
            parts.push_str(&format!(
                "<span class=\"vireo-mail\" data-mail=\"{}\">{}</span>",
                escape_text(mail).replace('"', "&quot;"),
                escape_text(entry)
            ));
        }
        lines.push_str(&format!("<div><b>{label}:</b> {parts}</div>"));
    }
    if lines.is_empty() {
        return String::new();
    }
    let hidden = if always_shown { "" } else { " hidden" };
    format!("<div class=\"vireo-rcpt\"{hidden}>{lines}</div>")
}

/// Split an address-list header on commas, honouring double-quoted display
/// names (`"Doe, Jane" <j@x>` stays one entry).
fn split_addr_list(list: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    for (i, c) in list.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                out.push(&list[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&list[start..]);
    out
}

/// Every URL in `html` that would cause a fetch from a remote host, as byte
/// ranges into `html`.
///
/// One walk feeds both the detector and the stripper, so the banner and the
/// blocking can no longer disagree about what counts as remote. The pair of
/// substring lists this replaces had already drifted: `SRC="HTTP://…"` was
/// detected but not stripped, and `src = "http://…"` (whitespace around `=`,
/// which HTML permits) was neither.
fn remote_url_spans(html: &str) -> Vec<(usize, usize)> {
    let b = html.as_bytes();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        let closing = j < b.len() && b[j] == b'/';
        if closing {
            j += 1;
        }
        let name_start = j;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'-' || b[j] == b':') {
            j += 1;
        }
        if j == name_start {
            // Not a tag (a comment, a stray `<`, an entity) — nothing to parse.
            i += 1;
            continue;
        }
        let tag = html[name_start..j].to_ascii_lowercase();

        // Attributes, up to the closing `>`.
        while j < b.len() && b[j] != b'>' {
            if b[j].is_ascii_whitespace() || b[j] == b'/' {
                j += 1;
                continue;
            }
            let an_start = j;
            while j < b.len()
                && !matches!(b[j], b'=' | b'>' | b'/')
                && !b[j].is_ascii_whitespace()
            {
                j += 1;
            }
            if j == an_start {
                j += 1;
                continue;
            }
            let attr = String::from_utf8_lossy(&b[an_start..j]).to_ascii_lowercase();

            // `=` may be surrounded by whitespace; an attribute may have no value.
            let mut k = j;
            while k < b.len() && b[k].is_ascii_whitespace() {
                k += 1;
            }
            if k >= b.len() || b[k] != b'=' {
                continue;
            }
            k += 1;
            while k < b.len() && b[k].is_ascii_whitespace() {
                k += 1;
            }
            let (vs, ve) = if k < b.len() && (b[k] == b'"' || b[k] == b'\'') {
                let q = b[k];
                k += 1;
                let start = k;
                while k < b.len() && b[k] != q {
                    k += 1;
                }
                let end = k;
                if k < b.len() {
                    k += 1;
                }
                (start, end)
            } else {
                let start = k;
                while k < b.len() && b[k] != b'>' && !b[k].is_ascii_whitespace() {
                    k += 1;
                }
                (start, k)
            };
            j = k;

            match attr.as_str() {
                // "url 1x, url 2x, …" — the URL is each candidate's first token.
                "srcset" | "imagesrcset" => {
                    let mut at = vs;
                    while at < ve {
                        let mut end = at;
                        while end < ve && b[end] != b',' {
                            end += 1;
                        }
                        push_url(b, at, end, &mut spans);
                        at = end + 1;
                    }
                }
                "style" => push_css(b, vs, ve, &mut spans),
                _ if fetches(&tag, &attr) => push_url(b, vs, ve, &mut spans),
                _ => {}
            }
        }

        // A `<style>` element's body carries `url()` and `@import`. Only the
        // opening tag starts one — treating `</style>` as another would take the
        // rest of the document for stylesheet text and skip past everything in
        // it, which is how the `<img>` after a `<style>` block went unstripped.
        if tag == "style" && !closing {
            let start = (j + 1).min(b.len());
            let end = find_ci(b, b"</style", start).unwrap_or(b.len());
            push_css(b, start, end, &mut spans);
            j = end;
        }

        i = if j > i { j } else { i + 1 };
    }

    // Only spans that land on character boundaries can be sliced or replaced.
    spans.retain(|(s, e)| s < e && html.is_char_boundary(*s) && html.is_char_boundary(*e));
    spans.sort_unstable();
    spans.dedup();
    spans
}

/// Whether an attribute on this element causes a fetch on its own.
fn fetches(tag: &str, attr: &str) -> bool {
    match attr {
        "src" | "poster" | "background" | "lowsrc" | "dynsrc" | "codebase" | "xlink:href" => true,
        "data" => tag == "object",
        // `href` fetches on `<link>` (stylesheets, preloads, icons) and on SVG
        // `<image>`/`<use>`. On `<a>` it is a destination the user must click,
        // and treating those as remote content would put the banner on nearly
        // every message and so teach people to ignore it.
        "href" => matches!(tag, "link" | "image" | "use"),
        _ => false,
    }
}

/// Record `b[start..end]` (trimmed) if it points at a remote host.
fn push_url(b: &[u8], start: usize, end: usize, spans: &mut Vec<(usize, usize)>) {
    let mut s = start;
    let mut e = end.min(b.len());
    while s < e && b[s].is_ascii_whitespace() {
        s += 1;
    }
    // For `srcset` the candidate is "<url> <descriptor>"; the URL ends at the
    // first space. For a plain attribute there is no descriptor to trim.
    let mut u = s;
    while u < e && !b[u].is_ascii_whitespace() {
        u += 1;
    }
    e = u;
    if s < e && is_remote_url(&b[s..e]) {
        spans.push((s, e));
    }
}

/// Record every remote `url(…)` and `@import "…"` in a stylesheet or `style=`.
fn push_css(b: &[u8], start: usize, end: usize, spans: &mut Vec<(usize, usize)>) {
    let end = end.min(b.len());
    let mut at = start;
    while let Some(p) = find_ci(&b[..end], b"url(", at) {
        if p >= end {
            break;
        }
        let mut k = p + 4;
        while k < end && b[k].is_ascii_whitespace() {
            k += 1;
        }
        let quote = if k < end && (b[k] == b'"' || b[k] == b'\'') {
            let q = b[k];
            k += 1;
            Some(q)
        } else {
            None
        };
        let term = quote.unwrap_or(b')');
        let vs = k;
        while k < end && b[k] != term && !(quote.is_none() && b[k].is_ascii_whitespace()) {
            k += 1;
        }
        if vs < k && is_remote_url(&b[vs..k]) {
            spans.push((vs, k));
        }
        at = (p + 4).max(k);
    }
    let mut at = start;
    while let Some(p) = find_ci(&b[..end], b"@import", at) {
        if p >= end {
            break;
        }
        let mut k = p + 7;
        while k < end && b[k].is_ascii_whitespace() {
            k += 1;
        }
        // The `url(…)` form is already covered by the loop above; this is the
        // bare-string form, `@import "//host/x.css"`.
        if k < end && (b[k] == b'"' || b[k] == b'\'') {
            let q = b[k];
            k += 1;
            let vs = k;
            while k < end && b[k] != q {
                k += 1;
            }
            if vs < k && is_remote_url(&b[vs..k]) {
                spans.push((vs, k));
            }
        }
        at = p + 7;
    }
}

/// Whether a URL reaches a remote host.
///
/// `data:` and `cid:` carry their own bytes, and a path-relative or root-relative
/// URL resolves against `vireo.localhost`, which serves nothing. A
/// protocol-relative `//host/x` does reach the network — that one is the bypass
/// the old substring list missed.
fn is_remote_url(u: &[u8]) -> bool {
    let u = std::str::from_utf8(u).unwrap_or("").trim();
    if u.starts_with("//") {
        return true;
    }
    match u.split_once(':') {
        Some((scheme, _)) if !scheme.is_empty() && scheme.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')
        }) => !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "data" | "cid" | "blocked" | "mailto" | "tel" | "about" | "javascript"
        ),
        _ => false,
    }
}

/// Case-insensitive byte search from `from`.
fn find_ci(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() || needle.is_empty() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
        .map(|p| p + from)
}

/// Neutralize remote resource references so nothing is fetched while blocked.
/// Targets resource-loading attributes only; `<a href>` links are left intact.
fn strip_remote(html: &str) -> String {
    let spans = remote_url_spans(html);
    if spans.is_empty() {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len());
    let mut at = 0usize;
    for (s, e) in spans {
        if s < at {
            continue; // overlapping match already rewritten
        }
        out.push_str(&html[at..s]);
        out.push_str("blocked://");
        at = e;
    }
    out.push_str(&html[at..]);
    out
}

/// Does the HTML reference remote (network-reachable) resources?
///
/// This decides whether the "remote content was blocked" banner appears, and
/// nothing more — the blocking itself follows the user's setting, not this. A
/// miss here is a missing banner, not a leaked request.
fn has_remote_resources(html: &str) -> bool {
    !remote_url_spans(html).is_empty()
}

/// Inject a Content-Security-Policy `<meta>` into the document head. When remote
/// content is disallowed only inline styles and `data:` URIs are permitted.
///
/// `allow_remote` is the user's own choice, never the output of
/// [`has_remote_resources`]. That is what makes this an independent second line
/// of defence: if the detector fails to spot a reference, the engine still
/// refuses the fetch.
// ===== Dark-mode colour adaptation (issue #35) =====
//
// Emails are designed for light rendering: dark text, light (or absent)
// backgrounds. In dark mode an email that sets `color:#333` but no background
// paints near-black text on the reader's dark ground. The sandboxed frames run
// no JavaScript, so the fix happens here, on the document text, at render
// time (never in the on-disk body cache): every colour the message declares is
// checked and, when its lightness is wrong for a dark ground, flipped in HSL —
// hue and saturation kept, lightness mirrored. Text colours darker than
// mid-grey become light; backgrounds lighter than mid-grey become dark;
// everything already suited to a dark ground is left untouched, so mail
// designed dark passes through unchanged.

/// Parse a CSS colour token to linear [r, g, b, a] in 0..=1. Handles hex
/// (#rgb/#rgba/#rrggbb/#rrggbbaa), rgb()/rgba() with numbers or percentages,
/// and the common named colours. `bare_hex` additionally accepts legacy
/// attribute values like `bgcolor=ffffff` with no `#`.
fn parse_css_color(token: &str, bare_hex: bool) -> Option<[f32; 4]> {
    let t = token.trim();
    let hex = |s: &str| -> Option<[f32; 4]> {
        let v = |i: usize, n: usize| {
            u8::from_str_radix(&s[i..i + n], 16)
                .ok()
                .map(|b| if n == 1 { (b * 17) as f32 / 255.0 } else { b as f32 / 255.0 })
        };
        match s.len() {
            3 => Some([v(0, 1)?, v(1, 1)?, v(2, 1)?, 1.0]),
            4 => Some([v(0, 1)?, v(1, 1)?, v(2, 1)?, v(3, 1)?]),
            6 => Some([v(0, 2)?, v(2, 2)?, v(4, 2)?, 1.0]),
            8 => Some([v(0, 2)?, v(2, 2)?, v(4, 2)?, v(6, 2)?]),
            _ => None,
        }
    };
    if let Some(rest) = t.strip_prefix('#') {
        return hex(rest);
    }
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("rgb(") || lower.starts_with("rgba(") {
        let inner = t[t.find('(')? + 1..].strip_suffix(')')?;
        let parts: Vec<&str> = inner
            .split(|c| c == ',' || c == '/' || char::is_whitespace(c))
            .filter(|p| !p.is_empty())
            .collect();
        if parts.len() < 3 {
            return None;
        }
        let chan = |s: &str| -> Option<f32> {
            if let Some(p) = s.strip_suffix('%') {
                p.trim().parse::<f32>().ok().map(|v| v / 100.0)
            } else {
                s.trim().parse::<f32>().ok().map(|v| v / 255.0)
            }
        };
        let alpha = |s: &str| -> Option<f32> {
            if let Some(p) = s.strip_suffix('%') {
                p.trim().parse::<f32>().ok().map(|v| v / 100.0)
            } else {
                s.trim().parse::<f32>().ok()
            }
        };
        return Some([
            chan(parts[0])?.clamp(0.0, 1.0),
            chan(parts[1])?.clamp(0.0, 1.0),
            chan(parts[2])?.clamp(0.0, 1.0),
            parts.get(3).and_then(|s| alpha(s)).unwrap_or(1.0).clamp(0.0, 1.0),
        ]);
    }
    let named: Option<u32> = match lower.as_str() {
        "black" => Some(0x000000),
        "white" => Some(0xffffff),
        "gray" | "grey" => Some(0x808080),
        "dimgray" | "dimgrey" => Some(0x696969),
        "darkgray" | "darkgrey" => Some(0xa9a9a9),
        "lightgray" | "lightgrey" => Some(0xd3d3d3),
        "gainsboro" => Some(0xdcdcdc),
        "whitesmoke" => Some(0xf5f5f5),
        "silver" => Some(0xc0c0c0),
        "red" => Some(0xff0000),
        "darkred" | "maroon" => Some(0x800000),
        "green" => Some(0x008000),
        "darkgreen" => Some(0x006400),
        "blue" => Some(0x0000ff),
        "navy" | "darkblue" => Some(0x000080),
        "midnightblue" => Some(0x191970),
        "purple" => Some(0x800080),
        "indigo" => Some(0x4b0082),
        "brown" => Some(0xa52a2a),
        "orange" => Some(0xffa500),
        "yellow" => Some(0xffff00),
        "teal" => Some(0x008080),
        "olive" => Some(0x808000),
        _ => None,
    };
    if let Some(rgb) = named {
        return Some([
            ((rgb >> 16) & 0xff) as f32 / 255.0,
            ((rgb >> 8) & 0xff) as f32 / 255.0,
            (rgb & 0xff) as f32 / 255.0,
            1.0,
        ]);
    }
    if bare_hex && t.len() == 6 && t.bytes().all(|b| b.is_ascii_hexdigit()) {
        return hex(t);
    }
    None
}

fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < f32::EPSILON {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if (max - r).abs() < f32::EPSILON {
        ((g - b) / d).rem_euclid(6.0)
    } else if (max - g).abs() < f32::EPSILON {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } / 6.0;
    (h, s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    if s <= 0.0 {
        return (l, l, l);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let f = |mut t: f32| {
        t = t.rem_euclid(1.0);
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    (f(h + 1.0 / 3.0), f(h), f(h - 1.0 / 3.0))
}

/// Flip a colour for the dark ground when its lightness calls for it: text
/// darker than mid-grey mirrors up (floored so it stays clearly readable),
/// backgrounds lighter than mid-grey mirror down (floored above pure black so
/// they read as surfaces, like the reader's own grounds). `None` = keep as is.
fn adapt_color(token: &str, background: bool, bare_hex: bool) -> Option<String> {
    let [r, g, b, a] = parse_css_color(token, bare_hex)?;
    if a <= 0.01 {
        return None; // effectively transparent either way
    }
    let (h, s, l) = rgb_to_hsl(r, g, b);
    let flipped = if background {
        if l <= 0.6 {
            return None;
        }
        (1.0 - l).max(0.08)
    } else {
        if l >= 0.5 {
            return None;
        }
        (1.0 - l).max(0.72)
    };
    let (nr, ng, nb) = hsl_to_rgb(h, s, flipped);
    let to8 = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    Some(if a < 1.0 {
        format!("rgba({},{},{},{:.2})", to8(nr), to8(ng), to8(nb), a)
    } else {
        format!("#{:02x}{:02x}{:02x}", to8(nr), to8(ng), to8(nb))
    })
}

/// Rewrite one CSS declaration list (an inline `style` attribute's content or
/// a rule body). Declarations are split at `;` outside parentheses and quotes
/// — data: URLs contain semicolons — and only `color`, `background-color`,
/// and `background`'s colour tokens are touched.
fn rewrite_declarations(decls: &str) -> String {
    let mut out = String::with_capacity(decls.len());
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut start = 0;
    let mut i = 0;
    while i < decls.len() {
        let c = decls[i..].chars().next().unwrap();
        // Comments may contain `;` — `/* background: #fff; */` must not split
        // the declaration after it in half (seen in the wild, issue #35).
        if quote.is_none() && decls[i..].starts_with("/*") {
            i += decls[i..].find("*/").map(|r| r + 2).unwrap_or(decls.len() - i);
            continue;
        }
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"') | (None, '\'') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => depth = depth.saturating_sub(1),
            (None, ';') if depth == 0 => {
                out.push_str(&rewrite_one_declaration(&decls[start..i]));
                out.push(';');
                start = i + 1;
            }
            _ => {}
        }
        i += c.len_utf8();
    }
    out.push_str(&rewrite_one_declaration(&decls[start..]));
    out
}

/// One `prop: value` declaration, colour-adapted when the property carries a
/// colour whose direction we know. Anything unrecognised passes through
/// byte-for-byte.
fn rewrite_one_declaration(decl: &str) -> String {
    // Step over any leading comments so `/* old */ background: #fff` still
    // parses to a property we recognise; the comment is kept verbatim.
    let mut p = 0;
    loop {
        let rest = &decl[p..];
        let trimmed = rest.trim_start();
        p += rest.len() - trimmed.len();
        if trimmed.starts_with("/*") {
            match trimmed.find("*/") {
                Some(e) => p += e + 2,
                None => return decl.to_string(),
            }
        } else {
            break;
        }
    }
    let (head, decl_body) = decl.split_at(p);
    let Some(colon) = decl_body.find(':') else { return decl.to_string() };
    let prop = decl_body[..colon].trim().to_ascii_lowercase();
    let value = &decl_body[colon + 1..];
    let background = match prop.as_str() {
        "color" => false,
        "background-color" | "background" => true,
        _ => return decl.to_string(),
    };
    // Keep any !important, transform the value's colour tokens.
    let (value_body, important) = match value.to_ascii_lowercase().find("!important") {
        Some(at) => (&value[..at], &value[at..]),
        None => (value, ""),
    };
    let mut rewritten = String::with_capacity(value_body.len());
    for piece in split_value_tokens(value_body) {
        match &piece {
            ValuePiece::Token(t) => match adapt_color(t, background, false) {
                Some(new) => rewritten.push_str(&new),
                None => rewritten.push_str(t),
            },
            ValuePiece::Raw(r) => rewritten.push_str(r),
        }
    }
    format!("{head}{}:{rewritten}{important}", &decl_body[..colon])
}

enum ValuePiece<'a> {
    /// A candidate colour token (word or function call).
    Token(&'a str),
    /// Whitespace, url(...), strings — copied verbatim.
    Raw(&'a str),
}

/// Split a CSS value into colour-candidate tokens and verbatim runs, keeping
/// `url(...)` and quoted strings intact (their contents are not colours, and
/// data: URLs may contain anything).
fn split_value_tokens(value: &str) -> Vec<ValuePiece<'_>> {
    let mut pieces = Vec::new();
    let lower = value.to_ascii_lowercase();
    let mut i = 0;
    while i < value.len() {
        let c = value[i..].chars().next().unwrap();
        if c.is_whitespace() || c == ',' {
            let start = i;
            while i < value.len() {
                let c = value[i..].chars().next().unwrap();
                if c.is_whitespace() || c == ',' {
                    i += c.len_utf8();
                } else {
                    break;
                }
            }
            pieces.push(ValuePiece::Raw(&value[start..i]));
        } else if lower[i..].starts_with("url(") {
            let start = i;
            i += 4;
            let mut depth = 1;
            while i < value.len() && depth > 0 {
                let c = value[i..].chars().next().unwrap();
                if c == '(' {
                    depth += 1;
                } else if c == ')' {
                    depth -= 1;
                }
                i += c.len_utf8();
            }
            pieces.push(ValuePiece::Raw(&value[start..i]));
        } else if c == '"' || c == '\'' {
            let start = i;
            i += 1;
            while i < value.len() {
                let ch = value[i..].chars().next().unwrap();
                i += ch.len_utf8();
                if ch == c {
                    break;
                }
            }
            pieces.push(ValuePiece::Raw(&value[start..i]));
        } else {
            // A word, possibly a function like rgb(...): take through balanced
            // parens if one opens immediately after the name.
            let start = i;
            while i < value.len() {
                let ch = value[i..].chars().next().unwrap();
                if ch.is_whitespace() || ch == ',' {
                    break;
                }
                i += ch.len_utf8();
                if ch == '(' {
                    let mut depth = 1;
                    while i < value.len() && depth > 0 {
                        let c2 = value[i..].chars().next().unwrap();
                        if c2 == '(' {
                            depth += 1;
                        } else if c2 == ')' {
                            depth -= 1;
                        }
                        i += c2.len_utf8();
                    }
                    break;
                }
            }
            pieces.push(ValuePiece::Token(&value[start..i]));
        }
    }
    pieces
}

/// Rewrite the declaration bodies inside a `<style>` block, leaving selectors,
/// at-rules, comments, and strings untouched. Brace-nesting (`@media { sel {
/// … } }`) is handled by only treating brace-content that closes without
/// opening another brace as declarations.
fn rewrite_css(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut i = 0;
    let mut seg_start = 0;
    let mut in_comment = false;
    let mut quote: Option<char> = None;
    let mut decl_start: Option<usize> = None;
    while i < css.len() {
        let c = css[i..].chars().next().unwrap();
        if in_comment {
            if css[i..].starts_with("*/") {
                in_comment = false;
                i += 2;
                continue;
            }
        } else if let Some(q) = quote {
            if c == q {
                quote = None;
            }
        } else if css[i..].starts_with("/*") {
            in_comment = true;
            i += 2;
            continue;
        } else if c == '"' || c == '\'' {
            quote = Some(c);
        } else if c == '{' {
            out.push_str(&css[seg_start..i + 1]);
            seg_start = i + 1;
            decl_start = Some(i + 1);
        } else if c == '}' {
            match decl_start.take() {
                // The segment closed without opening a nested brace: it is a
                // declaration list.
                Some(ds) => {
                    out.push_str(&rewrite_declarations(&css[ds..i]));
                    out.push('}');
                    seg_start = i + 1;
                }
                None => {
                    out.push_str(&css[seg_start..i + 1]);
                    seg_start = i + 1;
                }
            }
        }
        i += c.len_utf8();
    }
    out.push_str(&css[seg_start..]);
    out
}

/// Rewrite one tag's colour-bearing attributes: `style` (declarations),
/// `color`/`text` (text direction), `bgcolor` (background direction). `text`
/// only means a colour on `<body>`.
fn rewrite_tag_attrs(tag: &str) -> String {
    let lower = tag.to_ascii_lowercase();
    let is_body = lower.starts_with("<body");
    let mut out = String::with_capacity(tag.len());
    let mut i = 0;
    while i < tag.len() {
        let rest_lower = &lower[i..];
        let attr = ["style", "color", "bgcolor", "text"].iter().find(|a| {
            rest_lower.starts_with(**a)
                && i > 0
                && lower.as_bytes()[i - 1].is_ascii_whitespace()
                && rest_lower[a.len()..].trim_start().starts_with('=')
        });
        let Some(attr) = attr else {
            let c = tag[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
            continue;
        };
        if *attr == "text" && !is_body {
            let c = tag[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
            continue;
        }
        // name, '=', then a quoted or bare value.
        let eq = i + rest_lower.find('=').unwrap();
        let mut v = eq + 1;
        while v < tag.len() && tag.as_bytes()[v].is_ascii_whitespace() {
            v += 1;
        }
        let (val_start, val_end, quoted) = if v < tag.len()
            && (tag.as_bytes()[v] == b'"' || tag.as_bytes()[v] == b'\'')
        {
            let q = tag.as_bytes()[v] as char;
            let end = tag[v + 1..].find(q).map(|r| v + 1 + r).unwrap_or(tag.len());
            (v + 1, end, true)
        } else {
            let end = tag[v..]
                .find(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/')
                .map(|r| v + r)
                .unwrap_or(tag.len());
            (v, end, false)
        };
        let value = &tag[val_start..val_end];
        let new = match *attr {
            "style" => rewrite_declarations(value),
            "bgcolor" => adapt_color(value, true, true).unwrap_or_else(|| value.to_string()),
            _ => adapt_color(value, false, true).unwrap_or_else(|| value.to_string()),
        };
        out.push_str(&tag[i..val_start]);
        out.push_str(&new);
        i = val_end + usize::from(quoted && val_end < tag.len());
        if quoted && val_end < tag.len() {
            out.push(tag.as_bytes()[val_end] as char);
        }
    }
    out
}

/// Pin every `prefers-color-scheme` media query in the document to the ground
/// the reader actually chose, rather than the OS setting WebKit reads.
///
/// The reader sets each message's ground itself (a light card, or a dark one
/// via [`adapt_colors_for_dark`]) and injects a `color-scheme` to match. But
/// `@media (prefers-color-scheme: dark)` inside the sandboxed frame is evaluated
/// against the *desktop's* preference, which our injected value does not change.
/// So an email that ships its own dark rules — a Google Calendar invite forcing
/// `color:#9aa0a6 !important` under `prefers-color-scheme: dark`, say — paints
/// that light-grey text onto our white card whenever the desktop is in dark mode
/// but the message is shown light (a light message theme, or a mismatch between
/// the app's and the desktop's schemes). The result is near-invisible grey on
/// white, and the mirror case (light rules on our dark ground) is just as bad.
///
/// Rewriting the media *feature* to a width test that is always or never true —
/// keeping any surrounding `only screen and …` intact — makes the email's own
/// light/dark rules follow our ground deterministically. `999999px` stands in
/// for "never" (no message is a million pixels wide) and `0px` for "always".
fn pin_color_scheme(doc: &str, dark: bool) -> String {
    // (feature we're neutralising, replacement that matches iff we chose it)
    let (dark_repl, light_repl) = if dark {
        ("min-width:0px", "min-width:999999px")
    } else {
        ("min-width:999999px", "min-width:0px")
    };
    let mut out = String::with_capacity(doc.len());
    let lower = doc.to_ascii_lowercase();
    let needle = "prefers-color-scheme";
    let mut i = 0;
    while let Some(rel) = lower[i..].find(needle) {
        let start = i + rel;
        // Parse `prefers-color-scheme` [ws] `:` [ws] (`dark`|`light`).
        let after = start + needle.len();
        let rest = lower[after..].trim_start();
        let ws = lower[after..].len() - rest.len();
        if let Some(colon) = rest.strip_prefix(':') {
            let val = colon.trim_start();
            let vws = colon.len() - val.len();
            let repl = if val.starts_with("dark") {
                Some((4usize, dark_repl))
            } else if val.starts_with("light") {
                Some((5usize, light_repl))
            } else {
                None
            };
            if let Some((vlen, replacement)) = repl {
                out.push_str(&doc[i..start]);
                out.push_str(replacement);
                // Skip the original `prefers-color-scheme : <value>` span.
                i = after + ws + 1 + vws + vlen;
                continue;
            }
        }
        // Not a real prefers-color-scheme:<value> — keep it verbatim and move on.
        out.push_str(&doc[i..after]);
        i = after;
    }
    out.push_str(&doc[i..]);
    out
}

/// The whole-document pass: `<style>` blocks through the CSS rewriter, every
/// other tag through the attribute rewriter, text content untouched. Tag ends
/// are found quote-aware, since `>` inside attribute values is legal HTML.
fn adapt_colors_for_dark(doc: &str) -> String {
    let lower = doc.to_ascii_lowercase();
    let mut out = String::with_capacity(doc.len() + 64);
    let mut i = 0;
    while i < doc.len() {
        let Some(rel) = doc[i..].find('<') else {
            out.push_str(&doc[i..]);
            break;
        };
        let tag_start = i + rel;
        out.push_str(&doc[i..tag_start]);
        if lower[tag_start..].starts_with("<!--") {
            let end = doc[tag_start..]
                .find("-->")
                .map(|r| tag_start + r + 3)
                .unwrap_or(doc.len());
            out.push_str(&doc[tag_start..end]);
            i = end;
            continue;
        }
        if lower[tag_start..].starts_with("<style") {
            let Some(open) = doc[tag_start..].find('>') else {
                out.push_str(&doc[tag_start..]);
                break;
            };
            let css_start = tag_start + open + 1;
            out.push_str(&doc[tag_start..css_start]);
            let css_end = lower[css_start..]
                .find("</style")
                .map(|r| css_start + r)
                .unwrap_or(doc.len());
            out.push_str(&rewrite_css(&doc[css_start..css_end]));
            i = css_end;
            continue;
        }
        // Quote-aware scan for the tag's closing '>'.
        let mut j = tag_start;
        let mut quote: Option<u8> = None;
        let mut end = doc.len();
        while j < doc.len() {
            let b = doc.as_bytes()[j];
            match quote {
                Some(q) if b == q => quote = None,
                Some(_) => {}
                None if b == b'"' || b == b'\'' => quote = Some(b),
                None if b == b'>' => {
                    end = j + 1;
                    break;
                }
                None => {}
            }
            j += 1;
        }
        out.push_str(&rewrite_tag_attrs(&doc[tag_start..end]));
        i = end;
    }
    out
}

/// The stylesheet that lays the reader's own fonts and colours over a message
/// (#56), or nothing when `style` overrides nothing.
///
/// `scope` is the element the rules hang under: `:root` in a sandboxed frame,
/// the message's own block in the printed document. Each selector carries
/// three `:not(#…)` clauses it trivially satisfies, so it outranks anything
/// a sender wrote short of an inline `!important`, ids included — the sheet
/// goes in last, and at equal weight the later rule wins.
///
/// Fonts: one family and size for everything, headings scaled from it so a
/// message keeps its hierarchy, code kept monospaced. Colours: the reader's
/// text on a transparent ground (the frame paints the card's), links in the
/// accent; `img` carries no colour, so pictures stand. Background images
/// go with the backgrounds: they are decoration in the same sense.
fn reader_style_css(style: &crate::config::ReaderStyle, dark: bool, accent: &str, scope: &str) -> String {
    if !style.active() {
        return String::new();
    }
    // "everything under the scope", at id weight.
    let all = format!("{scope} :not(#vireo-a):not(#vireo-b):not(#vireo-c)");
    let of = |sel: &str| format!("{scope} {sel}:not(#vireo-a):not(#vireo-b):not(#vireo-c)");
    let mut css = String::new();
    if let Some(font) = &style.font {
        let (family, size, face) = css_font(font);
        css.push_str(&format!(
            "{scope},{all}{{font-family:{family} !important;font-size:{size} !important;\
             line-height:1.45 !important;{face}}}\
             {h}{{line-height:1.25 !important;}}\
             {h1}{{font-size:calc({size} * 1.6) !important;}}\
             {h2}{{font-size:calc({size} * 1.35) !important;}}\
             {h3}{{font-size:calc({size} * 1.15) !important;}}\
             {small}{{font-size:calc({size} * 0.85) !important;}}\
             {code}{{font-family:monospace !important;font-size:calc({size} * 0.9) !important;}}",
            h = of(":is(h1,h2,h3,h4,h5,h6)"),
            h1 = of("h1"),
            h2 = of("h2"),
            h3 = of("h3"),
            small = of(":is(small,sub,sup)"),
            code = of(":is(pre,code,kbd,samp,tt)"),
        ));
    }
    if style.colors {
        let fg = if dark { "#e6e6e6" } else { "#1a1a1a" };
        css.push_str(&format!(
            "{scope},{all}{{color:{fg} !important;-webkit-text-fill-color:{fg} !important;\
             background-color:transparent !important;background-image:none !important;\
             text-shadow:none !important;}}\
             {a},{a} *{{color:{accent} !important;-webkit-text-fill-color:{accent} !important;}}",
            a = of("a"),
        ));
    }
    css
}

/// A Pango font description as a CSS family list, a size, and the face's
/// declarations. Pango sizes are points (or device pixels when absolute);
/// `0` means none was given, and the interface font's customary 11pt stands
/// in. The face — weight, italic, stretch — is written only where it departs
/// from regular: "Adwaita Sans Black" must come out black, but a regular
/// pick must not flatten the sender's own bold and italic.
fn css_font(desc: &str) -> (String, String, String) {
    let fd = gtk::pango::FontDescription::from_string(desc);
    let family = fd
        .family()
        .map(|f| f.to_string())
        .filter(|f| !f.trim().is_empty())
        .unwrap_or_else(|| "sans-serif".to_string());
    // Every family quoted (a name with spaces or digits needs it; the rest
    // tolerate it); a generic fallback closes the list.
    let families: Vec<String> = family
        .split(',')
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(|f| format!("\"{}\"", f.replace('\\', "").replace('"', "")))
        .collect();
    let generic = if family.to_ascii_lowercase().contains("mono") { "monospace" } else { "sans-serif" };
    let family = format!("{},{generic}", families.join(","));
    let units = fd.size();
    let size = if units <= 0 {
        "11pt".to_string()
    } else if fd.is_size_absolute() {
        format!("{}px", units as f64 / gtk::pango::SCALE as f64)
    } else {
        format!("{}pt", units as f64 / gtk::pango::SCALE as f64)
    };
    let mut face = String::new();
    use gtk::glib::translate::IntoGlib;
    let weight = fd.weight().into_glib();
    if weight != gtk::pango::Weight::Normal.into_glib() {
        // CSS takes 1–1000; Pango's scale is the same, `Ultraheavy` = 1000.
        face.push_str(&format!("font-weight:{} !important;", weight.clamp(1, 1000)));
    }
    match fd.style() {
        gtk::pango::Style::Italic => face.push_str("font-style:italic !important;"),
        gtk::pango::Style::Oblique => face.push_str("font-style:oblique !important;"),
        _ => {}
    }
    let stretch = match fd.stretch() {
        gtk::pango::Stretch::UltraCondensed => "ultra-condensed",
        gtk::pango::Stretch::ExtraCondensed => "extra-condensed",
        gtk::pango::Stretch::Condensed => "condensed",
        gtk::pango::Stretch::SemiCondensed => "semi-condensed",
        gtk::pango::Stretch::SemiExpanded => "semi-expanded",
        gtk::pango::Stretch::Expanded => "expanded",
        gtk::pango::Stretch::ExtraExpanded => "extra-expanded",
        gtk::pango::Stretch::UltraExpanded => "ultra-expanded",
        _ => "",
    };
    if !stretch.is_empty() {
        face.push_str(&format!("font-stretch:{stretch} !important;"));
    }
    (family, size, face)
}

/// Put a stylesheet at the end of a document's body, after everything the
/// sender wrote, so its rules are the last word.
fn inject_reader_style(doc: &str, css: &str) -> String {
    if css.is_empty() {
        return doc.to_string();
    }
    let block = format!("<style>{css}</style>");
    let lower = doc.to_ascii_lowercase();
    for close in ["</body", "</html"] {
        if let Some(at) = lower.rfind(close) {
            return format!("{}{block}{}", &doc[..at], &doc[at..]);
        }
    }
    format!("{doc}{block}")
}

fn inject_csp(html: &str, allow_remote: bool, dark: bool) -> String {
    let policy = if allow_remote {
        "default-src 'none'; img-src http: https: data: cid:; \
         style-src 'unsafe-inline' http: https: data:; \
         font-src http: https: data:; media-src http: https: data:"
    } else {
        "default-src 'none'; img-src data: cid:; style-src 'unsafe-inline' data:; \
         font-src data:; media-src data:"
    };
    let lower = html.to_ascii_lowercase();
    // Every message gets a comfortable default inset: the UA's 8px body margin
    // is reset so content sits at exactly 20px of breathing room. Injected
    // ahead of the email's own CSS, so a message that styles its body (a
    // full-bleed design, say) still wins.
    let body_pad = "body{margin:0;padding:20px;box-sizing:border-box;}";
    // `color-scheme` makes the browser's default colours (for content that sets
    // none of its own) follow the app's light/dark setting; styled emails keep
    // their own colours untouched.
    let scheme = if dark { "dark" } else { "light" };
    let supported = if dark { "dark light" } else { "light dark" };
    let theme = format!(
        "<meta name=\"color-scheme\" content=\"{supported}\">\
         <style>:root{{color-scheme:{scheme};}}{body_pad}\
         @media print{{:root{{color-scheme:light;}}html,body{{background:#fff !important;}}}}\
         </style>"
    );
    // `no-referrer` keeps the synthetic `vireo.localhost` base URI from leaking as
    // a Referer/Origin header — both for privacy and because hotlink-protected
    // servers (e.g. some DreamHost sites) return 403 to foreign referrers, which
    // otherwise blocks legitimate images even once the sender is trusted.
    let meta = format!(
        "{theme}<meta name=\"referrer\" content=\"no-referrer\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"{policy}\">"
    );

    if let Some(head) = lower.find("<head") {
        if let Some(close) = html[head..].find('>') {
            let at = head + close + 1;
            return format!("{}{meta}{}", &html[..at], &html[at..]);
        }
    }
    if let Some(htmltag) = lower.find("<html") {
        if let Some(close) = html[htmltag..].find('>') {
            let at = htmltag + close + 1;
            return format!("{}<head>{meta}</head>{}", &html[..at], &html[at..]);
        }
    }
    format!("<!doctype html><html><head>{meta}</head><body>{html}</body></html>")
}

/// Wrapper-document script: size each message iframe to its content height so the
/// whole conversation scrolls as one page (the iframes have no inner scrollbars).
/// Re-measures as images load and as content reflows.
/// The reader's two grounds. A message sits on [`GROUND`]; in a conversation the
/// cards sit on the slightly deeper [`PAGE`], which is what makes them read as
/// cards. The spinner and the cover behind the WebView use the same pair, so
/// handing over to the document changes nothing on screen.
/// The three grounds for `dark`, resolved from the live libadwaita theme
/// instead of hard-coded values (issue #62): GROUND is the theme's own view
/// background, PAGE shades it a step deeper so conversation cards (and the
/// composer, #148) keep reading as a layer under any theme variant, and
/// CHROME is the window's own ground. `widget` is only where the theme is
/// read from. When the caller asks for a scheme the app isn't currently in,
/// the theme can't answer for that mode, so the stock GNOME values stand in.
#[allow(deprecated)] // lookup_color: named theme colours have no successor yet
pub fn theme_grounds_for(widget: &impl IsA<gtk::Widget>, dark: bool) -> (String, String, String) {
    let style = widget.style_context();
    if dark == adw::StyleManager::default().is_dark() {
        if let Some(c) = style.lookup_color("view_bg_color") {
            let hex = |r: f32, g: f32, b: f32| {
                format!(
                    "#{:02x}{:02x}{:02x}",
                    (r * 255.0).round() as u8,
                    (g * 255.0).round() as u8,
                    (b * 255.0).round() as u8,
                )
            };
            // The stock pairs' own ratios: #1e1e1e→#141414 and #fff→#f1f1f1.
            let f = if dark { 0.667 } else { 0.945 };
            let ground = hex(c.red(), c.green(), c.blue());
            let page = hex(c.red() * f, c.green() * f, c.blue() * f);
            // The window's own ground — what the GTK reader header (the
            // subject block) sits on. A full-bleed single message paints
            // its in-document header this colour so subject and header
            // read as one surface.
            let chrome = style
                .lookup_color("window_bg_color")
                .map(|w| hex(w.red(), w.green(), w.blue()))
                .unwrap_or_else(|| (if dark { CHROME.1 } else { CHROME.0 }).to_string());
            return (ground, page, chrome);
        }
    }
    let (g, p, c) = if dark {
        (GROUND.1, PAGE.1, CHROME.1)
    } else {
        (GROUND.0, PAGE.0, CHROME.0)
    };
    (g.to_string(), p.to_string(), c.to_string())
}

const GROUND: (&str, &str) = ("#ffffff", "#1e1e1e");
const PAGE: (&str, &str) = ("#f1f1f1", "#141414");
/// The window chrome's ground (stock GNOME `window_bg_color`): what the GTK
/// reader header — the subject block — sits on. A full-bleed single message
/// paints its in-document header this colour so the two read as one surface.
const CHROME: (&str, &str) = ("#fafafa", "#242424");

thread_local! {
    /// The grounds as resolved from the live libadwaita theme, refreshed by
    /// `document_html` just before each build (issue #62). The document
    /// builder is a static fn so tests can exercise it without a display —
    /// this hands it the theme without widening that signature. `None` (as in
    /// tests) falls back to the stock GNOME values above.
    static LIVE_GROUNDS: std::cell::RefCell<Option<(String, String, String)>> =
        const { std::cell::RefCell::new(None) };
    /// The tags (#71), handed to the document builder the same way, for the
    /// card headers' chips. Empty in tests: no chips.
    static LIVE_TAGS: std::cell::RefCell<Vec<crate::config::Tag>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// That ground as a colour the WebView itself can be painted with.
fn ground_rgba(hex: &str) -> gtk::gdk::RGBA {
    let v = |i: usize| {
        u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0) as f32 / 255.0
    };
    gtk::gdk::RGBA::new(v(1), v(3), v(5), 1.0)
}

// The trailing window-blur listener is what makes a click *inside* a message
// body select its card: the sandboxed frames don't reliably dispatch
// parent-attached listeners, but a click into an iframe focuses it and blurs
// the wrapper window — caught there, the focused frame names the card, and
// focus is handed straight back so the next click fires again.
const SIZE_SCRIPT: &str = "\
function s(f){if(f._s)return;f._s=1;try{var d=f.contentDocument;if(!d)return;\
var b=d.body,e=d.documentElement;\
var sy=window.scrollY;var _r=f.getBoundingClientRect();\
var above=_r.bottom<=0;var old=_r.height||0;\
f.style.width='';void f.offsetWidth;\
var w=Math.max(b?b.scrollWidth:0,e?e.scrollWidth:0);\
if(w>f.clientWidth+1)f.style.width=w+'px';\
var prev=f.style.height;f.style.height='0px';void f.offsetHeight;\
var h=Math.max(b?b.scrollHeight:0,e?e.scrollHeight:0,b?b.offsetHeight:0);\
if(h>0){f.style.height=h+'px';\
if(f.dataset.key&&f._h!==h){f._h=h;\
try{window.webkit.messageHandlers.vireo.postMessage('size:'+f.dataset.key+':'+h);}catch(_){}}}\
else{f.style.height=prev;h=old;}\
if(sy>0)window.scrollTo(0,above?sy+(h-old):sy);\
chase();pin();\
}catch(_){}finally{f._s=0;}}\
function pick(k,e){reportPos();\
var mo=e.shiftKey?'r':((e.ctrlKey||e.metaKey)?'t':'p');\
if(mo!=='p'){try{e.preventDefault();\
var g=(e.view&&e.view.getSelection)?e.view.getSelection():null;if(g)g.removeAllRanges();}catch(_){}}\
else{try{var t=(e.view&&e.view.getSelection)?e.view.getSelection():null;\
if(t&&String(t).length)return;}catch(_){}}\
try{window.webkit.messageHandlers.vireo.postMessage('sel:'+k+':'+mo);}catch(_){}}\
var QS='.vireo-quote-attr,.gmail_quote,blockquote,#divRplyFwdMsg';\
var SIG='.moz-signature,#Signature,.gmail_signature,[class*=\"signature\"]';\
function isq(n){return n.nodeType===1&&(n.matches(QS)||!!n.querySelector(QS));}\
function blank(n){return n.nodeType===3?!n.textContent.trim():(n.nodeType!==1||!n.textContent.trim());}\
function trailer(top,q){\
if(q.matches('.vireo-quote-attr,#divRplyFwdMsg'))return true;\
var n=top.nextSibling;\
while(n&&(isq(n)||blank(n)))n=n.nextSibling;\
var t='';\
for(;n;n=n.nextSibling){\
if(n.nodeType===3)t+=n.textContent;\
else if(n.nodeType===1&&!n.matches(SIG)){\
var c=n.cloneNode(true);var sg=c.querySelectorAll(SIG);\
for(var i=0;i<sg.length;i++)sg[i].parentNode.removeChild(sg[i]);\
var br=c.querySelectorAll('br');\
for(var j=0;j<br.length;j++)br[j].parentNode.replaceChild(c.ownerDocument.createTextNode('\\n'),br[j]);\
t+='\\n'+c.textContent;}}\
t=t.replace(/\\u00a0/g,' ').trim();\
return !t||/^[-_]{2,}[ \\t]*(\\n|$)/.test(t);}\
function quote(f){try{var d=f.contentDocument;if(!d||!d.body||f._q)return;\
var sel=['.vireo-quote-attr','.gmail_quote','blockquote[type=\"cite\"]','#divRplyFwdMsg','blockquote'];\
var q=null;for(var i=0;i<sel.length&&!q;i++)q=d.querySelector(sel[i]);\
if(!q)return;\
var top=q;while(top.parentNode&&top.parentNode!==d.body)top=top.parentNode;\
if(!top.parentNode)return;\
var before=false;\
for(var n=d.body.firstChild;n&&n!==top;n=n.nextSibling){\
if(n.nodeType===1||(n.nodeType===3&&n.textContent.trim()))before=true;}\
if(!before)return;\
if(!trailer(top,q))return;\
f._q=1;\
var box=d.createElement('div');top.parentNode.insertBefore(box,top);\
while(box.nextSibling)box.appendChild(box.nextSibling);\
box.style.display='none';\
if(!f.parentNode)return;\
var b=document.createElement('button');b.className='vireo-quote';\
b.type='button';b.textContent='\u{2022}\u{2022}\u{2022}';\
b.setAttribute('title','Show quoted text');\
f.parentNode.insertBefore(b,f.nextSibling);\
b.addEventListener('click',function(e){e.stopPropagation();e.preventDefault();\
var on=box.style.display==='none';\
var from=f.getBoundingClientRect().height;\
box.style.display=on?'':'none';\
b.classList.toggle('open',on);\
b.setAttribute('title',on?'Hide quoted text':'Show quoted text');\
var to=0;try{var dd=f.contentDocument,bb=dd.body,ee=dd.documentElement;\
var prev=f.style.height;f.style.height='0px';void f.offsetHeight;\
to=Math.max(bb?bb.scrollHeight:0,ee?ee.scrollHeight:0,bb?bb.offsetHeight:0);\
f.style.height=prev;void f.offsetHeight;}catch(_){}\
if(!to){s(f);return;}\
f.style.height=from+'px';f.classList.add('anim');\
void f.offsetHeight;\
f.style.height=to+'px';\
setTimeout(function(){f.classList.remove('anim');s(f);},280);});}catch(_){}}\
function init(f){quote(f);s(f);try{var d=f.contentDocument;if(d){if(window.ResizeObserver&&d.body){new ResizeObserver(function(){s(f);}).observe(d.body);}\
if(f.dataset.key&&!f._c){f._c=1;d.addEventListener('click',function(e){\
if(e.target&&e.target.closest&&e.target.closest('a'))return;pick(f.dataset.key,e);});}\
if(!f._u){f._u=1;['wheel','touchstart','mousedown','keydown'].forEach(function(ev){\
d.addEventListener(ev,function(){follow=null;hold=null;},{passive:true,capture:true});});\
d.addEventListener('keydown',selAll,true);d.addEventListener('keydown',copySel,true);}\
var im=d.images||[];for(var i=0;i<im.length;i++){if(!im[i].complete){im[i].addEventListener('load',function(){s(f);});im[i].addEventListener('error',function(){s(f);});}}}}catch(_){}\
setTimeout(function(){s(f);},250);setTimeout(function(){s(f);},1000);}\
function all(){return document.querySelectorAll('iframe.vireo-frame');}\
document.addEventListener('DOMContentLoaded',function(){\
var fs=all();var pend=fs.length,rdy=false;\
function ready(){if(rdy)return;rdy=true;\
try{window.webkit.messageHandlers.vireo.postMessage('ready:0:0');}catch(_){}\
var bd=document.body.dataset;\
if(bd.vireoNoscroll){var a=(bd.vireoAnchor||'').split(':');\
if(a.length===3){var el=document.querySelector('.vireo-msg[data-key=\"'+a[0]+':'+a[1]+'\"]');\
if(el){hold={el:el,off:parseInt(a[2],10)||0};\
if(el.querySelector('.vireo-dot'))follow=el;\
setTimeout(pin,0);}}return;}\
var ds=document.querySelectorAll('.vireo-msg .vireo-dot');\
var d=ds.length?ds[0]:null;\
if(d){var m=d.closest('.vireo-msg');\
if(m)setTimeout(function(){try{follow=m;m.scrollIntoView({block:'start'});}catch(_){}},0);}\
else if(bd.vireoNewest){\
var nm=document.querySelector('.vireo-msg[data-key=\"'+bd.vireoNewest+'\"]');\
if(nm)setTimeout(function(){try{follow=nm;nm.scrollIntoView({block:'start'});}catch(_){}},0);}}\
setTimeout(markClipped,0);setTimeout(markClipped,400);\
if(!pend)ready();\
for(var i=0;i<fs.length;i++){(function(f){var counted=false;\
function tick(){if(counted)return;counted=true;if(--pend<=0)ready();}\
if(f.contentDocument&&f.contentDocument.readyState==='complete'){init(f);tick();}\
f.addEventListener('load',function(){init(f);tick();});})(fs[i]);}\
setTimeout(ready,450);\
var hs=document.querySelectorAll('.vireo-msg-hdr');\
for(var j=0;j<hs.length;j++){hs[j].addEventListener('dblclick',function(){\
try{window.webkit.messageHandlers.vireo.postMessage('open:'+this.dataset.key);}catch(_){}});}\
var rbd=document.body.dataset;\
if(rbd.vireoReadmark&&document.body.classList.contains('vireo-conv')){\
var rdel=parseInt(rbd.vireoReadmark,10)||250;var rt={};\
var rio=new IntersectionObserver(function(es){es.forEach(function(en){\
var el=en.target,k=el.dataset.key;\
var vis=en.intersectionRatio>=0.5||en.intersectionRect.height>window.innerHeight*0.6;\
if(vis&&!rt[k]){rt[k]=setTimeout(function(){\
try{window.webkit.messageHandlers.vireo.postMessage('seen:'+k);}catch(_){}\
rio.unobserve(el);},rdel);}\
else if(!vis&&rt[k]){clearTimeout(rt[k]);delete rt[k];}\
});},{threshold:[0,0.25,0.5,0.75,1]});\
var rsecs=document.querySelectorAll('.vireo-msg');\
for(var ri=0;ri<rsecs.length;ri++){if(rsecs[ri].querySelector('.vireo-dot'))rio.observe(rsecs[ri]);}}\
window.vf={marks:[],cur:-1};\
window.vfDocs=function(){var ds=[document];var fs=document.querySelectorAll('iframe');\
for(var i=0;i<fs.length;i++){try{if(fs[i].contentDocument)ds.push(fs[i].contentDocument);}catch(_){}}return ds;};\
window.vfCss=function(d){if(d.getElementById('vireo-find-css'))return;\
var st=d.createElement('style');st.id='vireo-find-css';\
st.textContent='.vireo-find{background:rgba(255,198,0,0.5);color:#000;border-radius:5px;box-shadow:0 0 0 2px rgba(255,198,0,0.5);}.vireo-find.current{background:#ffc600;box-shadow:0 0 0 2px #ffc600;}';\
(d.head||d.documentElement).appendChild(st);};\
window.vireoFindClear=function(){for(var i=0;i<vf.marks.length;i++){var m=vf.marks[i];\
var p=m.parentNode;if(!p)continue;while(m.firstChild)p.insertBefore(m.firstChild,m);p.removeChild(m);p.normalize();}\
vf.marks=[];vf.cur=-1;};\
window.vfPost=function(){try{window.webkit.messageHandlers.vireo.postMessage(\
'found:0:0:'+(vf.marks.length?vf.cur+1:0)+','+vf.marks.length);}catch(_){}};\
window.vfShow=function(){for(var i=0;i<vf.marks.length;i++)vf.marks[i].className='vireo-find'+(i===vf.cur?' current':'');\
vfPost();\
var m=vf.marks[vf.cur];if(!m)return;\
var d=m.ownerDocument;\
if(d===document){m.scrollIntoView({block:'center'});}\
else{var fs=document.querySelectorAll('iframe');\
for(var j=0;j<fs.length;j++){if(fs[j].contentDocument===d){\
var r=m.getBoundingClientRect(),fr=fs[j].getBoundingClientRect();\
window.scrollTo({top:window.scrollY+fr.top+r.top-window.innerHeight/2});break;}}}};\
window.vfY=function(m){var d=m.ownerDocument,r=m.getBoundingClientRect();\
if(d===document)return r.top+window.scrollY;\
var fs=document.querySelectorAll('iframe');\
for(var j=0;j<fs.length;j++){if(fs[j].contentDocument===d)\
return fs[j].getBoundingClientRect().top+window.scrollY+r.top;}\
return r.top;};\
window.vireoFindStep=function(dir){if(!vf.marks.length)return;\
vf.cur=(vf.cur+dir+vf.marks.length)%vf.marks.length;vfShow();};\
window.vireoFind=function(q){vireoFindClear();q=(q||'').toLowerCase();\
if(q){var docs=vfDocs();\
for(var di=0;di<docs.length;di++){var d=docs[di];vfCss(d);\
var w=d.createTreeWalker(d.body||d.documentElement,NodeFilter.SHOW_TEXT,null);\
var nodes=[];var n;\
while((n=w.nextNode())){var pn=n.parentNode&&n.parentNode.nodeName;\
if(pn==='SCRIPT'||pn==='STYLE')continue;\
if(n.nodeValue&&n.nodeValue.toLowerCase().indexOf(q)>=0)nodes.push(n);}\
for(var ni=0;ni<nodes.length;ni++){var node=nodes[ni];\
var text=node.nodeValue,lower=text.toLowerCase(),pos=0,idx;\
var frag=d.createDocumentFragment(),had=false;\
while((idx=lower.indexOf(q,pos))>=0){had=true;\
frag.appendChild(d.createTextNode(text.slice(pos,idx)));\
var sp=d.createElement('span');sp.className='vireo-find';\
sp.textContent=text.slice(idx,idx+q.length);\
frag.appendChild(sp);vf.marks.push(sp);pos=idx+q.length;}\
frag.appendChild(d.createTextNode(text.slice(pos)));\
if(had)node.parentNode.replaceChild(frag,node);}}\
vf.marks=vf.marks.filter(function(m){return m.getClientRects().length>0;});\
if(vf.marks.length){vf.marks.sort(function(a,b){return vfY(a)-vfY(b);});\
vf.cur=0;vfShow();}else{vfPost();}}else{vfPost();}};\
var rs=document.querySelectorAll('.vireo-rcpt-toggle');\
for(var r=0;r<rs.length;r++){rs[r].addEventListener('click',function(e){\
e.stopPropagation();e.preventDefault();\
var h=this.closest('.vireo-msg-hdr');var b=h?h.querySelector('.vireo-rcpt'):null;if(!b)return;\
var on=b.hasAttribute('hidden');\
if(on)b.removeAttribute('hidden');else b.setAttribute('hidden','');\
this.classList.toggle('open',on);\
this.setAttribute('title',on?'Hide recipients':'Show recipients');});\
rs[r].addEventListener('dblclick',function(e){e.stopPropagation();});}\
document.addEventListener('click',function(e){\
if(e.target&&e.target.closest&&e.target.closest('.vireo-msg'))return;\
try{window.webkit.messageHandlers.vireo.postMessage('desel:0:0');}catch(_){}});\
var ms=document.querySelectorAll('.vireo-msg');\
for(var q=0;q<ms.length;q++){ms[q].addEventListener('click',function(e){\
var k=this.dataset.key;if(k)pick(k,e);});}\
var actsDelay=parseInt(document.body.dataset.vireoActsdelay||'0',10)||1200;\
document.documentElement.style.setProperty('--acts-delay',actsDelay+'ms');\
var ts=document.querySelectorAll('.vireo-acts-toggle');\
for(var t=0;t<ts.length;t++){ts[t].addEventListener('click',function(e){\
e.stopPropagation();e.preventDefault();\
var h=this.closest('.vireo-msg-hdr');var ac=h?h.querySelector('.vireo-acts'):null;if(!ac)return;\
var on=!ac.classList.contains('open');\
ac.classList.toggle('open',on);this.classList.toggle('open',on);});\
ts[t].addEventListener('dblclick',function(e){e.stopPropagation();});}\
for(var q2=0;q2<ms.length;q2++){(function(card){var timer=null;\
card.addEventListener('mouseleave',function(){\
var ac=card.querySelector('.vireo-acts.open');if(!ac)return;\
timer=setTimeout(function(){ac.classList.remove('open');\
var tg=card.querySelector('.vireo-acts-toggle');if(tg)tg.classList.remove('open');},actsDelay);});\
card.addEventListener('mouseenter',function(){if(timer){clearTimeout(timer);timer=null;}});})(ms[q2]);}\
var as=document.querySelectorAll('.vireo-act');\
for(var k=0;k<as.length;k++){as[k].addEventListener('click',function(e){\
e.stopPropagation();e.preventDefault();reportPos();\
if(this.dataset.act==='star')this.classList.toggle('on');\
try{window.webkit.messageHandlers.vireo.postMessage(this.dataset.act+':'+this.dataset.key);}catch(_){}});\
as[k].addEventListener('dblclick',function(e){e.stopPropagation();});}\
});\
function markClipped(){var as=document.querySelectorAll('.vireo-addr');\
for(var i=0;i<as.length;i++){var a=as[i];\
a.classList.toggle('clipped',a.scrollWidth>a.clientWidth+1);}}\
function hideMailMenu(){var m=window.__vireoMailMenu;\
if(m&&m.parentNode)m.parentNode.removeChild(m);window.__vireoMailMenu=null;\
var sc=window.__vireoMailScrim;\
if(sc&&sc.parentNode)sc.parentNode.removeChild(sc);window.__vireoMailScrim=null;}\
function mailMsg(mail){try{window.webkit.messageHandlers.vireo.postMessage('composeto:0:0:'+mail);}catch(_){}}\
function showMailMenu(x,y,mail){hideMailMenu();\
var sc=document.createElement('div');sc.className='vireo-mailmenu-scrim';\
['mousedown','contextmenu'].forEach(function(ev){sc.addEventListener(ev,function(e){\
e.preventDefault();e.stopPropagation();hideMailMenu();});});\
document.body.appendChild(sc);window.__vireoMailScrim=sc;\
var mn=document.createElement('div');mn.className='vireo-mailmenu';\
function item(label,fn){var b=document.createElement('button');b.type='button';\
b.textContent=label;b.addEventListener('click',function(ev){\
ev.stopPropagation();ev.preventDefault();hideMailMenu();fn();});mn.appendChild(b);}\
item('New Message',function(){mailMsg(mail);});\
item('Copy Address',function(){try{window.webkit.messageHandlers.vireo.postMessage('copyaddr:0:0:'+mail);}catch(_){}});\
item('Add to Contacts',function(){try{window.webkit.messageHandlers.vireo.postMessage('addcontact:0:0:'+mail);}catch(_){}});\
mn.style.left=Math.max(0,Math.min(x,window.innerWidth-200))+'px';\
mn.style.top=Math.max(0,Math.min(y,window.innerHeight-124))+'px';\
document.body.appendChild(mn);window.__vireoMailMenu=mn;}\
document.addEventListener('click',function(e){var m=window.__vireoMailMenu;\
if(m&&e.target&&m.contains(e.target))return;hideMailMenu();},true);\
window.addEventListener('scroll',hideMailMenu,{passive:true});\
document.addEventListener('click',function(e){\
var t=e.target&&e.target.closest?e.target.closest('.vireo-mail'):null;\
if(!t||!t.dataset.mail)return;\
e.preventDefault();e.stopPropagation();mailMsg(t.dataset.mail);},true);\
document.addEventListener('click',function(e){\
var v=e.target&&e.target.closest?e.target.closest('.vireo-verify,.vireo-pgp'):null;\
if(!v||!v.dataset.key)return;e.preventDefault();e.stopPropagation();\
var r=v.getBoundingClientRect();\
try{window.webkit.messageHandlers.vireo.postMessage(\
'senderinfo:'+v.dataset.key+':'+r.left+','+r.top+','+r.width+','+r.height+','+window.innerWidth);}catch(_){}},true);\
document.addEventListener('contextmenu',function(e){\
var t=e.target&&e.target.closest?e.target.closest('.vireo-mail'):null;\
if(!t||!t.dataset.mail)return;\
e.preventDefault();e.stopPropagation();\
showMailMenu(e.clientX,e.clientY,t.dataset.mail);},true);\
window.addEventListener('resize',function(){var fs=all();for(var i=0;i<fs.length;i++)s(fs[i]);\
markClipped();});\
var _st;function reportPos(){var ms=document.querySelectorAll('.vireo-msg');\
var best=null,off=0;\
for(var i=0;i<ms.length;i++){var r=ms[i].getBoundingClientRect();\
if(r.top<=1){best=ms[i];off=Math.max(0,-r.top);}else break;}\
if(best&&best.dataset.key){try{window.webkit.messageHandlers.vireo.postMessage(\
'scrollat:'+best.dataset.key+':'+Math.round(off));}catch(_){}}}\
window.addEventListener('scroll',function(){clearTimeout(_st);\
_st=setTimeout(reportPos,120);},{passive:true});\
var follow=null,hold=null;\
function selAll(e){if(!(e.ctrlKey||e.metaKey)||e.altKey||(e.key!=='a'&&e.key!=='A'))return;\
if(!document.body||!document.body.classList.contains('vireo-conv'))return;\
e.preventDefault();e.stopPropagation();\
try{window.webkit.messageHandlers.vireo.postMessage('selall:0:0');}catch(_){}}\
window.addEventListener('keydown',selAll,true);\
/* Ctrl+C. Focus is kept in this document (see the blur handler below) so\
   the single-key shortcuts keep working, which leaves the selection in a\
   body frame and the native copy with nothing to copy. Copy from the\
   frame that holds the selection instead; if the engine refuses, hand the\
   text to the host, which sets the clipboard itself. */\
function copyDoc(d,w){var sel=d&&d.getSelection();if(!sel||sel.isCollapsed||!String(sel).length)return false;\
var ok=false;try{if(w!==window)w.focus();ok=d.execCommand('copy');}catch(_){}\
try{if(w!==window)window.focus();}catch(_){}\
if(!ok){try{window.webkit.messageHandlers.vireo.postMessage('copy:0:0:'+String(sel));}catch(_){}}\
showCopied();return true;}\
/* Copies whatever is selected, in this document or in a body frame. Called\
   from the keydown below when this view has the keyboard, and by the host\
   when it does not: the list takes GTK focus back after a click in the\
   reader, so Ctrl+C usually lands on the window, not here. */\
window.__vireoCopySel=function(){try{if(copyDoc(document,window))return true;\
var fs=all();for(var i=0;i<fs.length;i++){try{if(copyDoc(fs[i].contentDocument,fs[i].contentWindow))return true;}catch(_){}}}catch(_){}\
return false;};\
function copySel(e){if(!(e.ctrlKey||e.metaKey)||e.altKey||e.shiftKey||(e.key!=='c'&&e.key!=='C'))return;\
var s=window.getSelection();if(s&&!s.isCollapsed&&String(s).length)return;\
if(window.__vireoCopySel()){e.preventDefault();e.stopPropagation();}}\
function showCopied(){var b=document.body;if(!b)return;\
var p=document.getElementById('vireo-copied');\
if(!p){p=document.createElement('div');p.id='vireo-copied';p.className='vireo-copied';b.appendChild(p);}\
p.textContent=b.dataset.vireoCopied||'Copied';\
void p.offsetHeight;p.classList.add('on');clearTimeout(p._t);\
p._t=setTimeout(function(){p.classList.remove('on');},1400);}\
window.addEventListener('keydown',copySel,true);\
function chase(){if(!follow)return;try{follow.scrollIntoView({block:'start'});}catch(_){}}\
function pin(){if(follow||!hold)return;try{var r=hold.el.getBoundingClientRect();\
var d=Math.round(r.top+hold.off);if(d)window.scrollBy(0,d);}catch(_){}}\
['wheel','touchstart','mousedown','keydown'].forEach(function(ev){\
window.addEventListener(ev,function(){follow=null;hold=null;},{passive:true,capture:true});});\
window.addEventListener('blur',function(){setTimeout(function(){\
var a=document.activeElement;\
if(a&&a.tagName==='IFRAME'&&a.classList&&a.classList.contains('vireo-frame')&&a.dataset.key){\
reportPos();\
try{window.webkit.messageHandlers.vireo.postMessage('sel:'+a.dataset.key+':p');}catch(_){}\
try{a.blur();window.focus();}catch(_){}}},0);});";

/// One message body as a sandboxed iframe: its own document (so CSS can't leak to
/// other messages) with no `allow-scripts` (so the email can't run JavaScript).
/// `allow-same-origin` lets the wrapper script measure its height.
fn message_frame(
    body: &str,
    restrict: bool,
    dark: bool,
    key: (u32, u32),
    height: Option<u32>,
    style: &crate::config::ReaderStyle,
    accent: &str,
) -> String {
    let doc = body_html(body);
    let doc = if restrict { strip_remote(&doc) } else { doc };
    // Dark mode: adapt the message's own colours so dark-on-dark text can't
    // happen (issue #35). `color-scheme` only helps unstyled mail; anything
    // that sets explicit dark text without a background needs its colours
    // transformed, and the sandboxed frames run no JS to do it live. Moot
    // when the reader's own colours are laid over the message anyway.
    let doc = if dark && !style.colors { adapt_colors_for_dark(&doc) } else { doc };
    // Make the email's own light/dark rules follow the ground we chose rather
    // than the desktop's preference (see `pin_color_scheme`). Runs after the
    // dark adaptation so a message's hand-authored dark palette is used as-is,
    // not double-transformed.
    let doc = pin_color_scheme(&doc, dark);
    // The reader's own fonts and colours (#56), last so they win.
    let doc = inject_reader_style(&doc, &reader_style_css(style, dark, accent, ":root"));
    let doc = inject_csp(&doc, !restrict, dark);
    format!(
        // `allow-same-origin` lets our wrapper script measure the frame height;
        // `allow-popups` lets `_blank` links reach the policy handler (which opens
        // them externally). No `allow-scripts`, so the email's own JS never runs.
        //
        // Opening at the height it had last time means the conversation lays out
        // correctly on its first frame. Without it every card starts at the
        // browser's default and jumps once its content is measured, which is a
        // visible lurch on a thread that has simply been reopened.
        // The `.vireo-pan` wrapper is the horizontal scroller for mail wider
        // than the pane. The frame itself is widened to its content (see
        // `s(f)`), so the sandboxed document is never scrollable — WebKit
        // latches wheel gestures to the innermost scrollable area under the
        // pointer, and a frame document that can scroll sideways swallows
        // vertical wheel deltas it cannot use, leaving the reader stuck.
        // A top-document div has no such latch: unused vertical deltas
        // bubble on to the page.
        "<div class=\"vireo-pan\"><iframe class=\"vireo-frame\" data-key=\"{aid}:{id}\"{style} \
         sandbox=\"allow-same-origin allow-popups\" srcdoc=\"{doc}\"></iframe></div>",
        aid = key.0,
        id = key.1,
        style = match height {
            Some(h) => format!(" style=\"height:{h}px\""),
            None => String::new(),
        },
        doc = attr_escape(&doc)
    )
}

/// Remove a document's structural tags, keeping everything inside them.
///
/// Printing cannot use the reader's sandboxed iframes: a print engine draws an
/// iframe at its on-screen size — scrollbars included — and clips whatever does
/// not fit, so a long message came out as one cropped page. Inlining each
/// message into the printed document instead lets it flow across pages.
///
/// `<style>` and `<meta>` are deliberately kept: a message's own CSS is what
/// makes it look like itself on paper too.
fn inline_body(doc: &str) -> String {
    let mut out = String::with_capacity(doc.len());
    let lower = doc.to_ascii_lowercase();
    let mut i = 0;
    while i < doc.len() {
        if lower[i..].starts_with("<!doctype") {
            match doc[i..].find('>') {
                Some(end) => {
                    i += end + 1;
                    continue;
                }
                None => break,
            }
        }
        let structural = ["<html", "</html", "<head", "</head", "<body", "</body"];
        if let Some(tag) = structural.iter().find(|t| lower[i..].starts_with(**t)) {
            // Only a real tag: "<bodyguard" is content, "<body class=…" is not.
            let after = i + tag.len();
            let next = doc.as_bytes().get(after).copied().unwrap_or(b'>');
            if next == b'>' || next.is_ascii_whitespace() || next == b'/' {
                match doc[i..].find('>') {
                    Some(end) => {
                        i += end + 1;
                        continue;
                    }
                    None => break,
                }
            }
        }
        let ch = doc[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Point a message's own `html`/`body` rules at the block it prints in.
///
/// Inlining costs the isolation an iframe gave: with every message in one
/// document, one sender's `body{font-family:monospace}` would restyle the whole
/// printout, including the next message in the conversation. Redirecting those
/// selectors to `.vireo-print-msg` keeps the rule working where it is meant to
/// and nowhere else. Bare type selectors (`p`, `a`) still reach across, which is
/// the remaining price of printing a thread as one page.
fn scope_styles(doc: &str, block: &str) -> String {
    let lower = doc.to_ascii_lowercase();
    let mut out = String::with_capacity(doc.len());
    let mut rest = 0;
    while let Some(open) = lower[rest..].find("<style").map(|i| i + rest) {
        let Some(body_start) = lower[open..].find('>').map(|i| open + i + 1) else {
            break;
        };
        let end = lower[body_start..]
            .find("</style")
            .map(|i| body_start + i)
            .unwrap_or(doc.len());
        out.push_str(&doc[rest..body_start]);
        out.push_str(&scope_css(&doc[body_start..end], block));
        rest = end;
    }
    out.push_str(&doc[rest..]);
    out
}

/// Rewrite `html`/`body` in every selector of a stylesheet.
fn scope_css(css: &str, block: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut prelude = String::new();
    // What each open brace holds: rules (an at-rule such as `@media`) or
    // declarations. Only the former contains further selectors.
    let mut stack: Vec<bool> = Vec::new();
    let in_rules = |stack: &Vec<bool>| stack.last().copied().unwrap_or(true);
    for ch in css.chars() {
        match ch {
            '{' => {
                let holds_rules = prelude.trim_start().starts_with('@');
                if in_rules(&stack) {
                    out.push_str(&scope_selectors(&prelude, block));
                } else {
                    out.push_str(&prelude);
                }
                prelude.clear();
                stack.push(holds_rules);
                out.push('{');
            }
            '}' => {
                out.push_str(&prelude);
                prelude.clear();
                stack.pop();
                out.push('}');
            }
            _ => prelude.push(ch),
        }
    }
    out.push_str(&prelude);
    out
}

/// Replace whole-word `html`/`body` in a selector list with the message's block.
fn scope_selectors(prelude: &str, block: &str) -> String {
    let bytes = prelude.as_bytes();
    let mut out = String::with_capacity(prelude.len());
    let mut i = 0;
    while i < prelude.len() {
        let word = ["html", "body"]
            .into_iter()
            .find(|w| prelude[i..].to_ascii_lowercase().starts_with(w));
        // A tag name, not part of `.body`, `#body` or `bodyguard`.
        let boundary_before = i == 0 || !matches!(bytes[i - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'#' | b'%');
        if let Some(word) = word {
            let after = bytes.get(i + word.len()).copied();
            let boundary_after = !matches!(after, Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_'));
            if boundary_before && boundary_after {
                out.push_str(block);
                i += word.len();
                continue;
            }
        }
        let ch = prelude[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Rules for the printed document, which is a plain page rather than a stack of
/// frames: long URLs wrap instead of running off the sheet, images shrink to the
/// page, and each message after the first starts a new one.
const PRINT_DOCUMENT_STYLES: &str = "\
    html,body{background:#fff;color:#000;margin:0;padding:0;}\
    body{font:11pt/1.5 system-ui,sans-serif;overflow-wrap:anywhere;}\
    img,table,pre{max-width:100% !important;}\
    img{height:auto !important;}\
    pre{white-space:pre-wrap;}\
    .vireo-print-msg + .vireo-print-msg{border-top:1pt solid #999;margin-top:12pt;padding-top:12pt;}\
    .vireo-print-msghdr{font:bold 10pt/1.45 system-ui,sans-serif;color:#000;margin:0 0 8pt;}\
    .vireo-print-hdr{display:block;padding:0 0 10pt;margin:0 0 12pt;\
      border-bottom:1pt solid #999;font:10pt/1.45 system-ui,sans-serif;color:#000;}\
    .vireo-print-subject{font-size:14pt;font-weight:700;margin:0 0 8pt;}\
    .vireo-print-row{margin:0 0 2pt;}\
    .vireo-print-label{font-weight:700;}";

/// The header block that only appears on paper: subject, who it is from and to,
/// and when.
///
/// On screen these facts are in the pane above the message, which is a GTK widget
/// and therefore cannot be printed — WebKit prints the document it is showing,
/// and that document is the body alone. Printed mail without a sender or a date
/// is close to useless (issue #16), so the same facts go into the document and
/// are hidden with `@media`.
/// The class naming the `n`th message's block in the printed document.
fn print_block_class(n: usize) -> String {
    format!("vireo-print-m{n}")
}

/// Assemble the printed page: the header, then every message inlined into it.
fn print_document(
    header: &str,
    messages: &[(String, String)],
    allow_remote: bool,
) -> String {
    let mut body = header.to_string();
    for (n, (head, doc)) in messages.iter().enumerate() {
        let block = print_block_class(n);
        body.push_str(&format!("<article class=\"vireo-print-msg {block}\">"));
        body.push_str(head);
        body.push_str(&scope_styles(&inline_body(doc), &format!(".{block}")));
        body.push_str("</article>");
    }
    // The same content policy the reader's frames get: no scripts, and remote
    // content only when the sender is trusted.
    inject_csp(
        &format!(
            "<!doctype html><html><head><meta charset=\"utf-8\">\
             <style>{PRINT_DOCUMENT_STYLES}</style></head><body>{body}</body></html>"
        ),
        allow_remote,
        false,
    )
}

/// Who sent one message of a printed conversation, and when.
fn print_message_header_html(m: &Message) -> String {
    let from = if m.from_addr.is_empty() {
        escape_text(&m.from_name)
    } else if m.from_name.trim().is_empty() {
        escape_text(&m.from_addr)
    } else {
        format!(
            "{} &lt;{}&gt;",
            escape_text(&m.from_name),
            escape_text(&m.from_addr)
        )
    };
    format!(
        "<div class=\"vireo-print-msghdr\">{from} — {date}</div>",
        date = escape_text(&m.datetime_full())
    )
}

fn print_header_html(message: Option<&Message>) -> String {
    // Newest first, so a conversation is described by the message on top.
    let Some(m) = message else {
        return String::new();
    };
    let row = |label: &str, value: &str| -> String {
        if value.trim().is_empty() {
            return String::new();
        }
        format!(
            "<div class=\"vireo-print-row\"><span class=\"vireo-print-label\">{}</span> {}</div>",
            escape_text(label),
            escape_text(value)
        )
    };
    let from = if m.from_addr.trim().is_empty() {
        m.from_name.clone()
    } else if m.from_name.trim().is_empty() || m.from_name == m.from_addr {
        m.from_addr.clone()
    } else {
        format!("{} <{}>", m.from_name, m.from_addr)
    };
    let subject = if m.subject.trim().is_empty() {
        "(no subject)".to_string()
    } else {
        m.subject.clone()
    };
    format!(
        "<div class=\"vireo-print-hdr\">\
           <div class=\"vireo-print-subject\">{subject}</div>{from}{to}{cc}{date}\
         </div>",
        subject = escape_text(&subject),
        from = row("From:", &from),
        to = row("To:", &m.to),
        cc = row("Cc:", &m.cc),
        date = row("Date:", &m.datetime_full()),
    )
}

/// Escape text for HTML content: a subject or an address that contains `<` must
/// not become a tag.
fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escape a string for use inside a double-quoted HTML **attribute** value
/// (e.g. `srcdoc`), and nothing else.
///
/// This deliberately leaves `<` and `>` alone — inside a quoted attribute they
/// are ordinary characters, and the `srcdoc` payload is a whole HTML document
/// that must survive intact. It is therefore **not** safe for element text
/// content: use [`escape_text`] there.
fn attr_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;")
}

/// The worker stores ready-to-render HTML, but cached bodies from older versions
/// (or odd messages) may be tag-less plain text — wrap those so they read well.
fn body_html(body: &str) -> String {
    if body.contains('<') {
        body.to_string()
    } else {
        format!(
            "<!doctype html><html><head><meta charset=\"utf-8\"><style>\
             body{{margin:0;padding:20px;font:14px/1.5 system-ui,sans-serif;\
             white-space:pre-wrap;word-wrap:break-word}}\
             </style></head><body>{}</body></html>",
            body.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
        )
    }
}

/// A message subject reduced to something usable as a filename: no separators,
/// no leading dots, and short enough for any filesystem.
fn sanitize_filename(subject: &str) -> String {
    let cleaned: String = subject
        .chars()
        .map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { '-' } else { c })
        .collect();
    // Leading dots would make a hidden file; leading dashes are what a stripped
    // path separator leaves behind, and look like a command-line flag.
    let cleaned = cleaned.trim().trim_start_matches(['.', '-']).trim();
    if cleaned.is_empty() {
        return "Message".to_string();
    }
    cleaned.chars().take(120).collect::<String>().trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== Dark-mode colour adaptation (issue #35) =====

    /// The core failure: near-black text with no background of its own must
    /// come out light, or it vanishes on the dark ground.
    #[test]
    fn dark_text_is_lightened() {
        let out = adapt_colors_for_dark(r#"<p style="color:#000">x</p>"#);
        assert_eq!(out, r#"<p style="color:#ffffff">x</p>"#);
        let out = adapt_colors_for_dark(r#"<p style="color: #333333;">x</p>"#);
        assert_eq!(out, r#"<p style="color: #cccccc;">x</p>"#);
        let out = adapt_colors_for_dark(r#"<p style="color:rgb(20, 20, 20)">x</p>"#);
        assert!(out.contains("color:#ebebeb"), "{out}");
    }

    /// Light backgrounds mirror down to surfaces; the floor keeps them above
    /// pure black so they still read as cards.
    #[test]
    fn light_backgrounds_are_darkened() {
        let out = adapt_colors_for_dark(r#"<td style="background-color:#ffffff">x</td>"#);
        assert_eq!(out, r#"<td style="background-color:#141414">x</td>"#);
        let out = adapt_colors_for_dark(r#"<td style="background:#f6f6f6 no-repeat">x</td>"#);
        assert!(out.contains("background:#141414 no-repeat"), "{out}");
    }

    /// Mail already designed for a dark ground passes through unchanged.
    #[test]
    fn dark_designed_mail_is_untouched() {
        let doc = r#"<div style="color:#eeeeee;background-color:#222222">x</div>"#;
        assert_eq!(adapt_colors_for_dark(doc), doc);
    }

    /// Legacy attributes carry colours too — `<font color>` and `bgcolor`,
    /// with or without the leading `#`.
    #[test]
    fn legacy_color_attributes_are_adapted() {
        let out = adapt_colors_for_dark(r##"<font color="#111111">x</font>"##);
        assert_eq!(out, r##"<font color="#eeeeee">x</font>"##);
        let out = adapt_colors_for_dark(r#"<table bgcolor="ffffff"><tr></tr></table>"#);
        assert_eq!(out, r##"<table bgcolor="#141414"><tr></tr></table>"##);
        let out = adapt_colors_for_dark(r#"<body text="black" bgcolor="white">x</body>"#);
        assert_eq!(out, r##"<body text="#ffffff" bgcolor="#141414">x</body>"##);
    }

    /// `<style>` blocks are rewritten rule by rule — selectors untouched,
    /// declarations adapted, nesting (`@media`) survives.
    #[test]
    fn style_blocks_are_adapted() {
        let doc = "<style>p{color:black}@media screen{.x{background:white}}</style><p>x</p>";
        let out = adapt_colors_for_dark(doc);
        assert!(out.contains("p{color:#ffffff}"), "{out}");
        assert!(out.contains(".x{background:#141414}"), "{out}");
        assert!(out.contains("@media screen"), "{out}");
    }

    /// A light render must never let an email's `prefers-color-scheme: dark`
    /// rules fire — that is the light-grey-on-white bug. A dark render wants
    /// the reverse: dark rules on, light rules off. The surrounding query
    /// (`only screen and …`) and the declarations inside must survive.
    #[test]
    fn color_scheme_queries_follow_the_chosen_ground() {
        let css = "@media only screen and (prefers-color-scheme: dark){.a{color:#9aa0a6}}\
                   @media (prefers-color-scheme:light){.b{color:#000}}";
        let light = pin_color_scheme(css, false);
        assert!(light.contains("only screen and (min-width:999999px){.a"), "dark off: {light}");
        assert!(light.contains("(min-width:0px){.b"), "light on: {light}");

        let dark = pin_color_scheme(css, true);
        assert!(dark.contains("(min-width:0px){.a"), "dark on: {dark}");
        assert!(dark.contains("(min-width:999999px){.b"), "light off: {dark}");
    }

    /// The rewrite keys off the real media feature, not the substring: a class
    /// or text that merely contains the words is left alone.
    #[test]
    fn color_scheme_rewrite_ignores_non_feature_text() {
        let doc = "<p>we support prefers-color-scheme detection</p>";
        assert_eq!(pin_color_scheme(doc, false), doc);
        let cls = "<div class=\"prefers-color-scheme-badge\">x</div>";
        assert_eq!(pin_color_scheme(cls, true), cls);
    }

    /// A data: URL inside a background shorthand contains semicolons and
    /// base64 — it must pass through byte-for-byte while the colour beside it
    /// is still adapted.
    #[test]
    fn urls_survive_color_adaptation() {
        let doc = r#"<div style="background:url(data:image/png;base64,AAAA//12) #fff">x</div>"#;
        let out = adapt_colors_for_dark(doc);
        assert!(out.contains("url(data:image/png;base64,AAAA//12)"), "{out}");
        assert!(out.contains("#141414"), "{out}");
    }

    /// !important must survive, and colours in properties we don't understand
    /// must be left alone rather than guessed at.
    #[test]
    fn important_kept_and_unknown_props_untouched() {
        let out = adapt_colors_for_dark(r#"<p style="color:#000 !important">x</p>"#);
        assert!(out.contains("color:#ffffff !important"), "{out}");
        let doc = r#"<p style="border-color:#000;box-shadow:0 0 2px #000">x</p>"#;
        assert_eq!(adapt_colors_for_dark(doc), doc);
    }

    /// A CSS comment carrying a semicolon must not split the declaration
    /// after it — a real newsletter commented out one background and declared
    /// another right behind it, and the white slipped through (issue #35).
    #[test]
    fn comments_with_semicolons_do_not_hide_declarations() {
        let doc = "<style>.t{ /* background-color: #f0f2f5; */ background-color: #fff; }</style>";
        let out = adapt_colors_for_dark(doc);
        assert!(out.contains("background-color: #141414;"), "{out}");
        assert!(out.contains("/* background-color: #f0f2f5; */"), "comment kept: {out}");
        let out = adapt_colors_for_dark(r#"<p style="/* x; */ color: #000">x</p>"#);
        assert!(out.contains("/* x; */ color: #ffffff"), "{out}");
    }

    /// Mid-lightness brand colours sit fine on either ground: leave them.
    #[test]
    fn mid_tones_are_left_alone() {
        let doc = r#"<a style="color:#3584e4;background-color:#26a269">x</a>"#;
        assert_eq!(adapt_colors_for_dark(doc), doc);
    }

    /// The light path never rewrites anything.
    #[test]
    fn light_mode_frames_are_untouched() {
        let body = r#"<p style="color:#000">x</p>"#;
        let frame = message_frame(body, true, false, (1, 1), None, &crate::config::ReaderStyle::NONE, "#3584e4");
        assert!(frame.contains("color:#000"), "{frame}");
        let frame = message_frame(body, true, true, (1, 1), None, &crate::config::ReaderStyle::NONE, "#3584e4");
        assert!(!frame.contains("color:#000"), "{frame}");
    }

    /// With nothing overridden the frame carries no reader stylesheet.
    #[test]
    fn reader_style_none_adds_nothing() {
        let frame = message_frame("<p>x</p>", true, false, (1, 1), None, &crate::config::ReaderStyle::NONE, "#3584e4");
        assert!(!frame.contains(":not(#vireo-a)"), "{frame}");
        assert_eq!(inject_reader_style("<p>x</p>", ""), "<p>x</p>");
    }

    /// The font override sets one family and size over everything, after the
    /// sender's own stylesheet so it has the last word, with headings scaled
    /// and code kept monospaced.
    #[test]
    fn reader_font_lands_after_the_senders_css() {
        let style = crate::config::ReaderStyle { font: Some("DejaVu Serif 12".into()), colors: false };
        let body = "<html><head><style>p{font-family:Comic Sans MS}</style></head>\
                    <body><p style=\"font-size:30px\">x</p></body></html>";
        let frame = message_frame(body, true, false, (1, 1), None, &style, "#3584e4");
        let ours = frame.find(":root :not(#vireo-a):not(#vireo-b):not(#vireo-c)").expect("override sheet");
        let theirs = frame.find("Comic Sans").expect("sender css kept");
        assert!(ours > theirs, "the reader's sheet must come last: {frame}");
        // srcdoc attribute: quotes are escaped, so match the family unquoted.
        assert!(frame.contains("DejaVu Serif&quot;,sans-serif !important;font-size:12pt !important"), "{frame}");
        assert!(frame.contains("h1:not(#vireo-a):not(#vireo-b):not(#vireo-c){font-size:calc(12pt * 1.6) !important"), "{frame}");
        assert!(frame.contains(":is(pre,code,kbd,samp,tt):not(#vireo-a):not(#vireo-b):not(#vireo-c){font-family:monospace !important"), "{frame}");
        // Colours untouched: no colour rule was asked for.
        assert!(!frame.contains("-webkit-text-fill-color"), "{frame}");
    }

    /// The colour override paints the reader's text and clears backgrounds,
    /// links in the accent; on the dark ground the dark adaptation is
    /// skipped, since the sender's colours are not shown anyway.
    #[test]
    fn reader_colours_force_text_and_links() {
        let style = crate::config::ReaderStyle { font: None, colors: true };
        let body = r#"<p style="color:#000;background:#ff0">x <a href="https://e.example">l</a></p>"#;
        let light = message_frame(body, true, false, (1, 1), None, &style, "#3584e4");
        assert!(light.contains("color:#1a1a1a !important;-webkit-text-fill-color:#1a1a1a !important;background-color:transparent !important;background-image:none !important"), "{light}");
        assert!(light.contains(":root a:not(#vireo-a):not(#vireo-b):not(#vireo-c),:root a:not(#vireo-a):not(#vireo-b):not(#vireo-c) *{color:#3584e4 !important"), "{light}");
        assert!(!light.contains("font-family"), "no font rule was asked for: {light}");
        let dark = message_frame(body, true, true, (1, 1), None, &style, "#3584e4");
        assert!(dark.contains("color:#e6e6e6 !important"), "{dark}");
        assert!(dark.contains("color:#000"), "sender colours left as written when overridden: {dark}");
    }

    /// Pango descriptions become a quoted CSS family list with a generic
    /// fallback, and a point size; a missing size falls back to 11pt.
    #[test]
    fn css_font_reads_pango_descriptions() {
        let s = |v: &str| v.to_string();
        assert_eq!(css_font("Cantarell 11"), (s("\"Cantarell\",sans-serif"), s("11pt"), s("")));
        assert_eq!(
            css_font("JetBrains Mono Bold 10"),
            (s("\"JetBrains Mono\",monospace"), s("10pt"), s("font-weight:700 !important;"))
        );
        assert_eq!(css_font("Sans"), (s("\"Sans\",sans-serif"), s("11pt"), s("")));
        assert_eq!(css_font("Noto Serif 10.5"), (s("\"Noto Serif\",sans-serif"), s("10.5pt"), s("")));
        // The face the user picked reaches the page: black, italic, condensed.
        assert_eq!(
            css_font("Adwaita Sans Black 16"),
            (s("\"Adwaita Sans\",sans-serif"), s("16pt"), s("font-weight:900 !important;"))
        );
        assert_eq!(
            css_font("Cantarell Condensed Light Italic 12").2,
            "font-weight:300 !important;font-style:italic !important;font-stretch:condensed !important;"
        );
    }

    /// The card's "sender's formatting" toggle: offered on every card while
    /// an override is on, lit on the card it was used on, and that card's
    /// frame goes out without the reader's sheet.
    #[test]
    fn sender_style_toggle_escapes_one_card() {
        let mut a = msg_for_print();
        a.body = "<p>one</p>".into();
        let mut b = msg_for_print();
        b.id = 2;
        b.body = "<p>two</p>".into();
        let style = crate::config::ReaderStyle { font: Some("Cantarell 11".into()), colors: false };
        let mut escaped = std::collections::HashSet::new();
        escaped.insert((a.account_id, a.id));
        let doc = MessageView::conversation_document(
            &[a.clone(), b.clone()],
            &Default::default(),
            &Default::default(),
            &Default::default(),
            &[],
            "#3584e4",
            true,
            false,
            false,
            false,
            &style,
            &escaped,
        );
        assert_eq!(doc.matches("data-act=\"senderfmt\"").count(), 2, "{doc}");
        assert_eq!(doc.matches("class=\"vireo-act on\" data-act=\"senderfmt\"").count(), 1, "{doc}");
        assert_eq!(doc.matches(":root :not(#vireo-a)").count(), 1, "only the other card is styled: {doc}");

        // No override: no toggle, nothing injected.
        let plain = MessageView::conversation_document(
            &[a, b],
            &Default::default(),
            &Default::default(),
            &Default::default(),
            &[],
            "#3584e4",
            true,
            false,
            false,
            false,
            &crate::config::ReaderStyle::NONE,
            &escaped,
        );
        assert!(!plain.contains("senderfmt"), "{plain}");
        assert!(!plain.contains(":not(#vireo-a)"), "{plain}");
    }

    /// The sheet goes in before `</body>` (or `</html>`), and at the very end
    /// of a bare fragment.
    #[test]
    fn reader_style_is_injected_last() {
        assert_eq!(
            inject_reader_style("<html><body><p>x</p></body></html>", "p{}"),
            "<html><body><p>x</p><style>p{}</style></body></html>"
        );
        assert_eq!(
            inject_reader_style("<html><p>x</p></HTML>", "p{}"),
            "<html><p>x</p><style>p{}</style></HTML>"
        );
        assert_eq!(inject_reader_style("<p>x</p>", "p{}"), "<p>x</p><style>p{}</style>");
    }

    fn msg_for_print() -> Message {
        Message {
            id: 1,
            account_id: 1,
            folder_id: 1,
            uid: 1,
            from_name: "Ada Lovelace".into(),
            from_addr: "ada@example.com".into(),
            reply_to: String::new(),
            to: "me@example.com".into(),
            cc: "carol@example.com".into(),
            subject: "Quarterly numbers".into(),
            preview: String::new(),
            body: "<p>hi</p>".into(),
            date: "09:14".into(),
            timestamp: 0,
            unread: false,
            starred: false,
            keywords: Vec::new(),
            has_attachment: false,
            message_id: String::new(),
            references: String::new(),
        }
    }

    /// A conversation is a stack of cards in the order it is handed, so the
    /// reader shows the message that started it first and the newest last.
    #[test]
    fn a_conversation_renders_one_card_per_message_in_order() {
        let mut first = msg_for_print();
        first.body = "<p>opening</p>".into();
        let mut second = msg_for_print();
        second.id = 2;
        second.from_name = "Grace Hopper".into();
        second.body = "<p>reply</p>".into();

        let doc = MessageView::conversation_document(
            &[first, second],
            &std::collections::HashMap::new(),
            &Default::default(),
            &Default::default(),
            &[],
            "#3584e4",
            true,
            false,
            false,
            false,
            &crate::config::ReaderStyle::NONE,
            &Default::default(),
        );
        assert_eq!(
            doc.matches("<section class=\"vireo-msg\"").count(),
            2,
            "each message gets its own card: {doc}"
        );
        let ada = doc.find("Ada Lovelace").expect("first sender present");
        let grace = doc.find("Grace Hopper").expect("second sender present");
        assert!(ada < grace, "cards must keep the order they were handed");
        assert!(doc.contains("<body class=\"vireo-conv\">"), "conversation padding");
        // The cards sit on the deeper page — the same colour the spinner and the
        // cover behind the WebView are painted, so the handover is invisible.
        assert!(doc.contains(&format!("background:{}", PAGE.0)), "page ground: {doc}");
    }

    /// Each card names everyone the message went to, collapsed behind a chip so
    /// the header stays one line tall — and a recipient list is header text an
    /// attacker controls, so it must land escaped in the trusted wrapper.
    #[test]
    fn every_card_carries_its_recipients_escaped_and_collapsed() {
        let mut first = msg_for_print();
        first.to = "me@example.com, <script>alert(1)</script>@evil.test".into();
        let mut second = msg_for_print();
        second.id = 2;
        second.cc = String::new();

        let doc = MessageView::conversation_document(
            &[first, second],
            &std::collections::HashMap::new(),
            &Default::default(),
            &Default::default(),
            &[],
            "#3584e4",
            true,
            false,
            false,
            false,
            &crate::config::ReaderStyle::NONE,
            &Default::default(),
        );
        assert_eq!(
            doc.matches("class=\"vireo-rcpt\" hidden").count(),
            2,
            "each card gets a collapsed To/Cc block: {doc}"
        );
        assert_eq!(
            doc.matches("class=\"vireo-rcpt-toggle\"").count(),
            2,
            "each card gets a recipients chip: {doc}"
        );
        // 3 addresses on the first card (2 To + 1 Cc), 1 on the second.
        assert!(doc.contains(">3 recipients<"), "recipient count on the chip: {doc}");
        assert!(doc.contains(">1 recipient<"), "singular for one recipient: {doc}");
        assert!(!doc.contains("<script>alert(1)</script>"), "recipients must be escaped: {doc}");
        assert!(doc.contains("&lt;script&gt;"), "escaped form present: {doc}");
    }

    /// A message that names no recipients at all gets neither chip nor block.
    #[test]
    fn a_card_with_no_recipients_shows_no_chip() {
        let mut only = msg_for_print();
        only.to = String::new();
        only.cc = " ".into();
        let mut second = msg_for_print();
        second.id = 2;
        let doc = MessageView::conversation_document(
            &[only, second],
            &std::collections::HashMap::new(),
            &Default::default(),
            &Default::default(),
            &[],
            "#3584e4",
            true,
            false,
            false,
            false,
            &crate::config::ReaderStyle::NONE,
            &Default::default(),
        );
        assert_eq!(
            doc.matches("<button type=\"button\" class=\"vireo-rcpt-toggle\"").count(),
            1,
            "only the second card: {doc}"
        );
    }

    /// Each card carries its own Reply/Reply all/Forward, keyed to that message
    /// — the toolbar's buttons are disabled in a conversation precisely because
    /// they could not say which message they meant.
    #[test]
    fn every_card_carries_its_own_actions_keyed_to_that_message() {
        let first = msg_for_print();
        let mut second = msg_for_print();
        second.id = 2;
        let doc = MessageView::conversation_document(
            &[first, second],
            &std::collections::HashMap::new(),
            &Default::default(),
            &Default::default(),
            &[],
            "#3584e4",
            true,
            false,
            false,
            false,
            &crate::config::ReaderStyle::NONE,
            &Default::default(),
        );
        for act in ["reply", "replyall", "forward"] {
            assert_eq!(
                doc.matches(&format!("data-act=\"{act}\"")).count(),
                2,
                "one {act} button per card: {doc}"
            );
        }
        // Keyed per message, so the action can't land on the wrong one.
        assert!(doc.contains("data-act=\"reply\" data-key=\"1:1\""), "{doc}");
        assert!(doc.contains("data-act=\"reply\" data-key=\"1:2\""), "{doc}");
    }

    /// An unread message in a conversation is marked with its dot; reading is
    /// click-driven now, so no scroll sentinel exists for any message.
    #[test]
    fn only_unread_conversation_messages_are_marked() {
        let read = msg_for_print();
        let mut unread = msg_for_print();
        unread.id = 2;
        unread.unread = true;

        let doc = MessageView::conversation_document(
            &[read, unread],
            &std::collections::HashMap::new(),
            &Default::default(),
            &Default::default(),
            &[],
            "#3584e4",
            true,
            false,
            false,
            false,
            &crate::config::ReaderStyle::NONE,
            &Default::default(),
        );
        assert_eq!(doc.matches("class=\"vireo-dot\"").count(), 1, "one dot: {doc}");
        assert_eq!(doc.matches("class=\"vireo-end\"").count(), 0, "no sentinel: {doc}");
        // Keyed to the unread message, so reading it can't clear another's.
        assert!(doc.contains("class=\"vireo-dot\" data-key=\"1:2\""), "{doc}");
    }

    /// Marking a message unread while its conversation is open must survive the
    /// message being in view: it keeps its dot, but no sentinel, so nothing
    /// reads it back until the conversation is opened afresh.
    #[test]
    fn a_deliberately_unread_message_keeps_its_mark() {
        let read = msg_for_print();
        let mut unread = msg_for_print();
        unread.id = 2;
        unread.unread = true;
        let suppressed: std::collections::HashSet<(u32, u32)> = [(1u32, 2u32)].into();

        let doc = MessageView::conversation_document(
            &[read, unread],
            &std::collections::HashMap::new(),
            &suppressed,
            &Default::default(),
            &[],
            "#3584e4",
            true,
            false,
            false,
            false,
            &crate::config::ReaderStyle::NONE,
            &Default::default(),
        );
        assert_eq!(doc.matches("class=\"vireo-dot\"").count(), 1, "still marked: {doc}");
        assert_eq!(doc.matches("class=\"vireo-end\"").count(), 0, "no sentinel: {doc}");
    }

    /// A frame opens at the height it had last time, so a reopened conversation
    /// lays out on its first frame rather than every card jumping once measured.
    #[test]
    fn a_known_frame_height_is_used_on_reopen() {
        let a = msg_for_print();
        let mut b = msg_for_print();
        b.id = 2;
        let heights: std::collections::HashMap<(u32, u32), u32> = [((1u32, 2u32), 640u32)].into();

        let doc = MessageView::conversation_document(
            &[a, b],
            &Default::default(),
            &Default::default(),
            &heights,
            &[],
            "#3584e4",
            true,
            false,
            false,
            false,
            &crate::config::ReaderStyle::NONE,
            &Default::default(),
        );
        assert!(doc.contains("style=\"height:640px\""), "known height used: {doc}");
        assert_eq!(doc.matches("style=\"height:").count(), 1, "only the known one");
        // Keyed per message, so a height can't be applied to the wrong frame.
        assert!(doc.contains("data-key=\"1:2\""), "{doc}");
    }

    /// A selected message is outlined in the accent colour, and only that one.
    #[test]
    fn a_selected_card_is_outlined_in_the_accent() {
        let a = msg_for_print();
        let mut b = msg_for_print();
        b.id = 2;
        let doc = MessageView::conversation_document(
            &[a, b],
            &Default::default(),
            &Default::default(),
            &Default::default(),
            &[(1, 2)],
            "#ff8800",
            true,
            false,
            false,
            false,
            &crate::config::ReaderStyle::NONE,
            &Default::default(),
        );
        assert!(doc.contains("class=\"vireo-msg selected\" data-key=\"1:2\""), "{doc}");
        assert_eq!(doc.matches("vireo-msg selected").count(), 1, "only the selected one");
        assert!(doc.contains("box-shadow:0 0 0 2px #ff8800"), "outlined in the accent: {doc}");
    }

    /// A lone message gets the same in-document header a conversation card
    /// does — its own action icons included, since the toolbar carries no
    /// per-message actions — but goes full-bleed: no card gutter, so it reads
    /// as a message, not a card.
    #[test]
    fn a_single_message_keeps_the_header_but_goes_full_bleed() {
        let doc = MessageView::conversation_document(
            &[msg_for_print()],
            &std::collections::HashMap::new(),
            &Default::default(),
            &Default::default(),
            &[],
            "#3584e4",
            true,
            false,
            false,
            false,
            &crate::config::ReaderStyle::NONE,
            &Default::default(),
        );
        assert!(doc.contains("<section class=\"vireo-msg\""), "message chrome: {doc}");
        assert!(doc.contains("class=\"vireo-msg-hdr\""), "in-document header: {doc}");
        assert!(!doc.contains("<body class=\"vireo-conv\">"), "no card gutter: {doc}");
        assert!(doc.contains("data-act=\"reply\""), "card actions present: {doc}");
        // Full-bleed also means the plain ground, not the cards' deeper page.
        assert!(doc.contains(&format!("background:{}", GROUND.0)), "plain ground: {doc}");
    }

    /// With the "single messages as cards" preference on (#57), a lone
    /// message renders exactly like a one-message conversation: carded, on
    /// the deeper page ground.
    #[test]
    fn a_single_message_cards_up_when_asked() {
        let doc = MessageView::conversation_document(
            &[msg_for_print()],
            &std::collections::HashMap::new(),
            &Default::default(),
            &Default::default(),
            &[],
            "#3584e4",
            true,
            false,
            false,
            true,
            &crate::config::ReaderStyle::NONE,
            &Default::default(),
        );
        assert!(doc.contains("<body class=\"vireo-conv\">"), "card gutter: {doc}");
        assert!(doc.contains(&format!("background:{}", PAGE.0)), "deeper page ground: {doc}");
    }

    #[test]
    fn a_message_from_another_folder_is_labelled_with_it() {
        let a = msg_for_print(); // the message on screen
        let mut b = msg_for_print();
        b.id = 2;
        b.folder_id = 3; // pulled in from Sent
        let labels = std::collections::HashMap::from([((1u32, 2u32), "Sent".to_string())]);
        let doc = MessageView::conversation_document(&[a, b], &labels, &Default::default(), &Default::default(), &[], "#3584e4", true, false, false, false, &crate::config::ReaderStyle::NONE, &Default::default());
        assert_eq!(
            doc.matches("vireo-folder").count(),
            // once in the stylesheet, once on the message that came from Sent —
            // and never on the one the reader is already showing the folder of.
            2,
            "only the message from another folder should carry a folder badge"
        );
        assert!(doc.contains(">Sent</span>"), "the badge should name the folder");
    }

    #[test]
    fn a_folder_label_cannot_smuggle_markup_into_the_header() {
        // Folder names come from the server (a mailbox can be called anything),
        // and this document is the trusted wrapper around the sandboxed frames.
        let a = msg_for_print();
        let mut b = msg_for_print();
        b.id = 2;
        let labels =
            std::collections::HashMap::from([((1u32, 2u32), "<img src=x onerror=alert(1)>".into())]);
        let doc = MessageView::conversation_document(&[a, b], &labels, &Default::default(), &Default::default(), &[], "#3584e4", true, false, false, false, &crate::config::ReaderStyle::NONE, &Default::default());
        assert!(!doc.contains("<img src=x"), "the label must be escaped, not rendered");
        assert!(doc.contains("&lt;img src=x"));
    }

    #[test]
    fn a_sender_cannot_put_markup_in_the_conversation_header() {
        // The wrapper document has JavaScript enabled (it sizes the message
        // iframes) and the iframes are same-origin, so anything that executes
        // here can read every message body in the thread. A `From:` display name
        // is attacker-controlled and RFC 2047-decoded, so it can carry any bytes
        // at all — it has to reach the page as text, never as markup.
        let mut a = msg_for_print();
        a.from_name = "<script>x=1</script>".into();
        a.from_addr = "<img src=y onerror=z>@example.com".into();
        let mut b = msg_for_print();
        b.id = 2;
        // Two messages: the per-message headers only render in conversation mode.
        let doc = MessageView::conversation_document(&[a, b], &Default::default(), &Default::default(), &Default::default(),
            &[], "#3584e4", true, false, false, false, &crate::config::ReaderStyle::NONE, &Default::default());

        assert!(!doc.contains("<script>x=1"), "{doc}");
        assert!(!doc.contains("<img src=y"), "{doc}");
        assert!(doc.contains("&lt;script&gt;x=1&lt;/script&gt;"), "{doc}");
        assert!(doc.contains("onerror=z&gt;"), "{doc}");
    }

    #[test]
    fn the_wrapper_only_runs_its_own_script() {
        let mut a = msg_for_print();
        a.from_name = "Ada".into();
        let mut b = msg_for_print();
        b.id = 2;
        let doc = MessageView::conversation_document(&[a, b], &Default::default(), &Default::default(), &Default::default(),
            &[], "#3584e4", true, false, false, false, &crate::config::ReaderStyle::NONE, &Default::default());

        // A nonce'd CSP, so an injected `<script>` or `onerror=` is refused by
        // the engine even if the escaping above ever regresses.
        // The wrapper's own policy — not one of the frames', which are embedded
        // in this same string as escaped `srcdoc` payloads.
        let policy = doc
            .split("<meta http-equiv=\"Content-Security-Policy\" content=\"")
            .nth(1)
            .and_then(|r| r.split('"').next())
            .expect("the wrapper declares a CSP");
        let nonce = policy
            .split("script-src 'nonce-")
            .nth(1)
            .and_then(|r| r.split('\'').next())
            .expect("wrapper CSP declares a nonce");
        assert!(nonce.len() >= 16, "nonce is {} chars", nonce.len());
        assert!(doc.contains(&format!("<script nonce=\"{nonce}\">")), "{doc}");
        // The sizing script is the *only* script, and it carries the nonce.
        assert_eq!(doc.matches("<script").count(), 1, "{doc}");
        // No `default-src` here: a wrapper policy is inherited by the srcdoc
        // frames, and one would re-block content the user has allowed. Each
        // frame brings its own `default-src 'none'`.
        assert!(!policy.contains("default-src"), "{policy}");
        // Two renders never share a nonce.
        let again =
            MessageView::conversation_document(
                &[msg_for_print(), msg_for_print()],
                &Default::default(),
                &Default::default(),
                &Default::default(),
                &[],
                "#3584e4",
                true,
                false,
                false,
                false,
                &crate::config::ReaderStyle::NONE,
                &Default::default(),
            );
        assert!(!again.contains(nonce), "nonce was reused across renders");
    }

    #[test]
    fn the_remote_detector_sees_past_the_obvious_spellings() {
        // Every row here loaded a tracking pixel with no banner shown, because
        // the old detector matched fixed substrings like `src="http`.
        for html in [
            // Protocol-relative: resolves against https://vireo.localhost.
            r#"<img src="//tracker.example/p.gif">"#,
            // HTML permits whitespace around `=`.
            r#"<img src = "http://tracker.example/p.gif">"#,
            // Neither `<svg><image href>` nor `poster` was in the attribute list.
            r#"<svg><image href="http://tracker.example/p.gif"/></svg>"#,
            r#"<video poster="http://tracker.example/p.gif">"#,
            // `@import` was only matched in its `url(http…)` spelling.
            r#"<style>@import url(//tracker.example/x.css)</style>"#,
            r#"<style>@import "https://tracker.example/x.css"</style>"#,
            // Case: the detector lowercased, the stripper did not.
            r#"<IMG SRC="HTTP://tracker.example/p.gif">"#,
            // Unquoted, and quoted with single quotes.
            r#"<img src=http://tracker.example/p.gif>"#,
            r#"<img src='//tracker.example/p.gif'>"#,
            r#"<div style="background:url('//tracker.example/p.gif')">"#,
            r#"<img srcset="//tracker.example/p.gif 1x, /local.gif 2x">"#,
            r#"<link rel="stylesheet" href="//tracker.example/x.css">"#,
        ] {
            assert!(has_remote_resources(html), "not detected: {html}");
            let stripped = strip_remote(html);
            assert!(
                !stripped.contains("tracker.example"),
                "not stripped: {html} -> {stripped}"
            );
        }
    }

    #[test]
    fn a_stylesheet_does_not_hide_what_follows_it() {
        // The walk has to resume after `</style>`, not treat the closing tag as
        // opening another stylesheet and take the rest of the document for CSS.
        let html = "<html><head><style>.a{background:url(https://cdn.example/b.png)}</style>\
                    </head><body><img src=\"https://cdn.example/i.png\"></body></html>";
        let out = strip_remote(html);
        assert!(!out.contains("cdn.example"), "{out}");
        assert_eq!(out.matches("blocked://").count(), 2, "{out}");
    }

    #[test]
    fn the_detector_leaves_self_contained_messages_alone() {
        // A false banner on ordinary mail teaches people to ignore the real one.
        for html in [
            r#"<img src="data:image/png;base64,iVBOR">"#,
            r#"<img src="cid:part1@example.com">"#,
            r#"<p>plain text, no resources at all</p>"#,
            // A link is a destination the user must click, not a fetch.
            r#"<a href="https://example.com/read-more">more</a>"#,
            r#"<div style="color:#333;font-weight:bold">styled</div>"#,
        ] {
            assert!(!has_remote_resources(html), "false positive: {html}");
            assert_eq!(strip_remote(html), html, "needlessly rewritten: {html}");
        }
    }

    #[test]
    fn blocking_follows_the_users_choice_not_the_detector() {
        // The point of the split: even for a body the detector says nothing
        // about, a frame built while remote content is disallowed carries the
        // restrictive policy. A detector miss costs a banner, not the blocking.
        let sneaky = r#"<img data-x="y" src="//tracker.example/p.gif">"#;
        let frame = message_frame(sneaky, true, false, (1, 1), None, &crate::config::ReaderStyle::NONE, "#3584e4");
        assert!(frame.contains("img-src data: cid:"), "{frame}");
        assert!(!frame.contains("img-src http:"), "{frame}");
        assert!(!frame.contains("tracker.example"), "{frame}");

        // And once the user allows it, the same body renders untouched.
        let allowed = message_frame(sneaky, false, false, (1, 1), None, &crate::config::ReaderStyle::NONE, "#3584e4");
        assert!(allowed.contains("img-src http: https:"), "{allowed}");
        assert!(allowed.contains("tracker.example"), "{allowed}");
    }

    #[test]
    fn only_web_and_mail_links_reach_the_desktop() {
        // An HTML body keeps its own `href` values, so a message can name any
        // scheme a third-party application has registered.
        for uri in ["http://example.com/", "https://example.com/", "MAILTO:a@b.c"] {
            assert!(is_launchable_uri(uri), "{uri}");
        }
        for uri in [
            "file:///etc/passwd",
            "smb://host/share",
            "nfs://host/export",
            "javascript:alert(1)",
            "data:text/html,<script>x</script>",
            "vscode://file/etc/passwd",
            "https:",
            "no-scheme-at-all",
        ] {
            assert!(!is_launchable_uri(uri), "{uri}");
        }
    }

    /// Renders the real wrapper document in a real WebView and reports what
    /// actually happened in the engine.
    ///
    /// Ignored by default: it needs a display and a WebKit process, which a CI
    /// runner has no business requiring. Run it by hand after touching the
    /// wrapper's CSP or its sizing script:
    ///
    /// ```text
    /// cargo test -- --ignored the_wrapper_in_a_real_engine
    /// ```
    #[test]
    #[ignore = "needs a display and a WebKit process"]
    fn the_wrapper_in_a_real_engine_sizes_frames_and_refuses_injected_script() {
        use gtk::prelude::*;
        use std::cell::RefCell;
        use std::rc::Rc;

        gtk::init().expect("a display");

        let mut a = msg_for_print();
        // If this ever executes, the assertions below see it.
        a.from_name = "<script>window.__pwned = 1</script>".into();
        a.body = "<p style=\"height:400px\">first</p>".into();
        let mut b = msg_for_print();
        b.id = 2;
        b.body = "<p style=\"height:300px\">second</p>".into();
        let html = MessageView::conversation_document(&[a, b], &Default::default(), &Default::default(), &Default::default(),
            &[], "#3584e4", true, false, false, false, &crate::config::ReaderStyle::NONE, &Default::default());

        let view = webkit6::WebView::new();
        let settings = webkit6::Settings::new();
        settings.set_enable_javascript(true);
        view.set_settings(&settings);
        // Off-screen is enough; WebKit still lays out and runs scripts.
        let win = gtk::Window::new();
        win.set_child(Some(&view));
        win.set_default_size(800, 600);
        win.present();
        view.load_html(&html, Some("https://vireo.localhost/message/1"));

        let answer: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let ctx = gtk::glib::MainContext::default();
        // Run the main loop until `done`, yielding when there is nothing to
        // dispatch. Spinning on a non-blocking iteration instead starves the
        // WebKit web process on a busy machine, which made this fail at random.
        let pump = |done: &dyn Fn() -> bool| {
            while !done() && std::time::Instant::now() < deadline {
                if !ctx.iteration(false) {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        };

        // Give the load and the two deferred `s(f)` passes time to run.
        let ready = Rc::new(std::cell::Cell::new(false));
        let r = ready.clone();
        view.connect_load_changed(move |_, ev| {
            if ev == webkit6::LoadEvent::Finished {
                r.set(true);
            }
        });
        pump(&|| ready.get());
        assert!(ready.get(), "the document never finished loading");
        // The sizing script measures on DOMContentLoaded and again on deferred
        // passes, so poll for the result rather than sampling once — sampling
        // made this fail under load, which is a flaky test, not a finding.
        // `ours` — not the frame heights — is what says the nonce'd script ran.
        // Heights come from `scrollHeight`, which is 0 until the window is
        // mapped and laid out, and whether a compositor has got round to that
        // has nothing to do with the CSP. The sizing script's top-level
        // functions become properties of `window` the moment it executes.
        let probe = "JSON.stringify({\
               pwned: typeof window.__pwned !== 'undefined',\
               ours: ['s','init','all'].every(f => typeof window[f] === 'function'),\
               heights: [...document.querySelectorAll('iframe.vireo-frame')]\
                          .map(f => f.style.height),\
               readable: (function(){ try { \
                 return document.querySelector('iframe.vireo-frame')\
                          .contentDocument.body.innerText.trim(); \
               } catch (e) { return 'blocked'; } })(),\
               from: document.querySelector('.vireo-from').textContent\
             })";
        let mut json = String::new();
        while std::time::Instant::now() < deadline {
            answer.replace(None);
            let out = answer.clone();
            view.evaluate_javascript(
                probe,
                None,
                None,
                gtk::gio::Cancellable::NONE,
                move |res| {
                    *out.borrow_mut() = Some(match res {
                        Ok(v) => v.to_str().to_string(),
                        Err(e) => format!("ERROR {e}"),
                    });
                },
            );
            pump(&|| answer.borrow().is_some());
            json = answer.borrow().clone().expect("the engine answered");
            if json.contains("\"ours\":true") {
                break;
            }
            let pause = std::time::Instant::now() + std::time::Duration::from_millis(250);
            pump(&|| std::time::Instant::now() >= pause);
        }

        // The injected script is inert: it survives as text in the header and
        // never runs.
        assert!(json.contains("\"pwned\":false"), "injected script ran: {json}");
        assert!(
            json.contains("<script>window.__pwned = 1</script>"),
            "the display name should read back as text: {json}"
        );
        // And our own nonce'd script did run.
        assert!(
            json.contains("\"ours\":true"),
            "the sizing script did not run, so the CSP nonce is not working: {json}"
        );
        // The frames are still same-origin, which is what the sizing relies on.
        assert!(json.contains("first"), "frame content unreadable: {json}");
    }

    #[test]
    fn the_printed_page_carries_the_header() {
        // On screen these facts live in a GTK pane, which WebKit cannot print —
        // so they have to be in the document itself (issue #16).
        let m = msg_for_print();
        let header = print_header_html(Some(&m));
        assert!(header.contains("Quarterly numbers"), "{header}");
        assert!(header.contains("Ada Lovelace &lt;ada@example.com&gt;"), "{header}");
        assert!(header.contains("me@example.com"), "{header}");
        assert!(header.contains("carol@example.com"), "{header}");
        assert!(header.contains("From:") && header.contains("To:") && header.contains("Date:"));
    }

    #[test]
    fn the_printed_header_is_text_not_markup() {
        let mut m = msg_for_print();
        m.subject = "<script>alert(1)</script>".into();
        m.cc = String::new();
        let header = print_header_html(Some(&m));
        assert!(!header.contains("<script>"), "{header}");
        assert!(header.contains("&lt;script&gt;"), "{header}");
        // An empty field is left out rather than printed as a blank line.
        assert!(!header.contains("Cc:"), "{header}");
        // No message at all: nothing to describe.
        assert_eq!(print_header_html(None), "");
    }

    #[test]
    fn printing_inlines_the_message_instead_of_framing_it() {
        // The bug: printed pages carried the reader's iframe scrollbars and were
        // cut off at the frame's on-screen height, because a print engine draws an
        // iframe as a box rather than paginating what is inside it.
        let doc = print_document(
            "<div class=\"vireo-print-hdr\">HEADER</div>",
            &[(
                String::new(),
                "<!DOCTYPE html><html><head><style>p{color:red}</style></head>\
                 <body class=\"x\"><p>Hello</p></body></html>"
                    .to_string(),
            )],
            false,
        );
        assert!(!doc.contains("<iframe"), "{doc}");
        assert!(doc.contains("HEADER"));
        assert!(doc.contains("<p>Hello</p>"));
        // The message keeps its own styling on paper.
        assert!(doc.contains("p{color:red}"));
        // Long URLs wrap rather than running off the sheet, and wide content is
        // scaled down instead of being clipped.
        assert!(doc.contains("overflow-wrap:anywhere"));
        assert!(doc.contains("img,table,pre{max-width:100% !important;}"));
        // Still no scripts, and blocked remote content stays blocked.
        assert!(doc.contains("Content-Security-Policy"));
    }

    #[test]
    fn inlining_keeps_everything_but_the_structure() {
        assert_eq!(
            inline_body("<!doctype html><html><head><meta charset=\"utf-8\"></head><body id=\"a\">hi</body></html>"),
            "<meta charset=\"utf-8\">hi"
        );
        // A tag that merely starts like one of them is content, not structure.
        assert_eq!(inline_body("<bodyguard>x</bodyguard>"), "<bodyguard>x</bodyguard>");
        assert_eq!(inline_body("<p>plain fragment</p>"), "<p>plain fragment</p>");
        // Unterminated tags must not swallow the rest of the message.
        assert_eq!(inline_body("text < 5 and > 2"), "text < 5 and > 2");
    }

    #[test]
    fn one_message_cannot_restyle_the_whole_printout() {
        // A conversation prints as one document, so a sender's body rules have to
        // land on that sender's block and nothing else.
        let out = scope_styles(
            "<style>body{font-family:monospace}\
             @media print{html,body>p{color:red}}\
             .bodyguard{margin:0}</style><p>hi</p>",
            ".vireo-print-m1",
        );
        assert!(out.contains(".vireo-print-m1{font-family:monospace}"), "{out}");
        assert!(
            out.contains("@media print{.vireo-print-m1,.vireo-print-m1>p{color:red}}"),
            "{out}"
        );
        // Only whole tag names: a class that merely contains "body" is untouched,
        // and so are declarations that mention one.
        assert!(out.contains(".bodyguard{margin:0}"), "{out}");
        assert!(out.contains("<p>hi</p>"));
        assert!(!scope_styles("<p>body{x}</p>", ".vireo-print-m0").contains("vireo-print-m0"));
        // Each message gets a class of its own, so the first sender's rules stop
        // at the first message.
        let doc = print_document(
            "",
            &[
                (String::new(), "<style>body{color:red}</style>one".into()),
                (String::new(), "two".into()),
            ],
            false,
        );
        assert!(doc.contains(".vireo-print-m0{color:red}"), "{doc}");
        assert!(doc.contains("vireo-print-msg vireo-print-m1"), "{doc}");
    }

    #[test]
    fn a_printed_conversation_says_who_sent_what() {
        let mut a = msg_for_print();
        a.from_name = "Alfonso".into();
        a.from_addr = "a@example.com".into();
        let head = print_message_header_html(&a);
        assert!(head.contains("Alfonso &lt;a@example.com&gt;"), "{head}");
        assert!(head.contains(&attr_escape(&a.datetime_full())));
    }

    #[test]
    fn a_preview_uri_is_escaped() {
        // Subjects become filenames, and mail subjects are full of spaces and
        // brackets: "file://" + path is not a URI, and GIO opens nothing.
        let path = std::path::Path::new("/tmp/vireo-print/[hyprlab] Sync 1.11.0.pdf");
        let uri = gtk::gio::File::for_path(path).uri().to_string();
        assert!(!uri.contains(' '), "{uri}");
        assert!(uri.contains("%20"), "{uri}");
        assert!(uri.starts_with("file:///tmp/vireo-print/"), "{uri}");
    }

    #[test]
    fn print_filenames_survive_real_subjects() {
        // The subject names the print job and seeds the filename when printing to
        // a file, so it must not carry path separators or other characters a
        // filesystem would refuse.
        assert_eq!(sanitize_filename("Quarterly numbers"), "Quarterly numbers");
        assert_eq!(sanitize_filename("Invoice 3/4 <urgent>"), "Invoice 3-4 -urgent-");
        assert_eq!(sanitize_filename("../../etc/passwd"), "etc-passwd");
        // Nothing usable left, or nothing to begin with.
        assert_eq!(sanitize_filename("   "), "Message");
        assert_eq!(sanitize_filename(""), "Message");
        assert_eq!(sanitize_filename("..."), "Message");
    }

    #[test]
    fn print_filenames_are_bounded() {
        let long = "word ".repeat(100);
        assert!(sanitize_filename(&long).chars().count() <= 120);
    }

    #[test]
    fn decodes_a_base64_data_uri() {
        let (mime, data) = decode_data_uri("data:image/jpeg;base64,/9j/4AAQ").expect("decodes");
        assert_eq!(mime, "image/jpeg");
        assert_eq!(&data[..3], b"\xff\xd8\xff"); // JPEG magic
    }

    #[test]
    fn rejects_uris_it_cannot_save_locally() {
        // Remote images keep WebKit's own (download-backed) menu item.
        assert!(decode_data_uri("https://example.com/a.png").is_none());
        // Non-base64 `data:` URIs aren't something the reader emits.
        assert!(decode_data_uri("data:image/png,rawbytes").is_none());
        assert!(decode_data_uri("not a uri").is_none());
    }

    #[test]
    fn link_preview_shows_the_plain_url_when_nothing_is_amiss() {
        assert_eq!(
            link_destination("https://example.com/a", Some("Read more")),
            "https://example.com/a"
        );
        // Link text that matches where it goes is not a mismatch.
        assert_eq!(
            link_destination("https://example.com/a", Some("example.com")),
            "https://example.com/a"
        );
        // Subdomains belong to the same site.
        assert_eq!(
            link_destination("https://mail.example.com/a", Some("example.com")),
            "https://mail.example.com/a"
        );
    }

    #[test]
    fn link_preview_calls_out_text_claiming_another_site() {
        let shown = link_destination("https://evil.example/login", Some("https://paypal.com"));
        assert!(shown.contains("paypal.com"), "{shown}");
        assert!(shown.contains("evil.example"), "{shown}");
        assert!(shown.contains('⚠'), "{shown}");
    }

    #[test]
    fn ordinary_link_text_is_never_mistaken_for_a_host() {
        assert_eq!(host_of("Click here to sign in"), None);
        assert_eq!(host_of("mailto:someone@example.com"), None);
        assert_eq!(host_of("https://example.com/path"), Some("example.com".into()));
        assert_eq!(host_of("example.com/path"), Some("example.com".into()));
        // Userinfo can't be used to disguise the real host.
        assert_eq!(
            host_of("https://paypal.com@evil.example/"),
            Some("evil.example".into())
        );
    }

    #[test]
    fn suggests_a_filename_from_the_mime_type() {
        assert_eq!(default_image_name("image/jpeg"), "image.jpg");
        assert_eq!(default_image_name("image/png"), "image.png");
        assert_eq!(default_image_name("image/svg+xml"), "image.svg");
    }
}

#[cfg(test)]
mod scan_perf {
    use super::*;
    #[test]
    #[ignore = "timing-sensitive"]
    fn scanning_a_large_message_is_quick() {
        // A big marketing email: lots of tags, a large stylesheet, many images.
        let mut html = String::from("<html><head><style>");
        for i in 0..2000 {
            html.push_str(&format!(
                ".c{i}{{background:url(https://cdn.example/bg{i}.png);color:#333}}"
            ));
        }
        html.push_str("</style></head><body>");
        for i in 0..5000 {
            html.push_str(&format!(
                "<div class=\"c{i}\" style=\"padding:2px\"><img src=\"https://cdn.example/i{i}.png\"><a href=\"https://example.com/{i}\">x</a></div>"
            ));
        }
        html.push_str("</body></html>");
        eprintln!("body is {} KB", html.len() / 1024);

        let t = std::time::Instant::now();
        assert!(has_remote_resources(&html));
        let detect = t.elapsed();
        let t = std::time::Instant::now();
        let stripped = strip_remote(&html);
        let strip = t.elapsed();
        eprintln!("detect {detect:?}, strip {strip:?}");
        assert!(!stripped.contains("cdn.example"));
        // Links are left alone.
        assert!(stripped.contains("https://example.com/4999"));
        assert!(detect.as_millis() < 250, "detection took {detect:?}");
        assert!(strip.as_millis() < 250, "stripping took {strip:?}");
    }
}

