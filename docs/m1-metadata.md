# M1 — Steam store metadata (appdetails)

This documents the first metadata slice: fetching Steam's `appdetails` payload,
rate limiting it, caching it in SQLite, and showing it in the detail page's
**About** block. Images and videos are deliberately out of scope — the art
pipeline owns images and v1 ships no `<video>`.

## Pieces

| Crate | Responsibility |
|---|---|
| `core` (`core::metadata`) | `GameMetadata`, `AppKind`, `Platforms` — plain domain types, no HTTP or SQL |
| `metadata` (`pitagames-metadata`) | Endpoint, request filters, HTML unescaping, token bucket, `CachePolicy` |
| `db` (`MetadataRepository`) | `game_metadata` table (migration 0002), JSON payloads |
| `app` (`persist`, `main.rs`, `detail.rs`) | Load the cache, decide what is stale, refresh in the background, render |
| `cli` (`pitagames-cli metadata`) | Live diagnostic: one fetch, human or `--json` output |

`pitagames-metadata` is framework-free and does not know SQLite; `db` stores a
`GameMetadata` as JSON without depending on the metadata crate; `crates/app`
is the only place that decides *when* to fetch.

## Endpoint behaviour (verified 2026-11)

```
https://store.steampowered.com/api/appdetails
    ?appids=<one appid>&l=<language>&filters=<fields>
```

- **One appid per request.** The old comma-separated batch form now returns
  body `null` for the whole request (checked with `curl` in 2026-11). The
  client therefore loops over the ids, one request each, and reports `null` as
  a per-app "no data" failure rather than a decode error.
- Duplicate ids are deduplicated before fetching, so a double-scanned library
  cannot spend two tokens on the same app.
- Default language is `english`; `--language` / `.language()` override it.
- Response shape: `{"<appid>": {"success": bool, "data": {…}}}`. A missing key,
  `success: false`, or a payload without a name is a per-app failure, not an
  error.

### Requested fields

```
basic,developers,publishers,genres,release_date,platforms,metacritic,background_raw
```

`basic` carries the name, type, short description, website, price flag and the
header/capsule URLs. Screenshots and movies are excluded: images belong to the
art pipeline and video is banned in the v1 renderer.

`GameMetadata` keeps the art URLs (`header_image`, `capsule_image`,
`background`) as strings; `crates/art` consumes them (see `docs/m1-art.md`).

## Rate limiting

Steam budgets `appdetails` per IP at roughly **200 requests / 5 minutes**.

- `RateLimit::steam_app_details()` = capacity 200, window 300 s.
- `TokenBucket` refills continuously: a cold start may fire 200 requests
  back-to-back, then one token becomes available every 1.5 s.
- The bucket lives in the client and is shared across chunks of one refresh.
- `fetch` sleeps for the reported delay instead of busy-waiting, and a failed
  acquisition consumes nothing.
- The bucket is clock-injectable (`Instant`), so the tests assert the drip
  without sleeping.
- Steam can still answer HTTP 429/5xx; that surfaces as `Error::Status` and the
  chunk is left un-cached for the next launch.

## Cache policy

```rust
StoredMetadata { game: GameId, metadata: Option<GameMetadata>, fetched_at: OffsetDateTime }
```

| Answer | Payload | TTL (`CachePolicy::DEFAULT`) |
|---|---|---|
| Game found | `Some(GameMetadata)` | 7 days |
| Store has no data | `None` | 24 hours |
| Transport / HTTP error | not cached | — |

Rationale: a game that exists today is unlikely to disappear, while a delisted
or region-locked app should be retried the next day. A transport failure is a
local problem and must never poison the cache. A `fetched_at` in the future
(the clock moved backwards) counts as fresh — a fetch storm is worse than
slightly stale data.

The table has no foreign key to `games`: metadata is a cache, and a game can
disappear from disk while its row remains useful. `store`/`store_id` are the
same identity pair used everywhere else.

## App flow

```
startup (background task)
  scan Steam                 → Library::load
  open SQLite, sync games    → apply_stored
  load game_metadata rows    → Library::apply_metadata
  Library::metadata_candidates(CachePolicy::DEFAULT, now)
      = installed/known Steam games whose row is missing or stale
  publish library (first paint), subtitle "fetching store metadata…"
  spawn refresh_metadata(db, library, targets)
      for each chunk of ≤20 appids:
          SteamClient::fetch → metadata_rows → save_metadata → apply_metadata
      (each chunk is saved and rendered as soon as it lands)
  clear "fetching…" and, if any appids have no store data, show a banner
```

- The fetch runs **after** the first paint: an offline or slow store never
  blocks the library.
- The detail page resolves its game from live library state by `GameId`, so
  metadata that arrives while the page is open appears without reopening it.
- Failures are warnings, not errors: the library, playtime and profiles all
  keep working.
- Chunking (20 ids) means a 400-game library or a network drop halfway keeps
  the chunks already fetched; the next launch asks only for what is still
  stale. The rate limiter is shared across chunks, so chunking does not raise
  the request rate.
- Cached metadata survives restarts; a second launch inside the TTL performs
  zero requests.

### What the UI shows

Detail page **About** block: short description, genre chips, developers,
publishers, release date (or "coming soon"), Metacritic score, platforms, and
"free to play" when flagged. A non-`game` app type (DLC, demo, tool) gets a
badge next to the title. The header line shows `store data updated <relative>`.

Without a cache entry the block renders an explanation instead of disappearing,
so a store outage looks like missing data rather than a broken page.

The grid cards read the cached metadata only for their capsule art (see
`docs/m1-art.md`); they still show playtime, store and appid as text. Genre and
year on cards are a later polish item.

## CLI

```bash
pitagames-cli metadata 620
pitagames-cli metadata 620 570 --json
pitagames-cli metadata 620 --language german
```

```
Portal 2  [game]
released Apr 18, 2011 · Valve · published by Valve
Genres: Action, Adventure
Metacritic: 95
Platforms: Windows, Linux

The "Perpetual Testing Initiative" has been expanded to allow you to design co-op puzzles for you and your friends!

Art: https://shared.akamai.steamstatic.com/store_item_assets/steam/apps/620/…/header.jpg
Site: http://www.thinkwithportals.com/
```

Per-app failures go to stderr as warnings; if nothing at all comes back the
command exits non-zero. The command fetches live and does not touch the cache,
which makes it the way to tell "Steam has no data" apart from "our cache is
wrong".

## Verification

Pure crates (macOS host):

```bash
CARGO_HTTP_MULTIPLEXING=false cargo test \
  -p pitagames-core -p pitagames-db -p pitagames-metadata \
  -p pitagames-stores -p pitagames-shim -p pitagames-cli
CARGO_HTTP_MULTIPLEXING=false cargo clippy \
  -p pitagames-core -p pitagames-db -p pitagames-metadata \
  -p pitagames-stores -p pitagames-shim -p pitagames-cli --all-targets -- -D warnings
cargo fmt --all -- --check
```

GUI (`crates/app`) in `archlinux:latest`:

```bash
cargo build -p pitagames --all-targets
cargo clippy -p pitagames --all-targets -- -D warnings
cargo test -p pitagames
```

Covered by tests and fixtures:

- `fixtures/steam/appdetails_620.json` (real Portal 2 response) and
  `appdetails_missing.json` (`success: false`): parsed fields, HTML entities,
  and the failure path.
- `parse_app_details`: `null`, missing ids, `success` without `data`, a payload
  without a name, malformed JSON.
- `SteamAppDetails::fetch` against a fake transport: one request per appid,
  deduplication, empty input, language/filters in the URL.
- Token bucket: burst then drip, no charge on a failed acquire, no overfill,
  backwards clock, and Steam's 200/300 s budget.
- `CachePolicy`: 7-day positive TTL, 24-hour negative TTL, future timestamps.
- `db`: metadata round-trip, payload/id mismatch rejection, corrupt JSON.
- `app`: cache rows for a mixed fetch, library candidates respecting the TTL,
  and the full scan → sync → metadata → reopen path in `wiring_tests`.

Container smoke run (`archlinux:latest`, Xvfb, fake Steam root, live network):

- 5 fake appids; appid 999 (no store data) is cached negatively, the other four
  get payloads.
- `select store_id, length(payload_json) from game_metadata` →
  `400|847`, `570|951`, `620|835`, `730|946`, `999|-1` (NULL = negative).
- The grid shows the banner "Steam has no store data for 1 of 5 appid(s)."
- The detail page for Portal 2 shows description, Action/Adventure, Valve,
  Apr 18 2011, Metacritic 95/100, Windows/Linux
  (`target/shot-metadata-detail.png`).

Known environment limitation: on this macOS host, locally built binaries time
out connecting to Steam (raw `TcpStream` fails while `curl` works), so the live
path is verified in the Linux container. Unit tests and the CLI run locally
against fixtures; nothing in the code depends on the host.

## Deliberately not in this slice

- Art download/resize/cache and the loopback image server — shipped next in
  `docs/m1-art.md`.
- Screenshots/movies, per-language cache partitioning, DLC/package traversal.
- Owned-but-not-installed games (needs the Steam Web API key).
- A manual "refresh metadata" button; the TTL and the next launch are enough
  for now.
