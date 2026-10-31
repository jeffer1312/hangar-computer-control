//! Wayland protocols Hyprland offers: whether the user is idle (ext-idle-notify) and the toplevel identifiers
//! `grim -T` captures by (ext-foreign-toplevel-list).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{self, ExtIdleNotificationV1};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;

/// Idle after this long without input.
const OCIOSO_MS: u32 = 2000;

struct Ocioso(Arc<AtomicBool>);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Ocioso {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wl_seat::WlSeat, ()> for Ocioso {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ExtIdleNotifierV1, ()> for Ocioso {
    fn event(_: &mut Self, _: &ExtIdleNotifierV1, _: <ExtIdleNotifierV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ExtIdleNotificationV1, ()> for Ocioso {
    fn event(s: &mut Self, _: &ExtIdleNotificationV1, e: ext_idle_notification_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match e {
            ext_idle_notification_v1::Event::Idled => s.0.store(true, Ordering::Relaxed),
            ext_idle_notification_v1::Event::Resumed => s.0.store(false, Ordering::Relaxed),
            _ => {}
        }
    }
}

/// Keeps `parado` true while the user has not touched keyboard or mouse for 2 s; a thread follows the compositor.
pub fn vigiar_ocioso(parado: Arc<AtomicBool>) -> Result<(), String> {
    let conexao = Connection::connect_to_env().map_err(|e| format!("wayland: {e}"))?;
    let (globais, mut fila) = registry_queue_init::<Ocioso>(&conexao).map_err(|e| format!("wayland: {e}"))?;
    let qh = fila.handle();
    let seat: wl_seat::WlSeat = globais.bind(&qh, 1..=1, ()).map_err(|e| format!("wayland wl_seat: {e}"))?;
    let notificador: ExtIdleNotifierV1 = globais.bind(&qh, 1..=1, ()).map_err(|e| format!("wayland ext_idle_notifier_v1: {e}"))?;
    let aviso = notificador.get_idle_notification(OCIOSO_MS, &seat, &qh, ());
    let mut estado = Ocioso(parado);
    std::thread::spawn(move || {
        let _manter = (aviso, seat, notificador);
        while fila.blocking_dispatch(&mut estado).is_ok() {}
        // Without the compositor nobody can say the user is active: do not hold the agent back forever.
        estado.0.store(true, Ordering::Relaxed);
    });
    Ok(())
}

#[derive(Default)]
struct Lista(Vec<(ExtForeignToplevelHandleV1, String, String, String)>);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Lista {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ExtForeignToplevelListV1, ()> for Lista {
    fn event(s: &mut Self, _: &ExtForeignToplevelListV1, e: ext_foreign_toplevel_list_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = e {
            s.0.push((toplevel, String::new(), String::new(), String::new()));
        }
    }
    event_created_child!(Lista, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}
impl Dispatch<ExtForeignToplevelHandleV1, ()> for Lista {
    fn event(s: &mut Self, h: &ExtForeignToplevelHandleV1, e: ext_foreign_toplevel_handle_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let Some(item) = s.0.iter_mut().find(|i| &i.0 == h) else { return };
        match e {
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => item.1 = identifier,
            ext_foreign_toplevel_handle_v1::Event::Title { title } => item.2 = title,
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => item.3 = app_id,
            _ => {}
        }
    }
}

/// The toplevel identifier of the single window with that title and app id (Hyprland's class).
pub fn identificador(titulo: &str, app_id: &str) -> Result<String, String> {
    let conexao = Connection::connect_to_env().map_err(|e| format!("wayland: {e}"))?;
    let (globais, mut fila) = registry_queue_init::<Lista>(&conexao).map_err(|e| format!("wayland: {e}"))?;
    let lista: ExtForeignToplevelListV1 = globais.bind(&fila.handle(), 1..=1, ()).map_err(|e| format!("wayland ext_foreign_toplevel_list_v1: {e}"))?;
    let mut estado = Lista::default();
    for _ in 0..2 {
        fila.roundtrip(&mut estado).map_err(|e| format!("wayland: {e}"))?;
    }
    lista.stop();
    let iguais: Vec<&String> = estado.0.iter().filter(|i| i.2 == titulo && i.3 == app_id).map(|i| &i.1).collect();
    match iguais[..] {
        [id] => Ok(id.clone()),
        [] => Err("janela não encontrada para o print".into()),
        _ => Err("mais de uma janela com o mesmo título; print da janela impossível".into()),
    }
}
