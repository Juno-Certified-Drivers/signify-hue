//! Hue-native scenes.
//!
//! A recall is always one CLIP v2 request. Hue then runs the palette or effect on the bridge and
//! lights; this module intentionally contains no timer, retry loop, or per-frame REST update.

use super::*;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const HUE_EFFECTS_V2: &str = "hue_effects_v2";
const HUE_LIGHTS: &str = "hue_lights";
const HUE_ROOMS: &str = "hue_rooms";
const HUE_SCENES: &str = "hue_scenes";
const HUE_SCENE_LINKS: &str = "hue_scene_links";
const HUE_SCENE_PENDING: &str = "hue_scene_pending";
const HUE_SCENE_PROBLEM: &str = "hue_scene_problem";

fn scene_key(scene: u32) -> String {
    scene.to_string()
}

fn cached<'a>(inst: &'a Instance, key: &str) -> impl Iterator<Item = &'a Value> {
    inst.scratch
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn cached_scene<'a>(inst: &'a Instance, id: &str) -> Option<&'a Value> {
    cached(inst, HUE_SCENES).find(|scene| scene.get("id").and_then(Value::as_str) == Some(id))
}

fn scene_name(scene: &Value) -> &str {
    scene
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .unwrap_or("Unnamed Hue scene")
}

fn has_dynamic_palette(scene: &Value) -> bool {
    scene
        .pointer("/palette/color")
        .and_then(Value::as_array)
        .is_some_and(|colors| !colors.is_empty())
        || scene
            .pointer("/palette/effects")
            .and_then(Value::as_array)
            .is_some_and(|effects| !effects.is_empty())
}

/// Convert Hue's current scene inventory into read-only runtime imports.
///
/// The stable light resource id is the join key Core can resolve against already-adopted child
/// devices. Provider animation is never translated into a Core update loop: the stored handle is
/// recalled by Hue scene id, and these steps are only a static preview/topology description.
fn borrowed_scene_snapshots(scenes: &[Value]) -> Vec<BorrowedSceneSnapshot> {
    scenes
        .iter()
        // A Hue scene created by Juno carries both a visible name token and this appdata marker.
        // Never re-import it as borrowed, even if the local ownership record was lost: ambiguity
        // must remove write authority, not manufacture a second scene with the wrong authority.
        .filter(|scene| {
            !scene
                .pointer("/metadata/appdata")
                .and_then(Value::as_str)
                .is_some_and(|appdata| appdata.starts_with("juno:"))
        })
        .filter_map(|scene| {
            let title = scene_name(scene).to_string();
            let resource = scene.get("id")?.as_str()?.to_string();
            let mut steps = Vec::new();
            for action in scene.get("actions").and_then(Value::as_array)? {
                let light = action.pointer("/target/rid")?.as_str()?.to_string();
                let body = action.get("action")?;
                let on = body
                    .pointer("/on/on")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let brightness = body.pointer("/dimming/brightness").and_then(Value::as_f64);
                let level = match (on, brightness) {
                    (false, _) => 0,
                    (true, Some(value)) => value.round().clamp(1.0, 100.0) as u64,
                    (true, None) => 100,
                };
                let properties = BTreeMap::from([("Light id".to_string(), json!(light))]);
                steps.push(BorrowedSceneStep {
                    properties: properties.clone(),
                    proxy: 1,
                    command: "set_level".into(),
                    args: BTreeMap::from([("level".to_string(), json!(level))]),
                });
                if on
                    && let Some(mirek) = body
                        .pointer("/color_temperature/mirek")
                        .and_then(Value::as_u64)
                        .filter(|mirek| *mirek > 0)
                {
                    steps.push(BorrowedSceneStep {
                        properties,
                        proxy: 1,
                        command: "set_cct".into(),
                        args: BTreeMap::from([("kelvin".to_string(), json!(1_000_000 / mirek))]),
                    });
                }
            }
            (!steps.is_empty()).then_some(BorrowedSceneSnapshot {
                title,
                resource,
                dynamic_palette: has_dynamic_palette(scene),
                steps,
            })
        })
        .collect()
}

fn scene_link(inst: &Instance, scene: u32) -> Option<Value> {
    inst.scratch
        .get(HUE_SCENE_LINKS)
        .and_then(Value::as_object)
        .and_then(|links| links.get(&scene_key(scene)))
        .cloned()
}

fn set_scene_link(inst: &mut Instance, scene: u32, link: Value) {
    if !inst
        .scratch
        .get(HUE_SCENE_LINKS)
        .is_some_and(Value::is_object)
    {
        inst.scratch.insert(HUE_SCENE_LINKS.into(), json!({}));
    }
    if let Some(links) = inst
        .scratch
        .get_mut(HUE_SCENE_LINKS)
        .and_then(Value::as_object_mut)
    {
        links.insert(scene_key(scene), link);
    }
}

fn remove_scene_link(inst: &mut Instance, scene: u32) {
    if let Some(links) = inst
        .scratch
        .get_mut(HUE_SCENE_LINKS)
        .and_then(Value::as_object_mut)
    {
        links.remove(&scene_key(scene));
    }
}

fn owned_scene_name(scene: u32, name: &str) -> (String, String, String) {
    let token = format!("[Juno {scene:08X}]");
    let appdata = format!("juno:{scene:08X}");
    let room = 32usize.saturating_sub(token.chars().count() + 1);
    let prefix: String = name.chars().take(room).collect();
    (format!("{prefix} {token}"), token, appdata)
}

fn member_light_ids(request: &SceneRequest) -> Result<Vec<String>, String> {
    let mut ids = Vec::with_capacity(request.members.len());
    for member in &request.members {
        let Some(id) = member.instance.property("Light id").as_str() else {
            return Err(format!("device {} is not a Hue light", member.device));
        };
        if id.is_empty() {
            return Err(format!("device {} has no Hue light id", member.device));
        }
        ids.push(id.to_string());
    }
    ids.sort();
    ids.dedup();
    if ids.len() != request.members.len() {
        return Err("the scene contains the same Hue light more than once".into());
    }
    if ids.is_empty() {
        return Err("the scene has no Hue lights".into());
    }
    Ok(ids)
}

fn capabilities(member: &SceneMember) -> BTreeSet<String> {
    member
        .instance
        .scratch
        .get(HUE_EFFECTS_V2)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn validate_animation(request: &SceneRequest) -> Result<(), String> {
    if request.animation.palette.len() > 9 {
        return Err("Hue dynamic palettes support at most 9 colors".into());
    }
    if request
        .animation
        .speed
        .is_some_and(|speed| !(0.0..=1.0).contains(&speed))
    {
        return Err("Hue scene speed must be between 0 and 1".into());
    }
    let effects: BTreeSet<&str> = request
        .animation
        .effects
        .iter()
        .map(|effect| effect.effect.as_str())
        .collect();
    if effects.len() > 3 {
        return Err("Hue scene palettes support at most 3 effects".into());
    }
    for requested in &request.animation.effects {
        let Some(member) = request
            .members
            .iter()
            .find(|member| member.device == requested.device)
        else {
            return Err(format!(
                "effect `{}` targets a light outside this scene",
                requested.effect
            ));
        };
        if !capabilities(member).contains(&requested.effect) {
            return Err(format!(
                "device {} did not report `{}` in effects_v2.status.effect_values",
                requested.device, requested.effect
            ));
        }
    }
    Ok(())
}

fn hue_action(member: &SceneMember, effect: Option<&str>) -> Option<Value> {
    let mut action = json!({});
    for stored in &member.actions {
        match stored.command.as_str() {
            "on" => action["on"] = json!({ "on": true }),
            "off" => action["on"] = json!({ "on": false }),
            "set_level" => {
                let level = stored
                    .args
                    .get("level")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(100) as u8;
                action["on"] = json!({ "on": level > 0 });
                if level > 0 {
                    action["dimming"] = json!({ "brightness": level as f64 });
                }
            }
            "set_cct" => {
                let kelvin = stored
                    .args
                    .get("kelvin")
                    .and_then(Value::as_u64)
                    .unwrap_or(2700);
                action["color_temperature"] =
                    json!({ "mirek": (1_000_000 / kelvin.max(1)).clamp(153, 500) });
            }
            "set_color" => {
                let hue = stored
                    .args
                    .get("hue")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                let sat = stored
                    .args
                    .get("sat")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                let (x, y) = hs_to_xy(hue, sat);
                action["color"] = json!({ "xy": { "x": x, "y": y } });
            }
            _ => {}
        }
    }
    if let Some(effect) = effect {
        // Hue scenes still spell this action field `effects`; support is deliberately validated
        // from the newer effects_v2 capability list before this body can be produced.
        action["effects"] = json!({ "effect": effect });
    }
    action
        .as_object()
        .is_some_and(|action| !action.is_empty())
        .then(|| {
            json!({
                "target": {
                    "rid": member.instance.property("Light id").as_str().unwrap_or(""),
                    "rtype": "light"
                },
                "action": action,
            })
        })
}

fn scene_body(request: &SceneRequest, group: Value) -> Result<(Value, String, String), String> {
    validate_animation(request)?;
    let (name, token, appdata) = owned_scene_name(request.scene, &request.name);
    let effects: BTreeMap<DeviceId, &str> = request
        .animation
        .effects
        .iter()
        .map(|effect| (effect.device, effect.effect.as_str()))
        .collect();
    let actions: Vec<Value> = request
        .members
        .iter()
        .filter_map(|member| hue_action(member, effects.get(&member.device).copied()))
        .collect();
    if actions.is_empty() {
        return Err("the scene has no Hue-compatible light actions".into());
    }

    let mut body = json!({
        "metadata": { "name": name, "appdata": appdata },
        "group": group,
        "actions": actions,
    });
    if request.animation.enabled {
        let colors: Vec<Value> = request
            .animation
            .palette
            .iter()
            .map(|color| {
                let mut item = json!({ "color": { "xy": { "x": color.x, "y": color.y } } });
                if let Some(brightness) = color.brightness {
                    item["dimming"] = json!({ "brightness": brightness.clamp(0.0, 100.0) });
                }
                item
            })
            .collect();
        let palette_effects: Vec<Value> = request
            .animation
            .effects
            .iter()
            .map(|effect| effect.effect.as_str())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|effect| json!({ "effect": effect }))
            .collect();
        body["palette"] = json!({ "color": colors, "effects": palette_effects });
        if let Some(speed) = request.animation.speed {
            body["speed"] = json!(speed);
        }
        body["auto_dynamic"] = json!(request.animation.auto_dynamic);
    }
    Ok((body, token, appdata))
}

fn room_light_ids(inst: &Instance, room: &Value) -> Vec<String> {
    let devices: BTreeSet<&str> = room
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|child| child.get("rtype").and_then(Value::as_str) == Some("device"))
        .filter_map(|child| child.get("rid").and_then(Value::as_str))
        .collect();
    let mut lights: Vec<String> = cached(inst, HUE_LIGHTS)
        .filter(|light| {
            light
                .pointer("/owner/rid")
                .and_then(Value::as_str)
                .is_some_and(|owner| devices.contains(owner))
        })
        .filter_map(|light| light.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    lights.sort();
    lights.dedup();
    lights
}

fn exact_group(inst: &Instance, light_ids: &[String]) -> Option<Value> {
    if let Some(zone) = cached_zones(inst)
        .into_iter()
        .find(|zone| zone_light_ids(zone) == light_ids)
    {
        return Some(json!({
            "rid": zone.get("id")?.as_str()?,
            "rtype": "zone",
        }));
    }
    cached(inst, HUE_ROOMS)
        .find(|room| room_light_ids(inst, room) == light_ids)
        .and_then(|room| {
            Some(json!({
                "rid": room.get("id")?.as_str()?,
                "rtype": "room",
            }))
        })
}

fn valid_owned_link(inst: &Instance, request: &SceneRequest) -> Result<Value, String> {
    let link = scene_link(inst, request.scene)
        .ok_or_else(|| "this Juno scene has not been published to Hue".to_string())?;
    if link.get("ownership").and_then(Value::as_str) != Some("juno") {
        return Err("the Hue scene is not recorded as Juno-owned".into());
    }
    if link.get("bridge_id").and_then(Value::as_str) != bridge_id(inst) {
        return Err("the saved scene belongs to a different Hue bridge".into());
    }
    let resource = link
        .get("scene")
        .and_then(Value::as_str)
        .ok_or_else(|| "the saved Hue scene link is incomplete".to_string())?;
    let native = cached_scene(inst, resource)
        .ok_or_else(|| "the Juno-created Hue scene no longer exists".to_string())?;
    let token = link.get("token").and_then(Value::as_str).unwrap_or("");
    let appdata = link.get("appdata").and_then(Value::as_str).unwrap_or("");
    if token.is_empty()
        || appdata.is_empty()
        || !scene_name(native).contains(token)
        || native.pointer("/metadata/appdata").and_then(Value::as_str) != Some(appdata)
    {
        return Err("the Juno ownership markers changed; refusing to modify this Hue scene".into());
    }
    Ok(link)
}

fn refused(problem: impl Into<String>, status: Value) -> SceneResponse {
    SceneResponse {
        disposition: GroupDisposition::Refused,
        problem: Some(problem.into()),
        status,
        ..Default::default()
    }
}

fn status(inst: &Instance, request: &SceneRequest) -> Value {
    let link = scene_link(inst, request.scene).map(|mut link| {
        let valid = link
            .get("scene")
            .and_then(Value::as_str)
            .and_then(|id| cached_scene(inst, id))
            .is_some_and(|native| {
                let token = link.get("token").and_then(Value::as_str).unwrap_or("");
                let appdata = link.get("appdata").and_then(Value::as_str).unwrap_or("");
                !token.is_empty()
                    && !appdata.is_empty()
                    && scene_name(native).contains(token)
                    && native.pointer("/metadata/appdata").and_then(Value::as_str) == Some(appdata)
                    && link.get("bridge_id").and_then(Value::as_str) == bridge_id(inst)
            });
        if let Some(link) = link.as_object_mut() {
            link.remove("token");
            link.remove("appdata");
            link.insert("valid".into(), json!(valid));
        }
        link
    });
    let borrowed = request
        .resource
        .as_deref()
        .and_then(|id| cached_scene(inst, id));
    let effects: Vec<Value> = request
        .members
        .iter()
        .map(|member| {
            json!({
                "device": member.device,
                "values": capabilities(member),
            })
        })
        .collect();
    json!({
        "bridge_ready": bridge_id(inst).is_some(),
        "ownership": request.ownership,
        "borrowed": borrowed.map(|scene| json!({
            "resource": scene.get("id"),
            "name": scene_name(scene),
            "dynamic_palette": has_dynamic_palette(scene),
        })),
        "linked": link,
        "effects_v2": effects,
        "pending": inst.scratch.get(HUE_SCENE_PENDING).is_some(),
        "problem": inst.scratch.get(HUE_SCENE_PROBLEM).cloned().unwrap_or(Value::Null),
    })
}

pub(crate) fn handle(inst: &mut Instance, request: &SceneRequest) -> SceneResponse {
    if Role::of(inst) != Role::Bridge {
        return refused("Hue native scenes must run on the bridge", Value::Null);
    }
    match &request.operation {
        SceneOperation::Status => SceneResponse {
            disposition: GroupDisposition::Handled,
            status: status(inst, request),
            ..Default::default()
        },
        SceneOperation::Recall { mode } => {
            let resource = match request.ownership {
                SceneOwnership::Borrowed => {
                    let Some(resource) = request.resource.as_deref() else {
                        return refused(
                            "the imported Hue scene has no resource id",
                            status(inst, request),
                        );
                    };
                    let Some(native) = cached_scene(inst, resource) else {
                        return refused(
                            "the imported Hue scene no longer exists",
                            status(inst, request),
                        );
                    };
                    if *mode == SceneRecall::Dynamic && !has_dynamic_palette(native) {
                        return refused(
                            "this Hue scene has no dynamic palette",
                            status(inst, request),
                        );
                    }
                    resource.to_string()
                }
                SceneOwnership::Juno => match valid_owned_link(inst, request) {
                    Ok(link) => link
                        .get("scene")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    Err(problem) => return refused(problem, status(inst, request)),
                },
            };
            let action = match mode {
                SceneRecall::Static => "static",
                SceneRecall::Dynamic => "dynamic_palette",
            };
            let Some(call) = bridge_http(
                inst,
                "PUT",
                &format!("/clip/v2/resource/scene/{resource}"),
                Some(json!({ "recall": { "action": action } })),
            ) else {
                return refused(
                    "the Hue bridge connection is not configured",
                    status(inst, request),
                );
            };
            SceneResponse {
                disposition: GroupDisposition::Handled,
                status: status(inst, request),
                calls: vec![call],
                ..Default::default()
            }
        }
        SceneOperation::Detach => {
            if request.ownership == SceneOwnership::Borrowed {
                return refused(
                    "borrowed Hue scenes remain attached and read-only",
                    status(inst, request),
                );
            }
            remove_scene_link(inst, request.scene);
            inst.scratch.remove(HUE_SCENE_PROBLEM);
            SceneResponse {
                disposition: GroupDisposition::Handled,
                status: status(inst, request),
                ..Default::default()
            }
        }
        SceneOperation::Synchronize => {
            if request.ownership == SceneOwnership::Borrowed {
                return refused(
                    "existing Hue scenes are borrowed and can never be modified by Juno",
                    status(inst, request),
                );
            }
            if inst.scratch.contains_key(HUE_SCENE_PENDING) {
                return refused(
                    "another Hue scene write is still in progress",
                    status(inst, request),
                );
            }
            let light_ids = match member_light_ids(request) {
                Ok(ids) => ids,
                Err(problem) => return refused(problem, status(inst, request)),
            };
            let Some(current_bridge) = bridge_id(inst).map(str::to_string) else {
                return refused(
                    "the Hue bridge identity has not loaded yet",
                    status(inst, request),
                );
            };
            let existing = match scene_link(inst, request.scene) {
                Some(_) => match valid_owned_link(inst, request) {
                    Ok(link) => link
                        .get("scene")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    Err(problem) => return refused(problem, status(inst, request)),
                },
                None => None,
            };
            let (name, token, appdata) = owned_scene_name(request.scene, &request.name);

            if let Some(group) = exact_group(inst, &light_ids) {
                let (body, _, _) = match scene_body(request, group.clone()) {
                    Ok(body) => body,
                    Err(problem) => return refused(problem, status(inst, request)),
                };
                return begin_scene_write(
                    inst,
                    request.scene,
                    current_bridge,
                    light_ids,
                    name,
                    token,
                    appdata,
                    group,
                    body,
                    existing,
                    status(inst, request),
                );
            }

            // Hue scenes belong to one room or zone. When this composition spans existing Hue
            // groups, make a dedicated Juno-owned zone instead of editing any pre-existing one.
            let zone_name = name.clone();
            let Some(call) = bridge_http(
                inst,
                "POST",
                "/clip/v2/resource/zone",
                Some(json!({
                    "metadata": { "name": zone_name, "archetype": "other" },
                    "children": zone_children(&light_ids),
                })),
            ) else {
                return refused(
                    "the Hue bridge connection is not configured",
                    status(inst, request),
                );
            };
            // Build with a placeholder group now; the verified zone id replaces it before the
            // scene request is emitted.
            let (body, _, _) =
                match scene_body(request, json!({ "rid": "pending", "rtype": "zone" })) {
                    Ok(body) => body,
                    Err(problem) => return refused(problem, status(inst, request)),
                };
            inst.scratch.insert(
                HUE_SCENE_PENDING.into(),
                json!({
                    "kind": "zone_create",
                    "scene": request.scene,
                    "bridge_id": current_bridge,
                    "light_ids": light_ids,
                    "name": name,
                    "token": token,
                    "appdata": appdata,
                    "body": body,
                    "existing": existing,
                }),
            );
            SceneResponse {
                disposition: GroupDisposition::Queued,
                status: json!({ "pending": "zone_create" }),
                calls: vec![call],
                ..Default::default()
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn begin_scene_write(
    inst: &mut Instance,
    scene: u32,
    bridge_id: String,
    light_ids: Vec<String>,
    name: String,
    token: String,
    appdata: String,
    group: Value,
    body: Value,
    existing: Option<String>,
    current_status: Value,
) -> SceneResponse {
    let (method, path, kind) = match existing.as_deref() {
        Some(resource) => (
            "PUT",
            format!("/clip/v2/resource/scene/{resource}"),
            "scene_update",
        ),
        None => (
            "POST",
            "/clip/v2/resource/scene".to_string(),
            "scene_create",
        ),
    };
    let Some(call) = bridge_http(inst, method, &path, Some(body.clone())) else {
        return refused(
            "the Hue bridge connection is not configured",
            current_status,
        );
    };
    inst.scratch.insert(
        HUE_SCENE_PENDING.into(),
        json!({
            "kind": kind,
            "scene": scene,
            "bridge_id": bridge_id,
            "light_ids": light_ids,
            "name": name,
            "token": token,
            "appdata": appdata,
            "group": group,
            "body": body,
            "resource": existing,
        }),
    );
    SceneResponse {
        disposition: GroupDisposition::Queued,
        status: json!({ "pending": kind }),
        calls: vec![call],
        ..Default::default()
    }
}

pub(crate) fn zone_write_pending(inst: &Instance) -> bool {
    inst.scratch
        .get(HUE_SCENE_PENDING)
        .and_then(|pending| pending.get("kind"))
        .and_then(Value::as_str)
        == Some("zone_create")
}

pub(crate) fn on_zone_response(inst: &mut Instance, args: &Args) -> Vec<HostCall> {
    let body = args.get("body").cloned().unwrap_or(Value::Null);
    let method = args.get("method").and_then(Value::as_str).unwrap_or("");
    let status = args.get("status").and_then(Value::as_u64).unwrap_or(200);
    if status >= 400 {
        inst.scratch.insert(
            HUE_SCENE_PROBLEM.into(),
            json!(format!(
                "Hue rejected the scene zone write with HTTP {status}"
            )),
        );
        inst.scratch.remove(HUE_SCENE_PENDING);
        return Vec::new();
    }
    if method.eq_ignore_ascii_case("POST") {
        let created = body
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|resource| {
                resource.get("rtype").and_then(Value::as_str) == Some("zone")
                    || resource.get("type").and_then(Value::as_str) == Some("zone")
            })
            .and_then(|resource| resource.get("rid").or_else(|| resource.get("id")))
            .and_then(Value::as_str)
            .map(str::to_string);
        if let (Some(zone), Some(pending)) = (
            created,
            inst.scratch
                .get_mut(HUE_SCENE_PENDING)
                .and_then(Value::as_object_mut),
        ) {
            pending.insert("zone".into(), json!(zone));
        } else {
            inst.scratch.insert(
                HUE_SCENE_PROBLEM.into(),
                json!("Hue did not identify the scene zone it created"),
            );
            inst.scratch.remove(HUE_SCENE_PENDING);
            return Vec::new();
        }
        return bridge_http(inst, "GET", "/clip/v2/resource/zone", None)
            .into_iter()
            .collect();
    }
    if method.eq_ignore_ascii_case("GET")
        && let Some(data) = body.get("data").and_then(Value::as_array)
    {
        cache_zone_inventory(inst, data);
        return continue_after_zone(inst);
    }
    Vec::new()
}

fn continue_after_zone(inst: &mut Instance) -> Vec<HostCall> {
    let Some(mut pending) = inst.scratch.get(HUE_SCENE_PENDING).cloned() else {
        return Vec::new();
    };
    let Some(zone_id) = pending
        .get("zone")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Vec::new();
    };
    let Some(zone) = cached_zone(inst, &zone_id) else {
        inst.scratch.insert(
            HUE_SCENE_PROBLEM.into(),
            json!("Hue did not return the scene zone after creating it"),
        );
        inst.scratch.remove(HUE_SCENE_PENDING);
        return Vec::new();
    };
    let expected: Vec<String> = pending
        .get("light_ids")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    let token = pending.get("token").and_then(Value::as_str).unwrap_or("");
    if zone_light_ids(zone) != expected || token.is_empty() || !zone_name(zone).contains(token) {
        inst.scratch.insert(
            HUE_SCENE_PROBLEM.into(),
            json!("Hue returned a scene zone that did not match Juno's requested name and lights"),
        );
        inst.scratch.remove(HUE_SCENE_PENDING);
        return Vec::new();
    }
    let group = json!({ "rid": zone_id, "rtype": "zone" });
    pending["group"] = group.clone();
    pending["body"]["group"] = group;
    let existing = pending
        .get("existing")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    let (method, path, kind) = match existing {
        Some(resource) => (
            "PUT",
            format!("/clip/v2/resource/scene/{resource}"),
            "scene_update",
        ),
        None => (
            "POST",
            "/clip/v2/resource/scene".to_string(),
            "scene_create",
        ),
    };
    pending["kind"] = json!(kind);
    inst.scratch
        .insert(HUE_SCENE_PENDING.into(), pending.clone());
    bridge_http(inst, method, &path, pending.get("body").cloned())
        .into_iter()
        .collect()
}

pub(crate) fn on_collection_response(inst: &mut Instance, args: &Args) -> Option<Vec<HostCall>> {
    let url = args.get("url").and_then(Value::as_str).unwrap_or("");
    let method = args.get("method").and_then(Value::as_str).unwrap_or("");
    let status = args.get("status").and_then(Value::as_u64).unwrap_or(200);
    let body = args.get("body").cloned().unwrap_or(Value::Null);

    if method.eq_ignore_ascii_case("GET") && url.ends_with("/clip/v2/resource/light") {
        if let Some(data) = body.get("data").and_then(Value::as_array) {
            inst.scratch.insert(HUE_LIGHTS.into(), json!(data));
        }
        return Some(Vec::new());
    }
    if method.eq_ignore_ascii_case("GET") && url.ends_with("/clip/v2/resource/room") {
        if let Some(data) = body.get("data").and_then(Value::as_array) {
            inst.scratch.insert(HUE_ROOMS.into(), json!(data));
        }
        return Some(Vec::new());
    }
    if !url.contains("/clip/v2/resource/scene") {
        return None;
    }
    if status >= 400 {
        if inst.scratch.contains_key(HUE_SCENE_PENDING) {
            inst.scratch.insert(
                HUE_SCENE_PROBLEM.into(),
                json!(format!("Hue rejected the scene write with HTTP {status}")),
            );
            inst.scratch.remove(HUE_SCENE_PENDING);
        }
        return Some(Vec::new());
    }
    if method.eq_ignore_ascii_case("POST") {
        let created = body
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|resource| {
                resource.get("rtype").and_then(Value::as_str) == Some("scene")
                    || resource.get("type").and_then(Value::as_str) == Some("scene")
            })
            .and_then(|resource| resource.get("rid").or_else(|| resource.get("id")))
            .and_then(Value::as_str)
            .map(str::to_string);
        if let (Some(scene), Some(pending)) = (
            created,
            inst.scratch
                .get_mut(HUE_SCENE_PENDING)
                .and_then(Value::as_object_mut),
        ) {
            pending.insert("resource".into(), json!(scene));
        } else if inst.scratch.contains_key(HUE_SCENE_PENDING) {
            inst.scratch.insert(
                HUE_SCENE_PROBLEM.into(),
                json!("Hue did not identify the scene it created"),
            );
            inst.scratch.remove(HUE_SCENE_PENDING);
            return Some(Vec::new());
        }
        return Some(
            bridge_http(inst, "GET", "/clip/v2/resource/scene", None)
                .into_iter()
                .collect(),
        );
    }
    if method.eq_ignore_ascii_case("PUT") && inst.scratch.contains_key(HUE_SCENE_PENDING) {
        return Some(
            bridge_http(inst, "GET", "/clip/v2/resource/scene", None)
                .into_iter()
                .collect(),
        );
    }
    if method.eq_ignore_ascii_case("GET")
        && let Some(data) = body.get("data").and_then(Value::as_array)
    {
        inst.scratch.insert(HUE_SCENES.into(), json!(data));
        finalize_pending(inst);
        let scenes = borrowed_scene_snapshots(data);
        return Some(
            (!scenes.is_empty())
                .then_some(HostCall::BorrowedScenes { scenes })
                .into_iter()
                .collect(),
        );
    }
    Some(Vec::new())
}

fn finalize_pending(inst: &mut Instance) {
    let Some(pending) = inst.scratch.get(HUE_SCENE_PENDING).cloned() else {
        return;
    };
    let Some(resource) = pending.get("resource").and_then(Value::as_str) else {
        return;
    };
    let Some(native) = cached_scene(inst, resource) else {
        inst.scratch.insert(
            HUE_SCENE_PROBLEM.into(),
            json!("Hue did not return the scene after writing it"),
        );
        inst.scratch.remove(HUE_SCENE_PENDING);
        return;
    };
    let token = pending.get("token").and_then(Value::as_str).unwrap_or("");
    let appdata = pending.get("appdata").and_then(Value::as_str).unwrap_or("");
    if token.is_empty()
        || appdata.is_empty()
        || !scene_name(native).contains(token)
        || native.pointer("/metadata/appdata").and_then(Value::as_str) != Some(appdata)
    {
        inst.scratch.insert(
            HUE_SCENE_PROBLEM.into(),
            json!("Hue returned a scene without Juno's ownership markers"),
        );
        inst.scratch.remove(HUE_SCENE_PENDING);
        return;
    }
    let Some(scene) = pending.get("scene").and_then(Value::as_u64) else {
        inst.scratch.remove(HUE_SCENE_PENDING);
        return;
    };
    set_scene_link(
        inst,
        scene as u32,
        json!({
            "ownership": "juno",
            "bridge_id": pending.get("bridge_id"),
            "scene": resource,
            "name": scene_name(native),
            "token": token,
            "appdata": appdata,
            "group": pending.get("group"),
            "light_ids": pending.get("light_ids"),
        }),
    );
    inst.scratch.remove(HUE_SCENE_PENDING);
    inst.scratch.remove(HUE_SCENE_PROBLEM);
}

pub(crate) fn cache_effects_v2(inst: &mut Instance, light: &Value) {
    if let Some(values) = light
        .pointer("/effects_v2/status/effect_values")
        .and_then(Value::as_array)
    {
        let values: Vec<Value> = values
            .iter()
            .filter_map(Value::as_str)
            .map(|effect| json!(effect))
            .collect();
        inst.scratch.insert(HUE_EFFECTS_V2.into(), json!(values));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge() -> Instance {
        let mut inst = Instance::new(1);
        inst.properties
            .insert("Bridge address".into(), json!("192.0.2.10"));
        inst.properties
            .insert("Application key".into(), json!("secret"));
        inst.scratch.insert(HUE_BRIDGE_ID.into(), json!("BRIDGE-A"));
        inst
    }

    #[test]
    fn runtime_scene_inventory_imports_only_hue_owned_resources() {
        let scenes = vec![
            json!({
                "id": "hue-relax",
                "metadata": { "name": "Relax" },
                "palette": { "color": [{ "color": { "xy": { "x": 0.4, "y": 0.3 } } }] },
                "actions": [{
                    "target": { "rid": "light-one", "rtype": "light" },
                    "action": {
                        "on": { "on": true },
                        "dimming": { "brightness": 37.0 },
                        "color_temperature": { "mirek": 250 }
                    }
                }]
            }),
            json!({
                "id": "juno-owned",
                "metadata": { "name": "Evening [Juno 00000007]", "appdata": "juno:00000007" },
                "actions": [{
                    "target": { "rid": "light-one", "rtype": "light" },
                    "action": { "dimming": { "brightness": 10.0 } }
                }]
            }),
        ];
        let imported = borrowed_scene_snapshots(&scenes);
        assert_eq!(imported.len(), 1, "Juno-owned scenes stay Juno-owned");
        assert_eq!(imported[0].resource, "hue-relax");
        assert_eq!(imported[0].title, "Relax");
        assert!(imported[0].dynamic_palette);
        assert_eq!(imported[0].steps.len(), 2);
        assert_eq!(
            imported[0].steps[0].properties.get("Light id"),
            Some(&json!("light-one"))
        );
        assert_eq!(imported[0].steps[0].args.get("level"), Some(&json!(37)));
        assert_eq!(imported[0].steps[1].args.get("kelvin"), Some(&json!(4000)));
    }

    fn borrowed(operation: SceneOperation) -> SceneRequest {
        SceneRequest {
            scene: 7,
            name: "Relax".into(),
            ownership: SceneOwnership::Borrowed,
            resource: Some("hue-relax".into()),
            members: Vec::new(),
            animation: SceneAnimation::default(),
            operation,
        }
    }

    fn owned(operation: SceneOperation, effect: &str) -> SceneRequest {
        let mut light = Instance::new(2);
        light.properties.insert("Light id".into(), json!("light-1"));
        light
            .scratch
            .insert(HUE_EFFECTS_V2.into(), json!(["candle", "fire"]));
        SceneRequest {
            scene: 8,
            name: "Fireside".into(),
            ownership: SceneOwnership::Juno,
            resource: None,
            members: vec![SceneMember {
                device: 2,
                proxy: 1,
                instance: light,
                actions: vec![SceneAction {
                    command: "set_level".into(),
                    args: [("level".into(), json!(35))].into_iter().collect(),
                }],
            }],
            animation: SceneAnimation {
                enabled: true,
                palette: vec![ScenePaletteColor {
                    x: 0.62,
                    y: 0.34,
                    brightness: Some(35.0),
                }],
                speed: Some(0.4),
                auto_dynamic: true,
                effects: vec![SceneEffect {
                    device: 2,
                    effect: effect.into(),
                }],
            },
            operation,
        }
    }

    fn http(response: &SceneResponse) -> &HttpRequest {
        let HostCall::Http(request) = &response.calls[0] else {
            panic!("expected one HTTP request")
        };
        request
    }

    #[test]
    fn a_borrowed_scene_can_be_recalled_but_never_synchronized() {
        let mut inst = bridge();
        inst.scratch.insert(
            HUE_SCENES.into(),
            json!([{
                "id": "hue-relax",
                "metadata": { "name": "Relax" },
                "palette": { "color": [{ "color": { "xy": { "x": 0.5, "y": 0.4 } } }] }
            }]),
        );

        let dynamic = handle(
            &mut inst,
            &borrowed(SceneOperation::Recall {
                mode: SceneRecall::Dynamic,
            }),
        );
        assert_eq!(dynamic.disposition, GroupDisposition::Handled);
        assert_eq!(
            dynamic.calls.len(),
            1,
            "recall is one bridge-side operation"
        );
        assert!(http(&dynamic).url.ends_with("/resource/scene/hue-relax"));
        let body: Value = serde_json::from_str(http(&dynamic).body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body.pointer("/recall/action"),
            Some(&json!("dynamic_palette"))
        );

        let sync = handle(&mut inst, &borrowed(SceneOperation::Synchronize));
        assert_eq!(sync.disposition, GroupDisposition::Refused);
        assert!(
            sync.calls.is_empty(),
            "borrowed scenes must never be written"
        );
    }

    #[test]
    fn a_supported_effect_is_published_in_one_native_scene_write() {
        let mut inst = bridge();
        inst.scratch.insert(
            HUE_ZONES.into(),
            json!([{
                "id": "zone-1",
                "type": "zone",
                "metadata": { "name": "Lounge" },
                "children": [{ "rid": "light-1", "rtype": "light" }]
            }]),
        );
        inst.scratch.insert(HUE_SCENES.into(), json!([]));

        let response = handle(&mut inst, &owned(SceneOperation::Synchronize, "candle"));
        assert_eq!(response.disposition, GroupDisposition::Queued);
        assert_eq!(response.calls.len(), 1);
        assert!(http(&response).url.ends_with("/clip/v2/resource/scene"));
        let body: Value = serde_json::from_str(http(&response).body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body.pointer("/actions/0/action/effects/effect"),
            Some(&json!("candle"))
        );
        assert_eq!(
            body.pointer("/palette/effects/0/effect"),
            Some(&json!("candle"))
        );
        assert_eq!(body.get("auto_dynamic"), Some(&json!(true)));
    }

    #[test]
    fn an_effect_absent_from_effects_v2_is_refused_before_http() {
        let mut inst = bridge();
        inst.scratch.insert(
            HUE_ZONES.into(),
            json!([{
                "id": "zone-1",
                "children": [{ "rid": "light-1", "rtype": "light" }]
            }]),
        );
        let response = handle(&mut inst, &owned(SceneOperation::Synchronize, "cosmos"));
        assert_eq!(response.disposition, GroupDisposition::Refused);
        assert!(response.calls.is_empty());
        assert!(response.problem.as_deref().unwrap().contains("effects_v2"));
    }

    #[test]
    fn a_scene_spanning_existing_groups_gets_its_own_verified_juno_zone() {
        let mut inst = bridge();
        inst.scratch.insert(HUE_ZONES.into(), json!([]));
        inst.scratch.insert(HUE_ROOMS.into(), json!([]));
        inst.scratch.insert(HUE_SCENES.into(), json!([]));

        let request = owned(SceneOperation::Synchronize, "fire");
        let create_zone = handle(&mut inst, &request);
        assert!(http(&create_zone).url.ends_with("/clip/v2/resource/zone"));
        let zone_body: Value =
            serde_json::from_str(http(&create_zone).body.as_deref().unwrap()).unwrap();
        let zone_name = zone_body.pointer("/metadata/name").unwrap().clone();

        let created_zone = Args::from([
            ("method".into(), json!("POST")),
            (
                "url".into(),
                json!("https://192.0.2.10/clip/v2/resource/zone"),
            ),
            ("status".into(), json!(200)),
            (
                "body".into(),
                json!({ "data": [{ "rid": "scene-zone", "rtype": "zone" }] }),
            ),
        ]);
        let refresh_zone = on_zone_response(&mut inst, &created_zone);
        assert_eq!(refresh_zone.len(), 1);

        let zone_inventory = Args::from([
            ("method".into(), json!("GET")),
            (
                "url".into(),
                json!("https://192.0.2.10/clip/v2/resource/zone"),
            ),
            ("status".into(), json!(200)),
            (
                "body".into(),
                json!({ "data": [{
                    "id": "scene-zone",
                    "type": "zone",
                    "metadata": { "name": zone_name },
                    "children": [{ "rid": "light-1", "rtype": "light" }]
                }] }),
            ),
        ]);
        let create_scene = on_zone_response(&mut inst, &zone_inventory);
        assert_eq!(create_scene.len(), 1);
        let HostCall::Http(create_scene) = &create_scene[0] else {
            panic!("expected scene create")
        };
        assert!(create_scene.url.ends_with("/clip/v2/resource/scene"));
        let scene_body: Value =
            serde_json::from_str(create_scene.body.as_deref().unwrap()).unwrap();

        let created_scene = Args::from([
            ("method".into(), json!("POST")),
            (
                "url".into(),
                json!("https://192.0.2.10/clip/v2/resource/scene"),
            ),
            ("status".into(), json!(200)),
            (
                "body".into(),
                json!({ "data": [{ "rid": "scene-owned", "rtype": "scene" }] }),
            ),
        ]);
        assert_eq!(
            on_collection_response(&mut inst, &created_scene)
                .unwrap()
                .len(),
            1
        );

        let scene_inventory = Args::from([
            ("method".into(), json!("GET")),
            (
                "url".into(),
                json!("https://192.0.2.10/clip/v2/resource/scene"),
            ),
            ("status".into(), json!(200)),
            (
                "body".into(),
                json!({ "data": [{
                    "id": "scene-owned",
                    "type": "scene",
                    "metadata": scene_body.get("metadata"),
                    "group": scene_body.get("group"),
                    "actions": scene_body.get("actions"),
                    "palette": scene_body.get("palette")
                }] }),
            ),
        ]);
        assert_eq!(
            on_collection_response(&mut inst, &scene_inventory)
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            scene_link(&inst, request.scene)
                .and_then(|link| link.get("scene").cloned()),
            Some(json!("scene-owned"))
        );

        let recall = handle(
            &mut inst,
            &owned(
                SceneOperation::Recall {
                    mode: SceneRecall::Dynamic,
                },
                "fire",
            ),
        );
        assert_eq!(recall.disposition, GroupDisposition::Handled);
        assert_eq!(recall.calls.len(), 1);
        assert!(http(&recall).url.ends_with("/resource/scene/scene-owned"));
    }

    #[test]
    fn the_startup_light_read_caches_each_lights_effects_v2_values() {
        let mut light = Instance::new(2);
        cache_effects_v2(
            &mut light,
            &json!({
                "effects_v2": {
                    "status": { "effect_values": ["candle", "fire", "underwater"] }
                }
            }),
        );
        assert_eq!(
            light.scratch.get(HUE_EFFECTS_V2),
            Some(&json!(["candle", "fire", "underwater"]))
        );
    }
}
