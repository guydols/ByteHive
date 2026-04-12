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
    const SIZE: usize = 22;
    let mut px = vec![0u8; SIZE * SIZE * 4];

    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - 10.5;
            let dy = y as f32 - 10.5;
            let r = (dx * dx + dy * dy).sqrt();

            let i = (y * SIZE + x) * 4;

            if r > 10.2 {
                continue;
            }

            px[i] = 0x3B;
            px[i + 1] = 0x82;
            px[i + 2] = 0xF6;
            px[i + 3] = 255;

            let angle = dy.atan2(dx);
            let in_arc =
                !(angle > std::f32::consts::FRAC_PI_4 && angle < std::f32::consts::FRAC_PI_2 * 1.5);
            if r > 5.5 && r < 8.0 && in_arc {
                px[i] = 255;
                px[i + 1] = 255;
                px[i + 2] = 255;
            }

            if dx > 0.5 && dy > -2.0 && dy < 2.0 && r < 8.0 && r > 4.0 {
                px[i] = 255;
                px[i + 1] = 255;
                px[i + 2] = 255;
            }
        }
    }
    Icon::from_rgba(px, SIZE as u32, SIZE as u32).expect("create tray icon pixels")
}
