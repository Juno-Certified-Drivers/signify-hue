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

Setup then reads five collections in turn and offers all of it at once:

| Read | For |
| --- | --- |
| `/device` | What is paired, and how each thing's services group |
| `/button` | `control_id`, so buttons are offered in the order printed on the remote |
| `/room` | Where the bridge says everything lives |
| `/behavior_instance` | What the bridge already has each switch driving |
| `/light` | The bulbs, with the colour and dimming detail that decides their capabilities |

A bridge carrying accessories and no bulbs of its own is a real setup and is not refused.

### Taking the bridge's word for where things are

The last two reads are what make adopting a whole house bearable. A Hue bridge is usually the
only one in the building, and its bulbs are named by the app — "Hue color lamp 3", forty times
over. Somebody already sat down and filed every one of them into a room. Without reading that
back, adopting the bridge means doing the same work again from a list where every row looks
identical.

So each candidate carries the Hue room it is in, and core matches or creates that room at the
moment of adoption. It is a **suggestion**, not an instruction: nothing is created behind
anybody's back, the list is on screen when it happens, and the driver cannot rename or delete a
room. Rooms rather than zones, because a Hue room is exclusive — a device is in exactly one — and
"Downstairs" and "Evening" are both zones and neither is where a lamp *is*.

`behavior_instance` is read for the same reason and answers a second question. A dimmer paired
through the app is already wired to something; that is what pairing it did. Knowing it drives the
kitchen both names it — "controls Kitchen" beats "Hue dimmer switch 2" in a list you have to pick
from — and places it, since battery remotes are routinely in no Hue room at all and what a switch
drives is the best available answer to where it is.

### Bringing the rules over

The behaviours are also offered as Juno automations, and they arrive **switched off** and tagged
with the driver that read them. That is the whole of what makes it safe: an imported rule is this
driver's *interpretation* of somebody else's automation, and nothing should start behaving
differently in a house because a bridge was adopted. They land on the Automations page as
proposals with their origin written on them.

How much of an interpretation is worth being plain about. A Hue behaviour says *that* a switch
drives a room; the per-button detail lives in a script whose shape is the script's own business and
changes between versions. So what is reconstructed is the layout every Hue remote has had since the
first one:

| Button | Rule |
| --- | --- |
| Top | `clicked` → room `all_lights_on` |
| Brighter | `clicked` **and** `repeating` → room `dim_up` |
| Dimmer | `clicked` **and** `repeating` → room `dim_down` |
| Bottom | `clicked` → room `all_lights_off` |

Brighter and dimmer take two triggers because that is one intention: Hue repeats while a button is
held, and `dim_up` is relative, so the same rule gives a step per click and a ramp per hold. A
motion sensor the bridge already lights a room with becomes one rule on `detected`.

A Tap Dial contributes only its ring — turn right brightens, turn left dims. Its four buttons
recall scenes, and a scene is the one thing on a Hue bridge with no Juno representation at all;
guessing a brightness for "Relax" would be inventing something nobody asked for.

Every imported rule crosses the same gate a hand-written one does. A trigger must name an event the
contract declares, a room command must exist, arguments are range-checked. Anything that does not
survive that is reported and dropped rather than bent until it fits.

The configuration inside a behaviour is shaped by whichever script it is an instance of, and those
shapes are neither documented nor stable. Rather than walk a known path — which would work for
today's dimmer script and quietly stop working for the next one — the driver collects every
`{rid, rtype}` anywhere in the structure and keeps the ones that name a room. Deliberately
structure-blind, because the one thing every script has in common is that it refers to things by
resource id.

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
