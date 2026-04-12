use crate::gui::config::GuiConfig;
use crate::gui::manager::SyncManager;
use crate::gui::state::{new_shared_state, SharedState, SyncSnapshot};
use crate::gui::tray::{TrayEvent, TrayHandle};

use iced::{
    widget::{button, column, container, progress_bar, row, scrollable, text, text_input, Space},
    window, Alignment, Color, Element, Length, Size, Subscription, Task, Theme,
};

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

type BtnStyle = Box<dyn Fn(&Theme, button::Status) -> button::Style>;

pub fn run(tray: TrayHandle) -> iced::Result {
    let tray_arc = Arc::new(Mutex::new(tray));
    iced::application(
        move || FileSyncGui::init(tray_arc.clone()),
        FileSyncGui::update,
        FileSyncGui::view,
    )
    .title(|_: &FileSyncGui| String::from("ByteHive FileSync"))
    .theme(|s: &FileSyncGui| s.theme())
    .subscription(|s: &FileSyncGui| s.subscription())
    .window(window::Settings {
        size: Size::new(640.0, 520.0),
        min_size: Some(Size::new(520.0, 420.0)),
        resizable: true,
        decorations: true,

        exit_on_close_request: false,
        ..Default::default()
    })
    .run()
}

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
    show_log: bool,
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

#[derive(Debug, Clone)]
enum Message {
    FolderInput(String),
    PickFolder,
    FolderPicked(Option<PathBuf>),
    ServerInput(String),
    TokenInput(String),
    SetupNext,
    SetupBack,
    SetupConnect,

    Tick,
    TogglePause,
    ToggleLog,
    OpenSyncFolder,

    CaptureWindowId(window::Id),
    HideWindow(window::Id),
    ShowWindow,
    Quit,
}

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
                    show_log: false,
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

    fn theme(&self) -> Theme {
        Theme::Light
    }

    fn update(&mut self, msg: Message) -> Task<Message> {
        match msg {
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
                        show_log: false,
                    });
                }
                Task::none()
            }

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

            Message::ToggleLog => {
                if let Screen::Dashboard(d) = &mut self.screen {
                    d.show_log = !d.show_log;
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

fn view_setup(s: &SetupState) -> Element<'_, Message> {
    let step_num = match s.step {
        SetupStep::Folder => 1u8,
        SetupStep::Server => 2,
        SetupStep::Review => 3,
    };

    let header = column![
        text("ByteHive FileSync")
            .size(28)
            .color(Color::from_rgb8(0x1E, 0x29, 0x3B)),
        vspace(4),
        text(format!("Setup  —  Step {step_num} of 3"))
            .size(14)
            .color(Color::from_rgb8(0x64, 0x74, 0x8B)),
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
            background: Some(Color::from_rgb8(0xF8, 0xFA, 0xFC).into()),
            ..Default::default()
        })
        .into()
}

fn step_dots(active: u8) -> Element<'static, Message> {
    let dots: Vec<Element<Message>> = (1u8..=3)
        .map(|i| {
            let colour = if i <= active {
                Color::from_rgb8(0x3B, 0x82, 0xF6)
            } else {
                Color::from_rgb8(0xCB, 0xD5, 0xE1)
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
            .style(style_secondary_btn()),
    ]
    .align_y(Alignment::Center);

    let mut inner = column![
        text("Sync Folder")
            .size(18)
            .color(Color::from_rgb8(0x1E, 0x29, 0x3B)),
        vspace(8),
        text("Choose the local folder to synchronise with the server.")
            .size(13)
            .color(Color::from_rgb8(0x64, 0x74, 0x8B)),
        vspace(20),
        text("Folder path")
            .size(13)
            .color(Color::from_rgb8(0x64, 0x74, 0x8B)),
        vspace(6),
        input_row,
    ]
    .spacing(0);

    if let Some(e) = &s.error {
        inner = inner.push(vspace(10)).push(
            text(e.as_str())
                .size(13)
                .color(Color::from_rgb8(0xEF, 0x44, 0x44)),
        );
    }
    inner = inner.push(vspace(24)).push(row![
        hspace_fill(),
        button("Next →")
            .on_press(Message::SetupNext)
            .padding([10, 24])
            .style(style_primary_btn()),
    ]);
    card(inner.into())
}

fn view_setup_server(s: &SetupState) -> Element<'_, Message> {
    let mut inner = column![
        text("Server Connection")
            .size(18)
            .color(Color::from_rgb8(0x1E, 0x29, 0x3B)),
        vspace(8),
        text("Enter the server address and your authentication token.")
            .size(13)
            .color(Color::from_rgb8(0x64, 0x74, 0x8B)),
        vspace(20),
        text("Server address  (host:port)")
            .size(13)
            .color(Color::from_rgb8(0x64, 0x74, 0x8B)),
        vspace(6),
        text_input("192.168.1.10:7878", &s.server_input)
            .on_input(Message::ServerInput)
            .on_submit(Message::SetupNext)
            .padding(10)
            .size(14),
        vspace(14),
        text("Auth token")
            .size(13)
            .color(Color::from_rgb8(0x64, 0x74, 0x8B)),
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
                .color(Color::from_rgb8(0xEF, 0x44, 0x44)),
        );
    }
    inner = inner.push(vspace(24)).push(row![
        button("← Back")
            .on_press(Message::SetupBack)
            .padding([10, 18])
            .style(style_secondary_btn()),
        hspace_fill(),
        button("Next →")
            .on_press(Message::SetupNext)
            .padding([10, 24])
            .style(style_primary_btn()),
    ]);
    card(inner.into())
}

fn view_setup_review(s: &SetupState) -> Element<'_, Message> {
    let mut inner = column![
        text("Review & Connect")
            .size(18)
            .color(Color::from_rgb8(0x1E, 0x29, 0x3B)),
        vspace(8),
        text("Confirm your settings before connecting.")
            .size(13)
            .color(Color::from_rgb8(0x64, 0x74, 0x8B)),
        vspace(20),
        review_row("Sync folder", s.folder_input.clone()),
        vspace(10),
        review_row("Server", s.server_input.clone()),
        vspace(10),
        review_row("Auth token", "●●●●●●●●".to_owned()),
        vspace(16),
        thin_rule(),
    ]
    .spacing(0);

    if let Some(e) = &s.error {
        inner = inner.push(vspace(10)).push(
            text(e.as_str())
                .size(13)
                .color(Color::from_rgb8(0xEF, 0x44, 0x44)),
        );
    }
    inner = inner.push(vspace(24)).push(row![
        button("← Back")
            .on_press(Message::SetupBack)
            .padding([10, 18])
            .style(style_secondary_btn()),
        hspace_fill(),
        button("Connect")
            .on_press(Message::SetupConnect)
            .padding([10, 28])
            .style(style_primary_btn()),
    ]);
    card(inner.into())
}

fn view_dashboard(d: &DashboardState) -> Element<'_, Message> {
    let snap = &d.snapshot;
    let paused = d.manager.is_paused();

    let [sr, sg, sb, _] = snap.status.colour();
    let status_dot = container(Space::new().width(0).height(0))
        .width(Length::Fixed(10.0))
        .height(Length::Fixed(10.0))
        .style(move |_: &Theme| container::Style {
            background: Some(Color::from_rgb8(sr, sg, sb).into()),
            border: iced::Border {
                radius: 5.0.into(),
                ..Default::default()
            },
            ..Default::default()
        });

    let pause_style: BtnStyle = if paused {
        style_primary_btn()
    } else {
        style_secondary_btn()
    };
    let status_row = row![
        status_dot,
        hspace(8),
        text(snap.status.label()).size(15),
        hspace_fill(),
        button(if paused { "▶  Resume" } else { "⏸  Pause" })
            .on_press(Message::TogglePause)
            .padding([8, 18])
            .style(pause_style),
    ]
    .align_y(Alignment::Center);

    let stats_row = row![
        stat_card("Files", snap.file_count.to_string()),
        hspace(12),
        stat_card("Dirs", snap.dir_count.to_string()),
        hspace(12),
        stat_card("Total size", fmt_bytes(snap.total_bytes)),
    ]
    .width(Length::Fill);

    let transfer_row = row![
        stat_card("Sent", fmt_bytes(snap.bytes_sent)),
        hspace(12),
        stat_card("Received", fmt_bytes(snap.bytes_received)),
        hspace(12),
        stat_card(
            "Last active",
            snap.last_connected
                .map(|t| format!("{} s ago", t.elapsed().as_secs()))
                .unwrap_or_else(|| "—".into()),
        ),
    ]
    .width(Length::Fill);

    use crate::gui::state::ConnectionStatus;
    let is_syncing = matches!(snap.status, ConnectionStatus::InitialSync);
    let progress_section: Option<Element<Message>> = if is_syncing {
        let transferred = snap.bytes_sent + snap.bytes_received;
        let (pct, label) = if snap.transfer_total > 0 {
            let pct = (transferred as f32 / snap.transfer_total as f32).min(1.0);
            let label = format!(
                "{} / {}  ({:.0}%)",
                fmt_bytes(transferred),
                fmt_bytes(snap.transfer_total),
                pct * 100.0
            );
            (pct, label)
        } else {
            (0.5_f32, format!("{} transferred…", fmt_bytes(transferred)))
        };

        let bar = container(
            column![
                row![
                    text("Syncing…")
                        .size(13)
                        .color(Color::from_rgb8(0x3B, 0x82, 0xF6)),
                    hspace_fill(),
                    text(label.clone())
                        .size(12)
                        .color(Color::from_rgb8(0x64, 0x74, 0x8B)),
                ]
                .align_y(Alignment::Center),
                vspace(6),
                progress_bar(0.0..=1.0, pct).style(|_: &Theme| iced::widget::progress_bar::Style {
                    background: Color::from_rgb8(0xE2, 0xE8, 0xF0).into(),
                    bar: Color::from_rgb8(0x3B, 0x82, 0xF6).into(),
                    border: iced::Border {
                        radius: 3.0.into(),
                        ..Default::default()
                    },
                }),
            ]
            .spacing(0),
        )
        .width(Length::Fill)
        .padding([14, 16])
        .style(|_: &Theme| container::Style {
            background: Some(Color::from_rgb8(0xEF, 0xF6, 0xFF).into()),
            border: iced::Border {
                color: Color::from_rgb8(0x93, 0xC5, 0xFD),
                width: 1.0,
                radius: 8.0.into(),
            },
            ..Default::default()
        });
        Some(bar.into())
    } else {
        None
    };

    let cfg_row = row![
        text(d.config.server_addr.as_str())
            .size(13)
            .color(Color::from_rgb8(0x64, 0x74, 0x8B)),
        hspace(8),
        text("→").size(13).color(Color::from_rgb8(0x94, 0xA3, 0xB8)),
        hspace(8),
        button(text(d.config.sync_root.to_string_lossy().to_string()).size(13))
            .on_press(Message::OpenSyncFolder)
            .padding(0)
            .style(style_link_btn()),
    ]
    .align_y(Alignment::Center);

    let log_label = if d.show_log {
        "Hide log ▲"
    } else {
        "Show log ▼"
    };

    let mut body = column![
        card(status_row.into()),
        vspace(12),
        stats_row,
        vspace(12),
        transfer_row,
        vspace(12),
        card(cfg_row.into()),
        vspace(8),
        row![
            hspace_fill(),
            button(log_label)
                .on_press(Message::ToggleLog)
                .padding([6, 12])
                .style(style_ghost_btn()),
        ],
    ]
    .spacing(0);

    if let Some(progress) = progress_section {
        body = body.push(vspace(12)).push(progress);
    }

    if d.show_log {
        body = body.push(vspace(8)).push(view_log(snap));
    }

    container(scrollable(body.padding([20, 24])))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_: &Theme| container::Style {
            background: Some(Color::from_rgb8(0xF8, 0xFA, 0xFC).into()),
            ..Default::default()
        })
        .into()
}

fn view_log(snap: &SyncSnapshot) -> Element<'_, Message> {
    let entries: Vec<Element<Message>> = snap
        .log
        .entries()
        .iter()
        .rev()
        .map(|line| {
            text(line.as_str())
                .size(12)
                .color(Color::from_rgb8(0x47, 0x55, 0x69))
                .into()
        })
        .collect();

    container(scrollable(column(entries).spacing(4).padding(12)).height(Length::Fixed(160.0)))
        .width(Length::Fill)
        .style(|_: &Theme| container::Style {
            background: Some(Color::from_rgb8(0xF1, 0xF5, 0xF9).into()),
            border: iced::Border {
                color: Color::from_rgb8(0xCB, 0xD5, 0xE1),
                width: 1.0,
                radius: 8.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn card(content: Element<Message>) -> Element<Message> {
    container(content)
        .width(Length::Fill)
        .padding(20)
        .style(|_: &Theme| container::Style {
            background: Some(Color::WHITE.into()),
            border: iced::Border {
                color: Color::from_rgb8(0xE2, 0xE8, 0xF0),
                width: 1.0,
                radius: 10.0.into(),
            },
            shadow: iced::Shadow {
                color: Color::from_rgba(0.0, 0.0, 0.0, 0.04),
                offset: iced::Vector { x: 0.0, y: 2.0 },
                blur_radius: 8.0,
            },
            ..Default::default()
        })
        .into()
}

fn stat_card(label_str: &'static str, value_str: String) -> Element<'static, Message> {
    container(
        column![
            text(value_str)
                .size(20)
                .color(Color::from_rgb8(0x1E, 0x29, 0x3B)),
            vspace(2),
            text(label_str)
                .size(12)
                .color(Color::from_rgb8(0x94, 0xA3, 0xB8)),
        ]
        .spacing(0),
    )
    .width(Length::Fill)
    .padding([14, 16])
    .style(|_: &Theme| container::Style {
        background: Some(Color::WHITE.into()),
        border: iced::Border {
            color: Color::from_rgb8(0xE2, 0xE8, 0xF0),
            width: 1.0,
            radius: 8.0.into(),
        },
        ..Default::default()
    })
    .into()
}

fn review_row(key: &'static str, value: String) -> Element<'static, Message> {
    row![
        text(key)
            .size(13)
            .width(Length::Fixed(110.0))
            .color(Color::from_rgb8(0x94, 0xA3, 0xB8)),
        text(value)
            .size(13)
            .color(Color::from_rgb8(0x1E, 0x29, 0x3B)),
    ]
    .align_y(Alignment::Center)
    .into()
}

fn thin_rule<'a>() -> Element<'a, Message> {
    container(Space::new().width(Length::Fill).height(0))
        .width(Length::Fill)
        .height(Length::Fixed(1.0))
        .style(|_: &Theme| container::Style {
            background: Some(Color::from_rgb8(0xE2, 0xE8, 0xF0).into()),
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

fn style_primary_btn() -> BtnStyle {
    Box::new(|_, status: button::Status| {
        let bg = if matches!(status, button::Status::Hovered | button::Status::Pressed) {
            Color::from_rgb8(0x25, 0x63, 0xEB)
        } else {
            Color::from_rgb8(0x3B, 0x82, 0xF6)
        };
        button::Style {
            background: Some(bg.into()),
            text_color: Color::WHITE,
            border: iced::Border {
                radius: 8.0.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    })
}

fn style_secondary_btn() -> BtnStyle {
    Box::new(|_, status: button::Status| {
        let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: Some(
                if hover {
                    Color::from_rgb8(0xE2, 0xE8, 0xF0)
                } else {
                    Color::from_rgb8(0xF1, 0xF5, 0xF9)
                }
                .into(),
            ),
            text_color: Color::from_rgb8(0x47, 0x55, 0x69),
            border: iced::Border {
                color: if hover {
                    Color::from_rgb8(0x94, 0xA3, 0xB8)
                } else {
                    Color::from_rgb8(0xCB, 0xD5, 0xE1)
                },
                width: 1.0,
                radius: 8.0.into(),
            },
            ..Default::default()
        }
    })
}

fn style_link_btn() -> BtnStyle {
    Box::new(|_, status: button::Status| button::Style {
        background: None,
        text_color: if matches!(status, button::Status::Hovered | button::Status::Pressed) {
            Color::from_rgb8(0x25, 0x63, 0xEB)
        } else {
            Color::from_rgb8(0x3B, 0x82, 0xF6)
        },
        ..Default::default()
    })
}

fn style_ghost_btn() -> BtnStyle {
    Box::new(|_, status: button::Status| {
        let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: if hover {
                Some(Color::from_rgb8(0xF1, 0xF5, 0xF9).into())
            } else {
                None
            },
            text_color: Color::from_rgb8(0x64, 0x74, 0x8B),
            border: iced::Border {
                radius: 6.0.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    })
}

fn fmt_bytes(b: u64) -> String {
    match b {
        0..1_024 => format!("{b} B"),
        1_024..1_048_576 => format!("{:.1} KiB", b as f64 / 1_024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MiB", b as f64 / 1_048_576.0),
        _ => format!("{:.2} GiB", b as f64 / 1_073_741_824.0),
    }
}
