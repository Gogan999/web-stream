//! GNOME: `org.gnome.Mutter.DisplayConfig` on the session bus, applied as a temporary
//! configuration.

use std::collections::HashMap;

use zbus::blocking::Connection;
use zbus::zvariant::{OwnedValue, Value};

use super::helper::Backend;
use super::plan::{Head, HeadMode, Target};

const DEST: &str = "org.gnome.Mutter.DisplayConfig";
const PATH: &str = "/org/gnome/Mutter/DisplayConfig";
/// `ApplyMonitorsConfig` method: applied now, not saved.
const TEMPORARY: u32 = 1;
/// `layout-mode`: positions in logical pixels.
const LAYOUT_LOGICAL: u32 = 1;
/// `layout-mode`: positions in physical pixels.
const LAYOUT_PHYSICAL: u32 = 2;

type Props = HashMap<String, OwnedValue>;
type Spec = (String, String, String, String);
type RawMode = (String, i32, i32, f64, f64, Vec<f64>, Props);
type RawMonitor = (Spec, Vec<RawMode>, Props);
type RawLogical = (i32, i32, f64, u32, bool, Vec<Spec>, Props);
/// `GetCurrentState`'s reply.
type RawState = (u32, Vec<RawMonitor>, Vec<RawLogical>, Props);

/// One logical monitor as `ApplyMonitorsConfig` takes it.
pub type LogicalConfig = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(String, String, HashMap<String, Value<'static>>)>,
);

#[derive(Debug, Clone, PartialEq)]
struct Logical {
    x: i32,
    y: i32,
    scale: f64,
    transform: u32,
    primary: bool,
    connectors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct Monitor {
    connector: String,
    modes: Vec<HeadMode>,
    current: Option<HeadMode>,
}

#[derive(Debug, Clone, PartialEq)]
struct DisplayState {
    serial: u32,
    monitors: Vec<Monitor>,
    logical: Vec<Logical>,
    layout_mode: u32,
    can_change_layout: bool,
}

fn flag(props: &Props, key: &str) -> bool {
    props
        .get(key)
        .and_then(|v| v.downcast_ref::<bool>().ok())
        .unwrap_or(false)
}

fn parse(raw: RawState) -> DisplayState {
    let (serial, monitors, logical, props) = raw;
    let monitors = monitors
        .into_iter()
        .map(|(spec, modes, _)| {
            let mut current = None;
            let mut list = Vec::new();
            for (id, w, h, refresh, _, scales, props) in modes {
                let mode = HeadMode {
                    width: u32::try_from(w).unwrap_or(0),
                    height: u32::try_from(h).unwrap_or(0),
                    refresh_mhz: (refresh * 1000.0).round() as u32,
                    id,
                    scales,
                };
                if flag(&props, "is-current") {
                    current = Some(mode.clone());
                }
                if !flag(&props, "is-interlaced") {
                    list.push(mode);
                }
            }
            Monitor {
                connector: spec.0,
                modes: list,
                current,
            }
        })
        .collect();
    let logical = logical
        .into_iter()
        .map(|(x, y, scale, transform, primary, specs, _)| Logical {
            x,
            y,
            scale,
            transform,
            primary,
            connectors: specs.into_iter().map(|s| s.0).collect(),
        })
        .collect();
    DisplayState {
        serial,
        monitors,
        logical,
        layout_mode: props
            .get("layout-mode")
            .and_then(|v| v.downcast_ref::<u32>().ok())
            .unwrap_or(LAYOUT_PHYSICAL),
        can_change_layout: flag(&props, "supports-changing-layout-mode"),
    }
}

/// One head per monitor in a logical monitor, with the logical monitor each belongs to.
fn heads(state: &DisplayState) -> (Vec<Head>, Vec<usize>) {
    let mut heads = Vec::new();
    let mut owner = Vec::new();
    for (i, lm) in state.logical.iter().enumerate() {
        for connector in &lm.connectors {
            let Some(m) = state.monitors.iter().find(|m| m.connector == *connector) else {
                continue;
            };
            let Some(current) = m.current.clone() else {
                continue;
            };
            heads.push(Head {
                name: connector.clone(),
                edid: None,
                x: lm.x,
                y: lm.y,
                scale: lm.scale,
                rotated: lm.transform % 2 == 1,
                logical: state.layout_mode == LAYOUT_LOGICAL,
                modes: m.modes.clone(),
                current,
                custom: false,
            });
            owner.push(i);
        }
    }
    (heads, owner)
}

/// `ApplyMonitorsConfig`'s logical monitors and properties for `targets`.
fn build(
    state: &DisplayState,
    heads: &[Head],
    owner: &[usize],
    targets: &[Target],
) -> Result<(Vec<LogicalConfig>, HashMap<String, Value<'static>>), String> {
    let mut out = Vec::new();
    for (i, lm) in state.logical.iter().enumerate() {
        let members: Vec<usize> = (0..heads.len()).filter(|&h| owner[h] == i).collect();
        let Some(&first) = members.first() else {
            return Err(format!("{} has no current mode", lm.connectors.join(", ")));
        };
        if members.len() > 1
            && members
                .iter()
                .any(|&h| !targets[h].mode.same(&heads[h].current))
        {
            return Err(format!(
                "{} are mirrored; their mode can't change on its own",
                lm.connectors.join(", ")
            ));
        }
        let t = &targets[first];
        out.push((
            t.x,
            t.y,
            t.scale,
            lm.transform,
            lm.primary,
            members
                .iter()
                .map(|&h| {
                    (
                        heads[h].name.clone(),
                        targets[h].mode.id.clone(),
                        HashMap::new(),
                    )
                })
                .collect(),
        ));
    }
    let mut props = HashMap::new();
    if state.can_change_layout {
        props.insert("layout-mode".to_string(), Value::U32(state.layout_mode));
    }
    Ok((out, props))
}

pub struct Mutter {
    conn: Connection,
    state: Option<(DisplayState, Vec<usize>)>,
}

impl Mutter {
    /// `None` when Mutter isn't on the session bus.
    pub fn connect() -> Result<Option<Self>, String> {
        let conn = Connection::session().map_err(|e| e.to_string())?;
        let owned: bool = conn
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "NameHasOwner",
                &(DEST,),
            )
            .and_then(|m| m.body().deserialize())
            .map_err(|e| e.to_string())?;
        Ok(owned.then_some(Self { conn, state: None }))
    }
}

impl Backend for Mutter {
    fn heads(&mut self) -> Result<Vec<Head>, String> {
        let raw: RawState = self
            .conn
            .call_method(Some(DEST), PATH, Some(DEST), "GetCurrentState", &())
            .and_then(|m| m.body().deserialize())
            .map_err(|e| e.to_string())?;
        let state = parse(raw);
        let (list, owner) = heads(&state);
        self.state = Some((state, owner));
        Ok(list)
    }

    fn apply(&mut self, heads: &[Head], targets: &[Target]) -> Result<(), String> {
        let (state, owner) = self.state.as_ref().ok_or("no display state read")?;
        let (logical, props) = build(state, heads, owner, targets)?;
        self.conn
            .call_method(
                Some(DEST),
                PATH,
                Some(DEST),
                "ApplyMonitorsConfig",
                &(state.serial, TEMPORARY, logical, props),
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::plan;
    use super::*;

    fn props(pairs: &[(&str, bool)]) -> Props {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), OwnedValue::from(*v)))
            .collect()
    }

    fn raw_mode(w: i32, h: i32, hz: f64, current: bool) -> RawMode {
        (
            format!("{w}x{h}@{hz}"),
            w,
            h,
            hz,
            1.0,
            vec![1.0, 2.0],
            props(&[("is-current", current)]),
        )
    }

    fn spec(c: &str) -> Spec {
        (c.into(), "DEL".into(), "U2720Q".into(), "123".into())
    }

    fn sample() -> RawState {
        let mut global = props(&[("supports-changing-layout-mode", true)]);
        global.insert("layout-mode".into(), OwnedValue::from(LAYOUT_LOGICAL));
        let mut interlaced = raw_mode(1920, 1080, 60.0, false);
        interlaced.0 = "1920x1080i".into();
        interlaced.6 = props(&[("is-interlaced", true)]);
        (
            7,
            vec![
                (
                    spec("DP-1"),
                    vec![
                        raw_mode(3840, 2160, 59.997, true),
                        raw_mode(2560, 1440, 59.951, false),
                        raw_mode(1920, 1080, 60.0, false),
                        interlaced,
                    ],
                    Props::new(),
                ),
                (
                    spec("HDMI-1"),
                    vec![raw_mode(1920, 1080, 60.0, true)],
                    Props::new(),
                ),
            ],
            vec![
                (0, 0, 2.0, 0, true, vec![spec("DP-1")], Props::new()),
                (1920, 0, 1.0, 0, false, vec![spec("HDMI-1")], Props::new()),
            ],
            global,
        )
    }

    #[test]
    fn reads_the_current_state() {
        let state = parse(sample());
        assert_eq!(state.serial, 7);
        assert_eq!(state.layout_mode, LAYOUT_LOGICAL);
        assert!(state.can_change_layout);
        let (heads, owner) = heads(&state);
        assert_eq!(owner, vec![0, 1]);
        assert_eq!(heads[0].name, "DP-1");
        assert_eq!(heads[0].modes.len(), 3);
        assert_eq!(heads[0].current.refresh_mhz, 59997);
        assert!(heads[0].logical);
        assert_eq!((heads[1].x, heads[1].scale), (1920, 1.0));
    }

    #[test]
    fn builds_a_temporary_config_for_one_monitor() {
        let state = parse(sample());
        let (heads, owner) = heads(&state);
        let i = plan::match_head(&heads, "HDMI-A-1", None).unwrap();
        assert_eq!(i, 1);
        let mode = plan::choose(&heads[0], 2560, 1440).unwrap();
        assert_eq!(mode.id, "2560x1440@59.951");
        let targets = plan::plan(&heads, 0, mode, None, None);
        let (logical, props) = build(&state, &heads, &owner, &targets).unwrap();
        assert_eq!(
            logical,
            vec![
                (
                    0,
                    0,
                    2.0,
                    0,
                    true,
                    vec![("DP-1".into(), "2560x1440@59.951".into(), HashMap::new())]
                ),
                (
                    1280,
                    0,
                    1.0,
                    0,
                    false,
                    vec![("HDMI-1".into(), "1920x1080@60".into(), HashMap::new())]
                ),
            ]
        );
        assert_eq!(props.get("layout-mode"), Some(&Value::U32(LAYOUT_LOGICAL)));
    }

    #[test]
    fn mirrored_monitors_refuse_a_lone_change() {
        let mut raw = sample();
        raw.2 = vec![(
            0,
            0,
            1.0,
            0,
            true,
            vec![spec("DP-1"), spec("HDMI-1")],
            Props::new(),
        )];
        let state = parse(raw);
        let (heads, owner) = heads(&state);
        let mode = plan::choose(&heads[0], 1920, 1080).unwrap();
        let targets = plan::plan(&heads, 0, mode, None, None);
        assert!(build(&state, &heads, &owner, &targets).is_err());
        let same = plan::plan(&heads, 0, heads[0].current.clone(), None, None);
        assert!(build(&state, &heads, &owner, &same).is_ok());
    }
}
