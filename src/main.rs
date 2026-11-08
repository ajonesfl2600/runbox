//! runbox: Super+R popup that runs one command at a time in a real terminal.
//!
//!   runbox                    open the popup (or bring the open one back)
//!   runbox --request "CMD"    put CMD in the popup for the user to review and run;
//!                             waits, prints what the run showed, exits with its code

mod runner;
mod ui;

use std::cell::RefCell;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gio::prelude::*;
use gtk::glib;
use gtk4 as gtk;

use runner::request_exit;

const APP_ID: &str = "io.github.ajonesfl2600.runbox";

const USAGE: &str = "usage:
  runbox                    open the popup
  runbox --request \"CMD\"    put CMD in the popup for review; Enter runs it, Esc declines.
                            Waits, prints the run's output, exits with the command's code.
                            125 = declined or not finished, 75 = busy, 2 = bad arguments.
";

enum Invocation {
    Open,
    Request(String),
    Help,
    Bad(String),
}

fn parse(args: &[String]) -> Invocation {
    match args {
        [] => Invocation::Open,
        [flag] if flag == "--help" || flag == "-h" => Invocation::Help,
        [flag, command] if flag == "--request" && !command.trim().is_empty() => {
            Invocation::Request(command.clone())
        }
        _ => Invocation::Bad(args.join(" ")),
    }
}

fn main() -> glib::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Answer these here: handing them to the window process would start a window.
    match parse(&args) {
        Invocation::Help => {
            print!("{USAGE}");
            return glib::ExitCode::SUCCESS;
        }
        Invocation::Bad(got) => {
            eprint!("runbox: bad arguments: {got}\n{USAGE}");
            return glib::ExitCode::from(request_exit::USAGE);
        }
        Invocation::Open | Invocation::Request(_) => {}
    }
    if matches!(parse(&args), Invocation::Request(_)) {
        // The caller must stay a separate process from the window, or it would only
        // return when the window closes. Start the window first if none is open.
        if let Err(err) = ensure_window_process() {
            eprintln!("runbox: {err}");
            return glib::ExitCode::from(request_exit::NOT_RUN);
        }
    }

    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    // The installed icon's name, so alt-tab and the window list show it.
    app.connect_startup(|_| gtk::Window::set_default_icon_name(APP_ID));

    // GApplication keeps one instance: later launches hand their arguments to it.
    let current: Rc<RefCell<Option<ui::RunBox>>> = Rc::default();
    app.connect_command_line(move |app, caller| {
        let window = || {
            current
                .borrow_mut()
                .get_or_insert_with(|| ui::RunBox::new(app))
                .clone()
        };
        let args: Vec<String> = caller
            .arguments()
            .iter()
            .skip(1)
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        match parse(&args) {
            Invocation::Open => {
                window().present();
                glib::ExitCode::SUCCESS
            }
            Invocation::Request(command) => {
                let runbox = window();
                runbox.present();
                match runbox.request(&command, caller.clone()) {
                    // Placeholder until the run finishes; anything abnormal reads as not run.
                    Ok(()) => glib::ExitCode::from(request_exit::NOT_RUN),
                    Err(why) => {
                        caller.printerr_literal(&format!("runbox: busy: {why}\n"));
                        glib::ExitCode::from(request_exit::BUSY)
                    }
                }
            }
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

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_invocations() {
        assert!(matches!(parse(&args(&[])), Invocation::Open));
        assert!(matches!(parse(&args(&["--help"])), Invocation::Help));
        assert!(matches!(
            parse(&args(&["--request", "sudo apt update"])),
            Invocation::Request(c) if c == "sudo apt update"
        ));
        assert!(matches!(parse(&args(&["--request"])), Invocation::Bad(_)));
        assert!(matches!(
            parse(&args(&["--request", "  "])),
            Invocation::Bad(_)
        ));
        assert!(matches!(parse(&args(&["--bogus"])), Invocation::Bad(_)));
    }
}
