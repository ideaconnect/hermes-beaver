# vendor/

`i-slint-core/` is the crates.io release of `i-slint-core` 1.18.1 with one upstream fix
backported: [slint-ui/slint#14543](https://github.com/slint-ui/slint/pull/14543), for
[#14435](https://github.com/slint-ui/slint/issues/14435). With the software renderer, a
Text that changes rarely (the clock) stopped repainting after about 1024 updates to other
text: its property changed, the screen kept the old value.

Only `item_rendering.rs` and `textlayout/sharedparley/cache.rs` differ from the release.
`Cargo.toml` uses it through `[patch.crates-io]`. Once a Slint release includes the fix,
delete this directory and the `[patch.crates-io]` section, and bump `slint`.
`ui::tests::clock_keeps_repainting_after_many_other_text_updates` guards the fix.
