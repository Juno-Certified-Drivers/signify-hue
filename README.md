# Philips Hue

Hue bridges and the bulbs behind them, over CLIP v2.

One package, two drivers. The bridge is a device in its own right: it holds the address and the
paired application key once, and every bulb behind it reads them from there — so a bridge that
moves to a new IP is edited in one place rather than six. They ship together because a version
skew between a bridge and its bulbs is invisible until something stops working.

| Driver | Proxies |
| --- | --- |
| `signify.hue.bridge` | `bridge` |
| `signify.hue.bulb` | `light` |

## Setup

Discovered over mDNS (`_hue._tcp`). Pairing needs the bridge's link button pressed; the driver
polls until it is, because the bridge refuses the request until then.

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
