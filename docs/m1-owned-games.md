# M1 · Owned games (Steam Web API key + owned-list cache)

Steam tells you what is **installed** on this machine (the `appmanifest_*.acf`
scan) but not what the account **owns**. Owned-but-uninstalled games only come
from the Steam Web API, and that needs a per-account key. This slice adds the
key, the request, a SQLite cache and an Account panel that also lists owned
games in the grid with an Install button.

Without a key nothing changes: the library stays "installed only" and the
Account panel explains how to add one.

## Pieces

| Layer | Where | What |
|---|---|---|
| Secret store | `crates/secrets` (`pitagames-secrets`) | `SteamApiKey` (validated, redacting), the environment → keyring → private-file chain, atomic `0600` writes |
| API client | `crates/metadata/src/owned.rs` | `SteamOwnedGames::fetch(key, steam_id)` over the shared `Transport`; key scrubbing on every error |
| SteamID | `crates/stores/src/steam.rs` | `user_ids`, `active_user_id`, `steam_id64` |
| Domain | `crates/core/src/store.rs` | `OwnedGame { id, title, playtime, last_played }` |
| Cache | `crates/db` migration `0003_owned_games.sql` + `OwnedRepository` | replace-all per store, `fetched_at` per refresh |
| GUI state | `crates/app/src/steam_account.rs` | framework-free `Account`, key save/forget, `fetch_owned`, `is_stale` |
| GUI view | `crates/app/src/account.rs` | the Account tab |
| Merge | `crates/app/src/library.rs` | `apply_owned`, `drop_owned`, owned-aware `metadata_candidates` |
| CLI | `pitagames-cli steam-account` | read, `--set-key`, `--forget-key`, `--refresh` |

## The key

`steamcommunity.com/dev/apikey` issues a 32-hex-character key tied to an
account. It is the only secret PitaGames stores.

Read order (`SteamKeyStore::load`):

1. `PITAGAMES_STEAM_API_KEY` — an explicit override for tests, CI and one-off
   commands. A *valid* value wins; an invalid one is a warning and is skipped,
   so a typo in the environment cannot hide a good key in the keyring.
2. The desktop keyring (`keyring` 4 over zbus Secret Service on Linux, Keychain
   on macOS).
3. `$XDG_CONFIG_HOME/pitagames/secrets/steam-api-key`, mode `0600`, directory
   `0700`, written through a `0600` temp file + `fsync` + `rename`. Used when
   there is no session keyring (containers, headless boxes) and always announced
   in a warning.

`save` prefers the keyring and deletes the fallback file when the keyring takes
the key, so there is one copy. `forget` clears every writable store and warns
when the environment still overrides.

`SteamApiKey` has no `Display`, its `Debug` prints `…9f3c`, and the raw value
needs `expose_secret()`. That is deliberate: request URLs carry the key, so the
type makes accidental logging a compile error rather than a code review.

`--set-key -` reads the key from stdin, so it need not land in shell history.

## The request

`IPlayerService/GetOwnedGames/v1/?key=…&steamid=…&include_appinfo=1&include_played_free_games=1&format=json`

- One request returns the whole library, so the `appdetails` budget (200 / 5
  min) does not apply. Startup refreshes at most once a day (`REFRESH_AFTER`),
  and the panel has a manual refresh.
- `steamid` is the SteamID64. `userdata` is named with the 32-bit account id, so
  `steam_id64(id) = 76561197960265728 + id`; `active_user_id` picks the account
  whose `userdata/<id>` directory was modified most recently, which is the one
  Steam last used.
- `{"response":{}}` means "private profile, or the key belongs to another
  account" and is reported as `OwnedError::PrivateProfile`, never as "you own
  nothing". HTTP 401/403 is `Rejected`, 429 is `RateLimited`.
- Every error message is scrubbed of the key before it leaves the module: the
  failure text echoes the request URL.

## The cache and the merge

`owned_games` is replaced wholesale on each successful refresh, so a refund
removes a row and a purchase adds one. `fetched_at` is one timestamp per
refresh. No foreign key to `games`: an owned game is usually not installed, and
the cache must survive a scan that finds nothing.

`Library::apply_owned` merges the list into the grid:

- an **installed** row wins: the scan knows the install state, path and Steam's
  own playtime from `localconfig.vdf` (whole minutes vs. the API's whole
  minutes, but the scan's is refreshed per scan); owned-only data fills a zero
  playtime / missing last-played;
- an **owned-but-uninstalled** game becomes a `NotInstalled` row, so its card
  offers **Install**; the count is shown as "N owned" in the subtitle;
- the merge is idempotent, so startup merges the cache and a refresh merges the
  new answer over the same rows.

A failed refresh **keeps** the cached rows and pushes a warning
(`OwnedSummary::warnings`); the next launch retries.

## Metadata budget

The grid asks the store for metadata for every game it shows. Owned lists can be
thousands of games, which would blow the `appdetails` budget in one run.
`Library::metadata_candidates(policy, now, owned_cap)` therefore always refreshes
**installed** games and caps the **owned-only** tail at `OWNED_METADATA_PER_RUN`
(100 per run). A large library drains over a few launches; each answer is cached
7 days, so it converges.

There is one metadata worker (`metadata_worker` in `main.rs`) holding the only
`SteamClient`, so there is exactly one token bucket regardless of who enqueues
(startup scan, owned refresh, Account panel). `MetadataQueue` counts queued
appids so the worker knows when the "fetching store metadata…" indicator can come
down.

## Verification

- **Pure crates**: fmt + clippy `-D warnings` + tests clean. `pitagames-secrets`
  9 tests (validation/redaction, `0600`/`0700` atomic file, chain order incl.
  invalid-env and corrupt-file cases, keyring→file fallback, env override
  refusing writes, forget clearing both). `pitagames-metadata` 28 (owned parser,
  private-profile vs. empty, nameless entries, sorting, key scrubbing on 401/429/
  500, SteamID validation before any request). `pitagames-db` 22 (owned
  round-trip, replace drops un-owned rows, per-store scoping, corrupt rows,
  empty-list clears). `pitagames-cli` 19 (new `steam-account` arg parsing).
  `pitagames-stores` 98 + 13 (SteamID64 arithmetic, `user_ids` ordering, no
  `userdata`).
- **`crates/app`** in `archlinux:latest`: build, clippy `-D warnings`, 46 tests
  (Account staleness/readiness, owned merge/cap/drop, the owned→SQLite→grid
  round trip).
- **CLI live run** (container, real network): a fixture `userdata/12345` resolved
  to SteamID `76561197960278073`; a bad key was rejected by name; a well-formed
  key was written to the `0600` fallback file with a warning (no session keyring
  in the container) and read back as `…CDEF (fallback file)`; `--refresh` reached
  `api.steampowered.com` and reported `HTTP 401` as "Steam rejected the API key"
  with no key in the message; `PITAGAMES_STEAM_API_KEY` refused a write;
  `--forget-key` removed the file.
- **GUI Xvfb smoke run** (fixture Steam root, live `appdetails`): the Account
  tab rendered `installed only · no API key`, the SteamID line and the keyring
  warning; typing a key and pressing **Save key** wrote the fallback file, showed
  "Key saved to the fallback file." and immediately refreshed (HTTP 401 surfaced
  as a warning); with three rows seeded into `owned_games`, a restart showed
  "4 Steam games · 3 owned" with Half-Life and Half-Life 2 as `NOT INSTALLED` +
  **Install** and Portal 2 keeping **Play**, and the Account tab showed
  "3 games owned · refreshed 1090 d ago". Screenshots:
  `target/shot-account-2.png`, `shot-account-4.png`, `shot-account-owned.png`,
  `shot-account-owned-2.png`.

## Known limits

- **No valid key was available for the test**, so the success path (a populated
  `owned_games` from the API) is verified by unit tests + a seeded cache, not by a
  real account. One real refresh on the RX 9070 / RX 5700 XT closes this.
- One account per store: rows are scoped by `store`, not SteamID. A multi-account
  view would add the account column via a migration.
- `game_count` from the API can exceed the rows Steam returns (it filters
  some); the cache stores the rows, so "N owned" is the row count until a refresh
  replaces it, and the CLI prints both numbers.
- The keyring is unavailable in a bare container; the fallback file is the
  documented path there. On a desktop with gnome-keyring/KWallet the keyring path
  is used.
- API key management is a PitaGames-side convenience only; the key still grants
  read access to the account's public data to anyone who has it, which is why it
  is stored in the keyring first.
