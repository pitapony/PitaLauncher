# M0 hardware test — launch one real Steam game with `DXVK_HUD=fps`

This is the last M0 checkbox. Everything else (scan, grid, shim) is already
verified; this test proves the whole slice on the target hardware:

```
Steam library → pitagames grid → Play → steam://rungameid → Steam launch options
→ pitagames-shim <appid> %command% → game starts with DXVK_HUD=fps
```

Run it once on the **RX 9070** and once on the **RX 5700 XT**. One pass is
enough to close M0; the second confirms the RDNA 1 path.

## 1. Build the binaries (on the Mac, in the Arch container)

The GUI only builds against Arch's WebKitGTK, so build all three binaries in
`archlinux:latest` and copy them into `dist/`:

```bash
cd /Users/julienpohie/code/pitagames
mkdir -p dist

docker run --rm \
  -v "$PWD":/src \
  -v "$PWD/dist":/dist \
  -v pitagames-cargo:/cargo \
  -v pitagames-pacman:/var/cache/pacman/pkg \
  -e CARGO_HOME=/cargo \
  -e CARGO_TARGET_DIR=/ci-target \
  -e CARGO_HTTP_MULTIPLEXING=false \
  archlinux:latest bash -c '
set -e
pacman -Syu --noconfirm --needed \
  git nodejs rust \
  webkit2gtk-4.1 gtk3 libsecret sqlite pkgconf openssl xdotool >/dev/null
cargo build --release -p pitagames -p pitagames-shim -p pitagames-cli
install -m755 /ci-target/release/pitagames \
                /ci-target/release/pitagames-shim \
                /ci-target/release/pitagames-cli /dist/
'
```

Expected result: `dist/pitagames`, `dist/pitagames-shim`,
`dist/pitagames-cli` (Linux x86_64).

## 2. Copy to the CachyOS machine

```bash
scp dist/pitagames dist/pitagames-shim dist/pitagames-cli \
    <user>@<cachyos-host>:/tmp/pitagames/
```

On the machine:

```bash
sudo install -Dm755 /tmp/pitagames/pitagames      /usr/local/bin/pitagames
sudo install -Dm755 /tmp/pitagames/pitagames-cli  /usr/local/bin/pitagames-cli
sudo install -Dm755 /tmp/pitagames/pitagames-shim /usr/local/bin/pitagames-shim

# Runtime libraries for the GUI (usually already present on CachyOS).
sudo pacman -S --needed webkit2gtk-4.1 gtk3 libsecret
```

## 3. Verify the pieces before touching a game

```bash
# Steam scan: should list installed games, not Proton/runtimes.
pitagames-cli scan

# Machine-readable, useful to attach to a bug report.
pitagames-cli scan --json | head -c 600; echo

# The shim must add DXVK_HUD=fps and keep the rest of the environment.
pitagames-shim 620 /usr/bin/env | grep '^DXVK_HUD='
# expected: DXVK_HUD=fps

# A pre-existing user value must win.
DXVK_HUD=devinfo pitagames-shim 620 /usr/bin/env | grep '^DXVK_HUD='
# expected: DXVK_HUD=devinfo

# The GUI: grid of installed games with cover-less tiles and a search box.
pitagames
```

If `pitagames-cli scan` finds nothing, point it at the right root first:

```bash
PITAGAMES_STEAM_ROOT="$HOME/.local/share/Steam" pitagames-cli scan
```

## 4. Set the shim in Steam launch options

Pick a **Windows game running through Proton**, ideally D3D11 (e.g. a
D3D11-era title). Native Linux builds and native Vulkan games do not use
DXVK, so `DXVK_HUD` will not appear. If needed, force Proton:

> Steam → Library → right-click the game → Properties → Compatibility →
> Force the use of a specific Steam Play compatibility tool → Proton

Then:

> Properties → General → Launch Options:

```text
/usr/local/bin/pitagames-shim <appid> %command%
```

Replace `<appid>` with the game's Steam appid (the number in
`appmanifest_<appid>.acf`). Keep `%command%` exactly as written.

Alternative once the launch-options writer is installed: quit Steam, then run
`pitagames-cli launch-options <appid> --shim` (add `--dry-run` first to see the
exact line). It backs up `localconfig.vdf`, writes atomically and can put the
original value back with `--restore`.

## 5. Launch and confirm

1. Open `pitagames`.
2. Click the game's **Play** button (or press Play in Steam — both end up in
   the same launch options).
3. The game starts.
4. **Expected:** the DXVK FPS counter is visible in a corner of the game
   (white text, updated every frame).
5. Alt-tab / close the game normally; the launcher still responds.

That is the M0 pass condition.

## 6. Troubleshooting

| Symptom | Likely cause | Check |
|---|---|---|
| No FPS overlay | Game is native Linux or native Vulkan | Force Proton in Steam properties; use a D3D11 game |
| No FPS overlay | Game is D3D12-only (VKD3D-Proton) | `DXVK_HUD` only covers DXVK (D3D9/10/11); test with a D3D11 game, or confirm the shim env separately in step 3 |
| Game starts but ignores the shim | Launch options not saved, or wrong appid | Re-open Properties; appid must match `appmanifest_<appid>.acf` |
| `pitagames-shim: first argument must be the numeric Steam appid` | `%command%` placed before the appid | Order is `pitagames-shim <appid> %command%` |
| `command not found` from Steam | `/usr/local/bin` not in Steam's PATH | Use the absolute path shown above |
| `pitagames` window is blank | WebKitGTK runtime missing or GPU/driver issue | `pacman -S webkit2gtk-4.1 gtk3`; run from a terminal and note stderr |
| Scan finds no games | Steam is Flatpak (unsupported) or library is elsewhere | Set `PITAGAMES_STEAM_ROOT`; Flatpak Steam is a non-goal |
| Shader compilation stutter on first launch | Normal DXVK pipeline cache warm-up | Relaunch once and re-check |

## 7. Record the result

| Machine | Game (appid) | Proton version | FPS overlay | Notes |
|---|---|---|---|---|
| RX 9070 | | | ☐ | |
| RX 5700 XT | | | ☐ | |

Attach `pitagames-cli scan --json` output and, if something fails, the exact
launch-options line and the terminal output from launching `pitagames`.

After both rows are checked, M0 is closed and the remaining M1 work
(install hand-off, appmanifest polling, Downloads view) is unblocked.
