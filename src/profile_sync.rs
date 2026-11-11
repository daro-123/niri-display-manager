//! Notifies open editor windows when the compact picker applies a profile.
//!
//! The picker and editor run as separate GTK applications, so the picker
//! broadcasts a session-bus signal after a successful switch. Editors keep a
//! subscription and reread the compositor layout when it arrives.

use gtk::gio;

const OBJECT_PATH: &str = "/io/github/niri_display/Profiles";
const INTERFACE: &str = "io.github.niri_display.Profiles";
const SWITCHED: &str = "Switched";

fn session_bus() -> Option<gio::DBusConnection> {
    gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>).ok()
}

/// Announce that a profile was applied and the compositor layout changed.
pub fn notify_switched() {
    let Some(connection) = session_bus() else {
        return;
    };
    let _ = connection.emit_signal(None, OBJECT_PATH, INTERFACE, SWITCHED, None);
}

/// Call `callback` whenever a picker successfully applies a profile.
///
/// Returns `None` when no session bus is available; the picker then has no
/// editors to notify either.
pub fn on_switched(callback: impl Fn() + 'static) -> Option<gio::SignalSubscription> {
    let connection = session_bus()?;
    Some(connection.subscribe_to_signal(
        None,
        Some(INTERFACE),
        Some(SWITCHED),
        Some(OBJECT_PATH),
        None,
        gio::DBusSignalFlags::NONE,
        move |_| callback(),
    ))
}
