//! Core application wiring — window settings, update loop, top-level view.
//!
//! Uses the ByteHive dark theme, header bar, status panel, stats/conflicts
//! main content area, and collapsible log panel from the component modules.
//! All real functionality (SyncManager, tray, config, etc.) is preserved.

use crate::gui::config::GuiConfig;
use crate::gui::manager::SyncManager;
use crate::gui::state::{new_shared_state, SharedState, SyncSnapshot};
use crate::gui::tray::{TrayEvent, TrayHandle};
use crate::gui::{components, theme};

use iced::{
    widget::{button, column, container, row, text, text_input, Space},
    window, Alignment, Background, Element, Length, Size, Subscription, Task, Theme,
};

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// ─── Entry point ──────────────────────────────────────────────────────────────

pub fn run(tray: TrayHandle) -> iced::Result {
    let tray_arc = Arc::new(Mutex::new(tray));
    iced::application(
        move || FileSyncGui::init(tray_arc.clone()),
        FileSyncGui::update,
        FileSyncGui::view,
    )
    .title(|_: &FileSyncGui| String::from("ByteHive FileSync"))
    .theme(|_: &FileSyncGui| theme::bytehive_theme())
    .subscription(|s: &FileSyncGui| s.subscription())
    .window(window::Settings {
        size: Size::new(1080.0, 760.0),
        min_size: Some(Size::new(800.0, 600.0)),
        resizable: true,
        decorations: true,
        exit_on_close_request: false,
        ..Default::default()
    })
    .run()
}

// ─── State ────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct SetupState {
    step: SetupStep,
    folder_input: String,
    server_input: String,
    token_input: String,
    error: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq)]
enum SetupStep {
    #[default]
    Folder,
    Server,
    Review,
}

struct DashboardState {
    config: GuiConfig,
    manager: Arc<SyncManager>,
    state: SharedState,
    snapshot: SyncSnapshot,
    log_expanded: bool,
}

enum Screen {
    Setup(SetupState),
    Dashboard(DashboardState),
}

struct FileSyncGui {
    screen: Screen,
    tray: Arc<Mutex<TrayHandle>>,
    window_id: Option<window::Id>,
}

// ─── Messages ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Message {
    // Setup
    FolderInput(String),
    PickFolder,
    FolderPicked(Option<PathBuf>),
    ServerInput(String),
    TokenInput(String),
    SetupNext,
    SetupBack,
    SetupConnect,

    // Dashboard
    Tick,
    TogglePause,
    ToggleLogPanel,
    OpenSyncFolder,

    // Conflicts
    OpenConflictFolder(usize),
    DismissConflict(usize),
    KeepLocalVersion(usize),
    KeepRemoteVersion(usize),

    // Window management
    CaptureWindowId(window::Id),
    HideWindow(window::Id),
    ShowWindow,
    Quit,
}

// ─── Application logic ───────────────────────────────────────────────────────

impl FileSyncGui {
    fn init(tray: Arc<Mutex<TrayHandle>>) -> (Self, Task<Message>) {
        let screen = match GuiConfig::load() {
            Some(cfg) if cfg.is_complete() => {
                let shared = new_shared_state();
                let manager = Arc::new(SyncManager::new(shared.clone()));
                let snapshot = shared.read().clone();
                manager.start(cfg.clone());
                Screen::Dashboard(DashboardState {
                    config: cfg,
                    manager,
                    state: shared,
                    snapshot,
                    log_expanded: false,
                })
            }
            _ => Screen::Setup(SetupState::default()),
        };
        (
            FileSyncGui {
                screen,
                tray,
                window_id: None,
            },
            Task::none(),
        )
    }

    fn update(&mut self, msg: Message) -> Task<Message> {
        match msg {
            // ── Setup messages ────────────────────────────────────────────────
            Message::FolderInput(v) => {
                if let Screen::Setup(s) = &mut self.screen {
                    s.folder_input = v;
                }
                Task::none()
            }

            Message::PickFolder => Task::perform(
                async {
                    rfd::AsyncFileDialog::new()
                        .set_title("Select Sync Folder")
                        .pick_folder()
                        .await
                        .map(|h| h.path().to_path_buf())
                },
                Message::FolderPicked,
            ),

            Message::FolderPicked(opt) => {
                if let Screen::Setup(s) = &mut self.screen {
                    if let Some(p) = opt {
                        s.folder_input = p.to_string_lossy().into_owned();
                    }
                }
                Task::none()
            }

            Message::ServerInput(v) => {
                if let Screen::Setup(s) = &mut self.screen {
                    s.server_input = v;
                }
                Task::none()
            }

            Message::TokenInput(v) => {
                if let Screen::Setup(s) = &mut self.screen {
                    s.token_input = v;
                }
                Task::none()
            }

            Message::SetupNext => {
                if let Screen::Setup(s) = &mut self.screen {
                    match s.step {
                        SetupStep::Folder => {
                            if s.folder_input.trim().is_empty() {
                                s.error = Some("Please select a sync folder.".into());
                            } else {
                                s.error = None;
                                s.step = SetupStep::Server;
                            }
                        }
                        SetupStep::Server => {
                            if s.server_input.trim().is_empty() {
                                s.error = Some("Please enter the server address.".into());
                            } else if s.token_input.trim().is_empty() {
                                s.error = Some("Please enter your auth token.".into());
                            } else {
                                s.error = None;
                                s.step = SetupStep::Review;
                            }
                        }
                        SetupStep::Review => {}
                    }
                }
                Task::none()
            }

            Message::SetupBack => {
                if let Screen::Setup(s) = &mut self.screen {
                    match s.step {
                        SetupStep::Server => s.step = SetupStep::Folder,
                        SetupStep::Review => s.step = SetupStep::Server,
                        _ => {}
                    }
                    s.error = None;
                }
                Task::none()
            }

            Message::SetupConnect => {
                if let Screen::Setup(s) = &mut self.screen {
                    let cfg = GuiConfig {
                        sync_root: PathBuf::from(s.folder_input.trim()),
                        server_addr: s.server_input.trim().to_string(),
                        auth_token: s.token_input.trim().to_string(),
                        ..Default::default()
                    };
                    if let Err(e) = cfg.save() {
                        s.error = Some(format!("Failed to save config: {e}"));
                        return Task::none();
                    }
                    let shared = new_shared_state();
                    let manager = Arc::new(SyncManager::new(shared.clone()));
                    let snapshot = shared.read().clone();
                    manager.start(cfg.clone());
                    self.screen = Screen::Dashboard(DashboardState {
                        config: cfg,
                        manager,
                        state: shared,
                        snapshot,
                        log_expanded: false,
                    });
                }
                Task::none()
            }

            // ── Dashboard messages ────────────────────────────────────────────
            Message::Tick => {
                if let Ok(handle) = self.tray.lock() {
                    while let Ok(ev) = handle.events.try_recv() {
                        match ev {
                            TrayEvent::Show => return self.do_show_window(),
                            TrayEvent::Quit => std::process::exit(0),
                        }
                    }
                }

                if let Screen::Dashboard(d) = &mut self.screen {
                    d.snapshot = d.state.read().clone();
                }
                Task::none()
            }

            Message::TogglePause => {
                if let Screen::Dashboard(d) = &mut self.screen {
                    if d.manager.is_paused() {
                        d.manager.resume();
                    } else {
                        d.manager.pause();
                    }
                }
                Task::none()
            }

            Message::ToggleLogPanel => {
                if let Screen::Dashboard(d) = &mut self.screen {
                    d.log_expanded = !d.log_expanded;
                }
                Task::none()
            }

            Message::OpenSyncFolder => {
                if let Screen::Dashboard(d) = &self.screen {
                    let _ = std::process::Command::new("xdg-open")
                        .arg(&d.config.sync_root)
                        .spawn();
                }
                Task::none()
            }

            // ── Conflict messages ─────────────────────────────────────────────
            Message::OpenConflictFolder(id) => {
                if let Screen::Dashboard(d) = &self.screen {
                    if let Some(conflict) = d.snapshot.conflicts.iter().find(|c| c.id == id) {
                        let _ = std::process::Command::new("xdg-open")
                            .arg(&conflict.folder_path)
                            .spawn();
                    }
                }
                Task::none()
            }

            Message::DismissConflict(id) => {
                if let Screen::Dashboard(d) = &mut self.screen {
                    {
                        let mut s = d.state.write();
                        s.conflicts.retain(|c| c.id != id);
                        s.log_event(format!("Conflict #{id} dismissed by user"));
                    }
                    d.snapshot = d.state.read().clone();
                }
                Task::none()
            }

            Message::KeepLocalVersion(id) => {
                if let Screen::Dashboard(d) = &mut self.screen {
                    {
                        let mut s = d.state.write();
                        s.conflicts.retain(|c| c.id != id);
                        s.log_event(format!("Conflict #{id} resolved: kept local version"));
                    }
                    d.snapshot = d.state.read().clone();
                }
                Task::none()
            }

            Message::KeepRemoteVersion(id) => {
                if let Screen::Dashboard(d) = &mut self.screen {
                    {
                        let mut s = d.state.write();
                        s.conflicts.retain(|c| c.id != id);
                        s.log_event(format!("Conflict #{id} resolved: kept remote version"));
                    }
                    d.snapshot = d.state.read().clone();
                }
                Task::none()
            }

            // ── Window management ─────────────────────────────────────────────
            Message::CaptureWindowId(id) => {
                if self.window_id.is_none() {
                    self.window_id = Some(id);
                }
                Task::none()
            }

            Message::HideWindow(id) => {
                self.window_id = Some(id);
                window::set_mode(id, window::Mode::Hidden)
            }

            Message::ShowWindow => self.do_show_window(),

            Message::Quit => std::process::exit(0),
        }
    }

    fn do_show_window(&self) -> Task<Message> {
        if let Some(id) = self.window_id {
            Task::batch(vec![
                window::set_mode(id, window::Mode::Windowed),
                window::gain_focus(id),
            ])
        } else {
            Task::none()
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let tick = iced::time::every(std::time::Duration::from_secs(1)).map(|_| Message::Tick);

        let window_events = iced::event::listen_with(|event, _status, id| match event {
            iced::Event::Window(window::Event::CloseRequested) => Some(Message::HideWindow(id)),
            iced::Event::Window(_) => Some(Message::CaptureWindowId(id)),
            _ => None,
        });

        Subscription::batch(vec![tick, window_events])
    }

    fn view(&self) -> Element<'_, Message> {
        match &self.screen {
            Screen::Setup(s) => view_setup(s),
            Screen::Dashboard(d) => view_dashboard(d),
        }
    }
}

// ─── Setup views ──────────────────────────────────────────────────────────────

fn view_setup(s: &SetupState) -> Element<'_, Message> {
    let step_num = match s.step {
        SetupStep::Folder => 1u8,
        SetupStep::Server => 2,
        SetupStep::Review => 3,
    };

    let header = column![
        text("ByteHive FileSync")
            .size(28)
            .style(|_: &Theme| iced::widget::text::Style {
                color: Some(theme::TEXT_PRIMARY),
            }),
        vspace(4),
        text(format!("Setup  \u{2014}  Step {step_num} of 3"))
            .size(14)
            .style(theme::secondary),
        vspace(16),
        step_dots(step_num),
    ]
    .spacing(0);

    let body: Element<Message> = match s.step {
        SetupStep::Folder => view_setup_folder(s),
        SetupStep::Server => view_setup_server(s),
        SetupStep::Review => view_setup_review(s),
    };

    let content = column![header, vspace(24), body].spacing(0).padding(40);

    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(theme::BG_PRIMARY)),
            ..Default::default()
        })
        .into()
}

fn step_dots(active: u8) -> Element<'static, Message> {
    let dots: Vec<Element<Message>> = (1u8..=3)
        .map(|i| {
            let colour = if i <= active {
                theme::AMBER
            } else {
                theme::TEXT_MUTED
            };
            container(Space::new().width(0).height(0))
                .width(Length::Fixed(10.0))
                .height(Length::Fixed(10.0))
                .style(move |_: &Theme| container::Style {
                    background: Some(colour.into()),
                    border: iced::Border {
                        radius: 5.0.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .into()
        })
        .collect();
    row(dots).spacing(8).into()
}

fn view_setup_folder(s: &SetupState) -> Element<'_, Message> {
    let input_row = row![
        text_input("/home/user/syncfolder", &s.folder_input)
            .on_input(Message::FolderInput)
            .padding(10)
            .size(14)
            .width(Length::Fill),
        hspace(8),
        button("Browse")
            .on_press(Message::PickFolder)
            .padding([10, 18])
            .style(theme::btn_ghost),
    ]
    .align_y(Alignment::Center);

    let mut inner = column![
        text("Sync Folder")
            .size(18)
            .style(|_: &Theme| iced::widget::text::Style {
                color: Some(theme::TEXT_PRIMARY),
            }),
        vspace(8),
        text("Choose the local folder to synchronise with the server.")
            .size(13)
            .style(theme::secondary),
        vspace(20),
        text("Folder path")
            .size(13)
            .style(theme::secondary),
        vspace(6),
        input_row,
    ]
    .spacing(0);

    if let Some(e) = &s.error {
        inner = inner.push(vspace(10)).push(
            text(e.as_str())
                .size(13)
                .style(theme::red_text),
        );
    }
    inner = inner.push(vspace(24)).push(row![
        hspace_fill(),
        button("Next \u{2192}")
            .on_press(Message::SetupNext)
            .padding([10, 24])
            .style(theme::btn_primary),
    ]);
    setup_card(inner.into())
}

fn view_setup_server(s: &SetupState) -> Element<'_, Message> {
    let mut inner = column![
        text("Server Connection")
            .size(18)
            .style(|_: &Theme| iced::widget::text::Style {
                color: Some(theme::TEXT_PRIMARY),
            }),
        vspace(8),
        text("Enter the server address and your authentication token.")
            .size(13)
            .style(theme::secondary),
        vspace(20),
        text("Server address  (host:port)")
            .size(13)
            .style(theme::secondary),
        vspace(6),
        text_input("192.168.1.10:7878", &s.server_input)
            .on_input(Message::ServerInput)
            .on_submit(Message::SetupNext)
            .padding(10)
            .size(14),
        vspace(14),
        text("Auth token")
            .size(13)
            .style(theme::secondary),
        vspace(6),
        text_input("your-secret-token", &s.token_input)
            .on_input(Message::TokenInput)
            .on_submit(Message::SetupNext)
            .secure(true)
            .padding(10)
            .size(14),
    ]
    .spacing(0);

    if let Some(e) = &s.error {
        inner = inner.push(vspace(10)).push(
            text(e.as_str())
                .size(13)
                .style(theme::red_text),
        );
    }
    inner = inner.push(vspace(24)).push(row![
        button("\u{2190} Back")
            .on_press(Message::SetupBack)
            .padding([10, 18])
            .style(theme::btn_ghost),
        hspace_fill(),
        button("Next \u{2192}")
            .on_press(Message::SetupNext)
            .padding([10, 24])
            .style(theme::btn_primary),
    ]);
    setup_card(inner.into())
}

fn view_setup_review(s: &SetupState) -> Element<'_, Message> {
    let mut inner = column![
        text("Review & Connect")
            .size(18)
            .style(|_: &Theme| iced::widget::text::Style {
                color: Some(theme::TEXT_PRIMARY),
            }),
        vspace(8),
        text("Confirm your settings before connecting.")
            .size(13)
            .style(theme::secondary),
        vspace(20),
        review_row("Sync folder", s.folder_input.clone()),
        vspace(10),
        review_row("Server", s.server_input.clone()),
        vspace(10),
        review_row("Auth token", "\u{25CF}\u{25CF}\u{25CF}\u{25CF}\u{25CF}\u{25CF}\u{25CF}\u{25CF}".to_owned()),
        vspace(16),
        thin_rule(),
    ]
    .spacing(0);

    if let Some(e) = &s.error {
        inner = inner.push(vspace(10)).push(
            text(e.as_str())
                .size(13)
                .style(theme::red_text),
        );
    }
    inner = inner.push(vspace(24)).push(row![
        button("\u{2190} Back")
            .on_press(Message::SetupBack)
            .padding([10, 18])
            .style(theme::btn_ghost),
        hspace_fill(),
        button("Connect")
            .on_press(Message::SetupConnect)
            .padding([10, 28])
            .style(theme::btn_primary),
    ]);
    setup_card(inner.into())
}

// ─── Dashboard view ───────────────────────────────────────────────────────────

fn view_dashboard(d: &DashboardState) -> Element<'_, Message> {
    let snap = &d.snapshot;
    let is_paused = d.manager.is_paused();

    let header = components::header::view(&snap.status);
    let status_panel = components::status_panel::view(snap, is_paused);
    let main_area = main_content(snap, &d.config);
    let log_panel = components::log_panel::view(&snap.log, d.log_expanded);

    let root = column![
        header,
        divider(),
        status_panel,
        divider(),
        main_area,
        log_panel,
    ]
    .spacing(0);

    container(root)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_| {
            use iced::widget::container;
            container::Style {
                background: Some(Background::Color(theme::BG_PRIMARY)),
                ..Default::default()
            }
        })
        .into()
}

/// Main content area: stats when no conflicts, stats + conflicts panel side by side.
fn main_content<'a>(snap: &'a SyncSnapshot, config: &'a GuiConfig) -> Element<'a, Message> {
    let server_addr = config.server_addr.clone();
    let sync_root = config.sync_root.to_string_lossy().to_string();
    let stats = components::stats_panel::view(snap, server_addr, sync_root);

    if snap.conflicts.is_empty() {
        container(stats)
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(16)
            .into()
    } else {
        let conflicts = components::conflicts::view(&snap.conflicts);

        row![
            container(stats)
                .width(Length::FillPortion(3))
                .height(Length::Fill)
                .padding(16),
            container(conflicts)
                .width(Length::FillPortion(2))
                .height(Length::Fill)
                .padding(16),
        ]
        .spacing(0)
        .height(Length::Fill)
        .into()
    }
}

// ─── Setup helpers ────────────────────────────────────────────────────────────

fn setup_card(content: Element<Message>) -> Element<Message> {
    container(content)
        .width(Length::Fill)
        .padding(20)
        .style(theme::panel)
        .into()
}

fn review_row(key: &'static str, value: String) -> Element<'static, Message> {
    row![
        text(key)
            .size(13)
            .width(Length::Fixed(110.0))
            .style(theme::muted),
        text(value)
            .size(13)
            .style(|_: &Theme| iced::widget::text::Style {
                color: Some(theme::TEXT_PRIMARY),
            }),
    ]
    .align_y(Alignment::Center)
    .into()
}

fn thin_rule<'a>() -> Element<'a, Message> {
    container(Space::new().width(Length::Fill).height(0))
        .width(Length::Fill)
        .height(Length::Fixed(1.0))
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(theme::BORDER)),
            ..Default::default()
        })
        .into()
}

fn divider<'a>() -> Element<'a, Message> {
    container(Space::new().width(Length::Fill).height(0))
        .width(Length::Fill)
        .height(Length::Fixed(1.0))
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(theme::BORDER)),
            ..Default::default()
        })
        .into()
}

fn vspace(pixels: u16) -> Space {
    Space::new().height(Length::Fixed(pixels as f32))
}

fn hspace(pixels: u16) -> Space {
    Space::new().width(Length::Fixed(pixels as f32))
}

fn hspace_fill() -> Space {
    Space::new().width(Length::Fill)
}
