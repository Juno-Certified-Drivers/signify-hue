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
use serde_json::{Value, json};

#[derive(Default)]
pub struct HueBulb;

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
        let mut out = Vec::new();
        let mut a = Args::new();
        a.insert("online".into(), json!(true));
        out.push(HostCall::notify(1, "online_changed", a));

        if let (Some(bridge), Some(key), Some(id)) = (
            inst.property("Bridge address").as_str(),
            inst.property("Application key").as_str(),
            inst.property("Light id").as_str(),
        ) {
            out.push(HostCall::Http(
                HttpRequest::new(
                    "GET",
                    format!("https://{bridge}/clip/v2/resource/light/{id}"),
                )
                .header("hue-application-key", key),
            ));
        }
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
        if note != "http_response" {
            return Vec::new();
        }
        // CLIP v2 answers `{"errors": [], "data": [ … ]}` even for a single resource.
        let Some(light) = args
            .get("body")
            .and_then(|b| b.get("data"))
            .and_then(Value::as_array)
            .and_then(|d| d.first())
        else {
            return Vec::new();
        };

        let mut out = Vec::new();
        let on = light.pointer("/on/on").and_then(Value::as_bool);
        let brightness = light
            .pointer("/dimming/brightness")
            .and_then(Value::as_f64);

        if let Some(on) = on {
            // Hue keeps brightness and on/off separately; Juno's level folds them together,
            // so an off bulb is level 0 whatever brightness it remembers.
            let level = if on {
                brightness.unwrap_or(100.0).round().clamp(1.0, 100.0) as u64
            } else {
                0
            };
            if level > 0 {
                inst.scratch.insert("level".into(), json!(level));
            }
            inst.scratch.insert("on".into(), json!(on));

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
                        format!("https://{addr}/clip/v2/resource/light"),
                    )
                    .header("hue-application-key", &key),
                    note: "reading the light list".into(),
                },
                json!({ "phase": "lights", "address": addr, "key": key,
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
                                format!("https://{address}/clip/v2/resource/light"),
                            )
                            .header("hue-application-key", key),
                            note: "reading the light list".into(),
                        },
                        json!({ "phase": "lights", "address": address, "key": key }),
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

            // The real bulbs, read off the bridge. CLIP v2 answers
            // `{"errors": [], "data": [ … ]}`, each entry carrying a UUID and its own state.
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

                let Some(data) = response
                    .and_then(|r| r.get("data"))
                    .and_then(Value::as_array)
                    .filter(|d| !d.is_empty())
                else {
                    return (
                        SetupStep::Failed {
                            reason: "the bridge reported no lights — are any paired to it?"
                                .into(),
                        },
                        Value::Null,
                    );
                };

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
                        let verified = match on {
                            Some(true) => "bridge reports it on".to_string(),
                            Some(false) => "bridge reports it off".to_string(),
                            None => "bridge cannot reach it — is it powered?".to_string(),
                        };
                        // What the bulb can do decides which commands exist, so read it here
                        // rather than assuming every Hue is a colour bulb.
                        let kind = if light.get("color").is_some() {
                            "colour light"
                        } else if light.get("color_temperature").is_some() {
                            "tunable white"
                        } else if light.get("dimming").is_some() {
                            "dimmable"
                        } else {
                            "light"
                        };
                        Some(Candidate {
                            label: name.to_string(),
                            kind: kind.to_string(),
                            driver_id: "signify.hue.bulb".into(),
                            properties: [
                                ("Bridge address".to_string(), json!(address)),
                                ("Application key".to_string(), json!(key)),
                                ("Light id".to_string(), json!(id)),
                            ]
                            .into_iter()
                            .collect(),
                            verified,
                        })
                    })
                    .collect();
                options.sort_by(|a, b| a.label.cmp(&b.label));

                (
                    SetupStep::Choose {
                        title: format!("{} light(s) on this bridge", options.len()),
                        body: "Pick the ones to add. Anything the bridge cannot reach is \
                               marked — a bulb switched off at the wall will say so."
                            .into(),
                        options,
                        multiple: true,
                    },
                    json!({
                        "phase": "chosen",
                        "address": address,
                        "key": key,
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
                    .and_then(|c| serde_json::from_value(c.clone()).ok())
                    .unwrap_or_default();

                // The bridge comes first and carries the connection; the bulbs carry only
                // what makes them individual. Core adopts the parent, then attaches the rest.
                // Browsing an existing bridge: only the bulbs are new.
                if state.get("browse").and_then(Value::as_bool) == Some(true) {
                    let devices = chosen
                        .into_iter()
                        .map(|mut c| {
                            c.properties.remove("Bridge address");
                            c.properties.remove("Application key");
                            c.driver_id = "signify.hue.bulb".into();
                            c
                        })
                        .collect();
                    return (SetupStep::Done { devices }, Value::Null);
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
                    verified: format!("{} light(s) behind it", chosen.len()),
                }];

                for mut c in chosen {
                    // Drop the inherited copies — the bridge holds them now.
                    c.properties.remove("Bridge address");
                    c.properties.remove("Application key");
                    c.driver_id = "signify.hue.bulb".into();
                    devices.push(c);
                }
                (SetupStep::Done { devices }, Value::Null)
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
