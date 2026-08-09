//! Philips Hue bulbs over the bridge's CLIP v2 API, and the setup flow that finds them.
//!
//! # Setting one up
//!
//! The driver owns the whole conversation, because only it knows what a Hue bridge wants:
//!
//! ```text
//!   1. ask for the bridge address (or check the one offered)
//!   2. GET  /api/config                — is this really a bridge?
//!   3. "press the link button"
//!   4. POST /api                       — refused until it is pressed, so poll
//!   5. GET  /clip/v2/resource/light    — the real bulbs, read off the hardware
//!   6. offer them; whatever is picked is confirmed and adopted
//! ```
//!
//! Everything is CLIP v2 over HTTPS, including setup. Mixing v1 and v2 is a trap: v1 numbers
//! its lights `1`, `2`, `3` while v2 identifies them by UUID, so a flow that pairs with one
//! and commands with the other produces device ids that 404. The bridge's certificate is
//! self-signed — see the note on the controller's TLS layer.
//!
//! Core performs every request and renders every screen without knowing any of this.
//!
//! One device per bulb rather than one per bridge: five bulbs are five devices sharing one
//! loaded module, and each can live in a different room. The bridge address and application
//! key are per-device properties, so two bridges in one house need no special handling.

use driver_sdk::*;
use driver_sdk::{Value, json};

mod button;
mod catalog;
mod sensor;

#[derive(Default)]
pub struct HueBulb;

/// What a given device behind this bridge actually is.
///
/// One loaded module answers for all of them. Core tells `discover` and `setup` which manifest they
/// are running as, but `on_bind`, `on_command` and `on_event` are not given a driver id — so the
/// runtime half has to work it out, and the honest signal is which properties the installer's
/// adoption actually set. A bulb has a `Light id` and a keypad does not; nothing else needs asking.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The hub. Holds the address and key, owns the event stream, and is not in any room.
    Bridge,
    Bulb,
    Motion,
    /// A keypad, remote, wall module or dial.
    Control,
}

impl Role {
    fn of(inst: &Instance) -> Role {
        let has = |property: &str| inst.property(property).as_str().is_some_and(|s| !s.is_empty());
        if has("Light id") {
            Role::Bulb
        } else if has("Motion id") {
            Role::Motion
        } else if has("Button 1 id") || has("Rotary id") {
            Role::Control
        } else {
            Role::Bridge
        }
    }

    /// Every binding this device has, so all of them can be brought online at bind rather than
    /// binding 1 alone. A multi-sensor whose temperature never came online reads as a broken probe.
    fn bindings(self, inst: &Instance) -> Vec<LocalId> {
        let set = |property: &str| inst.property(property).as_str().is_some_and(|s| !s.is_empty());
        match self {
            Role::Bridge | Role::Bulb => vec![1],
            Role::Motion => [("Motion id", 1), ("Temperature id", 2), ("Light level id", 3)]
                .iter()
                .filter(|(p, _)| set(p))
                .map(|(_, id)| *id)
                .collect(),
            Role::Control => [
                ("Button 1 id", 1),
                ("Button 2 id", 2),
                ("Button 3 id", 3),
                ("Button 4 id", 4),
                ("Rotary id", 5),
            ]
            .iter()
            .filter(|(p, _)| set(p))
            .map(|(_, id)| *id)
            .collect(),
        }
    }
}

/// Hue takes brightness as a percentage but treats 0 as "dimmest on", not off — so a level of
/// 0 has to become `on: false` or the bulb sits at 1% instead of going out.
fn level_body(level: u8, ramp_ms: Option<u64>) -> Value {
    let mut body = json!({ "on": { "on": level > 0 } });
    if level > 0 {
        body["dimming"] = json!({ "brightness": level as f64 });
    }
    if let Some(ms) = ramp_ms {
        body["dynamics"] = json!({ "duration": ms.min(6_553_000) });
    }
    body
}

/// CIE xy from hue/saturation at full value. The bridge wants a gamut point, not HSV.
fn hs_to_xy(hue_deg: f64, sat_pct: f64) -> (f64, f64) {
    let h = hue_deg.rem_euclid(360.0) / 60.0;
    let s = (sat_pct / 100.0).clamp(0.0, 1.0);
    let c = s;
    let x = c * (1.0 - ((h % 2.0) - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = 1.0 - c;
    let (r, g, b) = (r + m, g + m, b + m);

    // sRGB -> linear -> CIE XYZ (Wide RGB D65, the transform Signify documents)
    let lin = |u: f64| {
        if u > 0.04045 {
            ((u + 0.055) / 1.055).powf(2.4)
        } else {
            u / 12.92
        }
    };
    let (r, g, b) = (lin(r), lin(g), lin(b));
    let big_x = r * 0.649_926 + g * 0.103_455 + b * 0.197_109;
    let big_y = r * 0.234_327 + g * 0.743_075 + b * 0.022_598;
    let big_z = g * 0.053_077 + b * 1.035_763;
    let sum = big_x + big_y + big_z;
    if sum <= 0.0 {
        return (0.3127, 0.3290); // D65 white; a black point has no chromaticity
    }
    (big_x / sum, big_y / sum)
}

/// The collections the bridge reads once at start, so a controller that has just come up knows
/// where the house stands without waiting for something to change.
///
/// `button` is deliberately not among them, and the omission is the interesting part. A button
/// resource carries its *last* event, which on a bridge that has been up for a week is a press from
/// last Tuesday — reading it at start would report that press as if it had just happened, and every
/// rule attached to that button would fire on a controller restart. Lights, motion, temperature and
/// battery are states and can be read; a press is an event and can only be listened for.
const AT_START: &[&str] = &[
    "light",
    "motion",
    "temperature",
    "light_level",
    "device_power",
];

impl HueBulb {
    fn request(inst: &Instance, body: Value) -> Option<HostCall> {
        let bridge = inst.property("Bridge address").as_str()?.to_string();
        let key = inst.property("Application key").as_str().unwrap_or("");
        let id = inst.property("Light id").as_str()?.to_string();
        if bridge.is_empty() || id.is_empty() {
            return None;
        }
        Some(HostCall::Http(
            HttpRequest::new("PUT", format!("https://{bridge}/clip/v2/resource/light/{id}"))
                .header("hue-application-key", key)
                .json(body.to_string()),
        ))
    }

    /// One frame of the bridge's event stream, as it concerns this bulb.
    ///
    /// The frame is the whole house — a scene recall names eight lights in one push — and every
    /// bulb behind the bridge is handed the same text. Keeping only what names us is the rule
    /// that makes one connection serve twenty-four devices: without it, one light changing at a
    /// wall switch would move all of them.
    ///
    /// ```text
    /// [{"type":"update","data":[{"id":"<rid>","type":"light","dimming":{"brightness":42}}]}]
    /// ```
    fn on_stream(&self, inst: &mut Instance, args: &Args) -> Vec<HostCall> {
        let Some(text) = args.get("data").and_then(Value::as_str) else {
            return Vec::new();
        };
        let Ok(frame) = serde_json::from_str::<Value>(text) else {
            return Vec::new(); // a keep-alive or a partial line core has not finished
        };

        let mut out = Vec::new();
        for update in frame.as_array().into_iter().flatten() {
            for resource in update.get("data").and_then(Value::as_array).into_iter().flatten() {
                out.extend(Self::mine(inst, resource));
            }
        }
        out
    }

    /// One CLIP v2 resource, handed to whichever half of this driver knows what it means.
    ///
    /// Empty for everything that is not about this device, which is most of every frame — the
    /// bridge publishes the whole house on one connection and core hands each frame to all of it.
    /// The dispatch is on the device's role rather than on the resource's `type`, because the
    /// question being answered is "is this mine", and only the device knows which ids are its own.
    fn mine(inst: &mut Instance, resource: &Value) -> Vec<HostCall> {
        match Role::of(inst) {
            Role::Bulb => {
                let mine = inst.property("Light id").as_str().map(str::to_string);
                match (mine, resource.get("id").and_then(Value::as_str)) {
                    (Some(mine), Some(id)) if mine == id => Self::report(inst, resource),
                    _ => Vec::new(),
                }
            }
            Role::Motion => sensor::report(inst, resource),
            Role::Control => button::report(inst, resource),
            // The bridge hearing its own stream. Everything on it belongs to something behind it.
            Role::Bridge => Vec::new(),
        }
    }

    /// Turn a light resource — from a poll or from the stream, they are the same shape — into
    /// what changed. Shared so the two paths cannot drift into disagreeing about a bulb.
    ///
    /// An event carries only the fields that moved: brightness alone when someone drags a
    /// slider, `on` alone when they flick a switch. So a missing field means "unchanged", and
    /// the remembered value fills it in — reading it as zero would darken the bulb on screen
    /// every time somebody changed its colour.
    fn report(inst: &mut Instance, light: &Value) -> Vec<HostCall> {
        let mut out = Vec::new();
        let known_level = inst.scratch.get("level").and_then(Value::as_u64).unwrap_or(100);
        let known_on = inst.scratch.get("on").and_then(Value::as_bool);

        let on = light.pointer("/on/on").and_then(Value::as_bool).or(known_on);
        let brightness = light.pointer("/dimming/brightness").and_then(Value::as_f64);

        if on.is_some() || brightness.is_some() {
            let level = match on {
                Some(false) => 0,
                _ => brightness
                    .map(|b| b.round().clamp(1.0, 100.0) as u64)
                    .unwrap_or(known_level),
            };
            if level > 0 {
                inst.scratch.insert("level".into(), json!(level));
            }
            inst.scratch.insert("on".into(), json!(level > 0));

            let mut a = Args::new();
            a.insert("level".into(), json!(level));
            out.push(HostCall::notify(1, "level_changed", a));
        }

        if let Some(mirek) = light
            .pointer("/color_temperature/mirek")
            .and_then(Value::as_u64)
            .filter(|m| *m > 0)
        {
            let mut a = Args::new();
            a.insert("kelvin".into(), json!(1_000_000 / mirek));
            out.push(HostCall::notify(1, "cct_changed", a));
        }

        if let (Some(x), Some(y)) = (
            light.pointer("/color/xy/x").and_then(Value::as_f64),
            light.pointer("/color/xy/y").and_then(Value::as_f64),
        ) {
            let mut a = Args::new();
            a.insert("hue".into(), json!(x));
            a.insert("sat".into(), json!(y));
            out.push(HostCall::notify(1, "color_changed", a));
        }
        out
    }

    /// Report the change immediately rather than waiting for the bridge.
    ///
    /// The bulb is on a mesh; a round trip is 100–300 ms and the UI would visibly lag. We
    /// state the intent now and let the next poll correct us if the bridge disagreed — which
    /// is what every Hue integration worth using does.
    fn optimistic(level: u8) -> Vec<HostCall> {
        let mut args = Args::new();
        args.insert("level".into(), json!(level));
        vec![HostCall::notify(1, "level_changed", args)]
    }
}

impl DriverModule for HueBulb {
    fn discover(&self, _driver_id: &str, state: &Value, input: &Args) -> (SetupStep, Value) {
        self.step(state, input)
    }

    fn setup(&self, _driver_id: &str, state: &Value, input: &Args) -> (SetupStep, Value) {
        self.step(state, input)
    }

    fn on_command(
        &self,
        inst: &mut Instance,
        _proxy: LocalId,
        cmd: &str,
        args: &Args,
    ) -> Vec<HostCall> {
        let ramp = args.get("ramp_ms").and_then(Value::as_u64);
        let last = inst
            .scratch
            .get("level")
            .and_then(Value::as_u64)
            .unwrap_or(100) as u8;

        let (body, level) = match cmd {
            "on" => {
                // Returning to the level it was at is what people expect from a light switch.
                let restore = if last == 0 { 100 } else { last };
                (level_body(restore, ramp), Some(restore))
            }
            "off" => (level_body(0, ramp), Some(0)),
            "toggle" => {
                let cur = inst
                    .scratch
                    .get("on")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let next = if cur { 0 } else if last == 0 { 100 } else { last };
                (level_body(next, ramp), Some(next))
            }
            "set_level" => {
                let l = args.get("level").and_then(Value::as_u64).unwrap_or(0) as u8;
                (level_body(l, ramp), Some(l))
            }
            "set_cct" => {
                let k = args.get("kelvin").and_then(Value::as_u64).unwrap_or(2700);
                // The bridge speaks mireds, and clamps to the bulb's real gamut.
                let mirek = (1_000_000 / k.max(1)).clamp(153, 500);
                (json!({ "color_temperature": { "mirek": mirek } }), None)
            }
            "set_color" => {
                let h = args.get("hue").and_then(Value::as_f64).unwrap_or(0.0);
                let s = args.get("sat").and_then(Value::as_f64).unwrap_or(0.0);
                let (x, y) = hs_to_xy(h, s);
                (json!({ "color": { "xy": { "x": x, "y": y } } }), None)
            }
            "ramp_start" | "ramp_stop" => {
                // Hue has no open-ended ramp; the UI's held button becomes a long fade.
                let up = args.get("direction").and_then(Value::as_str) == Some("up");
                let target = if cmd == "ramp_stop" {
                    last
                } else if up {
                    100
                } else {
                    1
                };
                (level_body(target, Some(4000)), Some(target))
            }
            other => return vec![HostCall::warn(format!("hue: unhandled `{other}`"))],
        };

        let Some(req) = Self::request(inst, body) else {
            return vec![HostCall::warn(
                "hue: set Bridge address and Light id on this device first",
            )];
        };

        let mut out = vec![req];
        if let Some(l) = level {
            // Remember the last level the bulb was actually AT, so `on` can restore it.
            // Writing 0 here would make the next `on` come back at 1%.
            if l > 0 {
                inst.scratch.insert("level".into(), json!(l));
            }
            inst.scratch.insert("on".into(), json!(l > 0));
            out.extend(Self::optimistic(l));
        }
        if cmd == "set_cct"
            && let Some(k) = args.get("kelvin").and_then(Value::as_u64)
        {
            let mut a = Args::new();
            a.insert("kelvin".into(), json!(k));
            out.push(HostCall::notify(1, "cct_changed", a));
        }
        if cmd == "set_color" {
            let mut a = Args::new();
            a.insert("hue".into(), args.get("hue").cloned().unwrap_or(json!(0.0)));
            a.insert("sat".into(), args.get("sat").cloned().unwrap_or(json!(0.0)));
            out.push(HostCall::notify(1, "color_changed", a));
        }
        out
    }

    /// Ask the bridge where this bulb actually is, rather than assuming.
    ///
    /// Without this a freshly adopted light shows no state until someone commands it, which
    /// reads as broken — and it is: the bulb may well already be on.
    fn on_bind(&self, inst: &mut Instance) -> Vec<HostCall> {
        let role = Role::of(inst);

        // Every binding, not just the first. A motion sensor is three of them and a Tap Dial five,
        // and a binding that never said it was reachable is drawn as a device that is not there.
        let mut out: Vec<HostCall> = role
            .bindings(inst)
            .into_iter()
            .map(|binding| {
                let mut a = Args::new();
                a.insert("online".into(), json!(true));
                HostCall::notify(binding, "online_changed", a)
            })
            .collect();

        // The bridge — the one instance with no resource id of its own — opens the event stream,
        // once, for the whole house.
        //
        // Nothing here is polled, and until this existed nothing was: a bulb changed in the
        // Hue app or at a wall switch never got back to Juno, because the only thing that ever
        // reported a level was this driver stating its own intent after a command. One
        // subscription is what Hue offers and all it wants — core hands every frame to the
        // bulbs behind this bridge, and each keeps the ones naming it.
        if role == Role::Bridge
            && let (Some(bridge), Some(key)) = (
                inst.property("Bridge address").as_str(),
                inst.property("Application key").as_str(),
            )
        {
            let request = format!(
                "GET /eventstream/clip/v2 HTTP/1.1\r\n\
                 Host: {bridge}\r\n\
                 Accept: text/event-stream\r\n\
                 Cache-Control: no-cache\r\n\
                 hue-application-key: {key}\r\n\r\n"
            );
            out.push(HostCall::Tx {
                control: 0,
                data: request.into_bytes(),
            });

            // And one read of each kind of state, so a freshly started controller knows where the
            // house stands without waiting for something to change.
            //
            // One request per collection, not one per device. This used to be the bulb's own job,
            // which meant twenty-four simultaneous GETs at every start — the bridge answered some
            // of them with 429 and the rest arrived as a burst it had no reason to be asked for.
            // The collection endpoints return everything in one answer each, and core hands a
            // bridge's answer to the devices behind it, so each one still picks itself out.
            for collection in AT_START {
                out.push(HostCall::Http(
                    HttpRequest::new(
                        "GET",
                        format!("https://{bridge}/clip/v2/resource/{collection}"),
                    )
                    .header("hue-application-key", key),
                ));
            }
            return out;
        }

        // Everything behind the bridge asks for nothing. Its state arrives with the reads above,
        // and every change after that arrives on the stream.
        out
    }

    /// The bridge answering a state read. Also how a light someone changed in the Hue app
    /// gets back to us on the next poll.
    fn on_event(
        &self,
        inst: &mut Instance,
        _control: LocalId,
        note: &str,
        args: &Args,
    ) -> Vec<HostCall> {
        // A frame off the bridge's event stream. Core hands it to every device behind that
        // bridge, so the first thing to do is find out whether any of it is about this bulb.
        if note == "rx" {
            return self.on_stream(inst, args);
        }
        if note != "http_response" {
            return Vec::new();
        }
        // CLIP v2 answers `{"errors": [], "data": [ … ]}` — one entry for a single resource,
        // everything of that type for a collection. Core hands a bridge's answer to the devices
        // behind it, so this is reached with the whole house in it and has to find its own lines.
        //
        // Plural, because a motion sensor has three: one answer to the temperature read carries
        // every sensor in the house, and the same pass serves all five collections the bridge asks
        // for at start. Anything not naming this device produces nothing.
        let Some(data) = args
            .get("body")
            .and_then(|b| b.get("data"))
            .and_then(Value::as_array)
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for resource in data {
            out.extend(Self::mine(inst, resource));
        }
        out
    }
}


export_driver!(HueBulb);


// ---------------------------------------------------------------------------------------
// Setup flow
// ---------------------------------------------------------------------------------------

/// Where the flow is. Core carries this between calls; the driver stays stateless.
fn phase(state: &Value) -> &str {
    state.get("phase").and_then(Value::as_str).unwrap_or("start")
}

/// Finish, asking first whether the bridge's scenes should come too.
///
/// Asked rather than assumed, because Signify creates four or five in every room whether anybody
/// wanted them or not, and a house that gained sixty scenes by adopting a bridge would be a mess
/// somebody has to clean up by hand. The ones that survive this far already touch a light that was
/// actually picked, so the question is only ever about scenes that would work.
///
/// Skipped entirely when there are none, which is most bridges browsed for a single bulb — a
/// screen that only ever has one answer is a screen not worth showing.
fn ask_about_scenes(
    devices: Vec<Candidate>,
    rules: Vec<ImportedRule>,
    scenes: Vec<ImportedScene>,
) -> (SetupStep, Value) {
    if scenes.is_empty() {
        return (
            SetupStep::Done {
                devices,
                rules,
                scenes,
            },
            Value::Null,
        );
    }

    let names: Vec<&str> = scenes.iter().take(4).map(|s| s.title.as_str()).collect();
    let n = scenes.len();
    (
        SetupStep::Form {
            title: format!("Bring over {n} scene{}?", if n == 1 { "" } else { "s" }),
            body: format!(
                "The bridge has {n} saved — {}{}. Each is a set of levels and colours, one per \
                 light, and they come over exactly as they are. Skipping this changes nothing \
                 else; the lights and remotes are added either way.",
                names.join(", "),
                if n > names.len() { " and others" } else { "" }
            ),
            fields: vec![Field {
                name: "scenes".into(),
                label: "Scenes".into(),
                kind: "choice".into(),
                help: String::new(),
                default: Some(json!("Bring them over")),
                options: vec!["Bring them over".into(), "Leave them".into()],
                required: true,
            }],
        },
        json!({
            "phase": "scene_choice",
            "devices": devices,
            "rules": rules,
            "scenes": scenes,
        }),
    )
}

/// "accessory" / "accessories", for a count that is only known at runtime.
fn plural(n: usize) -> &'static str {
    if n == 1 { "y" } else { "ies" }
}

fn field(name: &str, label: &str, help: &str) -> Field {
    Field {
        name: name.into(),
        label: label.into(),
        kind: "string".into(),
        help: help.into(),
        default: None,
        options: Vec::new(),
        required: true,
    }
}

impl HueBulb {
    /// Offer whatever announced itself, and let an address be typed anyway.
    ///
    /// Core scans for the `_hue._tcp` service this driver's manifest declares and hands the
    /// results in. Nobody should have to go and find an IP for hardware that is already
    /// shouting its own name on the network — but multicast is blocked on plenty of networks,
    /// so typing one has to keep working.
    fn ask_for_address(state: &Value) -> (SetupStep, Value) {
        let found: Vec<&Value> = state
            .get("mdns_candidates")
            .and_then(Value::as_array)
            .map(|v| v.iter().collect())
            .unwrap_or_default();

        let typed = Field {
            name: "address".into(),
            label: "Bridge address".into(),
            kind: "string".into(),
            help: "for example 192.168.1.42".into(),
            default: None,
            options: Vec::new(),
            required: true,
        };

        if found.is_empty() {
            return (
                SetupStep::Form {
                    title: "Find your Hue bridge".into(),
                    body: "Nothing announced itself on the network, so enter the bridge's \
                           address. It is in the Hue app under Settings → My Hue System → the \
                           (i) beside your bridge."
                        .into(),
                    fields: vec![typed],
                },
                json!({ "phase": "probe" }),
            );
        }

        // A table rather than a list: two bridges are told apart by their address and model,
        // and a single line of text cannot show both.
        let rows: Vec<PickRow> = found
            .iter()
            .filter_map(|f| {
                let address = f.get("address")?.as_str()?.to_string();
                let name = f.get("name").and_then(Value::as_str).unwrap_or("Hue bridge");
                // The advertised name is the full service instance; lead with the readable part.
                let short = name.split('.').next().unwrap_or(name).to_string();
                let txt = f.get("txt");
                let model = txt
                    .and_then(|t| t.get("modelid"))
                    .and_then(Value::as_str)
                    .unwrap_or("—")
                    .to_string();
                let id = txt
                    .and_then(|t| t.get("bridgeid"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                Some(PickRow {
                    value: address.clone(),
                    cells: vec![short, address.clone(), model],
                    // A loopback address is this machine, not a bridge anyone else can reach.
                    note: if address.starts_with("127.") {
                        "on this machine only".into()
                    } else if id.is_empty() {
                        String::new()
                    } else {
                        format!("id {id}")
                    },
                })
            })
            .collect();

        (
            SetupStep::Pick {
                title: format!(
                    "Found {} Hue bridge{}",
                    rows.len(),
                    if rows.len() == 1 { "" } else { "s" }
                ),
                body: "Pick the one to set up.".into(),
                columns: vec!["Bridge".into(), "Address".into(), "Model".into()],
                rows,
                field: "address".into(),
                manual: Some(typed),
            },
            json!({ "phase": "probe" }),
        )
    }
}

impl HueBulb {
    /// One step of the flow. Everything Hue-specific lives here rather than in the controller.
    fn step(&self, state: &Value, input: &Args) -> (SetupStep, Value) {
        let address = state
            .get("address")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                input
                    .get("address")
                    .and_then(Value::as_str)
                    .map(|s| s.trim().to_string())
            });

        // Browsing a bridge that already exists: it has an address and a key, so go straight
        // to listing. Making someone press the link button again to add a second bulb would
        // be pointless — the pairing has not expired.
        if phase(state) == "start"
            && state.get("browse").and_then(Value::as_bool) == Some(true)
        {
            let addr = state
                .get("Bridge address")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let key = state
                .get("Application key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if addr.is_empty() || key.is_empty() {
                return (
                    SetupStep::Failed {
                        reason: "this bridge has no address or key set — pair it first".into(),
                    },
                    Value::Null,
                );
            }
            return (
                SetupStep::Fetch {
                    request: HttpRequest::new(
                        "GET",
                        format!("https://{addr}/clip/v2/resource/device"),
                    )
                    .header("hue-application-key", &key),
                    note: "reading what is paired to the bridge".into(),
                },
                json!({ "phase": "devices", "address": addr, "key": key,
                        "browse": true, "parent": state.get("parent") }),
            );
        }

        match phase(state) {
            // Nothing known yet: ask where the bridge is.
            "start" => Self::ask_for_address(state),

            // Confirm it is a bridge before asking anyone to press anything.
            "probe" => {
                let Some(address) = address else {
                    return Self::ask_for_address(state);
                };
                (
                    SetupStep::Fetch {
                        request: HttpRequest::new(
                            "GET",
                            format!("https://{address}/api/config"),
                        ),
                        note: "checking the bridge".into(),
                    },
                    json!({ "phase": "probed", "address": address }),
                )
            }

            "probed" => {
                let address = address.unwrap_or_default();
                let model = input
                    .get("response")
                    .and_then(|r| r.get("modelid"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                // Every Hue bridge reports a BSB model id. Anything else is not one.
                if !model.starts_with("BSB") {
                    return (
                        SetupStep::Failed {
                            reason: format!(
                                "{address} did not answer as a Hue bridge. Check the address — \
                                 the Hue app shows it under Settings → My Hue System."
                            ),
                        },
                        Value::Null,
                    );
                }
                let name = input
                    .get("response")
                    .and_then(|r| r.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("Hue Bridge")
                    .to_string();
                (
                    SetupStep::Instruct {
                        title: format!("Press the link button on {name}"),
                        body: "The round button on top of the bridge. This is how the bridge \
                               confirms someone with physical access approved this — there is \
                               no way around it."
                            .into(),
                        continue_label: "I pressed it".into(),
                    },
                    json!({ "phase": "pair", "address": address, "name": name }),
                )
            }

            // Ask for a key. The bridge refuses until the button is pressed.
            "pair" => {
                let address = address.unwrap_or_default();
                (
                    SetupStep::Fetch {
                        request: HttpRequest::new("POST", format!("https://{address}/api")).json(
                            json!({ "devicetype": "juno#controller",
                                    "generateclientkey": true })
                            .to_string(),
                        ),
                        note: "pairing".into(),
                    },
                    json!({ "phase": "paired", "address": address,
                            "attempt": state.get("attempt").and_then(Value::as_u64).unwrap_or(0) + 1 }),
                )
            }

            "paired" => {
                let address = address.unwrap_or_default();
                let attempt = state.get("attempt").and_then(Value::as_u64).unwrap_or(1);
                let first = input.get("response").and_then(|r| r.get(0)).cloned();

                if let Some(key) = first
                    .as_ref()
                    .and_then(|f| f.pointer("/success/username"))
                    .and_then(Value::as_str)
                {
                    return (
                        SetupStep::Fetch {
                            request: HttpRequest::new(
                                "GET",
                                format!("https://{address}/clip/v2/resource/device"),
                            )
                            .header("hue-application-key", key),
                            note: "reading what is paired to the bridge".into(),
                        },
                        json!({ "phase": "devices", "address": address, "key": key }),
                    );
                }

                let description = first
                    .as_ref()
                    .and_then(|f| f.pointer("/error/description"))
                    .and_then(Value::as_str)
                    .unwrap_or("the bridge did not answer");

                if description.contains("link button") {
                    // Keep asking for about half a minute — long enough to walk to the bridge.
                    if attempt < 30 {
                        return (
                            SetupStep::Wait {
                                title: "Waiting for the link button".into(),
                                body: "Press the round button on top of the bridge.".into(),
                                retry_ms: 1000,
                            },
                            json!({ "phase": "pair", "address": address, "attempt": attempt }),
                        );
                    }
                    return (
                        SetupStep::Failed {
                            reason: "the link button was not pressed in time — start again"
                                .into(),
                        },
                        Value::Null,
                    );
                }
                (
                    SetupStep::Failed {
                        reason: description.to_string(),
                    },
                    Value::Null,
                )
            }

            // Everything paired to the bridge that is not a bulb — sensors, dimmers, wall modules,
            // dials. One request rather than one per resource type, because a device entry names
            // its own services and so arrives already grouped by the thing it is part of.
            "devices" => {
                let address = address.unwrap_or_default();
                let key = state
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let found = catalog::compact(input.get("response"));
                (
                    SetupStep::Fetch {
                        request: HttpRequest::new(
                            "GET",
                            format!("https://{address}/clip/v2/resource/button"),
                        )
                        .header("hue-application-key", &key),
                        note: "reading the buttons".into(),
                    },
                    json!({
                        "phase": "buttons",
                        "address": address,
                        "key": key,
                        "catalog": found,
                        "browse": state.get("browse").and_then(Value::as_bool).unwrap_or(false),
                        "parent": state.get("parent"),
                    }),
                )
            }

            // Which button is which. The device entry lists a remote's buttons as an unordered set,
            // and only the `/button` collection carries `metadata.control_id` — so without this
            // step every rule in the house would be attached to an arbitrary button and the remote
            // would look faulty.
            "buttons" => {
                let address = address.unwrap_or_default();
                let key = state
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let mut found: Vec<Value> = state
                    .get("catalog")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                catalog::order_buttons(&mut found, input.get("response"));
                (
                    SetupStep::Fetch {
                        request: HttpRequest::new(
                            "GET",
                            format!("https://{address}/clip/v2/resource/room"),
                        )
                        .header("hue-application-key", &key),
                        note: "reading the rooms".into(),
                    },
                    json!({
                        "phase": "rooms",
                        "address": address,
                        "key": key,
                        "catalog": found,
                        "browse": state.get("browse").and_then(Value::as_bool).unwrap_or(false),
                        "parent": state.get("parent"),
                    }),
                )
            }

            // Where the bridge says everything lives.
            //
            // The step that decides whether adopting a house is one press or an afternoon. A Hue
            // bridge is usually the only one in the building and its bulbs are named by the app —
            // "Hue color lamp 3", forty times — so the room is the only thing distinguishing one
            // row from another, and somebody already filed all of it once in the Hue app.
            "rooms" => {
                let address = address.unwrap_or_default();
                let key = state
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let mut found: Vec<Value> = state
                    .get("catalog")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                catalog::assign_rooms(&mut found, input.get("response"));
                let names = catalog::room_names(input.get("response"));
                (
                    SetupStep::Fetch {
                        request: HttpRequest::new(
                            "GET",
                            format!("https://{address}/clip/v2/resource/behavior_instance"),
                        )
                        .header("hue-application-key", &key),
                        note: "reading what the switches already control".into(),
                    },
                    json!({
                        "phase": "behaviours",
                        "address": address,
                        "key": key,
                        "catalog": found,
                        "rooms": names,
                        "browse": state.get("browse").and_then(Value::as_bool).unwrap_or(false),
                        "parent": state.get("parent"),
                    }),
                )
            }

            // What the bridge's own automations wire each switch to.
            //
            // Not imported as rules — Hue's semantics are not Juno's, and a rule whose origin
            // nobody can see is worse than no rule. It is read for what it says about identity and
            // place: "controls the Kitchen" is a usable name for a thing the app called "Hue
            // dimmer switch 2", and for a battery remote sitting in no Hue room at all it is the
            // best answer available to where the thing is.
            "behaviours" => {
                let address = address.unwrap_or_default();
                let key = state
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let mut found: Vec<Value> = state
                    .get("catalog")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let names = state.get("rooms").cloned().unwrap_or(Value::Null);
                catalog::apply_behaviours(&mut found, input.get("response"), &names);
                (
                    SetupStep::Fetch {
                        request: HttpRequest::new(
                            "GET",
                            format!("https://{address}/clip/v2/resource/scene"),
                        )
                        .header("hue-application-key", &key),
                        note: "reading the scenes".into(),
                    },
                    json!({
                        "phase": "hue_scenes",
                        "address": address,
                        "key": key,
                        "catalog": found,
                        "rooms": names,
                        "browse": state.get("browse").and_then(Value::as_bool).unwrap_or(false),
                        "parent": state.get("parent"),
                    }),
                )
            }

            // The bridge's own scenes, kept raw until it is known which bulbs were picked.
            //
            // A scene is a list of light services and what each should be doing, and a light
            // service only becomes something a scene can name once somebody has adopted it — so
            // this cannot be reduced yet, only carried.
            "hue_scenes" => {
                let address = address.unwrap_or_default();
                let key = state
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                (
                    SetupStep::Fetch {
                        request: HttpRequest::new(
                            "GET",
                            format!("https://{address}/clip/v2/resource/light"),
                        )
                        .header("hue-application-key", &key),
                        note: "reading the light list".into(),
                    },
                    json!({
                        "phase": "lights",
                        "address": address,
                        "key": key,
                        "catalog": state.get("catalog"),
                        "rooms": state.get("rooms"),
                        "hue_scenes": input.get("response"),
                        "browse": state.get("browse").and_then(Value::as_bool).unwrap_or(false),
                        "parent": state.get("parent"),
                    }),
                )
            }

            // The real bulbs, read off the bridge. CLIP v2 answers
            // `{"errors": [], "data": [ … ]}`, each entry carrying a UUID and its own state.
            // The accessories gathered by the two steps above are offered alongside them, so
            // somebody setting a bridge up picks everything once instead of going round three times.
            "lights" => {
                let address = address.unwrap_or_default();
                let key = state
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let response = input.get("response");

                if let Some(err) = response
                    .and_then(|r| r.get("errors"))
                    .and_then(Value::as_array)
                    .and_then(|e| e.first())
                    .and_then(|e| e.get("description"))
                    .and_then(Value::as_str)
                {
                    return (
                        SetupStep::Failed {
                            reason: format!("the bridge refused the light list: {err}"),
                        },
                        Value::Null,
                    );
                }

                // No lights is no longer a failure. A bridge with a motion sensor and a dimmer on it
                // and no bulbs of its own is a real setup — somebody using Hue accessories to drive
                // Lutron loads — and refusing it because one of the three collections came back
                // empty would be refusing a house that works.
                let data: Vec<Value> = response
                    .and_then(|r| r.get("data"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();

                // Read before the bulbs are built, because each of them wants its room out of it.
                // A bulb is offered by its *light service*, and a Hue room lists *devices* — so the
                // hop from one to the other goes through the device that owns the service, which is
                // exactly what the catalog recorded two steps ago.
                let found: Vec<Value> = state
                    .get("catalog")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();

                let mut options: Vec<Candidate> = data
                    .iter()
                    .filter_map(|light| {
                        let id = light.get("id")?.as_str()?;
                        let name = light
                            .pointer("/metadata/name")
                            .and_then(Value::as_str)
                            .unwrap_or("Hue light");
                        let on = light.pointer("/on/on").and_then(Value::as_bool);
                        // v2 omits `on` for a light the bridge cannot currently see.
                        let state = match on {
                            Some(true) => "bridge reports it on",
                            Some(false) => "bridge reports it off",
                            None => "bridge cannot reach it — is it powered?",
                        };
                        // What the bulb can do decides which commands exist, so read it here
                        // rather than assuming every Hue is a colour bulb. It belongs in
                        // `verified` beside the rest of what the bridge said, not in `kind`:
                        // a tunable white and a colour bulb are both a `light`, and putting
                        // the difference in the kind splits one list of bulbs into three.
                        let can = if light.get("color").is_some() {
                            "colour"
                        } else if light.get("color_temperature").is_some() {
                            "tunable white"
                        } else if light.get("dimming").is_some() {
                            "dimmable"
                        } else {
                            "on/off"
                        };
                        Some(Candidate {
                            label: name.to_string(),
                            kind: "light".into(),
                            driver_id: "signify.hue.bulb".into(),
                            properties: [
                                ("Bridge address".to_string(), json!(address)),
                                ("Application key".to_string(), json!(key)),
                                ("Light id".to_string(), json!(id)),
                            ]
                            .into_iter()
                            .collect(),
                            verified: format!("{can} — {state}"),
                            room: catalog::room_of_light(&found, id).unwrap_or_default(),
                        })
                    })
                    .collect();
                options.sort_by(|a, b| a.label.cmp(&b.label));
                let bulbs = options.len();

                // The sensors and controls gathered earlier, already sorted among themselves.
                let accessories = catalog::candidates(&found, &address, &key);
                let extras = accessories.len();
                options.extend(accessories);

                if options.is_empty() {
                    return (
                        SetupStep::Failed {
                            reason: "the bridge reported nothing paired to it — add your lights \
                                     and accessories in the Hue app first"
                                .into(),
                        },
                        Value::Null,
                    );
                }

                let title = match (bulbs, extras) {
                    (b, 0) => format!("{b} light(s) on this bridge"),
                    (0, e) => format!("{e} accessor{} on this bridge", plural(e)),
                    (b, e) => format!("{b} light(s) and {e} accessor{} on this bridge", plural(e)),
                };

                (
                    SetupStep::Choose {
                        title,
                        body: "Pick the ones to add. Anything the bridge cannot reach is \
                               marked — a bulb switched off at the wall will say so. A remote or \
                               a sensor is added as one device with a binding per button or \
                               measurement, so a rule can trigger on exactly one of them."
                            .into(),
                        options,
                        multiple: true,
                    },
                    json!({
                        "phase": "chosen",
                        "address": address,
                        "key": key,
                        "catalog": state.get("catalog"),
                        "rooms": state.get("rooms"),
                        "hue_scenes": state.get("hue_scenes"),
                        // Carried through, or the last step forgets it is browsing and
                        // offers a second copy of a bridge that is already set up.
                        "browse": state.get("browse").and_then(Value::as_bool).unwrap_or(false),
                        "parent": state.get("parent"),
                    }),
                )
            }

            "chosen" => {
                let address = address.unwrap_or_default();
                let key = state.get("key").and_then(Value::as_str).unwrap_or("").to_string();
                let chosen: Vec<Candidate> = input
                    .get("chosen")
                    .and_then(|c| driver_sdk::serde_json::from_value(c.clone()).ok())
                    .unwrap_or_default();

                // The bridge comes first and carries the connection; everything behind it carries
                // only what makes it individual. Core adopts the parent, then attaches the rest.
                // Browsing an existing bridge: only the children are new.
                //
                // Each candidate keeps the `driver_id` the step that built it chose. It used to be
                // overwritten with the bulb driver here, which was harmless while bulbs were the
                // only thing on offer and would now quietly turn every sensor and every keypad into
                // a light that 404s on its first command.
                // The rules the bridge already has, for whatever was actually picked. Built before
                // the inherited properties are stripped, because that is what identifies a device.
                let catalog: Vec<Value> = state
                    .get("catalog")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();

                if state.get("browse").and_then(Value::as_bool) == Some(true) {
                    let rules = catalog::rules(&catalog, &chosen);
                    let scenes = catalog::scenes(state.get("hue_scenes"), &chosen);
                    let devices: Vec<Candidate> = chosen
                        .into_iter()
                        .map(|mut c| {
                            c.properties.remove("Bridge address");
                            c.properties.remove("Application key");
                            c
                        })
                        .collect();
                    return ask_about_scenes(devices, rules, scenes);
                }

                let mut devices = vec![Candidate {
                    label: state
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("Hue Bridge")
                        .to_string(),
                    kind: "bridge".into(),
                    driver_id: "signify.hue.bridge".into(),
                    properties: [
                        ("Bridge address".to_string(), json!(address)),
                        ("Application key".to_string(), json!(key)),
                    ]
                    .into_iter()
                    .collect(),
                    verified: format!("{} device(s) behind it", chosen.len()),
                    // A bridge lives in a cupboard and serves the whole house. Core refuses to
                    // place infrastructure anyway; saying nothing here is the same answer said
                    // once rather than twice.
                    room: String::new(),
                }];

                // Indices are into the list core is handed, and the bridge is the first entry of
                // it — so the rules are built against that list rather than against `chosen`,
                // which is one shorter and would point every rule at the wrong device.
                for mut c in chosen {
                    // Drop the inherited copies — the bridge holds them now.
                    c.properties.remove("Bridge address");
                    c.properties.remove("Application key");
                    devices.push(c);
                }
                let rules = catalog::rules(&catalog, &devices);
                let scenes = catalog::scenes(state.get("hue_scenes"), &devices);
                ask_about_scenes(devices, rules, scenes)
            }

            // The answer to that question.
            "scene_choice" => {
                let take = input
                    .get("scenes")
                    .and_then(Value::as_str)
                    .is_some_and(|a| a.starts_with("Bring"));
                let devices: Vec<Candidate> = state
                    .get("devices")
                    .cloned()
                    .and_then(|v| driver_sdk::serde_json::from_value(v).ok())
                    .unwrap_or_default();
                let rules: Vec<ImportedRule> = state
                    .get("rules")
                    .cloned()
                    .and_then(|v| driver_sdk::serde_json::from_value(v).ok())
                    .unwrap_or_default();
                let scenes: Vec<ImportedScene> = match take {
                    false => Vec::new(),
                    true => state
                        .get("scenes")
                        .cloned()
                        .and_then(|v| driver_sdk::serde_json::from_value(v).ok())
                        .unwrap_or_default(),
                };
                (
                    SetupStep::Done {
                        devices,
                        rules,
                        scenes,
                    },
                    Value::Null,
                )
            }

            other => (
                SetupStep::Failed {
                    reason: format!("unknown setup phase `{other}`"),
                },
                Value::Null,
            ),
        }
    }
}
