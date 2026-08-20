//! Everything behind the bridge that is not a bulb, read off `/clip/v2/resource/device`.
//!
//! One request rather than one per resource type. A device entry already names its own services —
//! `{"rid": …, "rtype": "motion"}` — so motion, temperature, light level and battery all arrive
//! together and already grouped by the physical thing they belong to. Asking `/motion` and
//! `/temperature` separately would return the same ids with nothing saying which sensor they came
//! off, and re-assembling them by name would be guesswork.
//!
//! The one thing a device entry does not carry is which button is which. Services are a set, not an
//! order, and the bridge makes no promise about the order they come back in — so a second request to
//! `/clip/v2/resource/button` reads `metadata.control_id` and puts them in the order the buttons are
//! printed. Getting this wrong is not subtle: every rule in the house would be attached to the wrong
//! button, and it would look like the remote was faulty.

use driver_sdk::{
    Args, Candidate, ImportedAction, ImportedRule, ImportedScene, ImportedSceneResource, Value,
    json,
};
use std::collections::BTreeMap;

/// Every keypad is one binding, keys and dial alike. See `keypad.toml`.
const KEYPAD: u32 = 1;

/// A device worth offering, reduced to what the manifests need.
///
/// Kept small deliberately: this travels in the setup state, which core carries between every step
/// of the flow, and the raw `/device` answer for a large house is a few hundred kilobytes of
/// archetypes, firmware versions and product images that nothing here reads.
pub fn compact(response: Option<&Value>) -> Vec<Value> {
    let Some(data) = response
        .and_then(|r| r.get("data"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    data.iter()
        .filter_map(|device| {
            let services = device.get("services").and_then(Value::as_array)?;
            let of_type = |wanted: &str| -> Vec<String> {
                services
                    .iter()
                    .filter(|s| s.get("rtype").and_then(Value::as_str) == Some(wanted))
                    .filter_map(|s| Some(s.get("rid")?.as_str()?.to_string()))
                    .collect()
            };

            let buttons = of_type("button");
            let motion = of_type("motion");
            let lights = of_type("light");
            // The bridge itself, and anything whose only services are plumbing. Everything else is
            // kept — including bulbs, which are not offered from here but are the reason this step
            // knows anything: a room lists its *devices*, and a bulb's room can only be found by
            // going from its light service, to the device that owns it, to the room holding that.
            if buttons.is_empty() && motion.is_empty() && lights.is_empty() {
                return None;
            }

            let one = |list: Vec<String>| list.into_iter().next();
            let mut entry = BTreeMap::new();
            entry.insert(
                "id".to_string(),
                json!(device.get("id").and_then(Value::as_str).unwrap_or("")),
            );
            if !lights.is_empty() {
                entry.insert("lights".to_string(), json!(lights));
            }
            entry.insert(
                "name".to_string(),
                json!(
                    device
                        .pointer("/metadata/name")
                        .and_then(Value::as_str)
                        .unwrap_or("Hue device")
                ),
            );
            entry.insert(
                "model".to_string(),
                json!(
                    device
                        .pointer("/product_data/model_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                ),
            );
            entry.insert(
                "product".to_string(),
                json!(
                    device
                        .pointer("/product_data/product_name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                ),
            );
            if !buttons.is_empty() {
                entry.insert("buttons".to_string(), json!(buttons));
            }
            for (key, list) in [
                ("motion", motion),
                ("temperature", of_type("temperature")),
                ("light_level", of_type("light_level")),
                ("rotary", of_type("relative_rotary")),
                ("power", of_type("device_power")),
            ] {
                if let Some(rid) = one(list) {
                    entry.insert(key.to_string(), json!(rid));
                }
            }
            Some(Value::Object(entry.into_iter().collect()))
        })
        .collect()
}

/// Put each device's buttons in the order they are printed on it, using `metadata.control_id`.
///
/// A button the `/button` answer does not mention keeps its place at the end rather than being
/// dropped — a remote with one unrecognised button is still worth having.
pub fn order_buttons(catalog: &mut [Value], response: Option<&Value>) {
    let mut control: BTreeMap<String, u64> = BTreeMap::new();
    if let Some(data) = response
        .and_then(|r| r.get("data"))
        .and_then(Value::as_array)
    {
        for button in data {
            let (Some(id), Some(n)) = (
                button.get("id").and_then(Value::as_str),
                button
                    .pointer("/metadata/control_id")
                    .and_then(Value::as_u64),
            ) else {
                continue;
            };
            control.insert(id.to_string(), n);
        }
    }

    for device in catalog.iter_mut() {
        let Some(buttons) = device.get("buttons").and_then(Value::as_array) else {
            continue;
        };
        let mut ids: Vec<String> = buttons
            .iter()
            .filter_map(|b| Some(b.as_str()?.to_string()))
            .collect();
        // `u64::MAX` for anything unnumbered, so it sorts last instead of first.
        ids.sort_by_key(|id| control.get(id).copied().unwrap_or(u64::MAX));
        device["buttons"] = json!(ids);
    }
}

/// File each device under the Hue room holding it.
///
/// This is the step that makes adopting a whole house bearable. A bridge is usually one bridge for
/// everything, and its bulbs are named by the app — "Hue color lamp 3", forty times over. Somebody
/// already did the work of saying which room each one is in, once, in the Hue app; without reading
/// it back, adopting the bridge means doing that work again from a list where every row looks the
/// same.
///
/// Rooms rather than zones, because a Hue room is exclusive — a device is in exactly one — and a
/// zone is not. "Downstairs" and "Evening" are both zones and neither is where a lamp *is*.
pub fn assign_rooms(catalog: &mut [Value], response: Option<&Value>) {
    let mut of_child: BTreeMap<String, String> = BTreeMap::new();
    for room in rooms(response) {
        for child in room.children {
            of_child.insert(child, room.name.clone());
        }
    }
    for device in catalog.iter_mut() {
        // A room's children are *devices*; a zone's are the light *services* inside them. Both
        // are how somebody groups lights in the Hue app and both come through here, so a device
        // is claimed by either its own id or by any of the lights it owns.
        //
        // Matching only the device id meant a house organised into zones — or a room whose
        // firmware lists services — came through with every bulb unplaced, and the room somebody
        // had already filed all of it under in the Hue app was thrown away.
        let by_device = device
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| of_child.get(id));
        let by_light = || {
            device
                .get("lights")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(Value::as_str)
                .find_map(|light| of_child.get(light))
        };
        // A room set first stays: `assign_rooms` is called once per grouping resource, and the
        // room a device is *in* is a better answer than a zone it also belongs to.
        if device.get("room").and_then(Value::as_str).is_some_and(|r| !r.is_empty()) {
            continue;
        }
        if let Some(name) = by_device.cloned().or_else(|| by_light().cloned()) {
            device["room"] = json!(name);
        }
    }
}

/// Two `{id: name}` maps into one, with the first winning.
///
/// Rooms and zones are both groupings a person made and both are offered as places to file a
/// device; where an id somehow appears in both, the room is the one that says where the thing
/// physically is.
pub fn merge_names(first: Option<&Value>, second: Value) -> Value {
    let mut out = first
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(more) = second.as_object() {
        for (id, name) in more {
            out.entry(id.clone()).or_insert_with(|| name.clone());
        }
    }
    Value::Object(out)
}

/// Room resource id to room name, as a plain object so it can ride in the setup state.
///
/// Stashed rather than re-fetched: the behaviours step needs it and runs after the rooms step, and
/// asking the bridge for the same list twice to avoid carrying a dozen short strings would be the
/// wrong trade.
pub fn room_names(response: Option<&Value>) -> Value {
    let mut out = driver_sdk::serde_json::Map::new();
    if let Some(data) = response
        .and_then(|r| r.get("data"))
        .and_then(Value::as_array)
    {
        for room in data {
            if let (Some(id), Some(name)) = (
                room.get("id").and_then(Value::as_str),
                room.pointer("/metadata/name").and_then(Value::as_str),
            ) {
                out.insert(id.to_string(), json!(name));
            }
        }
    }
    Value::Object(out)
}

/// A room, reduced to the two things worth keeping.
struct Group {
    name: String,
    children: Vec<String>,
}

fn rooms(response: Option<&Value>) -> Vec<Group> {
    let Some(data) = response
        .and_then(|r| r.get("data"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    data.iter()
        .filter_map(|room| {
            Some(Group {
                name: room.pointer("/metadata/name")?.as_str()?.to_string(),
                children: room
                    .get("children")
                    .and_then(Value::as_array)
                    .map(|c| {
                        c.iter()
                            .filter_map(|child| Some(child.get("rid")?.as_str()?.to_string()))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// Every `{rid, rtype}` anywhere inside a value, however deeply it is nested.
///
/// Deliberately structure-blind. A behaviour's `configuration` is shaped by whichever script it is
/// an instance of, those shapes are not documented, and Signify adds new ones — so reading it by
/// walking a known path would work for the dimmer script today and silently stop working for the
/// next one. What every script does have in common is that it refers to things by `rid`, so that
/// is what is looked for and the rest is left alone.
fn referenced(value: &Value, out: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            if let (Some(Value::String(rid)), Some(Value::String(rtype))) =
                (map.get("rid"), map.get("rtype"))
            {
                out.push((rid.clone(), rtype.clone()));
            }
            for v in map.values() {
                referenced(v, out);
            }
        }
        Value::Array(items) => {
            for v in items {
                referenced(v, out);
            }
        }
        _ => {}
    }
}

/// What the bridge's own automations say each control surface drives.
///
/// A Hue dimmer paired to the app is already wired to something — that is what pairing it *did* —
/// and the bridge holds that as a `behavior_instance`. Reading it back is worth two things. It
/// names the switch: "Hue dimmer switch 2" and "controls the Kitchen" are very different rows to
/// pick from in a list. And where a remote is in no Hue room of its own, which is common for a
/// battery device somebody stuck to a wall, what it controls is the best available answer to where
/// it lives.
///
/// A suggestion either way. Nothing here imports a Hue automation as a Juno rule: the two have
/// different semantics, and a rule the household cannot see the origin of is worse than no rule.
pub fn apply_behaviours(catalog: &mut [Value], behaviours: Option<&Value>, room_names: &Value) {
    let Some(data) = behaviours
        .and_then(|r| r.get("data"))
        .and_then(Value::as_array)
    else {
        return;
    };

    for behaviour in data {
        let mut refs = Vec::new();
        referenced(behaviour, &mut refs);
        let targets: Vec<String> = refs
            .iter()
            .filter(|(_, rtype)| rtype == "room")
            .filter_map(|(rid, _)| Some(room_names.get(rid)?.as_str()?.to_string()))
            .collect();
        if targets.is_empty() {
            continue;
        }

        // Which of our devices this behaviour is about. Matched on the device itself or on any of
        // its buttons, because a script may name either.
        for device in catalog.iter_mut() {
            let id = device.get("id").and_then(Value::as_str).unwrap_or("");
            let mut mine: Vec<String> = vec![id.to_string()];
            if let Some(buttons) = device.get("buttons").and_then(Value::as_array) {
                mine.extend(buttons.iter().filter_map(|b| Some(b.as_str()?.to_string())));
            }
            if !refs.iter().any(|(rid, _)| mine.iter().any(|m| m == rid)) {
                continue;
            }
            let mut controls: Vec<String> = device
                .get("controls")
                .and_then(Value::as_array)
                .map(|c| {
                    c.iter()
                        .filter_map(|v| Some(v.as_str()?.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            for t in &targets {
                if !controls.contains(t) {
                    controls.push(t.clone());
                }
            }
            device["controls"] = json!(controls);
        }
    }
}

/// The Hue room a bulb's light service belongs to, found through the device that owns it.
pub fn room_of_light(catalog: &[Value], light: &str) -> Option<String> {
    catalog.iter().find_map(|device| {
        let owns = device
            .get("lights")
            .and_then(Value::as_array)?
            .iter()
            .any(|l| l.as_str() == Some(light));
        (owns).then(|| device.get("room")?.as_str().map(str::to_string))?
    })
}

/// Which manifest a control surface is, from its shape rather than its model number.
///
/// Shape rather than a list of model ids because the bridge is a Zigbee hub and not everything on it
/// is a Hue: a four-button Zigbee remote from another vendor pairs happily and reports presses
/// exactly the same way. Counting buttons works for all of them, where a whitelist would silently
/// exclude anything Signify did not make.
///
/// Three buttons, or five, round up to the four-button manifest. The surplus binding never fires,
/// which is visible and harmless; the alternative — rounding down — would drop a button somebody can
/// physically press, and nothing would say why it did nothing.
fn driver_for(buttons: usize, rotary: bool) -> Option<(&'static str, usize)> {
    Some(match (buttons, rotary) {
        (0, _) => return None,
        (_, true) => ("signify.hue.tap_dial", 4),
        (1, false) => ("signify.hue.smart_button", 1),
        (2, false) => ("signify.hue.wall_switch", 2),
        (_, false) => ("signify.hue.dimmer", 4),
    })
}

/// Everything in the catalog, as devices core can adopt.
pub fn candidates(catalog: &[Value], address: &str, key: &str) -> Vec<Candidate> {
    let mut out = Vec::new();

    for device in catalog {
        let name = device
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Hue device")
            .to_string();
        let product = device
            .get("product")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
            .map(str::to_string);
        let model = device.get("model").and_then(Value::as_str).unwrap_or("");
        let rid = |field: &str| {
            device
                .get(field)
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let controls: Vec<String> = device
            .get("controls")
            .and_then(Value::as_array)
            .map(|c| {
                c.iter()
                    .filter_map(|v| Some(v.as_str()?.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        // Where the bridge says it is, and failing that where what it drives is. The second is the
        // useful one for a battery remote: those are often in no Hue room at all, and "it turns the
        // kitchen lights on and off" is a better answer to where it lives than nothing.
        let room = rid("room")
            .or_else(|| controls.first().cloned())
            .unwrap_or_default();

        let mut properties: BTreeMap<String, Value> = BTreeMap::new();
        properties.insert("Bridge address".into(), json!(address));
        properties.insert("Application key".into(), json!(key));
        if let Some(power) = rid("power") {
            properties.insert("Power id".into(), json!(power));
        }

        let buttons: Vec<String> = device
            .get("buttons")
            .and_then(Value::as_array)
            .map(|b| {
                b.iter()
                    .filter_map(|v| Some(v.as_str()?.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        if let Some((driver_id, slots)) = driver_for(buttons.len(), rid("rotary").is_some()) {
            for (index, id) in buttons.iter().take(slots).enumerate() {
                properties.insert(format!("Button {} id", index + 1), json!(id));
            }
            if let Some(rotary) = rid("rotary") {
                properties.insert("Rotary id".into(), json!(rotary));
            }
            let dropped = buttons.len().saturating_sub(slots);
            let mut verified = match (product.as_deref(), dropped) {
                // Say it plainly rather than quietly binding four of five. Somebody holding the
                // remote is the only person who can tell us what the fifth one is.
                (_, n) if n > 0 => format!(
                    "{} buttons, {n} more than this driver has bindings for — report it",
                    buttons.len()
                ),
                (Some(p), _) => format!("{p} — {} button(s)", buttons.len()),
                (None, _) => format!("{} button(s) on {model}", buttons.len()),
            };
            // What the bridge already has it wired to. The single most useful thing that can be
            // said about a row in a list of things called "Hue dimmer switch 2".
            if !controls.is_empty() {
                verified.push_str(&format!(", controls {}", controls.join(" and ")));
            }
            out.push(Candidate {
                label: name.clone(),
                // The proxy it will be bound to, not the shape of the plastic. "switch" is
                // taken — in Juno it is a light that does not dim — so a dimmer switch
                // tagged with it reads as a lamp. Which of these has a dial is in the
                // product name and the properties; the tag says what it becomes.
                kind: "keypad".into(),
                driver_id: driver_id.into(),
                properties: properties.clone(),
                verified,
                room,
            });
            continue;
        }

        // Not a control surface, so it is the sensor: `compact` kept nothing else.
        if let Some(motion) = rid("motion") {
            properties.insert("Motion id".into(), json!(motion));
            let mut measures = vec!["motion"];
            if let Some(t) = rid("temperature") {
                properties.insert("Temperature id".into(), json!(t));
                measures.push("temperature");
            }
            if let Some(l) = rid("light_level") {
                properties.insert("Light level id".into(), json!(l));
                measures.push("light level");
            }
            out.push(Candidate {
                label: name,
                kind: "sensor".into(),
                driver_id: "signify.hue.motion".into(),
                properties,
                verified: match product {
                    Some(p) => format!("{p} — reports {}", measures.join(", ")),
                    None => format!("reports {}", measures.join(", ")),
                },
                room,
            });
        }
    }

    out.sort_by(|a, b| a.label.cmp(&b.label));
    out
}

/// The rules the bridge already has, as rules this house could have.
///
/// An interpretation, and worth being plain about how much of one. A Hue behaviour says *that* a
/// switch drives a room; the per-button detail is buried in a script whose shape is the script's
/// own business and changes between versions. So what is reconstructed here is the layout every
/// Hue remote has had since the first one: top turns the room on, bottom turns it off, and the two
/// in the middle go up and down.
///
/// That is why these arrive disabled. Core tags them with the driver that read them and puts them
/// on the automations page switched off, so what shows up is a proposal somebody can look at and
/// agree with, not a house that started behaving differently because a bridge was adopted.
///
/// The dial and the scene buttons of a Tap Dial are left alone. Its middle buttons recall scenes,
/// and a scene is the one thing in a Hue bridge that has no Juno representation at all — guessing
/// a brightness for "Relax" would be inventing something nobody asked for.
pub fn rules(catalog: &[Value], offered: &[Candidate]) -> Vec<ImportedRule> {
    let mut out = Vec::new();

    for device in catalog {
        let controls: Vec<String> = device
            .get("controls")
            .and_then(Value::as_array)
            .map(|c| {
                c.iter()
                    .filter_map(|v| Some(v.as_str()?.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let Some(room) = controls.first().cloned() else {
            continue; // the bridge has it wired to nothing we can name
        };
        let name = device
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Hue control");

        // Which offered device this is. Matched on a resource id rather than the label, because
        // two remotes called "Hue dimmer switch" is the ordinary case, not the awkward one.
        let key = |field: &str| device.get(field).and_then(Value::as_str);
        let first = key("buttons")
            .map(str::to_string)
            .or_else(|| {
                device
                    .get("buttons")
                    .and_then(Value::as_array)?
                    .first()?
                    .as_str()
                    .map(str::to_string)
            })
            .or_else(|| key("motion").map(str::to_string));
        let Some(first) = first else { continue };
        let Some(index) = offered.iter().position(|c| {
            ["Button 1 id", "Motion id"]
                .iter()
                .any(|p| c.properties.get(*p).and_then(Value::as_str) == Some(first.as_str()))
        }) else {
            continue; // not offered, so there is nothing to attach a rule to
        };

        let buttons = device
            .get("buttons")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        let rotary = device.get("rotary").is_some();
        let on = |command: &str| {
            vec![ImportedAction::Room {
                room: room.clone(),
                command: command.into(),
                args: Args::new(),
            }]
        };
        // A click steps and a hold ramps, from one rule: Hue repeats while a button is held, and
        // `dim_up` is relative, so the same action is right for both.
        let press = vec!["clicked".to_string()];
        let press_or_hold = vec!["clicked".to_string(), "repeating".to_string()];

        // The whole remote is one binding, so every rule names proxy 1 and says which key it
        // means in `when_params`. That is the same fact the old four-binding shape carried in the
        // proxy id, said in the place a trigger can now read it.
        let mut rule = |suffix: &str, key: u64, events: &[String], command: &str| {
            out.push(ImportedRule {
                label: format!("{name} — {suffix}"),
                when_device: index,
                when_proxy: KEYPAD,
                when_params: [("key".to_string(), json!(key))].into_iter().collect(),
                when_events: events.to_vec(),
                then: on(command),
                ..Default::default()
            });
        };

        match (buttons, rotary) {
            // A motion sensor the bridge already has lighting a room.
            (0, false) => out.push(ImportedRule {
                label: format!("{name} — movement"),
                when_device: index,
                when_proxy: 1,
                when_key: "detected".into(),
                when_becomes: Some(json!(true)),
                then: on("all_lights_on"),
                ..Default::default()
            }),
            // A Tap Dial: only the ring is unambiguous. Its four keys recall scenes.
            //
            // The ring reports on the same binding as the keys and carries no key of its own, so
            // these two name no parameter — the notification is what tells them apart.
            (_, true) => {
                out.push(ImportedRule {
                    label: format!("{name} — turn right"),
                    when_device: index,
                    when_proxy: KEYPAD,
                    when_events: vec!["rotated_clockwise".into()],
                    then: on("dim_up"),
                    ..Default::default()
                });
                out.push(ImportedRule {
                    label: format!("{name} — turn left"),
                    when_device: index,
                    when_proxy: KEYPAD,
                    when_events: vec!["rotated_counter_clockwise".into()],
                    then: on("dim_down"),
                    ..Default::default()
                });
            }
            (1, false) => {
                rule("press", 1, &press, "all_lights_on");
                rule("hold", 1, &["held".to_string()], "all_lights_off");
            }
            (2, false) => {
                rule("top", 1, &press, "all_lights_on");
                rule("bottom", 2, &press, "all_lights_off");
            }
            _ => {
                rule("on", 1, &press, "all_lights_on");
                rule("brighter", 2, &press_or_hold, "dim_up");
                rule("dimmer", 3, &press_or_hold, "dim_down");
                rule("off", 4, &press, "all_lights_off");
            }
        }
    }
    out
}

/// The bridge's own named arrangements, as scenes this house could have.
///
/// A Hue scene is the one thing on a bridge that is pure detail: five lights, each with a
/// brightness and often a colour temperature, decided by somebody sitting in the room. Every other
/// thing here can be described again in a sentence; this cannot, which is exactly why it is worth
/// carrying across.
///
/// Only the lights that were actually adopted contribute. A half-imported "Relax" is a reasonable
/// thing to end up with — the room has fewer lights in Juno than in Hue, and the arrangement of
/// the ones it has is still the arrangement.
///
/// Scenes with no adopted lights at all vanish, which is the common case for the four or five
/// Signify creates in every room whether anybody wanted them or not.
pub fn scenes(response: Option<&Value>, offered: &[Candidate]) -> Vec<ImportedScene> {
    let Some(data) = response
        .and_then(|r| r.get("data"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    // Light service rid -> which offered device it became.
    let light_at = |rid: &str| {
        offered
            .iter()
            .position(|c| c.properties.get("Light id").and_then(Value::as_str) == Some(rid))
    };

    data.iter()
        .filter_map(|scene| {
            let title = scene.pointer("/metadata/name")?.as_str()?.to_string();

            // The rooms are read off the lights the scene touches, not off the group it names.
            //
            // Better than following `group`, and simpler. A Hue scene can belong to a *zone*, and
            // a zone is exactly the thing that does not follow walls — an open-plan kitchen,
            // dining room and living room, or "Downstairs". Resolving the group would need the
            // zone list and would then have to work out which rooms a zone spans, which is what
            // its lights already say. This gets zone scenes right without asking the bridge
            // anything more, and gets room scenes right for the same reason.
            let mut rooms: Vec<String> = Vec::new();

            let mut steps = Vec::new();
            for action in scene.get("actions").and_then(Value::as_array)? {
                let Some(index) = action
                    .pointer("/target/rid")
                    .and_then(Value::as_str)
                    .and_then(light_at)
                else {
                    continue;
                };
                // Where that light is. A scene over an open plan collects two or three this way,
                // which is the honest answer to which rooms it covers.
                if let Some(room) = offered
                    .get(index)
                    .map(|c| c.room.clone())
                    .filter(|r| !r.is_empty())
                    && !rooms.contains(&room)
                {
                    rooms.push(room);
                }
                let body = action.get("action")?;
                let on = body
                    .pointer("/on/on")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let brightness = body.pointer("/dimming/brightness").and_then(Value::as_f64);

                // Off is a level of nothing rather than an `off`, so one step per light says the
                // whole of what that light should be doing — and a scene stays a list of levels.
                let level = match (on, brightness) {
                    (false, _) => 0.0,
                    (true, Some(b)) => b.round().clamp(1.0, 100.0),
                    (true, None) => 100.0,
                };
                steps.push(ImportedAction::Device {
                    device: index,
                    proxy: 1,
                    command: "set_level".into(),
                    args: [("level".to_string(), json!(level as u64))]
                        .into_iter()
                        .collect(),
                });

                // Colour temperature, where the scene sets one and the bulb is off no longer.
                if on
                    && let Some(mirek) = body
                        .pointer("/color_temperature/mirek")
                        .and_then(Value::as_u64)
                        .filter(|m| *m > 0)
                {
                    steps.push(ImportedAction::Device {
                        device: index,
                        proxy: 1,
                        command: "set_cct".into(),
                        args: [("kelvin".to_string(), json!(1_000_000 / mirek))]
                            .into_iter()
                            .collect(),
                    });
                }
            }

            (!steps.is_empty()).then_some(ImportedScene {
                title,
                rooms,
                steps,
                native: Some(ImportedSceneResource {
                    resource: scene.get("id")?.as_str()?.to_string(),
                    dynamic_palette: ["color", "effects"]
                        .into_iter()
                        .any(|kind| {
                            scene
                                .pointer(&format!("/palette/{kind}"))
                                .and_then(Value::as_array)
                                .is_some_and(|values| !values.is_empty())
                        }),
                }),
            })
        })
        .collect()
}


#[cfg(test)]
mod room_chain_tests {
    use super::*;

    /// A bulb's room, the whole way: light service → the device that owns it → the room that
    /// holds that device. Shapes are CLIP v2's, with a room's `children` being *devices*.
    #[test]
    fn a_lights_room_is_found_through_the_device_that_owns_it() {
        let devices = json!({ "data": [
            { "id": "dev-1", "metadata": { "name": "Left Lamp" },
              "product_data": { "product_name": "Hue color lamp" },
              "services": [ { "rtype": "light", "rid": "light-1" } ] }
        ]});
        let mut found = compact(Some(&devices));
        assert_eq!(found.len(), 1, "the bulb's device is kept");

        let rooms = json!({ "data": [
            { "id": "room-1", "metadata": { "name": "Living Room" },
              "children": [ { "rid": "dev-1", "rtype": "device" } ] }
        ]});
        assign_rooms(&mut found, Some(&rooms));

        assert_eq!(room_of_light(&found, "light-1").as_deref(), Some("Living Room"));
    }

    /// The same, for a house organised into zones — whose children are light services rather
    /// than devices. Every bulb used to come through unplaced.
    #[test]
    fn a_zone_naming_light_services_places_its_bulbs_too() {
        let devices = json!({ "data": [
            { "id": "dev-1", "metadata": { "name": "Left Lamp" },
              "product_data": { "product_name": "Hue color lamp" },
              "services": [ { "rtype": "light", "rid": "light-1" } ] }
        ]});
        let mut found = compact(Some(&devices));

        let zones = json!({ "data": [
            { "id": "zone-1", "metadata": { "name": "Living Room" },
              "children": [ { "rid": "light-1", "rtype": "light" } ] }
        ]});
        assign_rooms(&mut found, Some(&zones));

        assert_eq!(room_of_light(&found, "light-1").as_deref(), Some("Living Room"));
    }

    /// A room wins over a zone the same bulb is also in: it is where the bulb *is*.
    #[test]
    fn a_room_already_set_is_not_replaced_by_a_zone() {
        let devices = json!({ "data": [
            { "id": "dev-1", "metadata": { "name": "Left Lamp" },
              "product_data": { "product_name": "Hue color lamp" },
              "services": [ { "rtype": "light", "rid": "light-1" } ] }
        ]});
        let mut found = compact(Some(&devices));
        assign_rooms(&mut found, Some(&json!({ "data": [
            { "id": "room-1", "metadata": { "name": "Kitchen" },
              "children": [ { "rid": "dev-1", "rtype": "device" } ] }
        ]})));
        assign_rooms(&mut found, Some(&json!({ "data": [
            { "id": "zone-1", "metadata": { "name": "Downstairs" },
              "children": [ { "rid": "light-1", "rtype": "light" } ] }
        ]})));
        assert_eq!(room_of_light(&found, "light-1").as_deref(), Some("Kitchen"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: &str, services: Value) -> Value {
        json!({
            "id": format!("dev-{name}"),
            "metadata": { "name": name },
            "product_data": { "model_id": "TEST", "product_name": "Test device" },
            "services": services,
        })
    }

    /// A bulb is kept by `compact` and not offered by `candidates`, and the difference matters.
    ///
    /// It is kept because a Hue room lists devices while a bulb is adopted by its light *service*,
    /// so the only route from one to the other is through the device that owns it. It is not
    /// offered because the `/light` step builds bulbs properly, with the colour and dimming detail
    /// that decides their capabilities. The bridge is neither kept nor offered.
    #[test]
    fn a_bulb_is_kept_for_its_room_but_not_offered_as_a_device() {
        let response = json!({ "data": [
            device("Bridge", json!([{ "rid": "b1", "rtype": "bridge" }])),
            device("Kitchen bulb", json!([{ "rid": "l1", "rtype": "light" }])),
            device("Hall sensor", json!([
                { "rid": "m1", "rtype": "motion" },
                { "rid": "t1", "rtype": "temperature" },
                { "rid": "p1", "rtype": "device_power" },
            ])),
        ]});
        let catalog = compact(Some(&response));
        let names: Vec<&str> = catalog.iter().filter_map(|d| d["name"].as_str()).collect();
        assert_eq!(
            names,
            vec!["Kitchen bulb", "Hall sensor"],
            "the bridge is not a device"
        );

        let offered: Vec<String> = candidates(&catalog, "10.0.0.2", "key")
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert_eq!(offered, vec!["Hall sensor".to_string()]);
    }

    /// The whole point of the exercise: a bulb inherits the room the bridge already filed it under.
    #[test]
    fn a_bulb_finds_its_room_through_the_device_that_owns_it() {
        let devices = json!({ "data": [
            device("Hue color lamp 3", json!([{ "rid": "l1", "rtype": "light" }])),
        ]});
        let mut catalog = compact(Some(&devices));
        let rooms = json!({ "data": [{
            "id": "room-1",
            "metadata": { "name": "Kitchen" },
            "children": [{ "rid": "dev-Hue color lamp 3", "rtype": "device" }],
        }]});
        assign_rooms(&mut catalog, Some(&rooms));
        assert_eq!(room_of_light(&catalog, "l1").as_deref(), Some("Kitchen"));
        assert_eq!(room_of_light(&catalog, "nobody"), None);
    }

    /// A dimmer the bridge already drives a room with becomes four rules, off.
    ///
    /// Four and not six: brighter and dimmer each fire on a click *and* on a repeat, which is one
    /// intention through two events. Hue repeats while a button is held, and `dim_up` is relative,
    /// so the same rule is a step per press and a ramp per hold.
    #[test]
    fn a_dimmer_the_bridge_drives_a_room_with_becomes_four_rules() {
        let devices = json!({ "data": [
            device("Hall dimmer", json!([
                { "rid": "b1", "rtype": "button" }, { "rid": "b2", "rtype": "button" },
                { "rid": "b3", "rtype": "button" }, { "rid": "b4", "rtype": "button" },
            ])),
        ]});
        let mut catalog = compact(Some(&devices));
        let rooms = json!({ "data": [
            { "id": "room-1", "metadata": { "name": "Hall" }, "children": [] },
        ]});
        let behaviours = json!({ "data": [{
            "configuration": {
                "device": { "rid": "dev-Hall dimmer", "rtype": "device" },
                "where": [{ "group": { "rid": "room-1", "rtype": "room" } }],
            },
        }]});
        apply_behaviours(&mut catalog, Some(&behaviours), &room_names(Some(&rooms)));

        let offered = candidates(&catalog, "10.0.0.2", "key");
        let made = rules(&catalog, &offered);

        // Which key is a parameter now, not a binding. All four rules name the one keypad.
        let summary: Vec<(Value, Vec<String>, String)> = made
            .iter()
            .map(|r| {
                let command = match &r.then[0] {
                    ImportedAction::Room { command, .. } => command.clone(),
                    _ => "?".into(),
                };
                (
                    r.when_params.get("key").cloned().unwrap_or(Value::Null),
                    r.when_events.clone(),
                    command,
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    json!(1),
                    vec!["clicked".into()],
                    "all_lights_on".to_string()
                ),
                (
                    json!(2),
                    vec!["clicked".into(), "repeating".into()],
                    "dim_up".into()
                ),
                (
                    json!(3),
                    vec!["clicked".into(), "repeating".into()],
                    "dim_down".into()
                ),
                (json!(4), vec!["clicked".into()], "all_lights_off".into()),
            ]
        );
        assert!(
            made.iter().all(|r| r.when_proxy == KEYPAD),
            "a remote is one binding; the key is what tells the rules apart"
        );
        assert!(made.iter().all(|r| r.when_device == 0));
        assert!(
            made.iter().all(|r| r.label.starts_with("Hall dimmer — ")),
            "each rule says which remote it came off"
        );
    }

    /// A Hue scene comes over as the levels and colours it actually is.
    ///
    /// The detail is the whole value: "Relax" is not a room at 40%, it is one lamp warm and low
    /// and another off, and that is a thing nobody would reconstruct from a description.
    #[test]
    fn a_scene_becomes_a_level_and_a_colour_per_light() {
        // Two lights in two different rooms — an open plan, as far as the bridge is concerned
        // a zone, and as far as anybody standing there is concerned one space.
        let offered = vec![
            Candidate {
                label: "Lamp".into(),
                room: "Lounge".into(),
                properties: [("Light id".to_string(), json!("l1"))]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            Candidate {
                label: "Downlight".into(),
                room: "Kitchen".into(),
                properties: [("Light id".to_string(), json!("l2"))]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        ];
        let response = json!({ "data": [{
            "id": "scene-relax",
            "metadata": { "name": "Relax" },
            "group": { "rid": "room-1", "rtype": "room" },
            "actions": [
                { "target": { "rid": "l1", "rtype": "light" },
                  "action": { "on": { "on": true }, "dimming": { "brightness": 40.0 },
                              "color_temperature": { "mirek": 454 } } },
                // Off, which is a level of nothing rather than a separate command — one step per
                // light says the whole of what that light should be doing.
                { "target": { "rid": "l2", "rtype": "light" },
                  "action": { "on": { "on": false } } },
                // A bulb nobody adopted. The scene still comes over without it.
                { "target": { "rid": "gone", "rtype": "light" },
                  "action": { "on": { "on": true } } },
            ],
        }]});

        let made = scenes(Some(&response), &offered);
        assert_eq!(made.len(), 1);
        assert_eq!(made[0].title, "Relax");
        assert_eq!(
            made[0]
                .native
                .as_ref()
                .map(|native| native.resource.as_str()),
            Some("scene-relax")
        );
        // Read off the lights it touches rather than the group it names, which is what makes a
        // zone scene land on the rooms it actually covers without asking the bridge for zones.
        assert_eq!(
            made[0].rooms,
            vec!["Lounge".to_string(), "Kitchen".to_string()]
        );

        let steps: Vec<(usize, String, Value)> = made[0]
            .steps
            .iter()
            .map(|s| match s {
                ImportedAction::Device {
                    device,
                    command,
                    args,
                    ..
                } => (
                    *device,
                    command.clone(),
                    args.values().next().cloned().unwrap_or(Value::Null),
                ),
                _ => (999, "?".into(), Value::Null),
            })
            .collect();
        assert_eq!(
            steps,
            vec![
                (0, "set_level".to_string(), json!(40)),
                // 1_000_000 / 454 mirek is 2202 K, the warm end of a Hue bulb.
                (0, "set_cct".to_string(), json!(2202)),
                (1, "set_level".to_string(), json!(0)),
            ]
        );
    }

    /// A scene touching nothing that was adopted is not offered at all.
    ///
    /// Signify creates four or five in every room whether anybody wanted them or not, so the ones
    /// that survive this are the ones that would actually do something.
    #[test]
    fn a_scene_with_no_adopted_lights_is_dropped() {
        let offered = vec![Candidate {
            label: "Lamp".into(),
            properties: [("Light id".to_string(), json!("l1"))]
                .into_iter()
                .collect(),
            ..Default::default()
        }];
        let response = json!({ "data": [{
            "metadata": { "name": "Energize" },
            "group": { "rid": "room-9", "rtype": "room" },
            "actions": [{ "target": { "rid": "somebody-elses", "rtype": "light" },
                          "action": { "on": { "on": true } } }],
        }]});
        assert!(scenes(Some(&response), &offered).is_empty());
    }

    /// A switch the bridge drives nothing with produces no rules to guess at.
    #[test]
    fn a_switch_wired_to_nothing_imports_no_rules() {
        let devices = json!({ "data": [
            device("Spare remote", json!([{ "rid": "b1", "rtype": "button" }])),
        ]});
        let catalog = compact(Some(&devices));
        let offered = candidates(&catalog, "10.0.0.2", "key");
        assert!(rules(&catalog, &offered).is_empty());
    }

    /// A dimmer in no Hue room of its own still has a place: the one it drives.
    ///
    /// Battery remotes are routinely filed nowhere, and "controls the Kitchen" is both the best
    /// available answer to where it is and a far better name than "Hue dimmer switch 2".
    #[test]
    fn a_switch_is_placed_and_named_by_what_the_bridge_has_it_driving() {
        let devices = json!({ "data": [
            device("Hue dimmer switch 2", json!([
                { "rid": "btn1", "rtype": "button" },
                { "rid": "btn2", "rtype": "button" },
                { "rid": "btn3", "rtype": "button" },
                { "rid": "btn4", "rtype": "button" },
            ])),
        ]});
        let mut catalog = compact(Some(&devices));
        let rooms = json!({ "data": [
            { "id": "room-1", "metadata": { "name": "Kitchen" }, "children": [] },
        ]});
        let names = room_names(Some(&rooms));
        // The shape a Hue "dimmer switch" behaviour actually has: the device nested somewhere in a
        // configuration whose layout is the script's business, and the room it drives beside it.
        let behaviours = json!({ "data": [{
            "id": "beh-1",
            "configuration": {
                "device": { "rid": "dev-Hue dimmer switch 2", "rtype": "device" },
                "where": [{ "group": { "rid": "room-1", "rtype": "room" } }],
            },
        }]});
        apply_behaviours(&mut catalog, Some(&behaviours), &names);

        let offered = candidates(&catalog, "10.0.0.2", "key");
        assert_eq!(
            offered[0].room, "Kitchen",
            "it goes where the thing it drives is"
        );
        assert!(
            offered[0].verified.contains("controls Kitchen"),
            "and says so in the list: {}",
            offered[0].verified
        );
        // Never "switch": that is a light that does not dim, and the setup list shows this
        // string to somebody deciding what to tick.
        assert_eq!(offered[0].kind, "keypad");
    }

    #[test]
    fn buttons_come_back_in_the_order_they_are_printed() {
        let response = json!({ "data": [device("Dimmer", json!([
            { "rid": "third", "rtype": "button" },
            { "rid": "first", "rtype": "button" },
            { "rid": "second", "rtype": "button" },
        ]))]});
        let mut catalog = compact(Some(&response));
        let buttons = json!({ "data": [
            { "id": "first",  "metadata": { "control_id": 1 } },
            { "id": "second", "metadata": { "control_id": 2 } },
            { "id": "third",  "metadata": { "control_id": 3 } },
        ]});
        order_buttons(&mut catalog, Some(&buttons));
        assert_eq!(catalog[0]["buttons"], json!(["first", "second", "third"]));
    }

    #[test]
    fn the_shape_of_a_remote_picks_its_driver() {
        assert_eq!(driver_for(1, false).unwrap().0, "signify.hue.smart_button");
        assert_eq!(driver_for(2, false).unwrap().0, "signify.hue.wall_switch");
        assert_eq!(driver_for(4, false).unwrap().0, "signify.hue.dimmer");
        assert_eq!(driver_for(4, true).unwrap().0, "signify.hue.tap_dial");
        // Three rounds up rather than down: better an unused binding than an unreachable button.
        assert_eq!(driver_for(3, false).unwrap().0, "signify.hue.dimmer");
        assert!(driver_for(0, false).is_none());
    }

    #[test]
    fn a_sensor_carries_one_property_per_measurement() {
        let response = json!({ "data": [device("Hall", json!([
            { "rid": "m1", "rtype": "motion" },
            { "rid": "t1", "rtype": "temperature" },
            { "rid": "l1", "rtype": "light_level" },
            { "rid": "p1", "rtype": "device_power" },
        ]))]});
        let catalog = compact(Some(&response));
        let found = candidates(&catalog, "10.0.0.2", "key");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].driver_id, "signify.hue.motion");
        assert_eq!(found[0].properties["Motion id"], json!("m1"));
        assert_eq!(found[0].properties["Light level id"], json!("l1"));
        assert_eq!(found[0].properties["Power id"], json!("p1"));
    }

    #[test]
    fn a_dial_is_offered_as_a_tap_dial_with_its_rotary_id() {
        let response = json!({ "data": [device("Study dial", json!([
            { "rid": "b1", "rtype": "button" },
            { "rid": "b2", "rtype": "button" },
            { "rid": "b3", "rtype": "button" },
            { "rid": "b4", "rtype": "button" },
            { "rid": "r1", "rtype": "relative_rotary" },
            { "rid": "p1", "rtype": "device_power" },
        ]))]});
        let mut catalog = compact(Some(&response));
        order_buttons(&mut catalog, None);
        let found = candidates(&catalog, "10.0.0.2", "key");
        assert_eq!(found[0].driver_id, "signify.hue.tap_dial");
        assert_eq!(found[0].properties["Rotary id"], json!("r1"));
        assert_eq!(found[0].properties["Button 4 id"], json!("b4"));
        // A dial is a keypad that also turns. The driver tells them apart; the list does not
        // need a kind of its own to say so.
        assert_eq!(found[0].kind, "keypad");
    }
}
