# After Figs

FigCraft design and EffectCraft animation in one workspace. FigCraft is vendored under `vendor/figcraft`. EffectCraft is the [`storytold/effectcraft`](https://github.com/storytold/effectcraft) submodule at `vendor/effectcraft` (public, Apache-2.0 / MIT). There is no EffectCraft repo under this GitHub account.

## Checkout

```sh
git clone --recurse-submodules https://github.com/l3monaaade/after-figs.git
# or, after a plain clone:
git submodule update --init
```

The submodule is pinned to commit `8f02f52a44a4b1ea807d8824b92301b96f896a06` (`main` as of 2026-10-16). `cargo check --workspace` needs `vendor/effectcraft/crates/*/Cargo.toml`. Without the submodule those path dependencies are missing and the workspace does not resolve.

EffectCraft patches crates.io `hayro-syntax` to the copy in `storytold/craft-libs`. That patch does not apply to this workspace automatically, so the root `Cargo.toml` repeats it. Dropping the patch makes `craft-pdf` fail to compile.

A private EffectCraft tree can replace the submodule (`git submodule set-url vendor/effectcraft <url>` then update), or you can point fonts at it with `EFFECTCRAFT_DIR` / `EFFECTCRAFT_FONTS_DIR` without using it as the crate source. Crate source still has to be the submodule (or another checkout at `vendor/effectcraft`).

Rust 1.95 or newer (`rust-version` in `Cargo.toml`, edition 2024).

## Fonts

`vendor/figcraft/crates/text/build.rs` embeds fonts only when it can see them:

1. `CRAFT_FONTS_DIR` (craft-fonts manifests), if that directory is present.
2. Otherwise `EFFECTCRAFT_FONTS_DIR`, or `$EFFECTCRAFT_DIR/assets/fonts`, or `vendor/effectcraft/assets/fonts`.
3. If none of those exist, `CRAFT_FONTS` is empty, the build warns, and the app uses system fonts.

`.cargo/config.toml` sets `EFFECTCRAFT_DIR` to `vendor/effectcraft`. A missing directory does not fail the build. `include_bytes!` paths are written relative to the build script output so the generated source does not contain a host path.

The public EffectCraft tree ships Inter, JetBrains Mono, and Noto Serif. The other families in the fallback list are skipped when the files are absent.

## Web bundle and GitHub Pages

Do not commit `docs/motion_web_bg.wasm` or `docs/motion_web.js`. They are gitignored. [`.github/workflows/pages.yml`](.github/workflows/pages.yml) builds the bundle and deploys `target/web/dist` to GitHub Pages. [`.github/workflows/ci.yml`](.github/workflows/ci.yml) fails if those files are tracked again.

**Owner step:** in the repo settings, set Pages → Source to **GitHub Actions**. Until that is on, and until the workflow goes green, the site is not updated by this change. The workflow checks out the submodule and then builds EffectCraft plus its git dependencies (`storytold/filmcraft`, `storytold/craft-libs`). `cargo check --workspace` passes on the host with that pin. This change does not rebuild or commit a wasm binary; Pages publishes the next one.

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

The wasm that used to live at `docs/motion_web_bg.wasm` still contains those paths in git history. It was removed from the tree going forward and was not rebuilt here, because a wasm link needs EffectCraft and the filmcraft / craft-libs crates it pulls in. Crate names such as `filmcraft` can still appear as Rust package names after remapping; the flags remove directories, not dependency names. Those two crates are public (`storytold/filmcraft`, `storytold/craft-libs`).

## Dev server paths

`motion-web-server` serves files only from the dist directory. Request paths are percent-decoded, then each component is checked. `..`, absolute paths, and symlinks that resolve outside the root are rejected with 400. `cargo test -p motion-web-server` covers that, including the old `replace("..", "")` bypass (`/....//....//etc/passwd`).
