//! Presses, and the dial on a Tap Dial.
//!
//! A button is the one thing behind a Hue bridge that has nothing to say about its own state. It is
//! not on, it has no level, and reading it tells you only what it did last — so everything here is
//! an event, and the whole point of the module is getting the event onto the right binding with the
//! right name. A rule matches a notification by name at a binding, which is why "the brighter button
//! was held" has to arrive as `held` at binding 2 and not as `button_action { button: 2 }` anywhere.
//!
//! Hue's own vocabulary is transport-shaped — `short_release`, `long_press` — and is translated
//! here rather than passed through. The bridge is describing a switch closure; a house wants to know
//! somebody clicked.

use driver_sdk::{Args, HostCall, Instance, LocalId, Value, json};

/// The property holding each button's resource id, in binding order.
///
/// Five entries covers everything Signify ships: four buttons on a dimmer or a Tap Dial, two on a
/// wall module, one on a smart button. A device only carries the properties it has, and an unset
/// property matches nothing, so the same table serves all four manifests.
/// Which key each control id is, numbered from 1 as the contract asks.
///
/// A key rather than a binding: the whole remote is one binding now, and which key was touched
/// travels as a parameter. That is what lets a rule name one key without the device becoming
/// four devices — see `keypad.toml`.
const BUTTONS: &[(&str, u64)] = &[
    ("Button 1 id", 1),
    ("Button 2 id", 2),
    ("Button 3 id", 3),
    ("Button 4 id", 4),
];

/// Every keypad is one binding. The dial reports on it too.
const KEYPAD: LocalId = 1;

fn arg(key: &str, value: Value) -> Args {
    let mut a = Args::new();
    a.insert(key.into(), value);
    a
}

/// What the bridge calls it, and what it is.
///
/// `initial_press` is deliberately dropped. It is sent at the start of every interaction and is
/// always followed by one of the others, so reporting it would fire a rule on the way to firing the
/// real one — a click would both step and ramp a light. The press that matters is the one you can
/// tell apart from the others, and that is only known once the finger comes off.
fn action(event: &str) -> Option<&'static str> {
    Some(match event {
        "short_release" => "clicked",
        "double_short_release" => "double_clicked",
        "triple_short_release" => "triple_clicked",
        "long_press" => "held",
        "long_release" => "released",
        "repeat" => "repeating",
        _ => return None,
    })
}

/// The event this resource is reporting, newest form first.
///
/// As with the sensor services, v2 carries it twice — `button.last_event` as it always did, and
/// `button.button_report.event` since the firmware that added a timestamp. Both are read so a
/// bridge of either vintage works.
fn event(resource: &Value) -> Option<&str> {
    resource
        .pointer("/button/button_report/event")
        .or_else(|| resource.pointer("/button/last_event"))
        .and_then(Value::as_str)
}

fn rotation(resource: &Value) -> Option<(&str, u64)> {
    let last = resource
        .pointer("/relative_rotary/rotary_report/rotation")
        .or_else(|| resource.pointer("/relative_rotary/last_event/rotation"))?;
    let direction = last.get("direction").and_then(Value::as_str)?;
    // A report with no steps is the dial saying it started moving, which is not yet a rotation.
    let steps = last.get("steps").and_then(Value::as_u64)?;
    Some((direction, steps))
}

/// One CLIP v2 resource, as it concerns this control. Empty when it is about something else.
pub fn report(inst: &Instance, resource: &Value) -> Vec<HostCall> {
    let Some(id) = resource.get("id").and_then(Value::as_str) else {
        return Vec::new();
    };

    for (property, key) in BUTTONS {
        if inst.property(property).as_str() != Some(id) {
            continue;
        }
        let Some(name) = event(resource).and_then(action) else {
            return Vec::new(); // an initial press, or a shape this firmware invented
        };
        // `metadata.control_id` from /button is what makes key 1 the same key every time. It
        // used to choose which of four bindings to emit on; it now fills the parameter, which is
        // the same fact carried a shorter way.
        return vec![
            HostCall::notify(KEYPAD, name, arg("key", json!(key))),
            // The tile has nothing else to draw. A keypad is not on or off, and one showing blank
            // for ever reads as a remote that is not working — this is how somebody sees it is.
            //
            // Named, because "held" on a four-key remote does not say which.
            HostCall::SetState {
                proxy: KEYPAD,
                key: "last_action".into(),
                // The number, not the label. `key_labels` is a capability on the contract, so
                // the UI already knows this key is called "Brighter" — writing the name in here
                // too would be a second copy to keep in step, and the one that goes stale when
                // somebody renames it.
                value: json!(format!("key {key} {name}")),
            },
        ];
    }

    // The dial reports on the same binding as the keys; only the notification differs.
    let rotary_property = "Rotary id";
    if inst.property(rotary_property).as_str() == Some(id) {
        let Some((direction, steps)) = rotation(resource) else {
            return Vec::new();
        };
        // Anticlockwise is anything that is not clockwise: the bridge spells it `counter_clock_wise`
        // and has changed the spelling of these before, so the safe half of the test is the one
        // whose word cannot be mistaken for the other.
        let name = if direction == "clock_wise" {
            "rotated_clockwise"
        } else {
            "rotated_counter_clockwise"
        };
        return vec![
            HostCall::notify(KEYPAD, name, arg("steps", json!(steps))),
            HostCall::SetState {
                proxy: KEYPAD,
                key: "last_action".into(),
                value: json!(name),
            },
        ];
    }

    // Battery, on binding 1 — the only binding declaring `has_battery`, for the reason given in the
    // manifests: one remote has one battery, not one per button.
    if inst.property("Power id").as_str() == Some(id) {
        if let Some(percent) = resource
            .pointer("/power_state/battery_level")
            .and_then(Value::as_u64)
        {
            return vec![HostCall::notify(
                1,
                "battery_changed",
                arg("percent", json!(percent.min(100))),
            )];
        }
    }

    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_press_on_the_way_to_a_click_is_not_reported() {
        // Every interaction starts with this. Acting on it would fire a rule twice.
        assert_eq!(action("initial_press"), None);
        assert_eq!(action("short_release"), Some("clicked"));
        assert_eq!(action("long_press"), Some("held"));
        assert_eq!(action("long_release"), Some("released"));
        assert_eq!(action("repeat"), Some("repeating"));
    }

    #[test]
    fn both_firmware_shapes_of_an_event_are_read() {
        let old = json!({ "button": { "last_event": "short_release" } });
        let new = json!({ "button": { "button_report": { "event": "long_press" },
                                      "last_event": "short_release" } });
        assert_eq!(event(&old), Some("short_release"));
        // The report is the current one where a bridge sends both.
        assert_eq!(event(&new), Some("long_press"));
    }

    #[test]
    fn a_dial_that_has_only_started_moving_is_not_a_rotation() {
        let started = json!({ "relative_rotary": { "last_event": {
            "action": "start", "rotation": { "direction": "clock_wise" } } } });
        assert_eq!(rotation(&started), None);

        let turned = json!({ "relative_rotary": { "rotary_report": {
            "rotation": { "direction": "counter_clock_wise", "steps": 30 } } } });
        assert_eq!(rotation(&turned), Some(("counter_clock_wise", 30)));
    }
}
