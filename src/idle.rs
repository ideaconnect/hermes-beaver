//! Input activity from the compositor (ext-idle-notify-v1), so that a mouse move
//! wakes a monitor in standby. With every output powered off, sway shows the
//! dashboard's window on no output: the window keeps the keyboard focus and gets
//! keys, but there's nothing under the pointer to send pointer events to. The idle
//! notification belongs to the seat and counts any input, as swayidle relies on.

use std::thread;

use anyhow::{Context, Result};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    self, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;

/// Quiet time after which the next input counts as new activity.
const IDLE_MS: u32 = 1000;

struct State {
    on_activity: Box<dyn Fn() + Send>,
}

/// Calls `on_activity`, on a thread of its own, at the first input after a second
/// without any. Fails if there's no Wayland compositor or it lacks the protocol.
pub fn watch(on_activity: impl Fn() + Send + 'static) -> Result<()> {
    let conn = Connection::connect_to_env().context("connecting to the Wayland compositor")?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&conn).context("listing the compositor's globals")?;
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).context("no wl_seat")?;
    let notifier: ExtIdleNotifierV1 = globals
        .bind(&qh, 1..=2, ())
        .context("the compositor lacks ext-idle-notify-v1")?;
    // Version 2 counts input only, whatever inhibits idleness (a video, say).
    if notifier.version() >= 2 {
        notifier.get_input_idle_notification(IDLE_MS, &seat, &qh, ());
    } else {
        notifier.get_idle_notification(IDLE_MS, &seat, &qh, ());
    }
    let mut state = State {
        on_activity: Box::new(on_activity),
    };
    thread::Builder::new()
        .name("idle-notify".into())
        .spawn(move || {
            loop {
                if let Err(e) = queue.blocking_dispatch(&mut state) {
                    eprintln!("hermes-beaver: input activity from the compositor stopped: {e}");
                    return;
                }
            }
        })
        .context("starting the idle-notify thread")?;
    Ok(())
}

impl Dispatch<ExtIdleNotificationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_idle_notification_v1::Event::Resumed = event {
            (state.on_activity)();
        }
    }
}

// The other objects send nothing the watcher needs.
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtIdleNotifierV1,
        _: <ExtIdleNotifierV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
