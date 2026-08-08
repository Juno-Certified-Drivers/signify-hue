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

use driver_sdk::{Candidate, Value, json};
use std::collections::BTreeMap;

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
            // A bulb, the bridge itself, or a plug: all real devices, none of them ours. Bulbs come
            // off `/light` in the step after this one, with the colour and dimming detail that
            // decides which capabilities they get — there is nothing to gain by catching them here.
            if buttons.is_empty() && motion.is_empty() {
                return None;
            }

            let one = |list: Vec<String>| list.into_iter().next();
            let mut entry = BTreeMap::new();
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
                button.pointer("/metadata/control_id").and_then(Value::as_u64),
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
        let rid = |field: &str| device.get(field).and_then(Value::as_str).map(str::to_string);

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
            let verified = match (product.as_deref(), dropped) {
                // Say it plainly rather than quietly binding four of five. Somebody holding the
                // remote is the only person who can tell us what the fifth one is.
                (_, n) if n > 0 => format!(
                    "{} buttons, {n} more than this driver has bindings for — report it",
                    buttons.len()
                ),
                (Some(p), _) => format!("{p} — {} button(s)", buttons.len()),
                (None, _) => format!("{} button(s) on {model}", buttons.len()),
            };
            out.push(Candidate {
                label: name.clone(),
                kind: if rid("rotary").is_some() {
                    "dial".into()
                } else {
                    "switch".into()
                },
                driver_id: driver_id.into(),
                properties: properties.clone(),
                verified,
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
            });
        }
    }

    out.sort_by(|a, b| a.label.cmp(&b.label));
    out
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

    #[test]
    fn bulbs_and_the_bridge_are_not_offered_here() {
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
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0]["name"], json!("Hall sensor"));
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
    }
}
