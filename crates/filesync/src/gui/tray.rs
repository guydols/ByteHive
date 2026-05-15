use crossbeam_channel::{unbounded, Receiver};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, TrayIconBuilder, TrayIconEvent,
};

#[derive(Debug, Clone)]
pub enum TrayEvent {
    Show,
    Quit,
}

pub struct TrayHandle {
    pub events: Receiver<TrayEvent>,
}

pub fn setup_tray() -> TrayHandle {
    let (tx, rx) = unbounded::<TrayEvent>();

    std::thread::Builder::new()
        .name("tray-gtk".into())
        .spawn(move || {
            if gtk::init().is_err() {
                eprintln!(
                    "[tray] gtk::init() failed — \
                     is libgtk-3 installed and is DISPLAY/WAYLAND_DISPLAY set?"
                );
                return;
            }

            let menu = Menu::new();
            let show_item = MenuItem::new("Show Window", true, None);
            let quit_item = MenuItem::new("Quit", true, None);
            menu.append_items(&[&show_item, &PredefinedMenuItem::separator(), &quit_item])
                .expect("tray menu build");

            let icon = make_icon();

            let _tray = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_icon(icon)
                .with_tooltip("ByteHive FileSync")
                .build()
                .expect("create tray icon");

            let show_id = show_item.id().clone();
            let quit_id = quit_item.id().clone();

            glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
                if let Ok(ev) = TrayIconEvent::receiver().try_recv() {
                    if matches!(ev, TrayIconEvent::Click { .. }) {
                        let _ = tx.send(TrayEvent::Show);
                    }
                }

                if let Ok(ev) = MenuEvent::receiver().try_recv() {
                    if ev.id == show_id {
                        let _ = tx.send(TrayEvent::Show);
                    } else if ev.id == quit_id {
                        let _ = tx.send(TrayEvent::Quit);
                    }
                }

                glib::ControlFlow::Continue
            });

            gtk::main();
        })
        .expect("spawn tray-gtk thread");

    TrayHandle { events: rx }
}

fn make_icon() -> Icon {
    const ICON_PNG: &[u8] = include_bytes!("../../../core/assets/bytehive_icon_32x32.png");
    let img = image::load_from_memory(ICON_PNG)
        .expect("decode tray icon PNG")
        .into_rgba8();
    let (w, h) = img.dimensions();
    Icon::from_rgba(img.into_raw(), w, h).expect("create tray icon")
}
