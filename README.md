# After Figs

FigCraft design and EffectCraft animation in one workspace. FigCraft is vendored under `vendor/figcraft`. EffectCraft is your own source, committed as ordinary files under `vendor/effectcraft`. That directory is a placeholder until you add it. Steps are in [`vendor/effectcraft/README.md`](vendor/effectcraft/README.md).

```sh
git clone https://github.com/l3monaaade/after-figs.git
```

Rust 1.95 or newer (`rust-version` in `Cargo.toml`, edition 2024).

With the placeholder in place:

- `cargo test -p motion-web-server` runs.
- FigCraft's font build warns and embeds no EffectCraft fonts.
- `cargo check --workspace` stops on the placeholder crates. That is expected until your tree replaces them.

## Fonts

`vendor/figcraft/crates/text/build.rs` embeds fonts only when it can see them:

1. `CRAFT_FONTS_DIR` (craft-fonts manifests), if that directory is present.
2. Otherwise `EFFECTCRAFT_FONTS_DIR`, or `$EFFECTCRAFT_DIR/assets/fonts`, or `vendor/effectcraft/assets/fonts`.
3. If none of those exist, `CRAFT_FONTS` is empty, the build warns, and the app uses system fonts.

`.cargo/config.toml` sets `EFFECTCRAFT_DIR` to `vendor/effectcraft`. A missing font directory does not fail the build. `include_bytes!` paths are written relative to the build script output so the generated source does not contain a host path. Families in the fallback list are skipped when the files are absent.

## Web bundle and GitHub Pages

Do not commit `docs/motion_web_bg.wasm` or `docs/motion_web.js`. They are gitignored. [`.github/workflows/pages.yml`](.github/workflows/pages.yml) builds the bundle and deploys `target/web/dist` to GitHub Pages. [`.github/workflows/ci.yml`](.github/workflows/ci.yml) fails if those files are tracked again.

**Owner step:** in the repo settings, set Pages → Source to **GitHub Actions**. While `vendor/effectcraft/PLACEHOLDER` exists, the workflow skips the wasm build instead of failing. After you commit your EffectCraft tree and delete that marker, the workflow builds `target/web/dist` and deploys it. This change does not commit a wasm binary.

Local preview, once EffectCraft resolves:

```sh
cargo run -p motion-web-server
```

That serves `http://127.0.0.1:8080/` from `target/web/dist` (not from `docs/`).

### Why Actions, not Git LFS

| | GitHub Actions → Pages | Git LFS |
| --- | --- | --- |
| Future commits | Small. The wasm is an artifact. | A pointer per commit, but every rebuild still uploads ~70MB to LFS. |
| Fresh clone | Does not need the wasm. | Needs `git lfs` and downloads the current blob. |
| Host paths | The build remaps them before publish. | LFS stores whatever binary you commit. A dirty local build is published as-is. |
| Cost | Actions minutes and a successful EffectCraft wasm build. | LFS bandwidth and storage quota. |
| History | Old ~70MB blobs stay in previous commits. | Same, unless history is rewritten (this change does not). |

`git clone` of the full history still downloads the wasm blobs that are already on `main`. Shrinking that means a later history rewrite (`git filter-repo` or similar) and a force-push. That was intentionally not done.

### Stripping host paths

`profile.trim-paths` is **unstable** in Rust 1.95 (Cargo rejects it without `-Z`). Future builds use `--remap-path-prefix` instead:

- `.cargo/config.toml` remaps `/Users/d3dfly-laptop` when `RUSTFLAGS` and `CARGO_ENCODED_RUSTFLAGS` are unset. Cargo uses only one of those sources; config does not append to `RUSTFLAGS`.
- `motion-web-server` sets `CARGO_ENCODED_RUSTFLAGS` for the wasm rustc, with the longest prefixes first: workspace, `vendor/effectcraft`, Cargo home, home directory, and `/Users/d3dfly-laptop`.

`strip = true` removes symbols. It does not remove path strings. The remap flags do.

The wasm that used to live at `docs/motion_web_bg.wasm` still contains those paths in git history. It was removed from the tree going forward. A new bundle is produced by the Pages workflow after your EffectCraft source is in `vendor/effectcraft`. Remap flags strip directories. Crate names that your tree depends on can still appear in the binary as package names.

## Dev server paths

`motion-web-server` serves files only from the dist directory. Request paths are percent-decoded, then each component is checked. `..`, absolute paths, and symlinks that resolve outside the root are rejected with 400. `cargo test -p motion-web-server` covers that, including the old `replace("..", "")` bypass (`/....//....//etc/passwd`).
