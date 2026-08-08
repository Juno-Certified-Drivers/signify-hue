//! The measuring half of a Hue motion sensor.
//!
//! One physical sensor publishes four services on the bridge — motion, temperature, light level and
//! battery — each with its own resource id and each arriving as its own line on the event stream.
//! This module turns one of those resources into what changed, and knows nothing about how it got
//! here: the same code serves a state read at startup and a push thirty seconds later, so the two
//! cannot drift into disagreeing about a room.
//!
//! Which binding a resource belongs to is decided by the property holding its id, not by the order
//! services came back in. The bridge does not promise an order, and a house where the temperature
//! reading landed on the motion binding would be wrong in a way nobody would think to check.

use driver_sdk::{Args, HostCall, Instance, LocalId, Value, json};

/// The binding each service reports on, and the property holding its resource id.
///
/// Battery is deliberately absent: it is one reading for the whole device and is declared on
/// binding 1 alone, so it is handled separately below rather than given a row here.
const SERVICES: &[(&str, LocalId)] = &[
    ("Motion id", 1),
    ("Temperature id", 2),
    ("Light level id", 3),
];

fn arg(key: &str, value: Value) -> Args {
    let mut a = Args::new();
    a.insert(key.into(), value);
    a
}

/// Read a CLIP v2 measurement, preferring the report form.
///
/// Every service on a v2 bridge carries its value twice: `motion.motion` as it always did, and
/// `motion.motion_report.motion` since the 1.60-ish firmware that added a changed-at timestamp.
/// Signify has marked the flat one deprecated and still sends it, and older bridges send only that
/// — so both are read, newest first, and a bridge of either vintage works without asking which it is.
fn reading<'a>(resource: &'a Value, service: &str, field: &str) -> Option<&'a Value> {
    let block = resource.get(service)?;
    block
        .get(format!("{field}_report"))
        .and_then(|r| r.get(field))
        .or_else(|| block.get(field))
}

/// Lux from Hue's light level.
///
/// The bridge reports `10000 * log10(lux) + 1`, not lux — a scale that keeps one integer useful
/// across a moonlit room and direct sun, and that reads as a nonsense five-digit number if it
/// reaches a dashboard unconverted. Zero is the bridge's "less light than I can measure" and has no
/// logarithm, so it is passed through rather than run backwards into something enormous.
fn lux(level: f64) -> f64 {
    if level <= 0.0 {
        return 0.0;
    }
    let value = 10f64.powf((level - 1.0) / 10_000.0);
    // Two decimals is past the sensor's real precision but keeps a dark room from reading as a
    // bare 0 when it is actually at 0.03 lx, which is the difference the "is it dark?" rule turns on.
    (value * 100.0).round() / 100.0
}

/// One CLIP v2 resource, as it concerns this sensor. Empty when it is about something else.
pub fn report(inst: &Instance, resource: &Value) -> Vec<HostCall> {
    let Some(id) = resource.get("id").and_then(Value::as_str) else {
        return Vec::new();
    };

    for (property, binding) in SERVICES {
        if inst.property(property).as_str() != Some(id) {
            continue;
        }
        return match *property {
            "Motion id" => reading(resource, "motion", "motion")
                .and_then(Value::as_bool)
                .map(|detected| {
                    vec![HostCall::notify(
                        *binding,
                        "detected_changed",
                        arg("detected", json!(detected)),
                    )]
                })
                .unwrap_or_default(),

            "Temperature id" => reading(resource, "temperature", "temperature")
                .and_then(Value::as_f64)
                .map(|c| {
                    vec![HostCall::notify(
                        *binding,
                        "value_changed",
                        arg("value", json!((c * 10.0).round() / 10.0)),
                    )]
                })
                .unwrap_or_default(),

            // The light service names its value `light_level` inside a block called `light`, so the
            // service and field names differ here where they match everywhere else.
            "Light level id" => reading(resource, "light", "light_level")
                .and_then(Value::as_f64)
                .map(|level| {
                    vec![HostCall::notify(
                        *binding,
                        "value_changed",
                        arg("value", json!(lux(level))),
                    )]
                })
                .unwrap_or_default(),

            _ => Vec::new(),
        };
    }

    // Battery, reported on binding 1 — the only binding that declares `has_battery`. Sending it on
    // the others would be refused by the notification gate, correctly: they did not claim to have one.
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
    fn hue_light_level_becomes_lux() {
        // The bridge's own worked example: 10000*log10(1)+1 = 1 is one lux.
        assert_eq!(lux(1.0), 1.0);
        // 10000*log10(100)+1 = 20001 is a brightly lit room.
        assert_eq!(lux(20_001.0), 100.0);
        // Nothing measurable is not "10^-0.0001 lux".
        assert_eq!(lux(0.0), 0.0);
    }

    #[test]
    fn the_deprecated_flat_reading_is_still_read() {
        let old = json!({ "motion": { "motion": true, "motion_valid": true } });
        let new = json!({ "motion": { "motion_report": { "motion": true }, "motion": false } });
        assert_eq!(reading(&old, "motion", "motion").and_then(Value::as_bool), Some(true));
        // Where both are present the report wins — it is the one the bridge keeps current.
        assert_eq!(reading(&new, "motion", "motion").and_then(Value::as_bool), Some(true));
    }
}
