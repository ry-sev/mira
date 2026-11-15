# Icons

Mira, the red giant in Cetus: a white-hot core in a crimson glow, crossed by the four
diffraction spikes a telescope draws on a star this bright, inside the faint halo of its
optics. Its companion, Mira B, sits just off the glare, among a scattering of field stars.

Everything here is generated from the three plates in `source/` by `source/build.sh`.
Edit the SVGs, never the generated files, then run:

```bash
mira-app/icons/source/build.sh
```

It needs `rsvg-convert` (librsvg) and `python3`.

| Plate        | What it is                                       | Used for                                |
| ------------ | ------------------------------------------------ | --------------------------------------- |
| `macos.svg`  | Apple's grid: 824px body, margin and shadow      | `icon.icns`, at every size              |
| `square.svg` | Full-bleed, with every detail                    | PNGs from 128px; `.ico` at 64 and 256px |
| `small.svg`  | Full-bleed, the star alone, bigger and bolder    | `32x32.png`; `.ico` at 16 to 48px       |

The small plate exists because the halo, the companion and the field stars turn to noise
below 64px, and the thin spikes vanish. On a Windows taskbar and title bar, which is where
those sizes are seen, a bold star reads better than a blurred detailed one.

cargo-packager takes the list from `icons` under `[package.metadata.packager]` in the
crate's `Cargo.toml`, and picks each platform's format from it: the `.icns` for macOS, the
`.ico` for Windows, and the PNGs, by size, for Linux.
