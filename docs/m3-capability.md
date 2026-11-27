# M3 — the capability matrix (`platform::capability`)

`platform::gpu` detects the card, `platform::compat` curates what a *game*
supports and `platform::proton` resolves the runtime. `platform::capability` is
where those three meet and the launcher answers one question per graphics field:
**may we offer this, and if not, why?**

The verdict is `Availability` — `Available` or `Hidden { reason }`. The same
`reason` string is the UI's "why is this disabled?" copy and this document's
"Reason strings" table, so the two cannot drift. A test reads this file and
asserts every string in `capability::REASONS` appears verbatim.

Per ADR-0004 an unsupported option is **hidden, never greyed**; per ADR-0023 the
single injection slot means DLSSNR and an injected upscaler can never both be
offered. The module is framework-free and does no I/O: the GPU profile, the
compat entry and the resolved Proton build are all passed in, so the whole matrix
is tested against fixture values with no GPU, no game and no Proton installed.

## Pieces

| Crate | Responsibility |
|---|---|
| `platform` (`pitagames-platform`) | `Field`, `Availability`, `reason`/`REASONS`, `availability()`, `matrix()`, `Field::for_upscaler`/`for_frame_gen`/`for_injection` |
| `cli` | `pitagames-cli capability` diagnostic (text + `--json`) |

## The seam

```rust
pub fn availability(
    field: Field,
    profile: &Profile,
    gpu: &GpuProfile,
    compat: Option<&CompatEntry>,
    proton: Option<&ProtonInstall>,
) -> Availability;
```

- `field` is one graphics option (`Field::ALL` is the one list the UI, the CLI
  and this table iterate).
- `profile` supplies the single injection slot's current owner and, for neural
  rendering, the package architecture (`pkg_arch`).
- `compat` is the game's curated entry, or `None` when the database does not know
  the game.
- `proton` is the **resolved** build (`None` when Steam decides), because the
  gates need its kind and version — the profile's `ProtonRef` is not enough.

`envc` (M3) is the consumer: it asks `availability` for each field instead of
deciding on its own, and a `Hidden` field is never emitted. `matrix()` returns
every field's verdict in `Field::ALL` order for the CLI table and this document.

## The matrix (plan §6)

`Available` covers both the "yes" and the "experimental" cells — an experimental
option is still offered, marked "not tested by us" (ADR-0004). A "no" / "n/a"
cell is a `Hidden` reason from the table below. The header row is the compat
`Arch` bucket.

| Field | RDNA 4 | RDNA 3 | RDNA 2 | RDNA 1 | RTX | Arc | GTX |
|---|---|---|---|---|---|---|---|
| Native (no injection) | yes | yes | yes | yes | yes | yes | yes |
| System FSR (gamescope) | yes | yes | yes | yes | yes | yes | yes |
| FSR 4 upgrade | yes | RDNA 3 path | no | no | no | no | no |
| FSR 4 RDNA 3 upgrade | no | experimental | no | no | no | no | no |
| DLSS → FSR 4 (OptiScaler) | yes | experimental | no | no | no | no | no |
| DLSS → FSR 3.1 (OptiScaler) | yes | yes | yes | yes | yes | yes | GTX 16 |
| DLSS → XeSS (OptiScaler) | yes | yes | yes | yes | yes | yes | GTX 16 |
| Native DLSS | no | no | no | no | yes | no | no |
| XeSS upgrade | experimental | experimental | experimental | experimental | experimental | yes | GTX 16 |
| FFX 3 upgrade | yes | yes | yes | yes | no | no | no |
| FFX 4 upgrade | yes | experimental | no | no | no | no | no |
| HelixSR HLX DLSS | experimental | experimental | yes | yes (primary) | experimental | experimental | GTX 16 |
| HelixSR HLX FSR 4 | experimental | experimental | yes | yes (primary) | experimental | experimental | GTX 16 |
| HelixSR FSR 4 drop-in | experimental | experimental | yes | yes (primary) | experimental | experimental | GTX 16 |
| DLSS 5 neural rendering | yes | fork | no | no | no | no | no |
| ML frame generation (FSR 4) | yes | no | no | no | no | no | no |
| FSR 3 frame generation | yes | yes | yes | yes | yes | yes | GTX 16 |
| Tier A injection | gated on Proton | gated on Proton | gated on Proton | gated on Proton | gated on Proton | gated on Proton | gated on Proton |
| Tier B injection | yes | yes | yes | yes | yes | yes | yes |

"GTX 16" means the Turing GTX cards (no tensor cores) keep the OptiScaler and
HelixSR routes but not native DLSS; the Pascal GTX 10 cards are hidden from
those routes. `Native` and `System FSR` are the only fields offered on an
unrecognised card (`GpuArch::Unknown`); everything else is hidden there.

## The gates, in order

1. **Architecture** (plan §6). The table above; `Native` and `System FSR` skip
   this gate because they depend on no detected architecture.
2. **The single injection slot** (ADR-0023). While DLSSNR is enabled, every
   injected upscaler / frame-gen / Tier A / Tier B is hidden (`NR_OWNS_SLOT`).
   While an injected upscaler, frame-gen or Tier A/B is already selected, DLSSNR
   is hidden (`UPSCALER_OWNS_SLOT`).
3. **Game curation** (ADR-0025). A field that hooks into a game API needs the
   compat entry to permit it: an uncurated game hides every game route
   (`GAME_NOT_CURATED`), a curated game that lacks the API gets the specific
   game reason. The hardware-only paths (`Fsr4Upgrade`, `SystemFsr`, the
   injection tiers, …) stay available without an entry.
4. **64-bit** (plan §6). The game's own bitness needs its PE header, so that
   check belongs to the neural-rendering backends; the gate we *can* evaluate
   here is the profile's DLSSNR `pkg_arch`: an i686 package is hidden
   (`NR_32BIT`).
5. **Mesa**. Neural rendering needs Mesa 26.2 or newer (`NR_NEEDS_MESA`); an
   unreadable version (`vulkaninfo` absent) is `NR_MESA_UNKNOWN` — never guessed.
6. **Proton** (plan §6, ADR-0023). Tier A needs the Proton-CachyOS hooks
   (`TIER_A_NEEDS_CACHYOS`, or `TIER_A_STEAM_DEFAULT` when Steam decides; an
   unreadable build version is `TIER_A_VERSION_UNREADABLE` — never assumed new
   enough); the FFX upgrade hooks need the same build (`FFX_NEEDS_CACHYOS`).
   Neural rendering (`NR_NEEDS_RECENT_PROTON`, `NR_STEAM_DEFAULT_PROTON`) and
   HelixSR (`HLX_NEEDS_RECENT_PROTON`) need a recent build. "Recent" is
   Proton-CachyOS ≥ 11.0-20260506, GE-Proton ≥ 10, Valve/Experimental/umu-Proton
   ≥ 9; an unclassifiable directory is never claimed to be recent.

The matrix follows the detected hardware: a newer Mesa or Proton unlocks options.
A field is hidden the moment one gate fails, so the reason the user sees is the
first blocker.

## Reason strings

The exact text the UI shows, one row per `capability::REASONS` entry. A test
asserts each appears verbatim in this table.

| Constant | Reason |
|---|---|
| `GPU_UNKNOWN` | the graphics card was not recognised |
| `F4_NEEDS_RDNA3_PLUS` | FSR 4 needs an AMD RDNA 3 or newer card |
| `F4_AMD_ONLY` | FSR 4 is an AMD path; this card uses its own upscaler |
| `F4_RDNA3_ONLY` | the FSR 4 RDNA 3 upgrade path is for RDNA 3 only |
| `OPTISCALER_AMD_ONLY` | this OptiScaler path is for AMD cards |
| `DLSS_NEEDS_RTX` | DLSS needs an NVIDIA RTX card |
| `XESS_NEEDS_DP4A` | XeSS needs a card with DP4a support |
| `HELIXSR_NOT_OFFERED` | HelixSR is not offered for this card |
| `NR_NEEDS_RDNA3_4` | neural rendering needs an AMD RDNA 3 or RDNA 4 card |
| `MLFG_NEEDS_RDNA4` | ML frame generation needs an AMD RDNA 4 card |
| `FG_NOT_OFFERED` | frame generation is not offered for this card |
| `FFX_AMD_ONLY` | the FFX upgrade is an AMD path |
| `FFX4_NEEDS_RDNA3_PLUS` | the FFX 4 upgrade needs an AMD RDNA 3 or newer card |
| `GAME_NOT_CURATED` | this game is not in the compatibility database yet |
| `GAME_NO_DLSS` | this game does not expose DLSS |
| `GAME_NO_XESS` | this game does not expose XeSS |
| `GAME_NO_HELIXSR_SCALER` | HelixSR's Scaler mode is not curated for this game |
| `GAME_NO_HELIXSR_DROPIN` | this game does not ship an FSR 3.1 DLL for the HelixSR drop-in |
| `GAME_NO_NR` | neural rendering is not curated for this game |
| `TIER_A_NEEDS_CACHYOS` | Tier A needs Proton-CachyOS 11.0-20260506 or newer |
| `TIER_A_VERSION_UNREADABLE` | Tier A needs Proton-CachyOS 11.0-20260506 or newer, but the build version could not be read |
| `TIER_A_STEAM_DEFAULT` | Steam's default Proton cannot deploy OptiScaler; choose Proton-CachyOS |
| `FFX_NEEDS_CACHYOS` | the FFX upgrade hooks need Proton-CachyOS 11.0-20260506 or newer |
| `NR_NEEDS_RECENT_PROTON` | neural rendering needs a recent Proton or GE build |
| `NR_STEAM_DEFAULT_PROTON` | neural rendering needs a recent Proton or GE build; Steam's default is not enough |
| `HLX_NEEDS_RECENT_PROTON` | HelixSR needs a recent Proton or GE build |
| `NR_NEEDS_MESA` | DLSSNR needs Mesa 26.2 or newer |
| `NR_MESA_UNKNOWN` | DLSSNR needs Mesa 26.2 or newer, but the Mesa version could not be detected |
| `NR_OWNS_SLOT` | DLSSNR owns the injection slot |
| `UPSCALER_OWNS_SLOT` | an injected upscaler owns the injection slot |
| `NR_32BIT` | DLSSNR ships x86_64 only; 32-bit games are unsupported in v1 |

## CLI diagnostic

```bash
pitagames-cli capability                                    # this machine
pitagames-cli capability --game steam 1091500               # with a curated entry
pitagames-cli capability --root /path/to/fixture/sys --json # a fixture tree
pitagames-cli capability --proton /usr/share/steam/compatibilitytools.d/proton-cachyos
```

```
GPU: AMD Radeon RX 9070 XT (RDNA 4 (RX 9000))
Proton: proton-cachyos 11.0-20260516 (proton-cachyos)
Game: steam:1091500 Cyberpunk 2077

  available  Native (no injection)
  available  System FSR (gamescope)
  available  FSR 4 upgrade
  hidden     FSR 4 RDNA 3 upgrade — the FSR 4 RDNA 3 upgrade path is for RDNA 3 only
  …
```

`--json` prints `{ "gpu": …, "proton": …, "game": …, "fields": [ { "field",
"label", "available", "reason" } ] }` and is what scripted checks should parse.
`--root` suppresses host Proton discovery (pass `--proton` to add a fixture
build); `--game` loads the shipped/user compat directories (or `--shipped` /
`--user` for a checkout).

## Verification

Pure crates (macOS host and CI):

```bash
cargo test -p pitagames-platform -p pitagames-cli
cargo clippy -p pitagames-platform -p pitagames-cli --all-targets -- -D warnings
cargo fmt --all -- --check
```

`capability` covers: every field on every architecture resolving to `Available`
or a non-empty reason; the plan §6 rows for RDNA 1–4, RTX, Arc and the unknown
card; the game-curation gate (uncurated, curated-but-lacking, the HelixSR
drop-in mode); the Mesa and Proton gates and the "newer hardware unlocks it"
case; the injection-slot rule both ways; the 32-bit package gate; every profile
upscaler/frame-gen/injection choice mapping to a `Field`; the JSON round trip;
and the docs table quoting every reason string.

## Known limitations

- **Not run against a real GPU.** Every case is a fixture profile;
  `pitagames-cli capability` on the RX 9070 / RX 5700 XT is the check once a
  target machine is reachable.
- **The 64-bit game check is not here.** Without the game's PE header the only
  64-bit gate this seam evaluates is the profile's DLSSNR `pkg_arch`; the
  backends (ticket #28, `goblin`) own the game-binary check.
- **No cost/VRAM presets yet.** The neural-rendering cost table and the
  VRAM-aware `model_scale` defaults are the backends' data (#28).
- **Anti-cheat is not folded in yet.** The injection gate (`platform::anticheat`)
  refuses injection for a protected game; that wiring lands with the graphics
  screen so `capability` stays a pure function of GPU + entry + Proton.
