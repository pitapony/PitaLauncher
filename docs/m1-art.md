# M1 — Art pipeline (download → WebP → loopback cache)

This is the image slice that follows `docs/m1-metadata.md`: the store's art URLs
(from the cached `appdetails` payload) are downloaded in the background, resized
to the two sizes the UI renders, encoded as WebP, cached on disk, and served to
the webview by a token-guarded loopback HTTP server.

Images are the only media in the v1 UI. Screenshots and trailers stay out:
`<video>` is a hard non-goal (it would bind the renderer to GStreamer/WebKit and
block the planned Blitz migration).

## Pieces

| Crate | Responsibility |
|---|---|
| `art` (`pitagames-art`) | `ArtCache` (keys, read/write), `Downloader` (HTTP → decode → crop → WebP), `ArtServer` (axum loopback) |
| `app` (`art.rs`, `main.rs`, `library.rs`, `detail.rs`) | Which images a game wants, background warming, cache-gated `<img>` rendering |
| `core` (`core::metadata`, `core::paths`) | `GameMetadata` art URLs; `cache_dir()` → `~/.cache/pitagames` |

`pitagames-art` is framework-free: no Dioxus, no SQLite, no knowledge of which
screen is showing. `crates/app` owns every policy decision (when to fetch, what
to fall back to, what a failure looks like). `ArtContext::start` runs in the
startup task before the library is published; it creates the cache directory,
binds the loopback server on the tokio runtime and builds the shared HTTP
client.

## Sizes and cache layout

Steam's `capsule_image` is 231×87 and `header_image` is 460×215. Both are stored
at 2× so HiDPI displays do not upscale blurry art:

| `ArtSize` | Source | Pixels | UI use |
|---|---|---|---|
| `Grid` | `capsule_image` (fallback `header_image`) | 462×174 | library card |
| `Hero` | `header_image` (fallback `background`) | 920×430 | detail page |

```
~/.cache/pitagames/art/<sha256(url)>-<grid|hero>.webp
```

- The key is the hex SHA-256 of the **full source URL**, so a changed CDN URL
  produces a new file rather than serving stale art.
- One file per `(URL, size)` pair; the grid and hero of the same source are
  separate entries because they are resized differently.
- `ArtCache` (`cache.rs`) exposes `ensure_dir`, `exists`, `read`, `write`,
  `remove`, `entries`, `path_for`, `dir`. Nothing is created until
  `ArtContext::start` calls `ensure_dir`.
- The cache is a pure cache: deleting it is always safe. The next launch
  re-downloads what it needs.

## Download pipeline

`Downloader` (`fetch.rs`) holds one shared `reqwest::Client` (rustls, 20 s
timeout, `User-Agent: PitaGames/<version>`) so TLS sessions and the connection
pool are reused.

```
GET source URL            (one per image, no batch endpoint)
decode PNG/JPEG/WebP      image::load_from_memory
cover-crop                scale to fill 462×174 / 920×430, then centre-crop
encode                    lossless WebP (image-webp backend)
write                     ~/.cache/pitagames/art/<key>.webp
```

- **Cover-crop, not stretch.** Steam art is usually ~2.65:1 (capsule) and
  ~2.14:1 (header), close to the targets, but a few apps ship odd aspects.
  Scaling to fill and cropping the overflow keeps the art undistorted; CSS
  `object-fit: cover` then decides the final box.
- **Lossless WebP.** The `image` 0.25 WebP encoder only exposes lossless
  encoding (`image-webp` has no lossy encoder yet). Files are still far smaller
  than PNG (measured: grid 32–89 KB, hero 170–460 KB).
- `warm(cache, request)` is a no-op when the file already exists; it returns
  `true` only for a real download, which is what the UI uses to decide whether
  to repaint.

## Loopback server

`ArtServer` (`server.rs`) is an axum 0.8 app on the Dioxus (tokio) runtime:

- binds `127.0.0.1:0` — a random free port, never a fixed one;
- generates a fresh 32-character alphanumeric token per process;
- serves only `GET /<token>/<key>.webp`, reading from the art cache directory;
- answers `404` for a wrong token, a missing file, an empty name, and any path
  containing `/` or `..` (axum percent-decodes first, so `%2e%2e%2f` is
  rejected too);
- sends `Content-Type: image/webp` and
  `Cache-Control: public, immutable, max-age=31536000`;
- runs in a spawned task that is aborted when `ArtServer` is dropped.

The token is the only access control. The port is on loopback, but any local
process can connect, so an unguessable path is what keeps this session's cache
private. The webview only ever receives URLs the app built itself — no
`file://`, no direct path the page could forge.

```
http://127.0.0.1:<random>/<32-char-token>/<64-hex>-grid.webp
```

## App flow

```
startup (background task, after the library scan)
  ArtContext::start()
      cache_dir = ~/.cache/pitagames/art   → ensure_dir
      ArtServer::start(cache)               → port + token
      Downloader::new()
  library.art_requests()                    → images for cached metadata
  publish library (first paint, placeholders where art is missing)
  spawn warm_art(requests)                  → 4 concurrent downloads
  spawn refresh_metadata(targets)
      each chunk: fetch → save → apply_metadata → warm that chunk's art
  every completed download bumps art_revision → grid/detail re-render
```

Which image a game wants (`art::requests_for`):

| Slot | First choice | Fallback | Never |
|---|---|---|---|
| Grid | `capsule_image` | `header_image` | `background` (1920×620 is not worth cropping to a capsule) |
| Hero | `header_image` | `background` | — |

- A game with no cached metadata (or a negative cache row) contributes no
  requests: no store data means no art URLs to try.
- Requests are deduplicated per batch and across the initial library pass and
  each metadata chunk. `warm` then skips anything already on disk.
- Downloads run four at a time (`buffer_unordered(4)`): fast enough for a
  library page, gentle enough not to hammer Steam's CDN.
- The UI renders `<img>` **only when `cache.exists()` is true**. A card without
  art shows a gradient placeholder with the title's first letter; the detail
  page simply omits the hero. Nothing points the webview at a 404.
- The detail hero is constrained to `max-width:760px; aspect-ratio:460/215`
  with `object-fit: cover`, so the 2× header art is not stretched across a wide
  window.

### Failure policy

Art is decorative. A failed download, an unreachable CDN, a corrupt image or a
cache that cannot be created is:

- logged as `[pitagames] art <url>: <error>` on stderr,
- never shown as an error banner,
- retried on the next launch (nothing negative is cached).

If the server or the HTTP client cannot start at all, `ArtContext::start`
returns a display string that goes into the metadata warning area; the library
still scans, launches, tracks installs and saves playtime.

## Verification

Pure crates (macOS host, and CI):

```bash
CARGO_HTTP_MULTIPLEXING=false cargo test -p pitagames-art
CARGO_HTTP_MULTIPLEXING=false cargo clippy -p pitagames-art --all-targets -- -D warnings
cargo fmt --all -- --check
```

GUI (`archlinux:latest`):

```bash
cargo build -p pitagames --all-targets
cargo clippy -p pitagames --all-targets -- -D warnings
cargo test -p pitagames -p pitagames-art
```

Covered by tests:

- `cache`: deterministic keys, 64-hex + size suffix, grid ≠ hero, round-trip,
  silent missing entries, `entries` ignores non-WebP files, `ensure_dir`.
- `fetch`: encode → decode round-trip at 462×174, cover-crop produces the exact
  target size from square and ultrawide sources, `warm` on a cached file makes
  no request.
- `server` (real loopback, real HTTP): 200 + `image/webp` + `immutable` for the
  right token, 404 for a wrong token, a missing file and `%2e%2e%2f` traversal;
  two servers get different ports and tokens, and one token cannot read the
  other's cache.
- `app`: request selection (capsule → header → background rules), negative rows
  and duplicates skipped, `Library::art_requests`.

Container smoke run (`archlinux:latest`, Xvfb, fake Steam root, live network):

- 5 fake appids; 4 have store data, appid 999 is the negative case and keeps the
  letter placeholder.
- Cold start: 8 files cached (4 grid, 4 hero); grid shows real Steam capsules;
  Portal 2's detail page shows its header hero
  (`target/shot-art-grid.png`, `target/shot-art-detail.png`).
- Warm start (same cache, new process): art renders from disk in under 8 s with
  **0** metadata inserts and **0** CDN requests
  (`target/shot-art-warm.png`), proving the server + cache path needs no
  network.
- No `[pitagames] art` lines in either run: no silent download failures.

Known environment limitation: on the macOS host, raw TCP to Steam times out
while `curl` works, so live downloads are verified in the Linux container.
Unit tests exercise the parser, cache and server locally; nothing in the code
depends on the host.

## Known limitations

- **Cache growth is unbounded.** 100 games ≈ 50 MB (8 images per launch is not
  the cap — every game with metadata gets two). Pruning (LRU by file mtime) is
  planned for the hardening milestone, not this slice.
- **Lossless WebP only**, so heroes are larger than a lossy encode would be.
  Revisit when `image-webp` exposes lossy encoding, or switch encoders.
- **Art depends on metadata.** A game with no `appdetails` row (delisted, or
  before the first refresh) shows a placeholder until the next successful
  fetch.
- **No cache invalidation beyond the URL.** If Steam swaps the image behind the
  same URL, the cached file wins. Acceptable for store art that rarely changes.
- **No SteamGridDB** yet: non-Steam titles (M2 stores) have no art source.
- **No image prefetching below the fold.** All requests are fired at once for
  the whole library; virtualization (when the grid gets one) should gate this
  on visibility.

## Deliberately not in this slice

- Lossy WebP/AVIF encoding and a quality knob.
- Screenshots/trailers, animated capsules, dominant-colour extraction.
- Poster art and a poster/hero grid toggle (needs SteamGridDB).
- A manual "refresh art" button; TTLs and the next launch are enough for now.
- Cache size accounting in Settings (same milestone as pruning).
