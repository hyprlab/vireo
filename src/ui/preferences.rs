//! Settings window: privacy options (remote-content allowlist).
//!
//! Account credentials are managed in their own window (see `ui/accounts.rs`).

use adw::prelude::*;
use relm4::factory::FactoryVecDeque;
use relm4::prelude::*;

use crate::config::MessageTheme;

/// Initial data for the settings window.
#[derive(Debug)]
pub struct PrefInit {
    pub allowed_senders: Vec<String>,
    pub gravatar: bool,
    pub fetch_interval_secs: u64,
    pub push: bool,
    pub blacklist: Vec<String>,
    pub palette_collapse_secs: u64,
    pub threading: bool,
    pub threads_expanded: bool,
    pub message_theme: MessageTheme,
    pub notifications: bool,
}

/// Message-content appearance options, in combo order.
const MESSAGE_THEMES: &[(&str, MessageTheme)] = &[
    ("Follow system", MessageTheme::System),
    ("Light", MessageTheme::Light),
    ("Dark", MessageTheme::Dark),
];

/// Selectable mail-check intervals (label, seconds). 0 = manual only.
const FETCH_INTERVALS: &[(&str, u64)] = &[
    ("Manually", 0),
    ("Every minute", 60),
    ("Every 5 minutes", 300),
    ("Every 15 minutes", 900),
    ("Every 30 minutes", 1800),
];

// ---- Allowed-sender row -----------------------------------------------------

pub struct SenderRow {
    addr: String,
}

#[derive(Debug)]
pub enum SenderRowOutput {
    Remove(String),
}

#[relm4::factory(pub)]
impl FactoryComponent for SenderRow {
    type Init = String;
    type Input = ();
    type Output = SenderRowOutput;
    type CommandOutput = ();
    type ParentWidget = gtk::ListBox;

    view! {
        adw::ActionRow {
            set_title: &self.addr,
            add_suffix = &gtk::Button {
                set_icon_name: "co.hyprlab.Vireo-user-trash-symbolic",
                set_valign: gtk::Align::Center,
                set_tooltip_text: Some("Remove"),
                add_css_class: "flat",
                connect_clicked[sender, addr = self.addr.clone()] => move |_| {
                    let _ = sender.output(SenderRowOutput::Remove(addr.clone()));
                },
            },
        }
    }

    fn init_model(addr: Self::Init, _index: &DynamicIndex, _sender: FactorySender<Self>) -> Self {
        Self { addr }
    }
}

// ---- Preferences window -----------------------------------------------------

pub struct Preferences {
    senders: FactoryVecDeque<SenderRow>,
    sender_addrs: Vec<String>,
    blacklist: FactoryVecDeque<SenderRow>,
    blacklist_addrs: Vec<String>,
}

#[derive(Debug)]
pub enum PrefInput {
    AddSenderText(String),
    RemoveSenderRow(String),
    AddBlacklistText(String),
    RemoveBlacklistRow(String),
    ToggleGravatar(bool),
    ToggleThreading(bool),
    ToggleThreadsExpanded(bool),
    ChangeFetchInterval(u32),
    TogglePush(bool),
    ToggleNotifications(bool),
    ChangePaletteCollapse(u64),
    ChangeMessageTheme(u32),
}

#[derive(Debug)]
pub enum PrefOutput {
    AddSender(String),
    RemoveSender(String),
    AddBlacklist(String),
    RemoveBlacklist(String),
    SetGravatar(bool),
    SetThreading(bool),
    SetThreadsExpanded(bool),
    SetFetchInterval(u64),
    SetPush(bool),
    SetNotifications(bool),
    SetPaletteCollapse(u64),
    SetMessageTheme(MessageTheme),
    Closed,
}

#[relm4::component(pub)]
impl Component for Preferences {
    type Init = PrefInit;
    type Input = PrefInput;
    type Output = PrefOutput;
    type CommandOutput = ();

    view! {
        adw::Window {
            set_modal: false,
            set_default_width: 500,
            set_default_height: 620,
            set_title: Some("Settings"),

            connect_close_request[sender] => move |_| {
                let _ = sender.output(PrefOutput::Closed);
                gtk::glib::Propagation::Proceed
            },

            #[wrap(Some)]
            set_content = &adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {},

                #[wrap(Some)]
                set_content = &adw::PreferencesPage {
                    add = &adw::PreferencesGroup {
                        set_title: "Mail",

                        #[name = "fetch_row"]
                        adw::ComboRow {
                            set_title: "Check for new mail",
                            connect_selected_notify[sender] => move |row| {
                                sender.input(PrefInput::ChangeFetchInterval(row.selected()));
                            },
                        },

                        #[name = "push_row"]
                        adw::SwitchRow {
                            set_title: "Instant new mail (IMAP push)",
                            set_subtitle: "Uses IMAP IDLE to receive messages the moment they arrive.",
                            connect_active_notify[sender] => move |row| {
                                sender.input(PrefInput::TogglePush(row.is_active()));
                            },
                        },

                        #[name = "notifications_row"]
                        adw::SwitchRow {
                            set_title: "Desktop notifications",
                            set_subtitle: "Show system notifications for new mail and error alerts when Vireo isn't focused.",
                            connect_active_notify[sender] => move |row| {
                                sender.input(PrefInput::ToggleNotifications(row.is_active()));
                            },
                        },
                    },

                    add = &adw::PreferencesGroup {
                        set_title: "Message List",

                        #[name = "threading_row"]
                        adw::SwitchRow {
                            set_title: "Group messages by conversation",
                            set_subtitle: "Collapse replies into a single threaded conversation.",
                            connect_active_notify[sender] => move |row| {
                                sender.input(PrefInput::ToggleThreading(row.is_active()));
                            },
                        },

                        #[name = "threads_expanded_row"]
                        adw::SwitchRow {
                            set_title: "Expand conversations by default",
                            set_subtitle: "Show every message of a conversation in the list. \
                                           When off, conversations start collapsed to their \
                                           newest message.",
                            connect_active_notify[sender] => move |row| {
                                sender.input(PrefInput::ToggleThreadsExpanded(row.is_active()));
                            },
                        },

                        #[name = "palette_collapse_row"]
                        adw::SpinRow {
                            set_title: "Actions Palette timeout",
                            set_subtitle: "Seconds the Actions Palette stays open after the cursor leaves it.",
                            connect_value_notify[sender] => move |row| {
                                sender.input(PrefInput::ChangePaletteCollapse(row.value() as u64));
                            },
                        },
                    },

                    add = &adw::PreferencesGroup {
                        set_title: "Reading",

                        #[name = "message_theme_row"]
                        adw::ComboRow {
                            set_title: "Message appearance",
                            set_subtitle: "Theme for email content only, not the app itself.",
                            connect_selected_notify[sender] => move |row| {
                                sender.input(PrefInput::ChangeMessageTheme(row.selected()));
                            },
                        },
                    },

                    add = &adw::PreferencesGroup {
                        set_title: "Privacy",
                        set_description: Some(
                            "Vireo collects no telemetry and sends no analytics. Remote \
                             content (images, trackers) is blocked by default; you can allow \
                             it per message, or trust a sender to always load it."
                        ),

                        #[name = "gravatar_row"]
                        adw::SwitchRow {
                            set_title: "Use Gravatar when a contact has no photo",
                            set_subtitle: "Local GNOME Contacts photos are always preferred. \
                                           Gravatar sends a hash of the sender's email.",
                            connect_active_notify[sender] => move |row| {
                                sender.input(PrefInput::ToggleGravatar(row.is_active()));
                            },
                        },

                        #[name = "add_sender_row"]
                        adw::EntryRow {
                            set_title: "Always allow sender (email address)",
                            set_input_purpose: gtk::InputPurpose::Email,
                            set_show_apply_button: true,
                            connect_apply[sender] => move |row| {
                                sender.input(PrefInput::AddSenderText(row.text().to_string()));
                                row.set_text("");
                            },
                        },
                    },

                    add = &adw::PreferencesGroup {
                        set_title: "Allowed Senders",
                        set_description: Some(
                            "Messages from these senders load remote content automatically."
                        ),

                        #[local_ref]
                        senders_box -> gtk::ListBox {
                            add_css_class: "boxed-list",
                            set_selection_mode: gtk::SelectionMode::None,
                        },
                    },

                    add = &adw::PreferencesGroup {
                        set_title: "Blacklist",
                        set_description: Some(
                            "Incoming mail from these senders is deleted automatically \
                             (moved to Trash). Enter an email address, or a whole domain \
                             like \"example.com\" to block every sender there."
                        ),

                        #[name = "add_blacklist_row"]
                        adw::EntryRow {
                            set_title: "Block address or domain",
                            set_show_apply_button: true,
                            connect_apply[sender] => move |row| {
                                sender.input(PrefInput::AddBlacklistText(row.text().to_string()));
                                row.set_text("");
                            },
                        },

                        #[local_ref]
                        blacklist_box -> gtk::ListBox {
                            add_css_class: "boxed-list",
                            set_selection_mode: gtk::SelectionMode::None,
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
        let senders = FactoryVecDeque::builder()
            .launch(gtk::ListBox::new())
            .forward(sender.input_sender(), |out| match out {
                SenderRowOutput::Remove(addr) => PrefInput::RemoveSenderRow(addr),
            });
        let blacklist = FactoryVecDeque::builder()
            .launch(gtk::ListBox::new())
            .forward(sender.input_sender(), |out| match out {
                SenderRowOutput::Remove(addr) => PrefInput::RemoveBlacklistRow(addr),
            });

        let mut model = Preferences {
            senders,
            sender_addrs: Vec::new(),
            blacklist,
            blacklist_addrs: Vec::new(),
        };

        {
            let mut guard = model.senders.guard();
            for addr in &init.allowed_senders {
                model.sender_addrs.push(addr.clone());
                guard.push_back(addr.clone());
            }
        }
        {
            let mut guard = model.blacklist.guard();
            for addr in &init.blacklist {
                model.blacklist_addrs.push(addr.clone());
                guard.push_back(addr.clone());
            }
        }

        let senders_box = model.senders.widget();
        let blacklist_box = model.blacklist.widget();
        let widgets = view_output!();
        widgets.gravatar_row.set_active(init.gravatar);

        // Mail-check interval combo.
        let labels: Vec<&str> = FETCH_INTERVALS.iter().map(|(l, _)| *l).collect();
        widgets.fetch_row.set_model(Some(&gtk::StringList::new(&labels)));
        let selected = FETCH_INTERVALS
            .iter()
            .position(|(_, secs)| *secs == init.fetch_interval_secs)
            .unwrap_or(0);
        widgets.fetch_row.set_selected(selected as u32);
        widgets.push_row.set_active(init.push);
        widgets.notifications_row.set_active(init.notifications);
        widgets.threading_row.set_active(init.threading);
        widgets.threads_expanded_row.set_active(init.threads_expanded);

        // Message-content appearance combo.
        let theme_labels: Vec<&str> = MESSAGE_THEMES.iter().map(|(l, _)| *l).collect();
        widgets
            .message_theme_row
            .set_model(Some(&gtk::StringList::new(&theme_labels)));
        let theme_sel = MESSAGE_THEMES
            .iter()
            .position(|(_, t)| *t == init.message_theme)
            .unwrap_or(0);
        widgets.message_theme_row.set_selected(theme_sel as u32);

        // Hover-palette delay spinner (0–3000ms, step 50).
        // Actions Palette timeout: 1–30 seconds.
        let adj = gtk::Adjustment::new(init.palette_collapse_secs as f64, 1.0, 30.0, 1.0, 5.0, 0.0);
        widgets.palette_collapse_row.set_adjustment(Some(&adj));

        ComponentParts { model, widgets }
    }

    fn update(&mut self, message: Self::Input, sender: ComponentSender<Self>, _root: &Self::Root) {
        match message {
            PrefInput::ToggleGravatar(on) => {
                let _ = sender.output(PrefOutput::SetGravatar(on));
            }
            PrefInput::ToggleThreading(on) => {
                let _ = sender.output(PrefOutput::SetThreading(on));
            }
            PrefInput::ToggleThreadsExpanded(on) => {
                let _ = sender.output(PrefOutput::SetThreadsExpanded(on));
            }
            PrefInput::ChangeFetchInterval(index) => {
                let secs = FETCH_INTERVALS
                    .get(index as usize)
                    .map(|(_, s)| *s)
                    .unwrap_or(0);
                let _ = sender.output(PrefOutput::SetFetchInterval(secs));
            }
            PrefInput::TogglePush(on) => {
                let _ = sender.output(PrefOutput::SetPush(on));
            }
            PrefInput::ToggleNotifications(on) => {
                let _ = sender.output(PrefOutput::SetNotifications(on));
            }
            PrefInput::ChangePaletteCollapse(secs) => {
                let _ = sender.output(PrefOutput::SetPaletteCollapse(secs));
            }
            PrefInput::ChangeMessageTheme(index) => {
                let theme = MESSAGE_THEMES
                    .get(index as usize)
                    .map(|(_, t)| *t)
                    .unwrap_or_default();
                let _ = sender.output(PrefOutput::SetMessageTheme(theme));
            }
            PrefInput::AddSenderText(text) => {
                let addr = text.trim().to_lowercase();
                if !addr.is_empty() && !self.sender_addrs.contains(&addr) {
                    self.sender_addrs.push(addr.clone());
                    self.senders.guard().push_back(addr.clone());
                    let _ = sender.output(PrefOutput::AddSender(addr));
                }
            }

            PrefInput::RemoveSenderRow(addr) => {
                if let Some(pos) = self.sender_addrs.iter().position(|s| *s == addr) {
                    self.sender_addrs.remove(pos);
                    self.senders.guard().remove(pos);
                    let _ = sender.output(PrefOutput::RemoveSender(addr));
                }
            }

            PrefInput::AddBlacklistText(text) => {
                let addr = text.trim().to_lowercase();
                if !addr.is_empty() && !self.blacklist_addrs.contains(&addr) {
                    self.blacklist_addrs.push(addr.clone());
                    self.blacklist.guard().push_back(addr.clone());
                    let _ = sender.output(PrefOutput::AddBlacklist(addr));
                }
            }

            PrefInput::RemoveBlacklistRow(addr) => {
                if let Some(pos) = self.blacklist_addrs.iter().position(|s| *s == addr) {
                    self.blacklist_addrs.remove(pos);
                    self.blacklist.guard().remove(pos);
                    let _ = sender.output(PrefOutput::RemoveBlacklist(addr));
                }
            }
        }
    }
}
