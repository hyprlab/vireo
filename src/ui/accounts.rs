//! Accounts window: manage all mail accounts (add / edit / remove / reorder).
//!
//! A standalone window (opened from the main menu, separate from Preferences).
//! It uses an `AdwNavigationView` with two pages: a list of accounts (drag rows
//! to set the sidebar order) and a reusable editor form pushed on top.

use adw::prelude::*;
use relm4::prelude::*;

use crate::config::{AccountConfig, OAuthSettings, Protocol};
use crate::ui::rich_editor::{self, RichEditor};
use crate::worker::{self, ConnTest};

const DEFAULT_COLOR: &str = "#3584e4";

/// How an account signs in, chosen via the single Provider dropdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderKind {
    /// Manual IMAP/POP3 + password ("Other (IMAP/POP3)…").
    Manual,
    /// A known IMAP provider: password auth with auto-filled servers.
    Preset,
    /// Google OAuth (browser sign-in; falls back to GNOME Online Accounts).
    Google,
    /// Microsoft OAuth (browser sign-in).
    Microsoft,
    /// OAuth against a user-entered provider ("Custom (OAuth)…").
    CustomOAuth,
}

/// One entry in the Provider dropdown. It selects both the sign-in method and,
/// for `Preset`, the IMAP/SMTP servers to auto-fill. `hint` is shown as the row's
/// subtitle. Server fields are empty for non-`Preset` kinds (OAuth providers get
/// their servers from `crate::oauth::preset`; Manual/Custom are user-entered).
struct Provider {
    label: &'static str,
    kind: ProviderKind,
    imap_host: &'static str,
    imap_port: u16,
    smtp_host: &'static str,
    smtp_port: u16,
    hint: &'static str,
}

impl Provider {
    fn is_password(&self) -> bool {
        matches!(self.kind, ProviderKind::Manual | ProviderKind::Preset)
    }
    fn is_oauth(&self) -> bool {
        !self.is_password()
    }
    /// OAuth preset key for the built-in providers.
    fn oauth_name(&self) -> Option<&'static str> {
        match self.kind {
            ProviderKind::Google => Some("google"),
            ProviderKind::Microsoft => Some("microsoft"),
            _ => None,
        }
    }
}

const APP_PW: &str = "Requires an app-specific password (not your normal login password).";

/// The Provider dropdown, in display order. OAuth options first, then the major
/// app-password IMAP providers, then the two manual escape hatches. IMAP uses
/// SSL/TLS on 993; SMTP uses implicit TLS on 465 or STARTTLS on 587.
const PROVIDERS: &[Provider] = &[
    Provider { label: "Google (Gmail) — sign in", kind: ProviderKind::Google, imap_host: "", imap_port: 0, smtp_host: "", smtp_port: 0, hint: "Sign in with your browser — no password needed." },
    Provider { label: "Microsoft / Outlook — sign in", kind: ProviderKind::Microsoft, imap_host: "", imap_port: 0, smtp_host: "", smtp_port: 0, hint: "Sign in with your browser (experimental)." },
    Provider { label: "iCloud", kind: ProviderKind::Preset, imap_host: "imap.mail.me.com", imap_port: 993, smtp_host: "smtp.mail.me.com", smtp_port: 587, hint: APP_PW },
    Provider { label: "Yahoo Mail", kind: ProviderKind::Preset, imap_host: "imap.mail.yahoo.com", imap_port: 993, smtp_host: "smtp.mail.yahoo.com", smtp_port: 465, hint: APP_PW },
    Provider { label: "Proton Mail (Bridge)", kind: ProviderKind::Preset, imap_host: "127.0.0.1", imap_port: 1143, smtp_host: "127.0.0.1", smtp_port: 1025, hint: "Requires Proton Mail Bridge running locally." },
    Provider { label: "Fastmail", kind: ProviderKind::Preset, imap_host: "imap.fastmail.com", imap_port: 993, smtp_host: "smtp.fastmail.com", smtp_port: 465, hint: APP_PW },
    Provider { label: "AOL Mail", kind: ProviderKind::Preset, imap_host: "imap.aol.com", imap_port: 993, smtp_host: "smtp.aol.com", smtp_port: 465, hint: APP_PW },
    Provider { label: "Zoho Mail", kind: ProviderKind::Preset, imap_host: "imap.zoho.com", imap_port: 993, smtp_host: "smtp.zoho.com", smtp_port: 465, hint: "" },
    Provider { label: "GMX", kind: ProviderKind::Preset, imap_host: "imap.gmx.com", imap_port: 993, smtp_host: "mail.gmx.com", smtp_port: 587, hint: "Enable POP/IMAP access in GMX settings first." },
    Provider { label: "Yandex Mail", kind: ProviderKind::Preset, imap_host: "imap.yandex.com", imap_port: 993, smtp_host: "smtp.yandex.com", smtp_port: 465, hint: APP_PW },
    Provider { label: "Mail.com", kind: ProviderKind::Preset, imap_host: "imap.mail.com", imap_port: 993, smtp_host: "smtp.mail.com", smtp_port: 587, hint: "" },
    Provider { label: "Custom (OAuth)…", kind: ProviderKind::CustomOAuth, imap_host: "", imap_port: 0, smtp_host: "", smtp_port: 0, hint: "Enter your provider's OAuth endpoints, then sign in." },
    Provider { label: "Other (IMAP/POP3)…", kind: ProviderKind::Manual, imap_host: "", imap_port: 0, smtp_host: "", smtp_port: 0, hint: "Enter your server details manually." },
];

/// Dropdown index of the "Other (IMAP/POP3)…" manual entry (the default).
fn manual_index() -> u32 {
    PROVIDERS
        .iter()
        .position(|p| p.kind == ProviderKind::Manual)
        .unwrap_or(0) as u32
}

/// The provider entry for a dropdown index (clamped to the manual default).
fn provider_at(idx: u32) -> &'static Provider {
    PROVIDERS
        .get(idx as usize)
        .unwrap_or(&PROVIDERS[manual_index() as usize])
}

pub struct AccountsWindow {
    /// Accounts in display order.
    accounts: Vec<AccountConfig>,
    /// Index being edited; `None` while adding a new account.
    editing: Option<usize>,
    /// Emoji currently chosen in the editor (`None` → use initials).
    emoji: Option<String>,
    /// WYSIWYG editor for the account signature.
    sig_editor: RichEditor,
    /// The email value the label field currently mirrors, so the label auto-fills
    /// from the email until the user customizes it.
    label_synced: String,
    /// GNOME Online Accounts mail accounts available to import (not yet in Vireo).
    goa: Vec<crate::goa::GoaMailAccount>,
    /// Refresh token captured from a successful OAuth sign-in, applied on save.
    pending_oauth_refresh: Option<String>,
}

#[derive(Debug)]
pub enum AccountsInput {
    AddAccount,
    EditAccount(usize),
    /// The email field changed — mirror it into the (auto-filled) label field.
    EmailChanged,
    MoveRow { from: usize, to: usize },
    /// Enable/disable an account from the list toggle.
    ToggleEnabled { index: usize, enabled: bool },
    /// Enable/disable the account currently open in the editor (GOA group toggle).
    ToggleCurrentEnabled(bool),
    /// Import a GNOME Online Account (by index into `goa`) into Vireo.
    ImportGoa(usize),
    /// The provider dropdown changed — adapt the form (servers vs. OAuth).
    ProviderChanged,
    /// Start the OAuth browser sign-in flow.
    OAuthSignIn,
    /// Open GNOME Settings → Online Accounts (the Google path).
    OpenOnlineAccounts,
    SetEmoji(String),
    ClearEmoji,
    TestConnection,
    Save,
    /// Second phase of Save, once the signature HTML has been read from the editor.
    SaveWithSig(String),
    /// Clicked "Remove Account" — ask for confirmation first.
    RemoveCurrent,
    /// Confirmed in the dialog — actually remove the account being edited.
    ConfirmRemove,
}

#[derive(Debug)]
pub enum AccountsOutput {
    /// `original_email` is `Some` (the pre-edit email) when editing, `None` when adding.
    Saved {
        original_email: Option<String>,
        account: Box<AccountConfig>,
    },
    Removed { email: String },
    /// New display order, as account emails.
    Reordered(Vec<String>),
    /// An account was enabled/disabled from the list.
    EnabledChanged { email: String, enabled: bool },
    /// Import a GNOME Online Account into Vireo (with its credentials).
    ImportGoa(Box<AccountConfig>),
    Closed,
}

/// Background command results for the editor.
#[derive(Debug)]
pub enum AccountsCmd {
    /// Test-connection result.
    Test(ConnTest),
    /// OAuth sign-in result: the refresh token, or an error message.
    OAuth(Result<String, String>),
}

#[relm4::component(pub)]
impl Component for AccountsWindow {
    type Init = Vec<AccountConfig>;
    type Input = AccountsInput;
    type Output = AccountsOutput;
    type CommandOutput = AccountsCmd;

    view! {
        adw::Window {
            set_modal: false,
            set_default_width: 480,
            set_default_height: 620,
            set_title: Some("Accounts"),

            connect_close_request[sender] => move |_| {
                let _ = sender.output(AccountsOutput::Closed);
                gtk::glib::Propagation::Proceed
            },

            #[wrap(Some)]
            #[name = "nav"]
            set_content = &adw::NavigationView {

                // ---- list page ----
                add = &adw::NavigationPage {
                    set_title: "Accounts",
                    set_tag: Some("list"),

                    #[wrap(Some)]
                    set_child = &adw::ToolbarView {
                        add_top_bar = &adw::HeaderBar {},

                        #[wrap(Some)]
                        set_content = &adw::PreferencesPage {
                            add = &adw::PreferencesGroup {
                                set_title: "Mail Accounts",
                                set_description: Some(
                                    "Drag to set the order they appear in the sidebar."
                                ),

                                #[name = "accounts_list"]
                                gtk::ListBox {
                                    add_css_class: "boxed-list",
                                    set_selection_mode: gtk::SelectionMode::None,
                                    connect_row_activated[sender] => move |_, row| {
                                        sender.input(AccountsInput::EditAccount(row.index() as usize));
                                    },
                                },
                            },

                            #[name = "goa_group"]
                            add = &adw::PreferencesGroup {
                                set_title: "GNOME Online Accounts",
                                set_description: Some(
                                    "Mail accounts from GNOME Settings. Toggle one on to \
                                     use it in Vireo."
                                ),
                                set_visible: false,

                                #[name = "goa_list"]
                                gtk::ListBox {
                                    add_css_class: "boxed-list",
                                    set_selection_mode: gtk::SelectionMode::None,
                                },
                            },

                            add = &adw::PreferencesGroup {
                                gtk::Button {
                                    set_label: "Add Account",
                                    add_css_class: "suggested-action",
                                    add_css_class: "pill",
                                    set_halign: gtk::Align::Center,
                                    connect_clicked => AccountsInput::AddAccount,
                                },
                            },
                        },
                    },
                },

                // ---- editor page ----
                add = &adw::NavigationPage {
                    set_title: "Account",
                    set_tag: Some("editor"),

                    #[wrap(Some)]
                    set_child = &adw::ToolbarView {
                        add_top_bar = &adw::HeaderBar {
                            set_show_end_title_buttons: false,
                            pack_end = &gtk::Button {
                                set_label: "Save",
                                add_css_class: "suggested-action",
                                connect_clicked => AccountsInput::Save,
                            },
                        },

                        #[wrap(Some)]
                        set_content = &adw::PreferencesPage {
                            add = &adw::PreferencesGroup {
                                set_title: "Mail Account",

                                // Pick the provider first; the rest of the form
                                // adapts (server fields vs. OAuth sign-in).
                                #[name = "provider_row"]
                                adw::ComboRow {
                                    set_title: "Provider",
                                    set_subtitle: "Choose your email provider.",
                                    connect_selected_notify => AccountsInput::ProviderChanged,
                                },
                                #[name = "name_row"]
                                adw::EntryRow { set_title: "Display Name" },
                                #[name = "email_row"]
                                adw::EntryRow {
                                    set_title: "Email Address",
                                    set_input_purpose: gtk::InputPurpose::Email,
                                },
                                #[name = "protocol_row"]
                                adw::ComboRow {
                                    set_title: "Incoming Protocol",
                                },
                                #[name = "host_row"]
                                adw::EntryRow { set_title: "Incoming Server" },
                                #[name = "port_row"]
                                adw::EntryRow {
                                    set_title: "Port (IMAP 993 / POP3 995)",
                                    set_input_purpose: gtk::InputPurpose::Digits,
                                },
                                #[name = "smtp_row"]
                                adw::EntryRow { set_title: "SMTP Server (optional)" },
                                #[name = "smtp_port_row"]
                                adw::EntryRow {
                                    set_title: "SMTP Port (default 587)",
                                    set_input_purpose: gtk::InputPurpose::Digits,
                                },
                                #[name = "user_row"]
                                adw::EntryRow { set_title: "Username" },
                                #[name = "pass_row"]
                                adw::PasswordEntryRow { set_title: "Password" },

                                // ---- OAuth fields (shown when Authentication is an OAuth option) ----
                                #[name = "oauth_client_id_row"]
                                adw::EntryRow {
                                    set_title: "OAuth Client ID",
                                    set_visible: false,
                                },
                                #[name = "oauth_secret_row"]
                                adw::PasswordEntryRow {
                                    set_title: "OAuth Client Secret (optional)",
                                    set_visible: false,
                                },
                                #[name = "oauth_auth_url_row"]
                                adw::EntryRow {
                                    set_title: "Authorization URL",
                                    set_visible: false,
                                },
                                #[name = "oauth_token_url_row"]
                                adw::EntryRow {
                                    set_title: "Token URL",
                                    set_visible: false,
                                },
                                #[name = "oauth_scope_row"]
                                adw::EntryRow {
                                    set_title: "Scopes (space-separated)",
                                    set_visible: false,
                                },
                                #[name = "oauth_signin_btn"]
                                gtk::Button {
                                    set_label: "Sign In with Browser",
                                    set_halign: gtk::Align::Start,
                                    set_margin_top: 16,
                                    set_visible: false,
                                    add_css_class: "suggested-action",
                                    connect_clicked => AccountsInput::OAuthSignIn,
                                },
                                #[name = "oauth_status"]
                                gtk::Label {
                                    set_visible: false,
                                    set_halign: gtk::Align::Start,
                                    set_xalign: 0.0,
                                    set_wrap: true,
                                },

                                // Shown for Google when no built-in/own OAuth client
                                // is available: point the user at GNOME Online Accounts.
                                #[name = "goa_hint"]
                                gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,
                                    set_spacing: 12,
                                    set_margin_top: 8,
                                    set_visible: false,

                                    gtk::Label {
                                        set_wrap: true,
                                        set_xalign: 0.0,
                                        add_css_class: "dim-label",
                                        set_label: "Google sign-in uses GNOME Online Accounts.\n\n\
                                            1. Open Online Accounts and sign in with Google.\n\
                                            2. Come back to Vireo and reopen this window — your \
                                            Google account then appears under “GNOME Online \
                                            Accounts” at the top of this window. Enable it there.",
                                    },
                                    gtk::Button {
                                        set_label: "Open Online Accounts…",
                                        set_halign: gtk::Align::Start,
                                        add_css_class: "suggested-action",
                                        connect_clicked => AccountsInput::OpenOnlineAccounts,
                                    },
                                },

                                #[name = "smtp_separate_row"]
                                adw::SwitchRow {
                                    set_title: "Separate SMTP credentials",
                                    set_subtitle: "Use a different username and password for \
                                                   sending. Off = use the credentials above.",
                                },
                                #[name = "smtp_user_row"]
                                adw::EntryRow { set_title: "SMTP Username" },
                                #[name = "smtp_pass_row"]
                                adw::PasswordEntryRow { set_title: "SMTP Password" },

                                gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,
                                    set_spacing: 6,
                                    set_margin_top: 16,

                                    #[name = "test_btn"]
                                    gtk::Button {
                                        set_label: "Test Connection",
                                        set_halign: gtk::Align::Start,
                                        connect_clicked => AccountsInput::TestConnection,
                                    },
                                    #[name = "test_result"]
                                    gtk::Label {
                                        set_visible: false,
                                        set_halign: gtk::Align::Start,
                                        set_xalign: 0.0,
                                        set_wrap: true,
                                    },
                                },
                            },

                            add = &adw::PreferencesGroup {
                                set_title: "Appearance",
                                set_description: Some(
                                    "How this account is shown in the sidebar and \
                                     the All Inboxes view."
                                ),

                                #[name = "label_row"]
                                adw::EntryRow {
                                    set_title: "Label (defaults to email address)",
                                },

                                adw::ActionRow {
                                    set_title: "Circle color",
                                    #[name = "color_btn"]
                                    add_suffix = &gtk::ColorDialogButton {
                                        set_valign: gtk::Align::Center,
                                        set_dialog: &gtk::ColorDialog::new(),
                                    },
                                },

                                adw::ActionRow {
                                    set_title: "Emoji",
                                    set_subtitle: "Optional — shown instead of initials",

                                    #[name = "emoji_btn"]
                                    add_suffix = &gtk::MenuButton {
                                        set_valign: gtk::Align::Center,
                                        set_label: "Add",
                                        #[wrap(Some)]
                                        set_popover = &gtk::EmojiChooser {
                                            connect_emoji_picked[sender] => move |_, text| {
                                                sender.input(AccountsInput::SetEmoji(text.to_string()));
                                            },
                                        },
                                    },
                                    add_suffix = &gtk::Button {
                                        set_valign: gtk::Align::Center,
                                        set_label: "Use initials",
                                        set_tooltip_text: Some("Show name initials instead of an emoji"),
                                        connect_clicked => AccountsInput::ClearEmoji,
                                    },
                                },
                            },

                            add = &adw::PreferencesGroup {
                                set_title: "Signature",
                                set_description: Some(
                                    "Appended to new messages sent from this account."
                                ),

                                #[name = "sig_holder"]
                                gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,
                                    set_height_request: 180,
                                    set_margin_top: 6,
                                },
                            },

                            // GOA-imported accounts: you can't meaningfully "remove"
                            // one from Vireo while it still lives in GNOME Online
                            // Accounts — so disable it here, or open GOA to change it.
                            #[name = "goa_manage_group"]
                            add = &adw::PreferencesGroup {
                                set_visible: false,
                                set_title: "GNOME Online Account",
                                set_description: Some(
                                    "This account comes from GNOME Online Accounts. Turn it off \
                                     to hide it in Vireo without touching your system; to edit or \
                                     remove it, open Online Accounts."
                                ),

                                #[name = "goa_enabled_row"]
                                adw::SwitchRow {
                                    set_title: "Enabled in Vireo",
                                    connect_active_notify[sender] => move |row| {
                                        sender.input(AccountsInput::ToggleCurrentEnabled(row.is_active()));
                                    },
                                },

                                gtk::Button {
                                    set_label: "Open Online Accounts…",
                                    set_halign: gtk::Align::Center,
                                    set_margin_top: 12,
                                    connect_clicked => AccountsInput::OpenOnlineAccounts,
                                },
                            },

                            #[name = "remove_group"]
                            add = &adw::PreferencesGroup {
                                gtk::Button {
                                    set_label: "Remove Account",
                                    add_css_class: "destructive-action",
                                    set_halign: gtk::Align::Center,
                                    connect_clicked => AccountsInput::RemoveCurrent,
                                },
                            },

                            add = &adw::PreferencesGroup {
                                gtk::Label {
                                    set_wrap: true,
                                    set_xalign: 0.0,
                                    add_css_class: "dim-label",
                                    add_css_class: "caption",
                                    set_label: "Your password is stored in the system keyring \
                                                (secret-service), never in plain text on disk.",
                                },
                            },
                        },
                    },
                },
            },
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // GNOME Online Accounts mail accounts not already configured in Vireo.
        let goa: Vec<crate::goa::GoaMailAccount> = crate::goa::list_mail_accounts()
            .into_iter()
            .filter(|g| !init.iter().any(|a| a.email.eq_ignore_ascii_case(&g.email)))
            .collect();

        let model = AccountsWindow {
            accounts: init,
            editing: None,
            emoji: None,
            sig_editor: RichEditor::new(""),
            label_synced: String::new(),
            goa,
            pending_oauth_refresh: None,
        };

        let widgets = view_output!();
        widgets.sig_holder.append(&model.sig_editor.widget);
        model.rebuild_account_list(&widgets.accounts_list, &sender);
        model.rebuild_goa_list(&widgets.goa_list, &sender);
        widgets.goa_group.set_visible(!model.goa.is_empty());
        widgets
            .protocol_row
            .set_model(Some(&gtk::StringList::new(&["IMAP", "POP3"])));

        // The Provider dropdown picks both the sign-in method and (for known
        // providers) the servers. The default popup ellipsizes items; a factory
        // whose labels don't lets the list widen to the full option text.
        let provider_labels: Vec<&str> = PROVIDERS.iter().map(|p| p.label).collect();
        widgets
            .provider_row
            .set_model(Some(&gtk::StringList::new(&provider_labels)));
        widgets.provider_row.set_list_factory(Some(&non_ellipsizing_factory()));

        // Show the SMTP credential fields only when the toggle is on.
        widgets
            .smtp_separate_row
            .bind_property("active", &widgets.smtp_user_row, "visible")
            .sync_create()
            .build();
        widgets
            .smtp_separate_row
            .bind_property("active", &widgets.smtp_pass_row, "visible")
            .sync_create()
            .build();

        // Auto-fill the label from the email as it's typed (until customized).
        let es = sender.clone();
        widgets.email_row.connect_changed(move |_| es.input(AccountsInput::EmailChanged));

        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::Input,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match message {
            AccountsInput::AddAccount => {
                self.editing = None;
                self.emoji = None;
                self.label_synced = String::new();
                self.pending_oauth_refresh = None;
                clear_editor(widgets);
                self.apply_provider(widgets);
                set_connection_editable(widgets, true);
                self.sig_editor.set_html("");
                widgets.color_btn.set_rgba(&parse_color(DEFAULT_COLOR));
                widgets.emoji_btn.set_label("Add");
                widgets.remove_group.set_visible(false);
                widgets.goa_manage_group.set_visible(false);
                widgets.nav.push_by_tag("editor");
            }

            AccountsInput::EditAccount(i) => {
                let Some(acc) = self.accounts.get(i).cloned() else {
                    return;
                };
                self.editing = Some(i);
                self.pending_oauth_refresh = None;
                fill_editor(widgets, &acc);
                self.apply_provider(widgets);
                // Label mirrors the email until customized.
                self.label_synced = acc.email.clone();
                self.sig_editor
                    .set_html(&rich_editor::signature_to_html(acc.signature.as_deref().unwrap_or("")));
                widgets
                    .color_btn
                    .set_rgba(&parse_color(acc.color.as_deref().unwrap_or(DEFAULT_COLOR)));
                self.emoji = acc.emoji.clone();
                widgets.emoji_btn.set_label(self.emoji.as_deref().unwrap_or("Add"));
                // GOA accounts: no "Remove" (it lives in the system) — offer an
                // enable/disable toggle and a shortcut to Online Accounts instead.
                let is_goa = acc.goa_id.is_some();
                set_connection_editable(widgets, !is_goa);
                widgets.remove_group.set_visible(!is_goa);
                widgets.goa_manage_group.set_visible(is_goa);
                widgets.goa_enabled_row.set_active(acc.enabled);
                widgets
                    .goa_enabled_row
                    .set_sensitive(!acc.goa_mail_disabled);
                widgets.goa_enabled_row.set_subtitle(if acc.goa_mail_disabled {
                    "Mail is disabled for this account in GNOME Settings"
                } else {
                    ""
                });
                widgets.nav.push_by_tag("editor");
            }

            AccountsInput::EmailChanged => {
                let email = trimmed(&widgets.email_row);
                let label = widgets.label_row.text().to_string();
                // Mirror while the label is still tracking the email (or empty);
                // once the user types a custom label, stop.
                if label.is_empty() || label == self.label_synced {
                    widgets.label_row.set_text(&email);
                }
                self.label_synced = email;
            }

            AccountsInput::MoveRow { from, to } => {
                if from < self.accounts.len() {
                    let acc = self.accounts.remove(from);
                    let to = to.min(self.accounts.len());
                    self.accounts.insert(to, acc);
                    self.rebuild_account_list(&widgets.accounts_list, &sender);
                    let emails = self.accounts.iter().map(|a| a.email.clone()).collect();
                    let _ = sender.output(AccountsOutput::Reordered(emails));
                }
            }

            AccountsInput::ToggleEnabled { index, enabled } => {
                if let Some(acc) = self.accounts.get_mut(index) {
                    if acc.goa_mail_disabled {
                        return;
                    }
                    if acc.enabled != enabled {
                        acc.enabled = enabled;
                        let email = acc.email.clone();
                        let _ = sender.output(AccountsOutput::EnabledChanged { email, enabled });
                    }
                }
            }

            AccountsInput::ToggleCurrentEnabled(enabled) => {
                if let Some(i) = self.editing {
                    if let Some(acc) = self.accounts.get_mut(i) {
                        if acc.goa_mail_disabled {
                            return;
                        }
                        if acc.enabled != enabled {
                            acc.enabled = enabled;
                            let email = acc.email.clone();
                            let _ = sender.output(AccountsOutput::EnabledChanged { email, enabled });
                            self.rebuild_account_list(&widgets.accounts_list, &sender);
                        }
                    }
                }
            }

            AccountsInput::ImportGoa(index) => {
                if let Some(g) = self.goa.get(index).cloned() {
                    // GOA remains authoritative for secrets even on this legacy
                    // manual path; workers fetch them directly when connecting.
                    let account = g.to_config(String::new(), String::new(), g.oauth2);
                    self.goa.remove(index);
                    self.accounts.push(account.clone());
                    self.rebuild_account_list(&widgets.accounts_list, &sender);
                    self.rebuild_goa_list(&widgets.goa_list, &sender);
                    widgets.goa_group.set_visible(!self.goa.is_empty());
                    let _ = sender.output(AccountsOutput::ImportGoa(Box::new(account)));
                }
            }

            AccountsInput::SetEmoji(text) => {
                widgets.emoji_btn.set_label(&text);
                self.emoji = Some(text);
            }

            AccountsInput::ClearEmoji => {
                self.emoji = None;
                widgets.emoji_btn.set_label("Add");
            }

            AccountsInput::TestConnection => {
                let mut account = read_account(widgets, self.emoji.clone());
                if let Some(orig) = self.editing.and_then(|i| self.accounts.get(i)) {
                    preserve_hidden_transport(&mut account, orig);
                    account.goa_id = orig.goa_id.clone();
                    if orig.goa_id.is_some() {
                        account.oauth = orig.oauth;
                        account.oauth_settings = orig.oauth_settings.clone();
                    }
                }
                widgets.test_btn.set_sensitive(false);
                widgets.test_result.set_visible(true);
                widgets.test_result.set_css_classes(&["dim-label"]);
                widgets.test_result.set_label("Testing…");
                sender.oneshot_command(async move {
                    let r = tokio::task::spawn_blocking(move || {
                        worker::test_connection_blocking(account)
                    })
                    .await
                    .unwrap_or_else(|_| ConnTest {
                        incoming: Err("test could not run".into()),
                        smtp: Err("test could not run".into()),
                    });
                    AccountsCmd::Test(r)
                });
            }

            AccountsInput::ProviderChanged => {
                self.apply_provider(widgets);
            }

            AccountsInput::OAuthSignIn => {
                let settings = self.oauth_settings_from_form(widgets);
                if settings.client_id.trim().is_empty()
                    || settings.auth_url.is_empty()
                    || settings.token_url.is_empty()
                {
                    widgets.oauth_status.set_visible(true);
                    widgets.oauth_status.set_css_classes(&["error"]);
                    widgets
                        .oauth_status
                        .set_label("Enter a client ID (and endpoints for a custom provider) first");
                    return;
                }
                widgets.oauth_signin_btn.set_sensitive(false);
                widgets.oauth_status.set_visible(true);
                widgets.oauth_status.set_css_classes(&["dim-label"]);
                widgets
                    .oauth_status
                    .set_label("Opening browser… complete sign-in there.");
                sender.oneshot_command(async move {
                    let r = tokio::task::spawn_blocking(move || {
                        crate::oauth::run_flow(&settings).map(|f| f.refresh_token)
                    })
                    .await
                    .unwrap_or_else(|_| Err("sign-in task failed".into()));
                    AccountsCmd::OAuth(r)
                });
            }

            AccountsInput::OpenOnlineAccounts => open_online_accounts(),

            AccountsInput::Save => {
                // Pull the signature HTML out of the editor first (async), then
                // finish saving in SaveWithSig.
                let s = sender.clone();
                self.sig_editor
                    .extract_html(move |html| s.input(AccountsInput::SaveWithSig(html)));
            }

            AccountsInput::SaveWithSig(sig_html) => {
                widgets.host_row.remove_css_class("error");
                let mut account = read_account(widgets, self.emoji.clone());
                let sig = sig_html.trim();
                account.signature = if signature_is_empty(sig) {
                    None
                } else {
                    Some(sig_html.clone())
                };
                account.signature_html = true;

                // Editing preserves the enabled state; GOA accounts keep their
                // (GOA-driven) OAuth mechanism regardless of the Authentication combo.
                let editing_orig = self.editing.and_then(|i| self.accounts.get(i)).cloned();
                if let Some(orig) = &editing_orig {
                    preserve_hidden_transport(&mut account, orig);
                    account.enabled = orig.enabled;
                    account.goa_id = orig.goa_id.clone();
                    account.goa_mail_disabled = orig.goa_mail_disabled;
                    account.goa_enabled_before_mail_disabled =
                        orig.goa_enabled_before_mail_disabled;
                    if orig.goa_id.is_some() {
                        account.oauth = orig.oauth;
                        account.oauth_settings = orig.oauth_settings.clone();
                    }
                }

                // Native account: authentication comes from the provider dropdown.
                let is_oauth = provider_at(widgets.provider_row.selected()).is_oauth();
                if account.goa_id.is_none() {
                    if is_oauth {
                        account.oauth = true;
                        account.oauth_settings = Some(self.oauth_settings_from_form(widgets));
                        if account.username.trim().is_empty() {
                            account.username = account.email.clone();
                        }
                        // A fresh sign-in supplies a refresh token; otherwise keep
                        // the one already in the keyring (edit without re-signing).
                        if let Some(rt) = self.pending_oauth_refresh.clone() {
                            account.oauth_refresh = rt;
                        }
                    } else {
                        account.oauth = false;
                        account.oauth_settings = None;
                    }
                }

                // Validation.
                let oauth_ready = if account.oauth && account.goa_id.is_none() {
                    let has_client = account
                        .oauth_settings
                        .as_ref()
                        .is_some_and(|s| !s.client_id.trim().is_empty());
                    let signed_in = self.pending_oauth_refresh.is_some()
                        || editing_orig.as_ref().is_some_and(|o| o.oauth);
                    if !has_client || !signed_in {
                        widgets.oauth_status.set_visible(true);
                        widgets.oauth_status.set_css_classes(&["error"]);
                        widgets
                            .oauth_status
                            .set_label("Enter a client ID and sign in before saving");
                    }
                    has_client && signed_in
                } else {
                    true
                };
                // GOA-managed secrets are intentionally absent from this form:
                // workers fetch them directly from GOA when they connect.
                let goa_managed = account.goa_id.is_some();
                let password_ok = account.oauth || goa_managed || !account.password.is_empty();
                if account.imap_host.is_empty()
                    || account.username.is_empty()
                    || !password_ok
                    || !oauth_ready
                    || (account.smtp_separate
                        && (account.smtp_username.is_empty()
                            || (!goa_managed && account.smtp_password.is_empty())))
                {
                    widgets.host_row.add_css_class("error");
                    return;
                }
                self.pending_oauth_refresh = None;

                let original_email = self
                    .editing
                    .and_then(|i| self.accounts.get(i))
                    .map(|a| a.email.clone());
                match self.editing {
                    Some(i) if i < self.accounts.len() => self.accounts[i] = account.clone(),
                    _ => self.accounts.push(account.clone()),
                }
                self.rebuild_account_list(&widgets.accounts_list, &sender);
                let _ = sender.output(AccountsOutput::Saved {
                    original_email,
                    account: Box::new(account),
                });
                widgets.nav.pop();
            }

            AccountsInput::RemoveCurrent => {
                // Confirm before this destructive, keyring-clearing action.
                let Some(i) = self.editing else { return };
                let Some(account) = self.accounts.get(i) else { return };
                let name = if account.name.trim().is_empty() {
                    account.email.clone()
                } else {
                    account.name.clone()
                };
                let dialog = adw::MessageDialog::new(
                    Some(root),
                    Some("Remove Account?"),
                    Some(&format!(
                        "Remove {name} from Vireo? Its saved password is deleted from \
                         the keyring. Mail on the server is not affected."
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
                        s.input(AccountsInput::ConfirmRemove);
                    }
                });
                dialog.present();
            }
            AccountsInput::ConfirmRemove => {
                if let Some(i) = self.editing {
                    if i < self.accounts.len() {
                        let email = self.accounts[i].email.clone();
                        self.accounts.remove(i);
                        self.rebuild_account_list(&widgets.accounts_list, &sender);
                        let _ = sender.output(AccountsOutput::Removed { email });
                    }
                }
                widgets.nav.pop();
            }
        }
    }

    fn update_cmd_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        result: AccountsCmd,
        _sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        match result {
            AccountsCmd::Test(result) => {
                let line = |label: &str, r: &Result<(), String>| match r {
                    Ok(()) => format!("✓ {label}: connected"),
                    Err(e) => format!("✗ {label}: {e}"),
                };
                let incoming_label =
                    if widgets.protocol_row.selected() == 1 { "POP3" } else { "IMAP" };
                let text = format!(
                    "{}\n{}",
                    line(incoming_label, &result.incoming),
                    line("SMTP", &result.smtp),
                );
                let class = if result.incoming.is_ok() && result.smtp.is_ok() {
                    "success"
                } else {
                    "error"
                };
                widgets.test_result.set_label(&text);
                widgets.test_result.set_css_classes(&[class]);
                widgets.test_btn.set_sensitive(true);
            }
            AccountsCmd::OAuth(result) => {
                widgets.oauth_signin_btn.set_sensitive(true);
                widgets.oauth_status.set_visible(true);
                match result {
                    Ok(refresh) => {
                        self.pending_oauth_refresh = Some(refresh);
                        widgets.oauth_status.set_css_classes(&["success"]);
                        widgets.oauth_status.set_label("✓ Signed in — save the account to finish");
                    }
                    Err(e) => {
                        widgets.oauth_status.set_css_classes(&["error"]);
                        widgets.oauth_status.set_label(&format!("Sign-in failed: {e}"));
                    }
                }
            }
        }
    }
}

impl AccountsWindow {
    /// Rebuild the draggable account list.
    fn rebuild_account_list(&self, list: &gtk::ListBox, sender: &ComponentSender<Self>) {
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }

        for (pos, acc) in self.accounts.iter().enumerate() {
            let row = gtk::ListBoxRow::new();
            row.set_activatable(true);

            let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            hbox.add_css_class("account-list-row");

            let handle = gtk::Image::from_icon_name("co.hyprlab.Vireo-list-drag-handle-symbolic");
            handle.add_css_class("dim-label");
            hbox.append(&handle);

            let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
            vbox.set_hexpand(true);
            vbox.set_valign(gtk::Align::Center);
            let name = gtk::Label::new(Some(&display_name(acc)));
            name.set_halign(gtk::Align::Start);
            name.set_ellipsize(gtk::pango::EllipsizeMode::End);
            name.add_css_class("account-name");
            let email = gtk::Label::new(Some(&acc.email));
            email.set_halign(gtk::Align::Start);
            email.set_ellipsize(gtk::pango::EllipsizeMode::End);
            email.add_css_class("account-email");
            vbox.append(&name);
            vbox.append(&email);
            hbox.append(&vbox);

            // Source badge: is this account from GNOME Online Accounts, or added
            // directly in Vireo?
            let from_goa = acc.goa_id.is_some();
            let badge = gtk::Label::new(Some(if from_goa { "Online Account" } else { "Vireo" }));
            badge.set_valign(gtk::Align::Center);
            badge.add_css_class("account-source-badge");
            if from_goa {
                badge.add_css_class("goa");
                badge.set_tooltip_text(Some("Imported from GNOME Online Accounts"));
            } else {
                badge.set_tooltip_text(Some("Added directly in Vireo"));
            }
            hbox.append(&badge);

            // Enable/disable toggle. Disabled accounts stay configured but don't
            // sync or appear in the sidebar.
            let toggle = gtk::Switch::new();
            toggle.set_valign(gtk::Align::Center);
            toggle.set_tooltip_text(Some(if acc.goa_mail_disabled {
                "Mail is disabled for this account in GNOME Settings"
            } else {
                "Enable this account"
            }));
            toggle.set_active(acc.enabled);
            toggle.set_sensitive(!acc.goa_mail_disabled);
            let ti = sender.input_sender().clone();
            let tpos = pos;
            toggle.connect_state_set(move |_, state| {
                let _ = ti.send(AccountsInput::ToggleEnabled { index: tpos, enabled: state });
                gtk::glib::Propagation::Proceed
            });
            hbox.append(&toggle);

            let next = gtk::Image::from_icon_name("co.hyprlab.Vireo-go-next-symbolic");
            next.add_css_class("dim-label");
            hbox.append(&next);

            row.set_child(Some(&hbox));

            // Drag to reorder.
            let drag = gtk::DragSource::new();
            drag.set_actions(gtk::gdk::DragAction::MOVE);
            let from = pos as u32;
            drag.connect_prepare(move |_, _, _| {
                Some(gtk::gdk::ContentProvider::for_value(&from.to_value()))
            });
            row.add_controller(drag);

            let drop = gtk::DropTarget::new(gtk::glib::Type::U32, gtk::gdk::DragAction::MOVE);
            let to = pos;
            let input = sender.input_sender().clone();
            drop.connect_drop(move |_, value, _, _| {
                if let Ok(from) = value.get::<u32>() {
                    let _ = input.send(AccountsInput::MoveRow {
                        from: from as usize,
                        to,
                    });
                    true
                } else {
                    false
                }
            });
            row.add_controller(drop);

            list.append(&row);
        }
    }

    /// Populate the "GNOME Online Accounts" list with importable mail accounts.
    fn rebuild_goa_list(&self, list: &gtk::ListBox, sender: &ComponentSender<Self>) {
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        for (pos, g) in self.goa.iter().enumerate() {
            let row = adw::ActionRow::new();
            row.set_title(&g.email);
            let mut subtitle = if g.provider.is_empty() {
                "Mail".to_string()
            } else {
                g.provider.clone()
            };
            // OAuth providers (Gmail, Microsoft) sign in with a token from GNOME.
            if g.oauth2 && !g.password_based {
                subtitle.push_str(" · sign-in via GNOME");
            }
            row.set_subtitle(&subtitle);

            let toggle = gtk::Switch::new();
            toggle.set_valign(gtk::Align::Center);
            toggle.set_active(false);
            toggle.set_tooltip_text(Some("Use this account in Vireo"));
            let ti = sender.input_sender().clone();
            let tpos = pos;
            toggle.connect_state_set(move |_, state| {
                if state {
                    let _ = ti.send(AccountsInput::ImportGoa(tpos));
                }
                gtk::glib::Propagation::Proceed
            });
            row.add_suffix(&toggle);
            list.append(&row);
        }
    }

    /// Show/hide credential rows based on the Authentication combo, and pre-fill
    /// server settings for known OAuth providers.
    /// Adapt the editor to the selected provider: show server + credential fields
    /// for password providers, the OAuth sign-in for OAuth providers, and fill in
    /// the servers for known providers.
    fn apply_provider(&self, widgets: &AccountsWindowWidgets) {
        let p = provider_at(widgets.provider_row.selected());
        let is_password = p.is_password();
        let is_oauth = p.is_oauth();
        let is_custom = matches!(p.kind, ProviderKind::CustomOAuth);
        // Google with no built-in (or user-supplied) OAuth client: there's nothing
        // to sign in with, so guide the user to GNOME Online Accounts instead.
        let google_needs_goa = matches!(p.kind, ProviderKind::Google)
            && crate::oauth::provider_credentials("google").0.trim().is_empty();
        // Google/Microsoft servers come from the built-in preset (hidden). Custom
        // OAuth still needs its server addresses and client details entered.
        let show_servers = is_password || is_custom;

        widgets.provider_row.set_subtitle(p.hint);

        // Server/credential fields (password or Custom-OAuth manual servers).
        widgets.protocol_row.set_visible(is_password);
        widgets.host_row.set_visible(show_servers);
        widgets.port_row.set_visible(show_servers);
        widgets.smtp_row.set_visible(show_servers);
        widgets.smtp_port_row.set_visible(show_servers);
        widgets.user_row.set_visible(is_password);
        widgets.pass_row.set_visible(is_password);
        widgets.smtp_separate_row.set_visible(is_password);
        widgets.test_btn.set_visible(is_password);
        if is_oauth {
            widgets.smtp_separate_row.set_active(false);
        }

        // OAuth: the user just signs in. Google with no client falls back to the
        // GNOME Online Accounts panel, which replaces the sign-in + identity fields.
        widgets.name_row.set_visible(!google_needs_goa);
        widgets.email_row.set_visible(!google_needs_goa);
        widgets.goa_hint.set_visible(google_needs_goa);
        widgets.oauth_signin_btn.set_visible(is_oauth && !google_needs_goa);
        widgets.oauth_client_id_row.set_visible(is_custom);
        widgets.oauth_secret_row.set_visible(is_custom);
        widgets.oauth_auth_url_row.set_visible(is_custom);
        widgets.oauth_token_url_row.set_visible(is_custom);
        widgets.oauth_scope_row.set_visible(is_custom);
        if !is_oauth || google_needs_goa {
            widgets.oauth_status.set_visible(false);
        }

        // Auto-fill IMAP/SMTP: known password providers from the preset table,
        // Google/Microsoft from the OAuth preset (filled but hidden, so the saved
        // account still carries the right servers). Manual/Custom are left alone.
        let servers = match p.kind {
            ProviderKind::Preset => Some((p.imap_host, p.imap_port, p.smtp_host, p.smtp_port)),
            ProviderKind::Google | ProviderKind::Microsoft => crate::oauth::preset(p.oauth_name().unwrap())
                .map(|o| (o.imap_host, o.imap_port, o.smtp_host, o.smtp_port)),
            ProviderKind::Manual | ProviderKind::CustomOAuth => None,
        };
        if let Some((ih, ip, sh, sp)) = servers {
            widgets.protocol_row.set_selected(0); // IMAP
            widgets.host_row.set_text(ih);
            widgets.port_row.set_text(&ip.to_string());
            widgets.smtp_row.set_text(sh);
            widgets.smtp_port_row.set_text(&sp.to_string());
        }
    }

    /// Build the OAuth client config from the form. Google/Microsoft use built-in
    /// endpoints + credentials; "Custom OAuth" uses the user-entered fields.
    fn oauth_settings_from_form(&self, widgets: &AccountsWindowWidgets) -> OAuthSettings {
        let provider = provider_at(widgets.provider_row.selected()).oauth_name();
        if let Some(name) = provider {
            let p = crate::oauth::preset(name).unwrap();
            let (client_id, client_secret) = crate::oauth::provider_credentials(name);
            OAuthSettings {
                auth_url: p.auth_url.to_string(),
                token_url: p.token_url.to_string(),
                client_id,
                client_secret,
                scopes: p.scopes.to_string(),
            }
        } else {
            OAuthSettings {
                auth_url: trimmed(&widgets.oauth_auth_url_row),
                token_url: trimmed(&widgets.oauth_token_url_row),
                client_id: trimmed(&widgets.oauth_client_id_row),
                client_secret: widgets.oauth_secret_row.text().to_string(),
                scopes: trimmed(&widgets.oauth_scope_row),
            }
        }
    }
}

/// A list-item factory whose labels never ellipsize, so a `ComboRow` popup grows
/// to fit its longest option instead of truncating it.
/// Open GNOME Settings → Online Accounts. Uses D-Bus app activation so it works
/// both natively and inside a Flatpak (with `--talk-name=org.gnome.Settings`);
/// falls back to the CLI on non-GNOME/older setups.
fn open_online_accounts() {
    if activate_online_accounts_panel().is_err() {
        let _ = std::process::Command::new("gnome-control-center")
            .arg("online-accounts")
            .spawn();
    }
}

fn activate_online_accounts_panel() -> Result<(), gtk::glib::Error> {
    let conn = gtk::gio::bus_get_sync(gtk::gio::BusType::Session, gtk::gio::Cancellable::NONE)?;
    // org.freedesktop.Application.ActivateAction(action: s, parameter: av, a{sv}).
    // GNOME Settings' "launch-panel" action takes a (sav): (panel_id, extra_args).
    let panel = ("online-accounts", Vec::<gtk::glib::Variant>::new()).to_variant();
    let params: Vec<gtk::glib::Variant> = vec![panel];
    let platform: std::collections::HashMap<String, gtk::glib::Variant> =
        std::collections::HashMap::new();
    let args = ("launch-panel", params, platform).to_variant();
    conn.call_sync(
        Some("org.gnome.Settings"),
        "/org/gnome/Settings",
        "org.freedesktop.Application",
        "ActivateAction",
        Some(&args),
        None,
        gtk::gio::DBusCallFlags::NONE,
        -1,
        gtk::gio::Cancellable::NONE,
    )?;
    Ok(())
}

fn non_ellipsizing_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            let label = gtk::Label::new(None);
            label.set_xalign(0.0);
            label.set_ellipsize(gtk::pango::EllipsizeMode::None);
            label.set_margin_start(6);
            label.set_margin_end(6);
            item.set_child(Some(&label));
        }
    });
    factory.connect_bind(|_, item| {
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            let text = item
                .item()
                .and_downcast::<gtk::StringObject>()
                .map(|o| o.string())
                .unwrap_or_default();
            if let Some(label) = item.child().and_downcast::<gtk::Label>() {
                label.set_label(&text);
            }
        }
    });
    factory
}

fn display_name(acc: &AccountConfig) -> String {
    if acc.name.trim().is_empty() {
        acc.email.clone()
    } else {
        acc.name.clone()
    }
}

fn preserve_hidden_transport(account: &mut AccountConfig, original: &AccountConfig) {
    // The editor exposes ports but not the transport/authentication switches.
    // Keep custom modes when the corresponding port was not changed.
    if account.imap_port == original.imap_port {
        account.imap_starttls = original.imap_starttls;
        account.imap_implicit_tls = original.imap_implicit_tls;
    }
    if account.smtp_port == original.smtp_port {
        account.smtp_implicit_tls = original.smtp_implicit_tls;
    }
    account.smtp_auth = original.smtp_auth;
}

fn set_connection_editable(widgets: &AccountsWindowWidgets, editable: bool) {
    widgets.provider_row.set_sensitive(editable);
    widgets.name_row.set_sensitive(true); // local From/display name
    widgets.email_row.set_sensitive(editable);
    widgets.protocol_row.set_sensitive(editable);
    widgets.host_row.set_sensitive(editable);
    widgets.port_row.set_sensitive(editable);
    widgets.smtp_row.set_sensitive(editable);
    widgets.smtp_port_row.set_sensitive(editable);
    widgets.user_row.set_sensitive(editable);
    widgets.pass_row.set_sensitive(editable);
    widgets.smtp_separate_row.set_sensitive(editable);
    widgets.smtp_user_row.set_sensitive(editable);
    widgets.smtp_pass_row.set_sensitive(editable);
}

/// Build an `AccountConfig` from the current editor form values.
fn read_account(widgets: &AccountsWindowWidgets, emoji: Option<String>) -> AccountConfig {
    let protocol = if widgets.protocol_row.selected() == 1 {
        Protocol::Pop3
    } else {
        Protocol::Imap
    };
    let default_port = if protocol == Protocol::Pop3 { 995 } else { 993 };
    AccountConfig {
        name: trimmed(&widgets.name_row),
        email: trimmed(&widgets.email_row),
        protocol,
        imap_host: trimmed(&widgets.host_row),
        imap_port: trimmed(&widgets.port_row).parse().unwrap_or(default_port),
        // The native editor follows the conventional ports: 143 is STARTTLS,
        // while 993 is implicit TLS. GOA supplies this flag explicitly.
        imap_starttls: protocol == Protocol::Imap
            && trimmed(&widgets.port_row).parse::<u16>().unwrap_or(default_port) == 143,
        imap_implicit_tls: protocol == Protocol::Imap
            && trimmed(&widgets.port_row).parse::<u16>().unwrap_or(default_port) == 993,
        smtp_host: trimmed(&widgets.smtp_row),
        smtp_port: trimmed(&widgets.smtp_port_row).parse().unwrap_or(587),
        smtp_implicit_tls: trimmed(&widgets.smtp_port_row).parse::<u16>().unwrap_or(587) == 465,
        smtp_auth: true,
        username: trimmed(&widgets.user_row),
        password: widgets.pass_row.text().to_string(),
        smtp_separate: widgets.smtp_separate_row.is_active(),
        smtp_username: trimmed(&widgets.smtp_user_row),
        smtp_password: widgets.smtp_pass_row.text().to_string(),
        color: Some(crate::color::to_hex(&widgets.color_btn.rgba())),
        emoji,
        // Filled in by SaveWithSig from the rich-text editor.
        signature: None,
        signature_html: true,
        // Only store a custom label; blank or same-as-email falls back to email.
        label: {
            let l = trimmed(&widgets.label_row);
            let email = trimmed(&widgets.email_row);
            if l.is_empty() || l == email {
                None
            } else {
                Some(l)
            }
        },
        // Defaults for a new account; preserved from the original when editing.
        enabled: true,
        goa_id: None,
        goa_mail_disabled: false,
        goa_enabled_before_mail_disabled: false,
        oauth: false,
        oauth_settings: None,
        oauth_refresh: String::new(),
    }
}

/// Whether the editor's HTML is effectively empty (no visible content).
fn signature_is_empty(html: &str) -> bool {
    let stripped: String = {
        let mut out = String::new();
        let mut in_tag = false;
        for c in html.chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                _ if !in_tag => out.push(c),
                _ => {}
            }
        }
        out
    };
    stripped.replace("&nbsp;", " ").trim().is_empty()
}

fn fill_editor(widgets: &AccountsWindowWidgets, acc: &AccountConfig) {
    widgets.name_row.set_text(&acc.name);
    widgets.email_row.set_text(&acc.email);
    // Reflect the account's provider in the dropdown (OAuth by endpoint, known
    // password providers by server, otherwise "Other (IMAP/POP3)…").
    widgets.provider_row.set_selected(provider_index_for_account(acc));
    widgets
        .protocol_row
        .set_selected(if acc.protocol == Protocol::Pop3 { 1 } else { 0 });
    widgets.host_row.set_text(&acc.imap_host);
    widgets.port_row.set_text(&acc.imap_port.to_string());
    widgets.smtp_row.set_text(&acc.smtp_host);
    widgets.smtp_port_row.set_text(&acc.smtp_port.to_string());
    widgets.user_row.set_text(&acc.username);
    widgets.pass_row.set_text(&acc.password);
    widgets.smtp_separate_row.set_active(acc.smtp_separate);
    widgets.smtp_user_row.set_text(&acc.smtp_username);
    widgets.smtp_pass_row.set_text(&acc.smtp_password);
    // Show the effective label (custom, or the email address).
    widgets
        .label_row
        .set_text(acc.label.as_deref().unwrap_or(&acc.email));

    // OAuth client detail fields. GOA accounts (goa_id set) can't be
    // re-authenticated here, so they show as password (their mechanism is kept on
    // save); natively-added OAuth accounts show their client details.
    let s = (acc.goa_id.is_none() && acc.oauth)
        .then_some(acc.oauth_settings.as_ref())
        .flatten();
    widgets.oauth_client_id_row.set_text(s.map(|s| s.client_id.as_str()).unwrap_or(""));
    widgets.oauth_secret_row.set_text(s.map(|s| s.client_secret.as_str()).unwrap_or(""));
    widgets.oauth_auth_url_row.set_text(s.map(|s| s.auth_url.as_str()).unwrap_or(""));
    widgets.oauth_token_url_row.set_text(s.map(|s| s.token_url.as_str()).unwrap_or(""));
    widgets.oauth_scope_row.set_text(s.map(|s| s.scopes.as_str()).unwrap_or(""));
    widgets.oauth_status.set_visible(false);
    widgets.oauth_signin_btn.set_sensitive(true);

    // Signature is loaded into the rich-text editor by the caller.
    widgets.test_result.set_visible(false);
    widgets.test_btn.set_sensitive(true);
}

fn clear_editor(widgets: &AccountsWindowWidgets) {
    widgets.name_row.set_text("");
    widgets.email_row.set_text("");
    widgets.provider_row.set_selected(manual_index());
    widgets.protocol_row.set_selected(0);
    widgets.host_row.set_text("");
    widgets.port_row.set_text("993");
    widgets.smtp_row.set_text("");
    widgets.smtp_port_row.set_text("587");
    widgets.user_row.set_text("");
    widgets.pass_row.set_text("");
    widgets.smtp_separate_row.set_active(false);
    widgets.smtp_user_row.set_text("");
    widgets.smtp_pass_row.set_text("");
    widgets.label_row.set_text("");
    widgets.oauth_client_id_row.set_text("");
    widgets.oauth_secret_row.set_text("");
    widgets.oauth_auth_url_row.set_text("");
    widgets.oauth_token_url_row.set_text("");
    widgets.oauth_scope_row.set_text("");
    widgets.oauth_status.set_visible(false);
    widgets.test_result.set_visible(false);
    widgets.test_btn.set_sensitive(true);
}

fn trimmed(row: &impl IsA<gtk::Editable>) -> String {
    row.text().trim().to_string()
}

/// Dropdown index of the first provider entry of a given kind (the manual entry
/// if none — shouldn't happen for kinds present in the table).
fn kind_index(kind: ProviderKind) -> u32 {
    PROVIDERS
        .iter()
        .position(|p| p.kind == kind)
        .map(|i| i as u32)
        .unwrap_or_else(manual_index)
}

/// Dropdown index of the `Preset` provider whose incoming server matches `host`,
/// or the manual entry when nothing matches.
fn preset_index_for_host(host: &str) -> u32 {
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() {
        return manual_index();
    }
    PROVIDERS
        .iter()
        .position(|p| p.kind == ProviderKind::Preset && p.imap_host.eq_ignore_ascii_case(&host))
        .map(|i| i as u32)
        .unwrap_or_else(manual_index)
}

/// Dropdown index reflecting an existing account: its OAuth provider (by token
/// endpoint) for native OAuth accounts, otherwise the matching password provider.
fn provider_index_for_account(acc: &AccountConfig) -> u32 {
    if acc.goa_id.is_none() && acc.oauth {
        let kind = match acc.oauth_settings.as_ref() {
            Some(s) if s.token_url.contains("googleapis") => ProviderKind::Google,
            Some(s) if s.token_url.contains("microsoftonline") => ProviderKind::Microsoft,
            _ => ProviderKind::CustomOAuth,
        };
        return kind_index(kind);
    }
    preset_index_for_host(&acc.imap_host)
}

fn parse_color(hex: &str) -> gtk::gdk::RGBA {
    gtk::gdk::RGBA::parse(hex).unwrap_or_else(|_| gtk::gdk::RGBA::new(0.21, 0.52, 0.89, 1.0))
}

#[cfg(test)]
mod tests {
    use super::{manual_index, preset_index_for_host, provider_at, ProviderKind, PROVIDERS};

    #[test]
    fn known_host_maps_to_its_own_entry() {
        let idx = preset_index_for_host("imap.mail.me.com");
        assert_eq!(provider_at(idx).label, "iCloud");
        // Case-insensitive.
        assert_eq!(preset_index_for_host("IMAP.FASTMAIL.COM"), preset_index_for_host("imap.fastmail.com"));
        assert_eq!(provider_at(preset_index_for_host("imap.fastmail.com")).label, "Fastmail");
    }

    #[test]
    fn unknown_or_empty_host_falls_back_to_manual() {
        assert_eq!(preset_index_for_host("mail.example.org"), manual_index());
        assert_eq!(preset_index_for_host(""), manual_index());
        assert_eq!(preset_index_for_host("  "), manual_index());
        assert_eq!(provider_at(manual_index()).kind, ProviderKind::Manual);
    }

    #[test]
    fn removed_password_providers_are_gone() {
        // Gmail/Hotmail no longer work with a password — only via OAuth.
        for p in PROVIDERS {
            if p.kind == ProviderKind::Preset {
                assert_ne!(p.imap_host, "imap.gmail.com");
                assert_ne!(p.imap_host, "outlook.office365.com");
            }
        }
    }

    #[test]
    fn provider_table_is_well_formed() {
        // Distinct labels; presets have sane servers; exactly one Manual entry.
        let mut labels: Vec<&str> = PROVIDERS.iter().map(|p| p.label).collect();
        let n = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), n, "provider labels must be unique");
        assert_eq!(PROVIDERS.iter().filter(|p| p.kind == ProviderKind::Manual).count(), 1);
        for p in PROVIDERS {
            if p.kind == ProviderKind::Preset {
                assert!(!p.imap_host.is_empty() && !p.smtp_host.is_empty(), "{}", p.label);
                assert!(p.imap_port > 0 && p.smtp_port > 0, "{}", p.label);
                // Each preset's host round-trips to its own entry.
                assert_eq!(provider_at(preset_index_for_host(p.imap_host)).label, p.label);
            }
        }
    }
}
