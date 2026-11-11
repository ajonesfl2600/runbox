//! runbox: Super+R popup that runs one command at a time in a real terminal.
//!
//!   runbox                    open the popup (or bring the open one back)
//!   runbox --request "CMD"    open a window with CMD for the user to review and run;
//!                             waits, prints what the run showed, exits with its code
//!   runbox --list             show the open requests
//!
//! One process (the GApplication primary) owns every window. Each `--request`
//! gets its own window, tracked in a `registry::Registry`, and its own answer.

mod cli;
mod registry;
mod result;
mod runner;
mod ui;

use std::cell::RefCell;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gio::prelude::*;
use gtk::glib;
use gtk4 as gtk;

use cli::Invocation;
use registry::{MAX_OPEN, Registry, State};
use result::unix_now;
use runner::request_exit;

const APP_ID: &str = "io.github.ajonesfl2600.runbox";

/// The argument a `--request` passes when it has to start the window process
/// itself. That process opens no window of its own: the request opens its own.
/// Not in the usage text: nobody types it.
const SERVE_ARG: &str = "--serve-requests";
/// How long a window process started for a request waits for it before quitting.
const SERVE_GRACE: Duration = Duration::from_secs(10);

const USAGE: &str = "usage:
  runbox                    open the popup
  runbox --request \"CMD\" [--id NAME] [--json] [--output-file PATH]
                            open a window with CMD for review; Enter runs it, Esc declines.
                            Waits, prints the run's output, exits with the command's code.
                            125 = declined or not finished, 75 = busy, 2 = bad arguments.
      --id NAME             label for the window and the result (1-32 of A-Z a-z 0-9 . _ -);
                            left off, runbox picks one (r1, r2, ...)
      --json                print one JSON object instead of the raw output:
                            id, command, exit_code, status (ran|declined|killed),
                            started, ended (unix seconds), output
      --output-file PATH    also write the full output to PATH (created mode 0600)
  runbox --list             show the open requests: id, state, seconds open, command
";

/// Every window the primary process owns.
#[derive(Default)]
struct Hub {
    /// The plain Super+R popup, while it's open.
    plain: Option<ui::RunBox>,
    registry: Registry,
    /// Request windows, by their current request id.
    windows: Vec<(String, ui::RunBox)>,
}

impl Hub {
    fn window(&self, id: &str) -> Option<ui::RunBox> {
        self.windows
            .iter()
            .find(|(open, _)| open == id)
            .map(|(_, runbox)| runbox.clone())
    }

    fn forget(&mut self, id: &str) {
        self.registry.close(id);
        self.windows.retain(|(open, _)| open != id);
    }
}

/// Where a new request goes.
enum Slot {
    /// A finished request window, idle, that takes the new request.
    Reuse(ui::RunBox),
    /// A new window.
    New,
}

fn main() -> glib::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let serving = args.len() == 1 && args[0] == SERVE_ARG;
    // Answer these here: handing them to the window process would start a window.
    if !serving {
        match cli::parse(&args) {
            Invocation::Help => {
                print!("{USAGE}");
                return glib::ExitCode::SUCCESS;
            }
            Invocation::Bad(got) => {
                eprint!("runbox: bad arguments: {got}\n{USAGE}");
                return glib::ExitCode::from(request_exit::USAGE);
            }
            // No window process means nothing is open. Don't start one to say so.
            Invocation::List => {
                if !window_process_running().unwrap_or(false) {
                    println!("no open requests");
                    return glib::ExitCode::SUCCESS;
                }
            }
            Invocation::Request(_) => {
                // The caller must stay a separate process from the window, or it would
                // only return when the window closes. Start the window process first.
                if let Err(err) = ensure_window_process() {
                    eprintln!("runbox: {err}");
                    return glib::ExitCode::from(request_exit::NOT_RUN);
                }
            }
            Invocation::Open => {}
        }
    }

    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    // The installed icon's name, so alt-tab and the window list show it.
    app.connect_startup(|_| gtk::Window::set_default_icon_name(APP_ID));

    // GApplication keeps one instance: later launches hand their arguments to it.
    let hub: Rc<RefCell<Hub>> = Rc::default();
    app.connect_command_line(move |app, caller| {
        let args: Vec<String> = caller
            .arguments()
            .iter()
            .skip(1)
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        if args.len() == 1 && args[0] == SERVE_ARG {
            // Stay up long enough for the request that started us to arrive. Once
            // it has a window, that window keeps the process alive.
            let guard = app.hold();
            glib::timeout_add_local_once(SERVE_GRACE, move || drop(guard));
            return glib::ExitCode::SUCCESS;
        }
        match cli::parse(&args) {
            Invocation::Open => {
                open_plain(app, &hub).present();
                glib::ExitCode::SUCCESS
            }
            Invocation::List => {
                let text = hub.borrow().registry.list_text(unix_now());
                caller.print_literal(&format!("{text}\n"));
                glib::ExitCode::SUCCESS
            }
            Invocation::Request(req) => open_request(app, &hub, req, caller),
            Invocation::Help => {
                caller.print_literal(USAGE);
                glib::ExitCode::SUCCESS
            }
            Invocation::Bad(got) => {
                caller.printerr_literal(&format!("runbox: bad arguments: {got}\n{USAGE}"));
                glib::ExitCode::from(request_exit::USAGE)
            }
        }
    });

    app.run()
}

/// The plain popup, made when it isn't open.
fn open_plain(app: &gtk::Application, hub: &Rc<RefCell<Hub>>) -> ui::RunBox {
    if let Some(runbox) = hub.borrow().plain.clone() {
        return runbox;
    }
    let weak = Rc::downgrade(hub);
    let watcher: ui::Watcher = Rc::new(move |_, event| {
        if event == ui::Event::Closed
            && let Some(hub) = weak.upgrade()
        {
            hub.borrow_mut().plain = None;
        }
    });
    let runbox = ui::RunBox::new(app, None, watcher);
    hub.borrow_mut().plain = Some(runbox.clone());
    runbox
}

/// Give a request its own window, or answer 75 when there's no room.
fn open_request(
    app: &gtk::Application,
    hub: &Rc<RefCell<Hub>>,
    req: cli::Req,
    caller: &gio::ApplicationCommandLine,
) -> glib::ExitCode {
    // The window process may run in another folder than the caller.
    let output_file = req
        .output_file
        .as_deref()
        .map(|path| resolve(caller.cwd().as_deref(), path));

    let now = unix_now();
    // Decide and book the slot in one borrow, then build windows outside it: a
    // window's watcher borrows the hub too.
    let booked = {
        let mut hub = hub.borrow_mut();
        let slot = match pick_slot(&mut hub, req.id.as_deref()) {
            Ok(slot) => slot,
            Err(why) => return busy(caller, &why),
        };
        match hub.registry.open(req.id.clone(), &req.command, now) {
            Ok(id) => (id, slot),
            Err(why) => return busy(caller, &why),
        }
    };
    let (id, slot) = booked;

    let runbox = match slot {
        Slot::Reuse(runbox) => {
            runbox.set_id(&id);
            runbox
        }
        Slot::New => {
            let offset = hub.borrow().registry.offset(&id).unwrap_or((0, 0));
            let runbox = ui::RunBox::new(app, Some(&id), request_watcher(hub));
            runbox.place_at_offset(offset);
            runbox
        }
    };
    hub.borrow_mut().windows.push((id.clone(), runbox.clone()));

    let request_id = id.clone();
    let request = ui::Request::new(
        caller.clone(),
        id,
        req.command.clone(),
        req.json,
        output_file,
        now,
    );
    runbox.present();
    match runbox.request(&req.command, request) {
        // Placeholder until the run finishes; anything abnormal reads as not run.
        Ok(()) => glib::ExitCode::from(request_exit::NOT_RUN),
        Err(why) => {
            // Shouldn't happen: the window is new or finished. Undo the booking so
            // the id and the slot don't stay taken by a request that isn't there.
            hub.borrow_mut().forget(&request_id);
            busy(caller, why)
        }
    }
}

/// Free a slot for a new request: a finished window it can take over, or room for
/// a new one. Err with the reason when the id is taken or all windows are busy.
fn pick_slot(hub: &mut Hub, wanted: Option<&str>) -> Result<Slot, String> {
    // Asking for the id of a finished window takes that window back.
    let reuse = match wanted {
        Some(id) => match hub.registry.state(id) {
            Some(State::Done) => Some(id.to_string()),
            Some(_) => return Err(format!("id in use: {id} (pick another --id)")),
            None => hub.registry.oldest_done(),
        },
        None => hub.registry.oldest_done(),
    };
    if let Some(old) = reuse
        && let Some(runbox) = hub.window(&old)
    {
        hub.forget(&old);
        return Ok(Slot::Reuse(runbox));
    }
    if hub.registry.len() >= MAX_OPEN {
        return Err(format!(
            "{} requests already open, the most runbox allows. Answer or close one, then try again",
            hub.registry.len()
        ));
    }
    Ok(Slot::New)
}

/// Keeps the registry in step with one request window.
fn request_watcher(hub: &Rc<RefCell<Hub>>) -> ui::Watcher {
    let weak = Rc::downgrade(hub);
    Rc::new(move |id, event| {
        let (Some(hub), Some(id)) = (weak.upgrade(), id) else {
            return;
        };
        let mut hub = hub.borrow_mut();
        match event {
            ui::Event::Running => {
                hub.registry.set_state(id, State::Running);
            }
            ui::Event::Done => {
                hub.registry.set_state(id, State::Done);
            }
            ui::Event::Closed => hub.forget(id),
        }
    })
}

fn busy(caller: &gio::ApplicationCommandLine, why: &str) -> glib::ExitCode {
    let why = why.strip_prefix("busy: ").unwrap_or(why);
    caller.printerr_literal(&format!("runbox: busy: {why}\n"));
    glib::ExitCode::from(request_exit::BUSY)
}

/// `path` as the caller meant it: relative paths start in the caller's folder.
fn resolve(cwd: Option<&Path>, path: &str) -> String {
    match cwd {
        Some(cwd) if Path::new(path).is_relative() => cwd.join(path).display().to_string(),
        _ => path.to_string(),
    }
}

/// True when a runbox window process already owns the app's D-Bus name.
fn window_process_running() -> Result<bool, glib::Error> {
    let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)?;
    let reply = bus.call_sync(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "NameHasOwner",
        Some(&(APP_ID,).to_variant()),
        Some(glib::VariantTy::new("(b)").expect("valid type")),
        gio::DBusCallFlags::NONE,
        2000,
        gio::Cancellable::NONE,
    )?;
    Ok(reply
        .get::<(bool,)>()
        .map(|(owned,)| owned)
        .unwrap_or(false))
}

fn ensure_window_process() -> Result<(), String> {
    if window_process_running().map_err(|e| format!("no session bus: {e}"))? {
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|e| format!("can't find own binary: {e}"))?;
    Command::new(exe)
        .arg(SERVE_ARG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("can't start the window: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if window_process_running().unwrap_or(false) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err("the window didn't start within 5 s".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_output_file_starts_in_the_callers_folder() {
        let cwd = Path::new("/home/me/work");
        assert_eq!(resolve(Some(cwd), "out.txt"), "/home/me/work/out.txt");
        assert_eq!(
            resolve(Some(cwd), "logs/out.txt"),
            "/home/me/work/logs/out.txt"
        );
    }

    #[test]
    fn absolute_output_file_is_kept() {
        let cwd = Path::new("/home/me/work");
        assert_eq!(resolve(Some(cwd), "/tmp/out.txt"), "/tmp/out.txt");
        // No folder from the caller: the path goes through as given.
        assert_eq!(resolve(None, "out.txt"), "out.txt");
    }

    fn hub_with(open: &[(&str, State)]) -> Hub {
        let mut hub = Hub::default();
        for (i, (id, state)) in open.iter().enumerate() {
            hub.registry
                .open(Some(id.to_string()), "cmd", i as u64)
                .unwrap();
            hub.registry.set_state(id, *state);
        }
        hub
    }

    #[test]
    fn room_left_gives_a_new_window() {
        let mut hub = hub_with(&[("a", State::Running)]);
        assert!(matches!(pick_slot(&mut hub, None), Ok(Slot::New)));
        assert!(matches!(pick_slot(&mut hub, Some("b")), Ok(Slot::New)));
    }

    #[test]
    fn cap_reached_is_busy_with_a_clear_reason() {
        let mut hub = hub_with(&[
            ("a", State::Running),
            ("b", State::Waiting),
            ("c", State::Running),
            ("d", State::Waiting),
        ]);
        let why = pick_slot(&mut hub, None).err().unwrap();
        assert!(why.contains("4 requests already open"), "{why}");
        assert!(pick_slot(&mut hub, Some("e")).is_err());
        // Nothing was dropped to make room.
        assert_eq!(hub.registry.len(), MAX_OPEN);
    }

    #[test]
    fn id_of_a_live_request_is_refused() {
        let mut hub = hub_with(&[("job", State::Waiting)]);
        let why = pick_slot(&mut hub, Some("job")).err().unwrap();
        assert!(why.starts_with("id in use: job"), "{why}");
        assert_eq!(hub.registry.len(), 1);
    }

    #[test]
    fn done_entry_without_a_window_falls_back_to_a_new_one() {
        // The registry says done, but no window is tracked for it (no GTK in a unit
        // test). There's nothing to reuse, so a new window is the answer.
        let mut hub = hub_with(&[("a", State::Done)]);
        assert!(matches!(pick_slot(&mut hub, None), Ok(Slot::New)));
    }
}
