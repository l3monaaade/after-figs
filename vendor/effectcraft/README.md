# vendor/effectcraft

This directory is where **your** EffectCraft source lives, committed as normal files in this repo. It is not a submodule and it does not point at another remote.

What is here now is a placeholder. Each `crates/<name>` package is an empty crate that stops Cargo from erroring on a missing path. `cargo test -p motion-web-server` and the FigCraft font build run against that placeholder. `cargo check --workspace`, the desktop app, and the wasm bundle do not: those crates fail to compile on purpose until your tree replaces them.

Fonts are optional. If `assets/fonts` is missing, FigCraft embeds no EffectCraft fonts, prints a warning, and keeps building.

## Add your source

On the machine that has the EffectCraft checkout (the old symlink target was `/Users/d3dfly-laptop/effectcraft`):

```sh
cd /path/to/after-figs

# Drop the placeholder. A copy over the top would leave PLACEHOLDER behind.
rm -rf vendor/effectcraft
mkdir -p vendor/effectcraft
cp -a /Users/d3dfly-laptop/effectcraft/. vendor/effectcraft/

# The placeholder marker must not survive. rm -rf above already removes it.
test ! -f vendor/effectcraft/PLACEHOLDER

# These paths are what the workspace Cargo.toml depends on.
test -f vendor/effectcraft/Cargo.toml
test -f vendor/effectcraft/crates/color/Cargo.toml
test -f vendor/effectcraft/crates/time/Cargo.toml
test -f vendor/effectcraft/crates/geom/Cargo.toml
test -f vendor/effectcraft/crates/raster/Cargo.toml
test -f vendor/effectcraft/crates/path/Cargo.toml
test -f vendor/effectcraft/crates/keyframe/Cargo.toml
test -f vendor/effectcraft/crates/project/Cargo.toml
test -f vendor/effectcraft/crates/text/Cargo.toml
test -f vendor/effectcraft/crates/effects/Cargo.toml
test -f vendor/effectcraft/crates/render/Cargo.toml
test -f vendor/effectcraft/crates/gpu/Cargo.toml
test -f vendor/effectcraft/crates/lottie/Cargo.toml
test -f vendor/effectcraft/crates/svg/Cargo.toml
test -f vendor/effectcraft/crates/engine/Cargo.toml
test -f vendor/effectcraft/crates/export/Cargo.toml
test -f vendor/effectcraft/crates/host/Cargo.toml
test -f vendor/effectcraft/crates/ui-egui/Cargo.toml
```

Package names inside those manifests must stay `effectcraft-time`, `effectcraft-geom`, `effectcraft-color`, and so on. This repo's workspace lists them as path dependencies and excludes `vendor/effectcraft` so your tree keeps its own workspace.

If your EffectCraft `Cargo.toml` has a `[patch]` table, copy those patches into the `[patch]` table in the root `Cargo.toml` of this repo. Cargo does not apply patches declared only inside a path dependency's workspace.

Optional fonts: put them in `vendor/effectcraft/assets/fonts` (or set `EFFECTCRAFT_FONTS_DIR` / `EFFECTCRAFT_DIR`). `.cargo/config.toml` already points `EFFECTCRAFT_DIR` at `vendor/effectcraft`. Missing files are skipped.

Then lock and compile:

```sh
cargo check --workspace
git add vendor/effectcraft Cargo.toml Cargo.lock
git status
# vendor/effectcraft entries must be normal files, not a gitlink (mode 160000).
git commit -m "Add EffectCraft source"
git push
```

After `PLACEHOLDER` is gone, the GitHub Pages workflow builds the wasm bundle. While the marker is present, that workflow skips the build instead of failing.
