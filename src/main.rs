use anyhow::{bail, Result};
use gtk::{gio, prelude::*};
use niri_display::{daemon, editor, switcher};

const HELP: &str = "Configure Niri displays and saved profiles.

Usage: niri-display [--profiles]
       niri-display --daemon [--enable | --disable | --status]

Options:
  --profiles  Open the compact saved-profile switcher
  --daemon    Run the automatic layout restore daemon
  --enable    Enable and start the daemon's systemd user service
  --disable   Disable and stop the daemon's systemd user service
  --status    Show whether the daemon's user service is enabled
  --help      Show this help
  --version   Show the application version";

fn run() -> Result<i32> {
    let mut profiles = false;
    let mut background = false;
    let mut control = None;
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--help" | "-h" => {
                println!("{HELP}");
                return Ok(0);
            }
            "--version" | "-V" => {
                println!("niri-display {}", env!("CARGO_PKG_VERSION"));
                return Ok(0);
            }
            "--profiles" => profiles = true,
            "--daemon" => background = true,
            "--enable" | "--disable" | "--status" => {
                if control.replace(argument).is_some() {
                    bail!("Choose only one of --enable, --disable, or --status");
                }
            }
            _ => bail!("Unknown argument: {argument}. Use --help for usage."),
        }
    }
    if profiles && (background || control.is_some()) {
        bail!("--profiles cannot be combined with daemon options");
    }
    if control.is_some() && !background {
        bail!("Service controls require --daemon");
    }
    if background {
        match control.as_deref() {
            Some("--enable") => daemon::set_enabled(true)?,
            Some("--disable") => daemon::set_enabled(false)?,
            Some("--status") => println!(
                "{}",
                if daemon::enabled()? {
                    "enabled"
                } else {
                    "disabled"
                }
            ),
            _ => daemon::run()?,
        }
        return Ok(0);
    }

    let editor_renderer = std::env::var_os("GSK_RENDERER");
    if profiles && editor_renderer.is_none() {
        // This small, short-lived window does not benefit from GPU initialization.
        std::env::set_var("GSK_RENDERER", "cairo");
    }
    let app = gtk::Application::builder()
        .application_id(if profiles {
            "io.github.niri_display.profiles"
        } else {
            "io.github.niri_display"
        })
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| {
        if let Some(window) = app.active_window() {
            window.present();
        } else if profiles {
            switcher::present(app, editor_renderer.clone());
        } else {
            editor::present(app);
        }
    });
    Ok(app.run_with_args::<&str>(&[]).into())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(code) => std::process::ExitCode::from(code.clamp(0, 255) as u8),
        Err(error) => {
            eprintln!("niri-display: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
