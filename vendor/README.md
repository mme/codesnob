# Vendored crates

## gpui 0.2.2

`gpui/` is [GPUI](https://crates.io/crates/gpui) 0.2.2 as published, without
its examples (they and their `[[example]]` entries are left out), with one
patch: `gpui-wayland-touchpad.patch`. Cargo uses it through
`[patch.crates-io]` in the top-level `Cargo.toml`.

The patch makes GPUI's Wayland backend pass on two things it drops, which
the window (`codesnob --gui`) needs for touchpad flings:

- the touchpad's fingers lifting (`wl_pointer.axis_stop`), as a scroll
  event with `TouchPhase::Ended`, so a fling starts when the fingers lift;
- fingers coming to rest on it (the hold gesture of
  `zwp_pointer_gestures_v1`, version 3), as a scroll that moves nothing with
  `TouchPhase::Started`, so they stop a fling.

To move to a newer GPUI: copy the new crate in, remove its `examples/` and
`[[example]]` entries, and apply the patch (`patch -p1 -d vendor/gpui <
vendor/gpui-wayland-touchpad.patch`), resolving what no longer applies. Drop
all of it once GPUI does this itself (Zed's PR #55857 is heading there).
