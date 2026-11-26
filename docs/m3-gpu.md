# M3 — GPU detection (`platform::gpu`)

The graphics work starts by answering one question honestly: *what card is
this?* `platform::gpu` is the detection half and only the detection half. It
never decides which upscalers or injection paths are available — that is the
capability matrix (`platform::compat` + `platform::capability`), which consumes
the profile this module produces. Detection is read-only and framework-free.

The whole host boundary sits behind one injected value, [`GpuProbe`], so tests
run against a fixture `/sys` tree and captured `vulkaninfo` text with no GPU and
no `vulkaninfo` installed — the same shape as `platform::proton`'s injected
roots.

## Pieces

| Crate | Responsibility |
|---|---|
| `platform` (`pitagames-platform`) | `GpuProbe`, `GpuProfile`, `GpuVendor`, `GpuArch`, `HdrSupport`, `arch_for()`, `detect()` |
| `cli` | `pitagames-cli gpu` diagnostic (text + `--json`) |

## Inputs and degradation

Detection has two independent inputs and degrades between them:

| Input | Provides | When absent |
|---|---|---|
| sysfs (`<root>/class/drm/card*/device/`) | PCI vendor/device id, VRAM (`amdgpu`) | ids fall back to `vulkaninfo` |
| `vulkaninfo --summary` | marketing name, driver, **Mesa version** | arch still from the table, VRAM still from sysfs, **Mesa reported unknown — never guessed** |

`GpuProbe::host()` reads `/sys`, checks for `/dev/ntsync`, reads
`$XDG_CURRENT_DESKTOP` and runs `vulkaninfo --summary` once. `GpuProbe::at(root)`
builds a fixture probe with no optional inputs; the `with_vulkaninfo` /
`with_ntsync` / `with_desktop` builders add them.

The CLI's `--root <dir>` is the fixture form (it suppresses `vulkaninfo` and the
host hints); `--vulkaninfo <file>` supplies captured output. Without `--root`
the running machine is probed.

## The vendor/device table

`arch_for(vendor_id, device_id)` maps a PCI pair to a [`GpuArch`]. The table is
`const` data taken from the PCI ID database (`pci.ids`, 2026-11-25), grouped by
family, and is **vendor-scoped** so an AMD id can never be read as an NVIDIA one:

| Vendor | Families |
|---|---|
| AMD (`0x1002`) | RDNA 1 (Navi 10/12/14), RDNA 2 (Navi 21–24), RDNA 3 (Navi 31–33), RDNA 4 (Navi 44/48) |
| Intel (`0x8086`) | Arc Alchemist (DG2), Arc Battlemage (BMG) |
| NVIDIA (`0x10de`) | Turing (RTX 20 / GTX 16), Ampere (RTX 30), Ada (RTX 40), Blackwell (RTX 50), Pascal (GTX 10) |

The two in-house targets are anchored: RX 9070 XT `0x7550` → RDNA 4, RX 5700 XT
`0x731f` → RDNA 1. A device id that is **not** in the table degrades to
`GpuArch::Unknown` with the vendor still known and a warning naming the id — it is
never guessed from a neighbouring id.

`GpuArch` is deliberately finer than `compat::Arch` (which buckets a whole vendor
family as `Rtx`/`Arc`). `GpuArch::compat_arch()` bridges them; the GTX cards map
to `None` because the database has no bucket for a pre-tensor-core card.

## VRAM, Mesa and the hints

- **VRAM** comes from `amdgpu`'s `mem_info_vram_total` (falling back to
  `mem_info_vis_vram_total`). It is absent on non-AMD cards.
- **Mesa** is read from `driverInfo` (`Mesa 25.3.0-devel` → `25.3.0`), or from
  `driverVersion` when the driver id is a Mesa one. An NVIDIA proprietary driver
  reports no Mesa, so the field is `None`.
- **ntsync**: `/dev/ntsync` present → `suggested_env()` offers `PROTON_USE_NTSYNC=1`.
- **HDR**: `$XDG_CURRENT_DESKTOP` is one of the known HDR-capable compositors
  (KWin/KDE, GNOME/mango) → `suggested_env()` offers `DXVK_HDR=1`. A desktop we do
  not recognise is `Unsupported`; no desktop at all is `Unknown`.

`suggested_env()` only ever *offers* these; the graphics layer decides whether to
apply them.

## Multi-GPU

`detect()` reads every `card<N>` (connectors like `card0-DP-1` and `renderD*` are
ignored), picks the primary as the first card with a recognised architecture,
then the first card from a known vendor, then simply the first. The matching
`vulkaninfo` device block is chosen by device id, so a laptop reports its
dGPU's name and not its iGPU's.

## CLI diagnostic

```bash
pitagames-cli gpu                                  # this machine
pitagames-cli gpu --root /path/to/fixture/sys      # a fixture /sys tree
pitagames-cli gpu --root <fixture> --vulkaninfo captured.txt --json
```

```
GPU: AMD Radeon RX 9070 XT (RADV NAVI48)
  vendor       AMD
  device       0x7550
  arch         RDNA 4 (RX 9000)  [compat: rdna4]
  mesa         25.3
  vram         16.0 GiB (17179869184 bytes)
  ntsync       present (suggest PROTON_USE_NTSYNC=1)
  hdr          compositor HDR-capable (suggest DXVK_HDR=1)
```

`--json` prints `{ "profile": …, "suggested_env": [[key, value], …] }` and is
what scripted checks should parse. Missing `vulkaninfo` and an id outside the
table are emitted as `warning:` lines on stderr (and in the profile's
`warnings`).

## Verification

Pure crates (macOS host and CI):

```bash
CARGO_HTTP_MULTIPLEXING=false cargo test -p pitagames-platform -p pitagames-cli
CARGO_HTTP_MULTIPLEXING=false cargo clippy -p pitagames-platform -p pitagames-cli --all-targets -- -D warnings
cargo fmt --all -- --check
```

`pitagames-platform::gpu` covers: the RX 9070 XT from sysfs + `vulkaninfo`
(arch, name, Mesa, VRAM); the missing-`vulkaninfo` degrade (table arch, sysfs
VRAM, unknown Mesa); the two-card block match; the table across every family
including the in-house anchors and the vendor-scoping; the `compat_arch`/
`vendor` mapping; the ntsync/HDR hints; the no-card `vulkaninfo` fallback; an
unknown id's warning; connector exclusion and the visible-VRAM fallback; and a
JSON round trip.

## Known limitations

- **Not run against a real GPU.** Every case here is a fixture tree and captured
  text; `pitagames-cli gpu` on the RX 9070 / RX 5700 XT is the check once a target
  machine is reachable.
- **The table is a curated subset.** It covers shipping SKUs per family, not
  every engineering-sample id; unknown ids degrade rather than guess.
- **VRAM only on `amdgpu`.** Intel/`xe` and NVIDIA expose it differently; those
  cards report `None` for now.
- **The HDR hint is a compositor guess**, not a display/pipe capability check;
  the actual HDR verdict belongs to the hardware/TV checklist (ADR-0004).
- **No capability verdict here.** This slice detects; `platform::capability`
  turns `GpuArch` + `compat` + the resolved Proton into what is `Available` or
  `Hidden(reason)` — see `docs/m3-capability.md`.

## Deliberately not in this slice

- Selecting upscalers, frame generation or injection tiers (`platform::capability`
  now ships that as a pure verdict; `envc` compiling the env from it is M3).
- Compiling any environment (`envc` becomes the full M3 compiler on top of the
  capability verdict).
- A GUI graphics screen.
