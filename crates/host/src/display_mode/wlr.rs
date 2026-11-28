//! wlroots compositors (sway, Hyprland, river, labwc, ...): `zwlr_output_manager_v1`.

use std::collections::HashMap;

use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1,
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};

use super::helper::Backend;
use super::plan::{Head, HeadMode, Target};

#[derive(Default)]
struct ModeInfo {
    width: i32,
    height: i32,
    refresh: i32,
}

struct HeadInfo {
    proxy: ZwlrOutputHeadV1,
    name: String,
    enabled: bool,
    current: Option<ObjectId>,
    modes: Vec<ZwlrOutputModeV1>,
    x: i32,
    y: i32,
    transform: u32,
    scale: f64,
}

#[derive(Default)]
struct State {
    heads: Vec<HeadInfo>,
    modes: HashMap<ObjectId, ModeInfo>,
    serial: Option<u32>,
    finished: bool,
    /// How the last applied configuration went.
    outcome: Option<Result<(), String>>,
}

impl State {
    fn head(&mut self, proxy: &ZwlrOutputHeadV1) -> Option<&mut HeadInfo> {
        self.heads.iter_mut().find(|h| h.proxy == *proxy)
    }
}

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

impl Dispatch<ZwlrOutputManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputManagerV1,
        event: zwlr_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_manager_v1::Event::Head { head } => state.heads.push(HeadInfo {
                proxy: head,
                name: String::new(),
                enabled: false,
                current: None,
                modes: Vec::new(),
                x: 0,
                y: 0,
                transform: 0,
                scale: 1.0,
            }),
            zwlr_output_manager_v1::Event::Done { serial } => state.serial = Some(serial),
            zwlr_output_manager_v1::Event::Finished => state.finished = true,
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, ZwlrOutputManagerV1, [
        zwlr_output_manager_v1::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputHeadV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputHeadV1,
        event: zwlr_output_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_output_head_v1::Event;
        if let Event::Mode { mode } = &event {
            state.modes.insert(mode.id(), ModeInfo::default());
        }
        if matches!(event, Event::Finished) {
            state.heads.retain(|h| h.proxy != *proxy);
            return;
        }
        let Some(head) = state.head(proxy) else {
            return;
        };
        match event {
            Event::Name { name } => head.name = name,
            Event::Mode { mode } => head.modes.push(mode),
            Event::Enabled { enabled } => head.enabled = enabled != 0,
            Event::CurrentMode { mode } => head.current = Some(mode.id()),
            Event::Position { x, y } => (head.x, head.y) = (x, y),
            Event::Transform { transform } => {
                head.transform = match transform {
                    WEnum::Value(t) => t.into(),
                    WEnum::Unknown(t) => t,
                }
            }
            Event::Scale { scale } => head.scale = scale,
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, ZwlrOutputHeadV1, [
        zwlr_output_head_v1::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputModeV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputModeV1,
        event: zwlr_output_mode_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_output_mode_v1::Event;
        if matches!(event, Event::Finished) {
            state.modes.remove(&proxy.id());
            for h in &mut state.heads {
                h.modes.retain(|m| m != proxy);
            }
            return;
        }
        let Some(mode) = state.modes.get_mut(&proxy.id()) else {
            return;
        };
        match event {
            Event::Size { width, height } => (mode.width, mode.height) = (width, height),
            Event::Refresh { refresh } => mode.refresh = refresh,
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputConfigurationV1,
        event: zwlr_output_configuration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_output_configuration_v1::Event;
        state.outcome = match event {
            Event::Succeeded => Some(Ok(())),
            Event::Failed => Some(Err("the compositor refused the output configuration".into())),
            Event::Cancelled => Some(Err("the outputs changed meanwhile; try again".into())),
            _ => return,
        };
    }
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrOutputConfigurationHeadV1,
        _: <ZwlrOutputConfigurationHeadV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

pub struct Wlr {
    _conn: Connection,
    queue: EventQueue<State>,
    manager: ZwlrOutputManagerV1,
    state: State,
    /// The proxies behind the heads last returned by [`Backend::heads`].
    listed: Vec<ZwlrOutputHeadV1>,
}

impl Wlr {
    /// `None` when the compositor doesn't offer output management.
    pub fn connect() -> Result<Option<Self>, String> {
        let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
        let (globals, queue) = registry_queue_init::<State>(&conn).map_err(|e| e.to_string())?;
        let qh = queue.handle();
        let Ok(manager) = globals.bind::<ZwlrOutputManagerV1, _, _>(&qh, 1..=4, ()) else {
            return Ok(None);
        };
        Ok(Some(Self {
            _conn: conn,
            queue,
            manager,
            state: State::default(),
            listed: Vec::new(),
        }))
    }

    /// Waits for and handles the compositor's next events.
    fn wait(&mut self) -> Result<(), String> {
        self.queue
            .blocking_dispatch(&mut self.state)
            .map_err(|e| e.to_string())?;
        if self.state.finished {
            return Err("the compositor ended output management".into());
        }
        Ok(())
    }

    fn head_mode(&self, mode: &ZwlrOutputModeV1) -> Option<HeadMode> {
        let m = self.state.modes.get(&mode.id())?;
        Some(HeadMode {
            width: u32::try_from(m.width).ok()?,
            height: u32::try_from(m.height).ok()?,
            refresh_mhz: u32::try_from(m.refresh).unwrap_or(0),
            id: mode.id().protocol_id().to_string(),
            scales: Vec::new(),
        })
    }
}

impl Backend for Wlr {
    fn heads(&mut self) -> Result<Vec<Head>, String> {
        while self.state.serial.is_none() {
            self.wait()?;
        }
        let mut heads = Vec::new();
        self.listed.clear();
        for h in &self.state.heads {
            if !h.enabled {
                continue;
            }
            let Some(current) = h
                .modes
                .iter()
                .find(|m| Some(m.id()) == h.current)
                .and_then(|m| self.head_mode(m))
            else {
                continue;
            };
            heads.push(Head {
                name: h.name.clone(),
                edid: None,
                x: h.x,
                y: h.y,
                scale: h.scale,
                rotated: h.transform % 2 == 1,
                logical: true,
                modes: h.modes.iter().filter_map(|m| self.head_mode(m)).collect(),
                current,
                custom: h.name.starts_with("HEADLESS-"),
            });
            self.listed.push(h.proxy.clone());
        }
        Ok(heads)
    }

    fn apply(&mut self, heads: &[Head], targets: &[Target]) -> Result<(), String> {
        let serial = self.state.serial.ok_or("no output configuration yet")?;
        let qh = self.queue.handle();
        let config = self.manager.create_configuration(serial, &qh, ());
        for info in &self.state.heads {
            let Some(i) = self.listed.iter().position(|p| *p == info.proxy) else {
                if info.enabled {
                    config.enable_head(&info.proxy, &qh, ());
                } else {
                    config.disable_head(&info.proxy);
                }
                continue;
            };
            let (head, target) = (&heads[i], &targets[i]);
            let c = config.enable_head(&info.proxy, &qh, ());
            if !target.mode.same(&head.current) || target.mode.id != head.current.id {
                match info.modes.iter().find(|m| {
                    !target.mode.id.is_empty() && m.id().protocol_id().to_string() == target.mode.id
                }) {
                    Some(m) => c.set_mode(m),
                    None => c.set_custom_mode(
                        target.mode.width as i32,
                        target.mode.height as i32,
                        target.mode.refresh_mhz as i32,
                    ),
                }
            }
            if (target.x, target.y) != (head.x, head.y) {
                c.set_position(target.x, target.y);
            }
            if (target.scale - head.scale).abs() > 1e-6 {
                c.set_scale(target.scale);
            }
        }
        self.state.outcome = None;
        config.apply();
        while self.state.outcome.is_none() {
            self.wait()?;
        }
        config.destroy();
        self.state.serial = None;
        self.state.outcome.take().unwrap_or(Ok(()))
    }
}
