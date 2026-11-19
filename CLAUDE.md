# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Hardcover Desktop is a Linux desktop client for [hardcover.app](https://hardcover.app). It is written in Rust (edition 2024) with gpui-kit 0.10. The licence is MIT, and the app id is `io.github.alisayyah.HardcoverDesktop`.

## Commands

```sh
cargo run -p hardcover-desktop                    # run the app (signs in with OAuth, or restores the keyring session)
cargo test --workspace                            # unit tests; live-API tests are #[ignore]d
cargo test -p hardcover-core <name>               # one test, e.g. `unauthorized_refreshes_once_and_retries`
cargo clippy --workspace --all-targets
cargo fmt --all

# Live API: needs network and a token
HARDCOVER_TOKEN=... cargo test -p hardcover-core --test live_read -- --ignored
HARDCOVER_TOKEN=... cargo run -p hardcover-core --example smoke   # read-only
cargo run -p hardcover-core --example oauth_login                 # CLI sign-in, stores the session in the keyring
cargo run -p hardcover-core --example oauth_check -- <book-id>    # scope + write cycle on a book NOT in the library
cargo run -p hardcover-core --example api_check                   # check the live API for changes; updates schema/
cargo run -p hardcover-core --example api_check -- --check        # same, without writing; exit 1 on any change
```

`api_check` is local only and is not run in CI. It uses `HARDCOVER_TOKEN` if set, otherwise the app's keyring session; both see the same schema. It does three things:
1. Introspects the live schema and lists added, removed and changed types and fields.
2. Validates every document in `queries::ALL` against the live schema.
3. Checks the public scope map (`https://api.hardcover.app/capabilities.json`): `auth::DEFAULT_SCOPES` must still grant every top-level field the app uses.

It then rewrites `schema/hardcover.graphql` and `schema/hardcover-capabilities.json`; review the change with `git diff schema/`. It exits non-zero if a document breaks or a scope no longer covers it. Never edit `schema/` by hand.

Environment variables (see `.env.example`; `.env` is gitignored and must never be committed):
- `HARDCOVER_TOKEN`: a personal access token. For development only: it bypasses OAuth in the app, and the examples and live tests use it.
- `HARDCOVER_CLIENT_ID`: overrides the compiled-in OAuth client id, `auth::DEFAULT_CLIENT_ID`.
- `HARDCOVER_THEME=light`: forces the light theme.
- `HARDCOVER_NO_UPDATE_CHECK=1`: turns off automatic update checks. Debug builds never check on their own; "Check for updates" in the account menu always works.
- `live_writes` example: `HARDCOVER_LIVE_WRITES=yes`, `HARDCOVER_LIVE_PHASE`, `HARDCOVER_TEST_BOOK_ID`.

Live write tests run against a real account. Restore everything afterwards. Removing a book does not delete its `reading_journals` rows; clean those up separately, which needs a token with journal scope.

## Architecture

The workspace has two crates, in `crates/*`.

**`hardcover-core`** has no GPUI dependency, and every call in it is **synchronous and blocking**: there is no async runtime.
- `api`: `Client` over the GraphQL endpoint `https://api.hardcover.app/v1/graphql`.
  - `queries.rs` holds every GraphQL document as a const. A unit test validates each one against `schema/hardcover.graphql`, an introspected SDL loaded through apollo-compiler.
  - When you add or change a query, register it in `queries::ALL` so the test and `api_check` cover it. `api/sdl.rs` holds the validator; `api_check` includes the same file through `#[path]`, because apollo-compiler is a dev-dependency.
  - `wire.rs` holds the raw response shapes; `model.rs` holds the public types.
- `http.rs`: a small transport seam. Production uses ureq. `http::fake::FakeTransport` scripts responses for the tests in `api/tests.rs`.
  - All fixtures must be synthetic: no real user data.
  - `HttpRequest` deliberately does not implement `Debug`, because it carries tokens.
- `ratelimit.rs`: a client-side token bucket with burst 8 and a refill of 1 per second. Hardcover's own limits are 60 requests per minute, burst 10, and at most 10 top-level fields per request.
  - On a 429 the client waits and retries once.
  - On a 401 it calls `TokenSource::on_unauthorized` once, which refreshes the token, then retries.
- `auth`: OAuth for a native public client.
  - Authorization code with PKCE S256, redirected to a loopback `http://127.0.0.1:<port>/callback`. `state` and `iss` are checked.
  - `Session` stores tokens in the system keyring (`KeyringTokenStore`, secret-service) and refreshes them proactively. Refresh tokens are single-use, so refreshes are serialized.
  - `StaticToken` is the `HARDCOVER_TOKEN` path.
- `store`: `LibraryCache`, a SQLite database at `~/.local/share/hardcover-desktop/library.sqlite3`.
- `covers`: `CoverCache`, on disk at `~/.cache/hardcover-desktop/covers`.

**`app`** (the package is named `hardcover-desktop`) is the GPUI UI.
- `main.rs` sets up actions, keybindings and the window.
- `app_state.rs` holds the GPUI globals:
  - `Services` (client, session and cache) exists only while signed in.
  - `CoverService` lives for the whole run.
  - `startup()` picks the auth path in this order: `HARDCOVER_TOKEN`, then the keyring session, then the sign-in screen.
- Every core call runs inside `cx.background_spawn`, and its result is applied on the foreground. Never call `hardcover-core` on the UI thread.
- Covers are GPUI `Asset`s loaded with `window.use_asset`:
  - `CoverThumbnail` is 144 px and `CoverImage` is 480 px.
  - Decoding happens on one dedicated `cover-decoder` thread, not the background pool. Decoding on GPUI's 16 pool threads bloated glibc's per-thread malloc arenas: 62 MB versus 495 MB on a 442-cover benchmark. Keep large image decodes on that thread.
- `library/model.rs`: `LibraryModel` is the one entity every library view reads.
  - It loads the cache first, then syncs page by page.
  - Writes return the full entry, which replaces the local one and is upserted into the cache.
  - A sync that started before a write re-applies that write when the sync lands (`collection::merge_sync`).
  - `begin_write`/`end_write` track the writes in flight, per control.
- `shell.rs`: `Shell` switches between Starting, SignIn and Workspace. `Workspace` contains the sidebar (shelves by status) plus the current destination (library, search or book detail).

## Hardcover API quirks the code relies on

- Status ids: 1 Want to Read, 2 Currently Reading, 3 Read, 4 Paused, 5 Did Not Finish, 6 Ignored.
- Setting a rating without a status moves the book to Read. `set_rating` therefore takes a `keep_status` and resends the current status.
- Reading an entry right after a write can return stale data. Use the entry from the mutation's own response, and refetch only when it is missing.
- Moving a book to Currently Reading starts a new read. A progress update with no open read moves the book to Currently Reading first.
- Search has a 2 s server timeout. The `_like`, `_ilike` and regex filters are disabled.
- docs.hardcover.app blocks automated fetches. Read the docs source at github.com/hardcoverapp/hardcover-docs instead.

## gpui-kit

Use the bundled `gpui-kit` and `gpui-kit-design-guides` skills, in `.claude/skills`, before any UI or architecture work. When the skills and the crate disagree, the crate source wins: `~/.cargo/registry/src/index.crates.io-*/gpui-component-0.10.0`. For example, `ActiveTheme` is imported from `gpui_kit::component::ActiveTheme`.

## CI and releases

- `.github/workflows/ci.yml` runs on pull requests, on pushes to `master` and weekly:
  - fmt
  - clippy with `-D warnings`, and the tests, as two parallel jobs
  - `cargo deny` (`deny.toml`)
  - desktop-entry and metainfo validation
  - actionlint
- Clippy and the tests run in an `ubuntu:22.04` container, the same one the release builds in. `ci/install-build-deps.sh` holds the system packages; it is the single list for both.
- `.github/actions/setup-rust` sets up every build job. Its caches are saved only from `master`; other branches restore them.
  - apt: the Ubuntu mirrors sometimes stall for minutes, so the package lists and `.deb`s are cached under a monthly key. On an exact hit, CI jobs install without `apt-get update`. The release always updates (`fresh-packages: true`), so its packages are checked against freshly signed lists.
  - zstd is installed after the apt cache is saved, so that cache stays gzip; it is restored before zstd exists. The cargo caches (`Swatinem/rust-cache`, one per job) then use zstd.
- The toolchain is pinned in `rust-toolchain.toml`. Bump it on purpose, and fix any new clippy lints in the same commit.
- Actions are pinned to commit SHAs; Dependabot updates them.
- **To release:**
  1. Bump `version` in `crates/app/Cargo.toml`.
  2. Add a `<release>` to `packaging/linux/io.github.alisayyah.HardcoverDesktop.metainfo.xml`.
  3. Push a `vX.Y.Z` tag.

  `release.yml` checks that the three versions agree, builds with `--profile dist` (thin LTO, stripped), packs a tarball (`packaging/linux/package.sh`) and creates the GitHub release. Running it by hand builds the tarball as an artifact without publishing anything.

## Self-update

The core side is `hardcover_core::update`; the app side is `crates/app/src/update.rs` plus the sidebar row in `shell.rs`.
- **When it installs:** the app checks `releases/latest` on GitHub. It installs a release only when it runs from an unpacked release tarball (`<prefix>/bin/hardcover-desktop` with the desktop entry under `<prefix>/share`) in a writable prefix. Other installs only announce new releases.
- **Trust chain:**
  - The minisign signature must verify against `packaging/minisign.pub`.
  - The signature's `file:` comment must be the tarball name for the advertised version and architecture, which stops replays of older signed releases.
  - The version must be newer than the running one.
  - The archive's top-level directory must match the tarball name.
  - Only regular files under `bin/hardcover-desktop` and `share/` are unpacked. They are staged in the prefix and renamed into place, the binary last.
- **Restarting:** `main` resolves the executable path before anything else and passes it to `set_restart_path`. After an update, `/proc/self/exe` points to the deleted old binary.
- **Signing:**
  - `release.yml` signs in the publish job, with the `MINISIGN_SECRET_KEY` repository secret exposed to that one step only, then verifies with `packaging/minisign.pub`.
  - Never replace the key: installed copies trust only this one.
  - Never re-publish different bytes under an existing version. If a release goes wrong, release a new patch version.
- **Release notes:** generated changes come first, followed by `<!-- install -->` and `packaging/linux/release-notes.md`. The app's update dialog shows only the part before the marker.
