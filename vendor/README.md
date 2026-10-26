# Pinned dependencies

## Smithay 0.7.0

Source: https://crates.io/crates/smithay/0.7.0
Upstream commit: `a166cf4c94b5aedc332a65aa1dd753e8148829c3`
Crate archive SHA-256: `740cea6927892bc182d5bf70c8f79806c8bc9f68f2fb96e55a30be171b63af98`
License: `smithay/LICENSE.txt` (MIT).

The published crate is checked in to make the KMS restoration fix reproducible
without modifying Cargo's shared cache or executing a download/patch at build
time. `Cargo.toml` selects it through `[patch.crates-io]`.

Local changes are recorded in `../patches/smithay-kms-restore.patch`:

- Exclude immutable properties, legacy DPMS and per-commit fence/writeback/damage
  properties from the atomic snapshot. Replaying metadata/DPMS rejects the whole
  atomic commit with `EINVAL`.
- Copy blob contents when taking the snapshot. Blob IDs do not retain the data:
  resetting KMS or changing modes can destroy the original MODE_ID/color blobs.
- Recreate saved blobs for restoration and release the temporary handles on both
  success and failure. The applied kernel state retains its own references.

No output names, GPU IDs or personal settings are special-cased. Remove the patch
after an upstream release includes the fix, then run the native VM exit/restart
and hotplug tests. The dependency remains excluded from workspace formatting.
Cargo's cache marker and the upstream crate's ignored lockfile are not tracked;
the root `Cargo.lock` pins the application dependency graph.

The native regression test is `native_vm_control.py … smoke-exit`, using a fresh
`native_vm.py --manual-start` guest. It checks three start/exit cycles, including
a mode change and a disconnect/reconnect, against the original console's KMS
framebuffer, routing, timings and screenshot pixels. `smoke-modes` also rejects
any restore failure on session restart.

The property policy test can be run separately:

```sh
cargo test --manifest-path vendor/smithay/Cargo.toml --release \
  --no-default-features \
  --features backend_drm,backend_gbm,backend_udev,backend_libinput,backend_session_libseat,renderer_glow,desktop,wayland_frontend,backend_winit \
  --lib atomic_restore_excludes_metadata_and_per_commit_handles
```

Smithay 0.7.0 emits six existing compiler warnings with the application's feature
set on the current Rust toolchain (unused imports and a function-pointer cast).
They are unrelated to this patch; workspace lint failures are not suppressed.
