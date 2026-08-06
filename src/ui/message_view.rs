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

pub struct MessageView {
    /// The newest message in the thread — drives the header, avatar and actions.
    current: Option<Message>,
    /// The whole conversation, newest first. One entry = a single message (shown
    /// exactly as before); more = a scrollable conversation in the body view.
    thread: Vec<Message>,
    /// Remote content is present and currently withheld.
    blocked: bool,
    /// Whether Gravatar loading is enabled.
    gravatar: bool,
    /// Decoded Gravatar for the current sender, if any.
    avatar_texture: Option<gtk::gdk::Texture>,
    /// Owning account's display name (header chip).
    account_name: Option<String>,
    /// Provider holding the header chip's per-account colours.
    chip_provider: gtk::CssProvider,
    /// True while the body is being fetched (show a spinner instead).
    loading: bool,
    /// False from when a render starts until the WebView reports it finished
    /// loading — a themed cover hides the WebView's white inter-document gap.
    webview_ready: bool,
    webview: webkit6::WebView,
    /// Bumped per render: each load gets a unique base URI so WebKit treats it
    /// as a fresh document and re-fetches resources (reusing `about:blank` does
    /// not). An https base also lets https images load without mixed-content.
    seq: std::cell::Cell<u64>,
    /// Forced dark flag for message content, or `None` to follow the system UI.
    /// This themes email content only, not the app chrome.
    content_dark: Option<bool>,
}

#[derive(Debug)]
pub enum MessageViewInput {
    Show {
        /// The conversation, newest first. A single message for a normal open;
        /// several for a threaded conversation.
        thread: Vec<Message>,
        /// The sender is trusted, so remote content may auto-load.
        allow_remote: bool,
        /// Whether Gravatar loading is enabled.
        gravatar: bool,
        /// Owning account's display name and colour, for the header chip.
        account_name: Option<String>,
        account_color: Option<String>,
        /// The body is still being fetched — show a spinner.
        loading: bool,
    },
    LoadRemoteOnce,
    AllowSenderAlways,
    /// The sender email link in the header was clicked.
    ComposeSender,
    /// The system/app light-dark preference changed; re-render to match.
    ThemeChanged,
    /// Set the message-content theme: `None` follows the system, `Some(dark)`
    /// forces light/dark for email content only (not the app UI).
    SetContentTheme(Option<bool>),
    /// EDS/CardDAV changed; re-resolve the current sender photo.
    ContactPhotosChanged,
    /// The WebView finished loading the current document — reveal it.
    Rendered,
    /// A conversation message header was double-clicked — open that message in
    /// its own window.
    OpenHeader { account_id: u32, id: u32 },
}

#[derive(Debug)]
pub enum MessageViewOutput {
    /// Add this sender address to the remote-content allowlist.
    AllowSender(String),
    /// Compose a new message to this address.
    ComposeTo(String),
    /// Open a conversation message in its own window (header double-clicked).
    OpenWindow(Box<Message>),
}

#[relm4::component(pub)]
impl Component for MessageView {
    type Init = ();
    type Input = MessageViewInput;
    type Output = MessageViewOutput;
    /// Requested sender + setting + result, used to reject stale async lookups.
    type CommandOutput = (
        String,
        u64,
        crate::avatar::FetchMode,
        crate::avatar::FetchOutcome,
    );

    view! {
        gtk::Stack {
            set_transition_type: gtk::StackTransitionType::Crossfade,

            add_named[Some("empty")] = &adw::StatusPage {
                set_icon_name: Some("co.hyprlab.Vireo-mail-read-symbolic"),
                set_title: "No message selected",
                set_description: Some("Choose a message from the list to read it here."),
            },

            add_named[Some("message")] = &gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                add_css_class: "reader-pane",

                gtk::Revealer {
                    set_transition_type: gtk::RevealerTransitionType::SlideDown,
                    #[watch]
                    set_reveal_child: model.blocked,

                    gtk::Box {
                        add_css_class: "remote-alert",
                        set_spacing: 8,

                        gtk::Image { set_icon_name: Some("co.hyprlab.Vireo-security-high-symbolic") },
                        gtk::Label {
                            set_label: "Remote content (images, trackers) was blocked to protect your privacy.",
                            set_hexpand: true,
                            set_halign: gtk::Align::Start,
                            set_wrap: true,
                            set_xalign: 0.0,
                        },
                        gtk::Button {
                            set_label: "Load",
                            set_valign: gtk::Align::Center,
                            connect_clicked => MessageViewInput::LoadRemoteOnce,
                        },
                        gtk::Button {
                            set_label: "Always allow sender",
                            set_valign: gtk::Align::Center,
                            #[watch]
                            set_tooltip_text: model.current.as_ref().map(|m| m.from_addr.as_str()),
                            connect_clicked => MessageViewInput::AllowSenderAlways,
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

                    gtk::Box {
                        set_spacing: 12,
                        // For a conversation each message carries its own header in
                        // the scrollable body, so hide this single-message header.
                        #[watch]
                        set_visible: model.thread.len() <= 1,

                        adw::Avatar {
                            set_size: 44,
                            set_valign: gtk::Align::Center,
                            set_show_initials: true,
                            #[watch]
                            set_text: model.current.as_ref().map(|m| m.from_name.as_str()),
                            #[watch]
                            set_custom_image: model.avatar_texture.as_ref(),
                        },

                        gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            set_valign: gtk::Align::Center,
                            set_hexpand: true,

                            gtk::Label {
                                #[watch]
                                set_label: model.current.as_ref().map(|m| m.from_name.as_str()).unwrap_or_default(),
                                set_halign: gtk::Align::Start,
                                set_selectable: true,
                                add_css_class: "reader-from-name",
                            },
                            gtk::Label {
                                #[watch]
                                set_markup: &email_link_markup(model.current.as_ref()),
                                set_halign: gtk::Align::Start,
                                set_selectable: true,
                                set_tooltip_text: Some("Send a new message to this address"),
                                add_css_class: "reader-from-addr",
                                connect_activate_link[sender] => move |_, _uri| {
                                    sender.input(MessageViewInput::ComposeSender);
                                    gtk::glib::Propagation::Stop
                                },
                            },
                        },

                        gtk::Label {
                            #[watch]
                            set_label: &model.current.as_ref().map(|m| m.datetime_full()).unwrap_or_default(),
                            set_valign: gtk::Align::Start,
                            set_selectable: true,
                            add_css_class: "reader-date",
                        },
                    },

                    gtk::Label {
                        #[watch]
                        set_label: &cc_line(model.current.as_ref()),
                        #[watch]
                        set_visible: model.thread.len() <= 1
                            && model.current.as_ref().is_some_and(|m| !m.cc.trim().is_empty()),
                        set_halign: gtk::Align::Start,
                        set_wrap: true,
                        set_wrap_mode: gtk::pango::WrapMode::WordChar,
                        set_xalign: 0.0,
                        set_selectable: true,
                        add_css_class: "reader-cc",
                    },
                },

                gtk::Separator {},

                #[name = "body_stack"]
                gtk::Stack {
                    set_vexpand: true,
                    #[watch]
                    set_visible_child_name: model.body_page(),

                    // Themed cover shown while the WebView loads, so its white
                    // inter-document gap is never visible.
                    add_named[Some("blank")] = &gtk::Box {
                        set_vexpand: true,
                        set_hexpand: true,
                    },

                    add_named[Some("loading")] = &gtk::Box {
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
                            set_label: "Loading…",
                            add_css_class: "dim-label",
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
        let chip_provider = gtk::CssProvider::new();
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &chip_provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
        let model = MessageView {
            current: None,
            thread: Vec::new(),
            blocked: false,
            gravatar: false,
            avatar_texture: None,
            account_name: None,
            chip_provider,
            loading: false,
            webview_ready: false,
            webview,
            seq: std::cell::Cell::new(0),
            content_dark: None,
        };

        // Reveal the WebView only once it's finished loading the document.
        let ready_sender = sender.clone();
        model.webview.connect_load_changed(move |_view, event| {
            if event == webkit6::LoadEvent::Finished {
                ready_sender.input(MessageViewInput::Rendered);
            }
        });

        // Double-click on a conversation header → open that message's window.
        if let Some(ucm) = model.webview.user_content_manager() {
            let open_sender = sender.clone();
            ucm.connect_script_message_received(Some("vireo"), move |_ucm, value| {
                let key = value.to_str().to_string();
                if let Some((a, i)) = key.split_once(':') {
                    if let (Ok(account_id), Ok(id)) = (a.parse::<u32>(), i.parse::<u32>()) {
                        open_sender.input(MessageViewInput::OpenHeader { account_id, id });
                    }
                }
            });
        }

        // Re-render the body when the light/dark preference changes so unstyled
        // content tracks the theme live.
        let style_manager = adw::StyleManager::default();
        model.apply_webview_bg(style_manager.is_dark());
        let theme_sender = sender.clone();
        style_manager.connect_dark_notify(move |_| {
            theme_sender.input(MessageViewInput::ThemeChanged);
        });

        let widgets = view_output!();
        widgets
            .body_stack
            .add_named(&model.webview, Some("body"));
        widgets.body_stack.set_visible_child_name("body");
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            MessageViewInput::Show {
                thread,
                allow_remote,
                gravatar,
                account_name,
                account_color,
                loading,
            } => {
                self.current = thread.first().cloned();
                self.thread = thread;
                self.gravatar = gravatar;
                self.account_name = account_name;
                self.loading = loading;
                if let Some(color) = &account_color {
                    let css = format!(
                        ".vireo-account-chip {{ background-color: {}; color: {}; }}",
                        crate::color::pale(color, 0.18),
                        color
                    );
                    self.chip_provider.load_from_data(&css);
                }
                let has_remote = self
                    .thread
                    .iter()
                    .any(|m| has_remote_resources(&m.body));
                self.blocked = has_remote && !allow_remote;
                self.load_avatar(&sender);
                // While loading, the spinner page is shown; rendering the (empty)
                // body would just flash blank, so wait for the real body.
                if !self.loading {
                    self.render();
                }
            }
            MessageViewInput::LoadRemoteOnce => {
                self.blocked = false;
                self.render();
            }
            MessageViewInput::AllowSenderAlways => {
                if let Some(m) = &self.current {
                    let _ = sender.output(MessageViewOutput::AllowSender(m.from_addr.clone()));
                }
                self.blocked = false;
                self.render();
            }
            MessageViewInput::ComposeSender => {
                if let Some(m) = &self.current {
                    if !m.from_addr.is_empty() {
                        let _ = sender.output(MessageViewOutput::ComposeTo(m.from_addr.clone()));
                    }
                }
            }
            MessageViewInput::ThemeChanged => {
                let dark = self.effective_dark();
                self.apply_webview_bg(dark);
                if self.current.is_some() && !self.loading {
                    self.render();
                }
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
            MessageViewInput::ContactPhotosChanged => self.load_avatar(&sender),
            MessageViewInput::Rendered => {
                self.webview_ready = true;
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

    fn update_cmd(
        &mut self,
        (email, generation, mode, outcome): Self::CommandOutput,
        sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        let retry_stale = crate::avatar::cache_result(&email, generation, mode, outcome);
        // IMAP UIDs are only mailbox-scoped, so correlate by the sender that
        // actually initiated the lookup rather than the numeric message id.
        let still_current = self.current.as_ref().is_some_and(|message| {
            message.from_addr.eq_ignore_ascii_case(&email)
        });
        if still_current {
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
}

impl MessageView {
    /// Set the sender avatar from local contacts, with optional Gravatar
    /// fallback, correlating background results by sender address.
    fn load_avatar(&mut self, sender: &ComponentSender<Self>) {
        self.avatar_texture = None;
        let Some(m) = self.current.as_ref() else {
            return;
        };
        let email = m.from_addr.clone();
        if email.is_empty() {
            return;
        }
        match crate::avatar::lookup(&email, self.gravatar) {
            crate::avatar::CacheLookup::Texture(texture) => {
                self.avatar_texture = Some(texture);
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

    /// Whether message content should render dark: the user's forced choice, or
    /// the system UI theme when following it.
    fn effective_dark(&self) -> bool {
        self.content_dark
            .unwrap_or_else(|| adw::StyleManager::default().is_dark())
    }

    fn render(&mut self) {
        let dark = self.effective_dark();
        self.apply_webview_bg(dark);
        // Hide the WebView behind the themed cover until this load finishes.
        self.webview_ready = false;
        let html = self.document_html(dark);
        let n = self.seq.get().wrapping_add(1);
        self.seq.set(n);
        self.webview
            .load_html(&html, Some(&format!("https://vireo.localhost/message/{n}")));
    }

    /// Which body-stack page to show: spinner while fetching, themed cover while
    /// the WebView loads, then the rendered message(s).
    fn body_page(&self) -> &'static str {
        if self.loading {
            "loading"
        } else if !self.webview_ready {
            "blank"
        } else {
            "body"
        }
    }

    /// The wrapper document: one sandboxed iframe per message (so each email's CSS
    /// is fully isolated and its scripts can't run), with per-message headers in
    /// conversation mode. A small script sizes each iframe to its content.
    fn document_html(&self, dark: bool) -> String {
        let conversation = self.thread.len() > 1;
        let mut sections = String::new();
        for m in &self.thread {
            let body = if m.body.trim().is_empty() {
                "<div class=\"vireo-loading\">Loading…</div>".to_string()
            } else {
                message_frame(&m.body, self.blocked, dark)
            };
            if conversation {
                sections.push_str(&format!(
                    "<section class=\"vireo-msg\">\
                       <header class=\"vireo-msg-hdr\" data-key=\"{aid}:{id}\" \
                         title=\"Double-click to open in a new window\">\
                         <span class=\"vireo-from\">{from}</span>{addr}\
                         <span class=\"vireo-date\">{date}</span>\
                       </header>{body}</section>",
                    aid = m.account_id,
                    id = m.id,
                    from = attr_escape(&m.from_name),
                    addr = if m.from_addr.is_empty() {
                        String::new()
                    } else {
                        format!("<span class=\"vireo-addr\">&lt;{}&gt;</span>", attr_escape(&m.from_addr))
                    },
                    date = attr_escape(&m.datetime_full()),
                    body = body,
                ));
            } else {
                sections.push_str(&body);
            }
        }
        let scheme = if dark { "dark" } else { "light" };
        // Paint the wrapper and the (still-loading) iframes in the theme colour so
        // there's no white flash before each message's content renders.
        let bg = if dark { "#1e1e1e" } else { "#ffffff" };
        format!(
            "<!doctype html><html><head><meta charset=\"utf-8\">\
             <meta name=\"color-scheme\" content=\"{scheme}\">\
             <style>\
               :root{{color-scheme:{scheme};}}\
               body{{margin:0;padding:0;background:{bg};font:14px/1.55 system-ui,sans-serif;}}\
               iframe.vireo-frame{{width:100%;border:0;display:block;background:{bg};}}\
               .vireo-msg{{border-bottom:1px solid rgba(128,128,128,0.25);}}\
               .vireo-msg-hdr{{display:flex;gap:8px;align-items:baseline;flex-wrap:wrap;padding:12px 16px;cursor:default;user-select:none;transition:background 120ms ease;}}\
               .vireo-msg-hdr:hover{{background:rgba(128,128,128,0.16);}}\
               .vireo-from{{font-weight:700;}}\
               .vireo-addr{{opacity:0.55;font-size:0.9em;}}\
               .vireo-date{{margin-left:auto;opacity:0.55;font-size:0.85em;}}\
               .vireo-loading{{opacity:0.5;padding:16px;}}\
             </style>\
             <script>{SIZE_SCRIPT}</script>\
             </head><body>{sections}</body></html>"
        )
    }

    /// Paint the WebView canvas in the theme colour so unstyled bodies (and the
    /// gap before a load) match light/dark mode instead of flashing white.
    fn apply_webview_bg(&self, dark: bool) {
        let rgba = if dark {
            gtk::gdk::RGBA::new(0.118, 0.118, 0.118, 1.0)
        } else {
            gtk::gdk::RGBA::new(1.0, 1.0, 1.0, 1.0)
        };
        self.webview.set_background_color(&rgba);
    }
}

/// Create a sandboxed WebView: no JavaScript or dev tools, smooth scrolling, and
/// links routed to the external browser.
fn new_webview() -> webkit6::WebView {
    // A user-content manager with a script message handler lets the wrapper
    // document notify us (e.g. a double-clicked conversation header).
    let ucm = webkit6::UserContentManager::new();
    ucm.register_script_message_handler("vireo", None);
    let webview = webkit6::WebView::builder()
        .user_content_manager(&ucm)
        .build();

    let settings = webkit6::Settings::new();
    // JavaScript runs only in our own (trusted) wrapper document — it sizes each
    // message's iframe to its content. Every email body is embedded in a
    // `sandbox`ed iframe WITHOUT `allow-scripts`, so message scripts never run.
    settings.set_enable_javascript(true);
    settings.set_enable_developer_extras(false);
    webview.set_settings(&settings);

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
                            let _ = gtk::gio::AppInfo::launch_default_for_uri(
                                &uri,
                                None::<&gtk::gio::AppLaunchContext>,
                            );
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

/// Markup for the sender address as a clickable mailto link.
fn email_link_markup(m: Option<&Message>) -> String {
    match m {
        Some(m) if !m.from_addr.is_empty() => {
            let esc = gtk::glib::markup_escape_text(&m.from_addr);
            format!("<a href=\"mailto:{esc}\">{esc}</a>")
        }
        _ => String::new(),
    }
}

/// "Cc: a@b, c@d" for the header, or empty when there are no Cc recipients.
fn cc_line(m: Option<&Message>) -> String {
    match m {
        Some(m) if !m.cc.trim().is_empty() => format!("Cc: {}", m.cc.trim()),
        _ => String::new(),
    }
}

/// Neutralize remote resource references so nothing is fetched while blocked.
/// Targets resource-loading attributes only; `<a href>` links are left intact.
fn strip_remote(html: &str) -> String {
    html.replace("src=\"http", "src=\"blocked://")
        .replace("src='http", "src='blocked://")
        .replace("src=http", "src=blocked://")
        .replace("srcset=", "data-blocked-srcset=")
        .replace("background=\"http", "background=\"blocked://")
        .replace("url(http", "url(blocked://")
        .replace("url('http", "url('blocked://")
        .replace("url(\"http", "url(\"blocked://")
}

/// Inject a Content-Security-Policy `<meta>` into the document head as a second
/// line of defense. When remote content is disallowed only inline styles and
/// `data:` URIs are permitted.
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
    // An "unstyled" message brings no CSS of its own; give it comfortable padding
    // so text isn't flush against the edges. Styled emails keep their own layout.
    let unstyled = !lower.contains("<style") && !lower.contains("style=");
    let body_pad = if unstyled {
        // Reset the UA's default 8px body margin so content sits at exactly 16px
        // (which lines up with the conversation headers).
        "body{margin:0;padding:16px;box-sizing:border-box;}"
    } else {
        ""
    };
    // `color-scheme` makes the browser's default colours (for content that sets
    // none of its own) follow the app's light/dark setting; styled emails keep
    // their own colours untouched.
    let scheme = if dark { "dark" } else { "light" };
    let supported = if dark { "dark light" } else { "light dark" };
    let theme = format!(
        "<meta name=\"color-scheme\" content=\"{supported}\">\
         <style>:root{{color-scheme:{scheme};}}{body_pad}</style>"
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

/// Heuristic: does the HTML reference remote (http/https) resources?
fn has_remote_resources(html: &str) -> bool {
    let h = html.to_ascii_lowercase();
    h.contains("src=\"http")
        || h.contains("src='http")
        || h.contains("src=http")
        || h.contains("srcset=")
        || h.contains("url(http")
        || h.contains("url('http")
        || h.contains("url(\"http")
        || h.contains("background=\"http")
        || (h.contains("<link") && h.contains("stylesheet") && h.contains("http"))
}

/// Wrapper-document script: size each message iframe to its content height so the
/// whole conversation scrolls as one page (the iframes have no inner scrollbars).
/// Re-measures as images load and as content reflows.
const SIZE_SCRIPT: &str = "\
function s(f){try{var d=f.contentDocument;if(!d)return;var b=d.body,e=d.documentElement;\
var h=Math.max(b?b.scrollHeight:0,e?e.scrollHeight:0,b?b.offsetHeight:0);if(h>0)f.style.height=h+'px';}catch(_){}}\
function init(f){s(f);try{var d=f.contentDocument;if(d){if(window.ResizeObserver&&d.body){new ResizeObserver(function(){s(f);}).observe(d.body);}\
var im=d.images||[];for(var i=0;i<im.length;i++){if(!im[i].complete){im[i].addEventListener('load',function(){s(f);});im[i].addEventListener('error',function(){s(f);});}}}}catch(_){}\
setTimeout(function(){s(f);},250);setTimeout(function(){s(f);},1000);}\
function all(){return document.querySelectorAll('iframe.vireo-frame');}\
document.addEventListener('DOMContentLoaded',function(){\
var fs=all();\
for(var i=0;i<fs.length;i++){(function(f){\
if(f.contentDocument&&f.contentDocument.readyState==='complete'){init(f);}\
f.addEventListener('load',function(){init(f);});})(fs[i]);}\
var hs=document.querySelectorAll('.vireo-msg-hdr');\
for(var j=0;j<hs.length;j++){hs[j].addEventListener('dblclick',function(){\
try{window.webkit.messageHandlers.vireo.postMessage(this.dataset.key);}catch(_){}});}});\
window.addEventListener('resize',function(){var fs=all();for(var i=0;i<fs.length;i++)s(fs[i]);});";

/// One message body as a sandboxed iframe: its own document (so CSS can't leak to
/// other messages) with no `allow-scripts` (so the email can't run JavaScript).
/// `allow-same-origin` lets the wrapper script measure its height.
fn message_frame(body: &str, blocked: bool, dark: bool) -> String {
    let doc = body_html(body);
    let doc = if blocked { strip_remote(&doc) } else { doc };
    let doc = inject_csp(&doc, !blocked, dark);
    format!(
        // `allow-same-origin` lets our wrapper script measure the frame height;
        // `allow-popups` lets `_blank` links reach the policy handler (which opens
        // them externally). No `allow-scripts`, so the email's own JS never runs.
        "<iframe class=\"vireo-frame\" sandbox=\"allow-same-origin allow-popups\" srcdoc=\"{}\"></iframe>",
        attr_escape(&doc)
    )
}

/// Escape a string for use inside a double-quoted HTML attribute (e.g. `srcdoc`).
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
             body{{margin:0;padding:16px;font:14px/1.5 system-ui,sans-serif;\
             white-space:pre-wrap;word-wrap:break-word}}\
             </style></head><body>{}</body></html>",
            body.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
        )
    }
}
