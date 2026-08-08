# Philips Hue

Hue bridges and everything paired to them — bulbs, motion sensors, dimmers, wall modules and
dials — over CLIP v2.

One package, several drivers. The bridge is a device in its own right: it holds the address and
the paired application key once, and everything behind it reads them from there — so a bridge
that moves to a new IP is edited in one place rather than six. They ship together because a
version skew between a bridge and its children is invisible until something stops working.

| Driver | Proxies | What it is |
| --- | --- | --- |
| `signify.hue.bridge` | `bridge` | The hub. Holds the connection, owns the event stream. |
| `signify.hue.bulb` | `light` | A bulb. |
| `signify.hue.motion` | `sensor` ×3 | Motion, temperature and light level — one binding each. |
| `signify.hue.dimmer` | `button` ×4 | The four-button dimmer switch. |
| `signify.hue.tap_dial` | `button` ×5 | Four buttons and the dial. |
| `signify.hue.wall_switch` | `button` ×2 | The module behind an existing rocker. |
| `signify.hue.smart_button` | `button` | One button. |

### Why so many bindings

A multi-sensor is three bindings and a dimmer is four, rather than one device reporting several
things, because a rule triggers on a binding and a notification name. "When the hall is dark and
somebody moves" is two triggers on two bindings and cannot be written against one; "when the
brighter button is held" would fire on all four buttons if they shared a binding.

The same reasoning splits the actions apart — `clicked`, `held`, `repeating` and `released` are
separate notifications rather than values of one — so a click can step a light while a hold ramps
it.

Which control-surface driver a device becomes is decided by counting its buttons and looking for
a dial, not by matching a model number: the bridge is a Zigbee hub, and a four-button remote from
another vendor pairs with it and reports presses exactly the same way.

## Setup

Discovered over mDNS (`_hue._tcp`). Pairing needs the bridge's link button pressed; the driver
polls until it is, because the bridge refuses the request until then.

Setup then reads three collections in turn — `/device` for what is paired and how its services
group, `/button` for `control_id` so buttons are offered in the order they are printed on the
remote, and `/light` for the bulbs — and offers all of it at once. A bridge carrying accessories
and no bulbs of its own is a real setup and is not refused.

## State

The bridge opens `/eventstream/clip/v2` once for the whole house, and core hands every frame to
the devices behind it — each keeps the lines naming its own resource ids. At startup the bridge
also reads `light`, `motion`, `temperature`, `light_level` and `device_power`, so a controller
that has just come up knows where the house stands without waiting for something to change.

`button` is deliberately not among them. A button resource carries its *last* event, so reading
it at startup would report a press from whenever it happened as though it had just occurred, and
every rule attached to that button would fire on a restart. State can be read; an event can only
be listened for.

Everything is CLIP v2, including setup. Mixing v1 and v2 is a trap: v1 numbers its lights
`1`, `2`, `3` while v2 identifies them by UUID, so a flow that pairs with one and commands with
the other produces device ids that 404.

The bridge's certificate is self-signed — the controller identifies it by the pairing secret it
issued, not by a public CA.

## Building

```bash
cargo build --release
```

Releases are built by [`junohouse/driver-ci`](https://github.com/junohouse/driver-ci): push to
`main` for a beta, tag `v1.2.0` for a release. To work on this against a local core checkout,
uncomment the `[patch]` block in `Cargo.toml`.
