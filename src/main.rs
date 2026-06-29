mod drivers;
use crate::drivers::commands::ClearImages;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, window_size, Clear, ClearType,
    EnterAlternateScreen, LeaveAlternateScreen, WindowSize,
};
use drivers::commands::{
    DisableMouseCapturePixels, EnableMouseCapturePixels, PointerShape, SetPointerShape,
};
use drivers::graphics::terminal_graphics_test_support;
use keybinds::{KeyInput, Keybinds};

mod threads;
use threads::event::InputEvent;

mod image;
use crate::image::*;

mod viewer;
use crate::viewer::*;

mod globals;
use crate::globals::*;

mod config;
use crate::config::*;

/* Lightweight debug logging, enabled by setting MEOWPDF_DEBUG=1. Appends to
 * /tmp/meowpdf-debug.log. Used to diagnose the draw/transmit handshake over SSH. */
fn dlog(msg: &str) {
    use std::io::Write as _;
    if std::env::var("MEOWPDF_DEBUG").is_err() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/meowpdf-debug.log")
    {
        let _ = writeln!(f, "{}", msg);
    }
}

use std::hash::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::sync::RwLock;
use std::time::{Duration, SystemTime};

/* Tracks the last executed times of signals for throattling */
struct LastExecuted {
    pub load: SystemTime,
    pub alpha: SystemTime,
    pub inverse: SystemTime,
}

fn main() {
    /* ============================= Check the argument ============================= */
    let arg = std::env::args().nth(1).unwrap_or("".to_owned());
    match arg.as_str() {
        "-h" | "--help" | "" => {
            println!("{}", HELP_MSG);
            return;
        }
        "-v" | "--version" => {
            println!("meowpdf v{} ({})", VERSION, RELEASED);
            return;
        }
        _ => (),
    }

    /* ============================= Uncook the terminal ============================= */
    enable_raw_mode().expect("Could not cook the terminal");
    execute!(io::stdout(), EnterAlternateScreen).expect("Could not enter alt mode");
    execute!(io::stdout(), Hide).expect("Could not hide cursor");
    execute!(io::stdout(), Clear(ClearType::All)).expect("Could not clear terminal");
    execute!(io::stdout(), EnableMouseCapturePixels)
        .expect("Could not enable mouse capture");

    /* ========================== Cook the terminal on panic ========================= */
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        /* Atleast try to cook the terminal on error before printing the message.
         * Do not handle the error to prevent possible infinite loops when panicking. */

        let _ = execute!(io::stdout(), DisableMouseCapturePixels);
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = execute!(io::stdout(), Show);
        let _ = disable_raw_mode();
        default_panic(info);
    }));

    /* ============================= STDIN parser thread ============================= */
    let event_inputs = threads::event::spawn();
    RECEIVER_GR.get_or_init(|| Mutex::new(event_inputs.2));

    /* ========== Check if the terminal supports the Kitty graphics protocol ========= */
    terminal_graphics_test_support()
        .expect("Error when testing terminal support of the Kitty graphics protocol");

    /* ================================= Load config ================================= */
    let mut key_matcher;
    {
        let mut config = config_load_or_create().expect("Could not load config");
        key_matcher = config.bindings.unwrap();
        config.bindings = None;
        CONFIG.get_or_init(|| config);
    }

    /* ======================= Calculate padding for all images ====================== */
    let winsize_tmp = window_size().expect("Could not get win size");
    let winsize = WindowSize {
        rows: winsize_tmp.rows,
        columns: winsize_tmp.columns,
        width: winsize_tmp.width,
        height: winsize_tmp.height,
    };
    TERMINAL_SIZE.get_or_init(|| RwLock::new(winsize_tmp));

    let config = CONFIG.get().unwrap();

    let pxpercol = winsize.width as f64 / winsize.columns as f64;
    let pxperrow = winsize.height as f64 / winsize.rows as f64;

    let paddingcol = (pxpercol * config.viewer.render_precision
        / config.viewer.scale_min as f64)
        .ceil() as usize;
    let paddingrow = (pxperrow * config.viewer.render_precision
        / config.viewer.scale_min as f64)
        .ceil() as usize;

    IMAGE_PADDING.get_or_init(|| std::cmp::max(paddingcol, paddingrow));

    /* =========== Generate a random ID which is unique for every instance =========== */
    let random_u64 = RandomState::new().build_hasher().finish();
    SOFTWARE_ID.get_or_init(|| format!("{random_u64:X}"));

    /* ====================== Viewer - The core of this program ====================== */
    let (mut viewer, sender_rerender) = Viewer::new();

    let (mut renderer, result_receiver) = threads::renderer::Renderer::new();
    renderer.run(&arg).expect("Couldn't start renderer thread");
    renderer
        .send_and_confirm_action(threads::renderer::RendererAction::Load)
        .expect("Cannot send action to renderer thread");

    /* ========================= Thread notifying file change ======================== */
    let file_reload = threads::fnotify::spawn(&arg).expect("Could not init file watcher");

    /* ============================== Main program loop ============================== */
    let mut throttle_data = LastExecuted {
        load: SystemTime::now() - Duration::from_millis(500),
        alpha: SystemTime::now() - Duration::from_millis(500),
        inverse: SystemTime::now() - Duration::from_millis(500),
    };

    let mut current_mouse = MouseEvent {
        kind: MouseEventKind::Moved,
        column: u16::MAX,
        row: u16::MAX,
        modifiers: KeyModifiers::NONE,
    };

    'main: loop {
        /* sel[0..1] are the results from the renderer thread */
        let mut sel = result_receiver.construct_biased_select();
        sel.recv(&file_reload);
        sel.recv(&sender_rerender);
        /* Key event */
        sel.recv(&event_inputs.0);
        /* Mouse event */
        sel.recv(&event_inputs.1);
        /* Window size change input */
        sel.recv(&event_inputs.3);

        let index_ready = sel.ready();

        execute!(io::stdout(), ClearImages, Clear(ClearType::FromCursorDown))
            .expect("Could not clear images");

        match index_ready {
            0 | 1 => {
                let result = result_receiver
                    .try_recv_priority(index_ready)
                    .expect("Could not receive priority");

                match result {
                    threads::renderer::RendererResult::PageMetadata {
                        max_page_width,
                        cumulative_heights,
                        widths,
                        links,
                    } => {
                        let uninit = viewer.is_uninit();

                        viewer.update_metadata(
                            max_page_width,
                            &cumulative_heights,
                            &widths,
                            &links,
                        );
                        viewer.invalidate_registry();
                        viewer.center_viewer();
                        if uninit {
                            viewer.scale_page2terminal();
                        }
                        result_receiver.clear_priority(1);
                    }
                    threads::renderer::RendererResult::Image { page, data } => {
                        viewer.handle_image(page, data);
                    }
                }
            }
            2 => {
                file_reload
                    .try_recv()
                    .expect("Could not receive file reload");
                if throttle_data.load.elapsed().unwrap() >= Duration::from_millis(1000) {
                    renderer
                        .send_and_confirm_action(threads::renderer::RendererAction::Load)
                        .expect("Cannot send action to renderer thread");
                }
            }
            3 => {
                sender_rerender
                    .try_recv()
                    .expect("Could not receive rerender");
            }
            4 => {
                let input = event_inputs.0.try_recv().expect("Could not receive input");
                match input {
                    InputEvent::Key(key) => {
                        if handle_key(
                            key,
                            &mut key_matcher,
                            &mut viewer,
                            &renderer,
                            &mut throttle_data,
                        ) {
                            break 'main;
                        }
                    }
                    InputEvent::MouseScroll(kind, _modifiers) => {
                        if handle_mouse_scroll(kind, &mut viewer) {
                            break 'main;
                        }
                    }
                }
            }
            5 => {
                current_mouse =
                    event_inputs.1.try_recv().expect("Could not receive mouse");
            }
            6 => {
                /* Drain the resize notification. crossterm's Resize event carries
                 * columns/rows (cells), NOT the pixel dimensions that the geometry
                 * math in image.rs depends on. Writing those cell counts into the
                 * pixel fields collapses pxpercol/pxperrow to ~1, which destroys the
                 * aspect ratio and page positioning. Instead, re-query the real
                 * terminal size (rows, columns, and pixel width/height) so every
                 * field stays consistent. */
                let _ = event_inputs
                    .3
                    .try_recv()
                    .expect("Could not receive from win-size");

                if let Ok(new_size) = window_size() {
                    dlog(&format!(
                        "RESIZE -> rows={} cols={} xpix={} ypix={}",
                        new_size.rows, new_size.columns, new_size.width, new_size.height
                    ));
                    let mut handle = TERMINAL_SIZE
                        .get()
                        .unwrap()
                        .write()
                        .expect("Could not get win size handle");
                    *handle = new_size;
                }
            }
            _ => unreachable!(),
        };

        if let Some(link) = viewer.intersect_link(current_mouse) {
            execute!(io::stdout(), SetPointerShape(PointerShape::Pointer))
                .expect("Could not set pointer shape");

            viewer.uri_hint(&link);
            if current_mouse.kind.is_down() {
                /* URI points to page in this document */
                if link.uri.starts_with('#') {
                    if let Some(destination) = &link.dest {
                        let _ = viewer.jump(destination.loc.page_number as usize);
                    }
                } else {
                    let _ = open::that_detached(link.uri);
                }

                execute!(io::stdout(), SetPointerShape(PointerShape::Default))
                    .expect("Could not set pointer shape");

                /* Since the mouse position is saved but this loop runs on other triggers
                 * such as key press, don't allow the mouse to accidentely click on other
                 * links when the viewer is scrolled down by key presses */
                current_mouse.kind = MouseEventKind::Moved;
            }
        } else {
            execute!(io::stdout(), SetPointerShape(PointerShape::Default))
                .expect("Could not set pointer shape");
        }

        let gr = RECEIVER_GR.get().unwrap().lock().unwrap();
        let displayed = viewer
            .display_pages(&renderer)
            .expect("Could not display pages");
        if !displayed.is_empty() {
            dlog(&format!(
                "display_pages -> {} page(s): {:?}",
                displayed.len(),
                displayed
            ));
        }
        let n_displayed = displayed.len();
        let mut timed_out = false;
        for (i, page) in displayed.into_iter().enumerate() {
            dlog(&format!(
                "  awaiting response {}/{} (page {})",
                i + 1,
                n_displayed,
                page
            ));
            /* Bounded wait: a draw command can occasionally produce no terminal
             * response (e.g. a partial page placed at a document edge, which the
             * terminal silently drops). Waiting forever on that ack hard-freezes
             * the whole viewer. Real acks arrive in single-digit milliseconds even
             * over SSH, so a short timeout keeps edge frames smooth while still
             * guaranteeing the loop never blocks. */
            match gr.recv_timeout(Duration::from_millis(80)) {
                Ok(res) => {
                    dlog(&format!("  got response (page {}): {:?}", page, res.payload()));
                    if !res.payload().contains("OK") {
                        viewer.schedule_transfer(page);
                    }
                }
                Err(_) => {
                    dlog(&format!(
                        "  NO RESPONSE for page {} ({}/{}) within timeout -- continuing",
                        page,
                        i + 1,
                        n_displayed
                    ));
                    timed_out = true;
                    break;
                }
            }
        }
        /* If we bailed out early, drain any late/stale responses so the next
         * frame's response handshake stays aligned. */
        if timed_out {
            while gr.try_recv().is_ok() {}
        }
    }

    RUNNING.store(false, Ordering::Release);

    /* ========================== Cook the terminal on exit ========================== */
    execute!(io::stdout(), DisableMouseCapturePixels)
        .expect("Could not disable mouse capture");
    execute!(io::stdout(), LeaveAlternateScreen).expect("Could not leave alt mode");
    execute!(io::stdout(), Show).expect("Could not show cursor");
    disable_raw_mode().expect("Could not uncook the terminal");
}

fn handle_mouse_scroll(kind: MouseEventKind, viewer: &mut Viewer) -> bool {
    let config = CONFIG.get().unwrap();

    let inverse_factor = if config.viewer.inverse_scroll {
        1.0
    } else {
        -1.0
    };

    match kind {
        MouseEventKind::ScrollUp => {
            viewer.scroll((0.0f32, inverse_factor * config.viewer.scroll_speed));
            false
        }
        MouseEventKind::ScrollDown => {
            viewer.scroll((0.0f32, -inverse_factor * config.viewer.scroll_speed));
            false
        }
        MouseEventKind::ScrollLeft => {
            viewer.scroll((-config.viewer.scroll_speed, 0.0f32));
            false
        }
        MouseEventKind::ScrollRight => {
            viewer.scroll((config.viewer.scroll_speed, 0.0f32));
            false
        }
        _ => false,
    }
}

fn handle_key(
    key: KeyEvent,
    key_matcher: &mut Keybinds<ConfigAction>,
    viewer: &mut Viewer,
    renderer: &threads::renderer::Renderer,
    throttle_data: &mut LastExecuted,
) -> bool {
    let config = CONFIG.get().unwrap();

    let possible_action = key_matcher.dispatch(KeyInput::from(key));
    if possible_action.is_none() {
        return false;
    }

    let action = possible_action.unwrap();

    /* `true` indicates that the caller should exit *safely* the current process */
    match action {
        ConfigAction::MoveUp => {
            viewer.scroll((0.0f32, -config.viewer.scroll_speed));
            false
        }
        ConfigAction::MoveDown => {
            viewer.scroll((0.0f32, config.viewer.scroll_speed));
            false
        }
        ConfigAction::MoveLeft => {
            viewer.scroll((-config.viewer.scroll_speed, 0.0f32));
            false
        }
        ConfigAction::MoveRight => {
            viewer.scroll((config.viewer.scroll_speed, 0.0f32));
            false
        }
        ConfigAction::PrevPage => {
            let current_page = viewer.page_first();
            if current_page > 0 {
                let _ = viewer.jump(current_page - 1);
            }
            false
        }
        ConfigAction::NextPage => {
            let current_page = viewer.page_first();
            let last_page = viewer.pages() - 1;
            if current_page < last_page {
                let _ = viewer.jump(current_page + 1);
            }
            false
        }
        ConfigAction::CenterViewer => {
            viewer.center_viewer();
            false
        }
        ConfigAction::JumpFirstPage => {
            let _ = viewer.jump(0);
            false
        }
        ConfigAction::JumpLastPage => {
            let last_page = viewer.pages() - 1;
            let _ = viewer.jump(last_page);
            false
        }
        ConfigAction::Quit => true,
        ConfigAction::ToggleAlpha => {
            if throttle_data.alpha.elapsed().unwrap() < Duration::from_millis(500) {
                return false;
            }

            throttle_data.alpha = SystemTime::now();

            renderer
                .send_and_confirm_action(threads::renderer::RendererAction::ToggleAlpha)
                .expect("Could not send action to renderer");
            viewer.invalidate_registry();
            false
        }
        ConfigAction::ToggleInverse => {
            if throttle_data.inverse.elapsed().unwrap() < Duration::from_millis(500) {
                return false;
            }

            throttle_data.inverse = SystemTime::now();
            renderer
                .send_and_confirm_action(threads::renderer::RendererAction::ToggleInverse)
                .expect("Could not send action to renderer");
            viewer.invalidate_registry();
            false
        }
        ConfigAction::ZoomIn => {
            viewer.scale(config.viewer.scale_amount);
            dlog(&format!("ZOOM IN  -> scale={}", viewer.get_scale()));
            false
        }
        ConfigAction::ZoomOut => {
            viewer.scale(-config.viewer.scale_amount);
            dlog(&format!("ZOOM OUT -> scale={}", viewer.get_scale()));
            false
        }
    }
}
