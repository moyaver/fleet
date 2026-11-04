# Rust conventions

How Fleet's Rust is written. Anything marked **(CI)** is enforced by `cargo xtask ci`; the rest is checked in review. The invariants in [PLAN §3](PLAN.md#3-invariants-non-negotiable) come first: when a convention and an invariant disagree, the invariant wins.

Sources are listed at the end; the bracketed tags point to them.

## Workspace and packages

- **Flat layout.** Crates live in `crates/fleet-*`, the folder name is the package name, the root manifest is virtual, and every crate is `publish = false` with `version = "0.0.0"`. [MATKLAD-WS] [MS]
- **Split a crate only along a real boundary**: a dependency rule (wasm-safe, no I/O), a separately cached compile unit, or a separate binary. Use modules for everything else. The boundaries are in PLAN §4.2 **(CI: `xtask lint`)**.
- **Thin binaries.** `main.rs` parses arguments, installs logging, builds the runtime and calls the library. Logic lives in `lib.rs`, where tests can reach it. [BOOK]
- **Module files are `foo.rs` plus `foo/bar.rs`.** **(CI: `mod_module_files`)**
- **Visibility.** `pub` is the crate's API; anything shared inside a crate is `pub(crate)`; binaries have no `pub`. **(CI: `unreachable_pub`)** Every item has one public path: no glob re-exports.
- **Imports** are grouped std, external, then `crate::`, one `use` per crate root. rustfmt can't do this on stable, so it's a review rule.
- **Edition 2024.** `std::env::set_var`/`remove_var` are `unsafe` now: pass configuration explicitly, and set a child's environment with `Command::env`.

## Dependencies and features

- **Every dependency is declared once in `[workspace.dependencies]` with a full `x.y.z` version**; members write `dep.workspace = true`. Versions track the latest stable releases (Dependabot proposes them).
- **Workspace entries declare minimal features; members add their own** with `{ workspace = true, features = [...] }`. Features unify across the graph, so a feature at workspace level reaches every member. [CARGO]
- **Heavy crates** (sysinfo, reqwest, object_store, sqlx) use `default-features = false`.
- **Features are additive**: no `no-*` features, and enabling a feature never removes an item.
- **The lockfile is committed and every command passes `--locked`.** **(CI)**
- **Only permissive licenses** in anything linked into a binary. **(CI: `cargo deny`, see `deny.toml`)**

## Errors and panics

- **Libraries define `thiserror` enums** (one per crate or per boundary). **Binaries use `anyhow`** (`fleet-server`, `fleet-agent`, `fleet-loadgen`, `fleet-e2e`, `xtask`). **(CI: `xtask lint` keeps `anyhow` out of library crates)**
- **A wrapped error goes in `#[source]`/`#[from]` or in the message, never both.** Use `#[error(transparent)]` to pass one through.
- **Messages are lowercase, without trailing punctuation, and name the failed operation**: `.with_context(|| format!("binding {listen}"))`. Print the chain with `{err:#}`.
- **Panics are for bugs.** `unwrap()` appears only in tests **(CI: `unwrap_used`)**. When an invariant guarantees a value, use `expect("<x> should <hold> because …")`.
- **Parsers of untrusted input never panic**: `fleet-maestro` and `fleet-core` also enable `indexing_slicing`, `string_slice`, `expect_used` and `panic`. A panic on a strange Maestro line would break fail-open (I2).
- **`std::process::exit` only in the agent's and server's entry points.** **(CI: `exit`)**
- **Release keeps `panic = "unwind"`**, so a panicking observer task becomes a `JoinError` instead of killing the supervisor (I1, I2). Only the `wasm-release` profile aborts.

## API design

- **Names** follow RFC 430 and the `as_`/`to_`/`into_` conversions; getters have no `get_` prefix; avoid weasel nouns (`Manager`, `Service`, `Factory`). [APIG]
- **Newtypes for IDs and validated values** (`RunId(Uuid)`), and **enums instead of `bool` or `Option` flags**.
- **Public types implement `Debug`** **(CI: `missing_debug_implementations`)**. Secrets get a redacting `Debug` (or `secrecy`) and a test for it (I6).
- **`#[must_use = "why"]`** on guards, handles and builders.
- **Generic parameters (`impl AsRef<Path>`, `impl BufRead`) only at public entry points**; internals stay concrete.
- **Parsers are sans-I/O**: they take `&str`, `&[u8]` or `impl BufRead` and never open files or sockets.
- **Builders only for four or more optional fields**, with a fallible `build()`; otherwise a plain struct with `Default`.
- **Third-party types don't cross crate boundaries** (no `sqlx::Error` out of `fleet-store`). `fleet-protocol` types are the shared vocabulary.

## Async

- **Nothing blocks inside async code.** Use `spawn_blocking` for short blocking work and a dedicated `std::thread` for long-lived blocking work (the agent supervisor is all `std`, ADR-0002).
- **A `std::sync::Mutex` is fine if it's never held across `.await`.** **(CI: `await_holding_lock`)**
- **`select!` arms are cancel-safe** (`mpsc::recv`, `accept`, `read`, `StreamExt::next`); pin reused futures outside the loop. Public async functions document `# Cancel safety`.
- **Structured concurrency**: every task belongs to a `JoinSet` or a `TaskTracker`. Shutdown is `CancellationToken::cancel()`, `tracker.close()`, then `wait()` under a timeout.
- **Every network wait has a timeout**, and failure is fail-open (I2).
- **The agent builds its observer runtime explicitly** (`tokio::runtime::Builder`, few workers), not with `#[tokio::main]` (I10).
- **Servers shut down on SIGINT and SIGTERM.**

## Logging and output

- **Libraries depend only on `tracing`; binaries install `tracing-subscriber`.** **(CI: `xtask lint`)**
- **Structured fields and a constant message**: `info!(%listen, "server listening")`. Spans use `#[instrument(skip_all, fields(...))]` so secrets and large arguments stay out.
- **Levels**: `error` needs an operator, `warn` means degraded or failing open, `info` is lifecycle, `debug`/`trace` are for development. Never log a secret value.
- **No `println!`/`eprintln!` outside tests** **(CI: `print_stdout`, `print_stderr`)**. `xtask` opts out at crate level. The agent's passthrough writes with `io::Write` and handles `EPIPE`.

## Testing

- **Unit tests sit in `#[cfg(test)] mod tests` in the same file.**
- **Integration tests** (`tests/`) are for what needs a built binary or the public API, such as the agent supervisor around a fake Maestro. Keep it to one test binary per crate where possible.
- **Snapshot tests (`insta`) for every fixture × parser pair**; commit the `.snap` files. **Property tests (`proptest`)** for the claim lattice and state machines; commit `proptest-regressions/`.
- **Run with `cargo nextest run`** (one process per test, hang timeouts in `.config/nextest.toml`) and doctests with `cargo test --doc`. **(CI)**
- **Tests may unwrap, expect, panic, print and index** (`clippy.toml`). Each test uses its own temp directory. Synchronize on events, not on `sleep`, except when replaying timed fixtures.
- **Fixtures are captured, never invented** (PLAN §0.6): `cargo xtask fixtures capture | import`. Anything else is marked `// UNVERIFIED`.

## Documentation

- **Every crate starts with a `//!` doc**: purpose, invariants, and the PLAN or ADR it implements.
- **Every `pub` item in a library is documented** **(CI: `missing_docs`, rustdoc `-D warnings`)**, with a first sentence of about 15 words. Add `# Panics`, `# Safety` and `# Cancel safety` where they apply. `# Errors` is optional: documented error variants carry that contract.
- **Examples use `?`**, and doctests are kept to `fleet-protocol` and `fleet-core`.

## Unsafe

- **`unsafe_code` is denied everywhere.** The only file that may opt out is `crates/fleet-agent/src/sys.rs`, with `#![expect(unsafe_code, reason = "…")]` **(CI: `xtask lint`)**. Prefer safe APIs first: `CommandExt::process_group`, `signal-hook`'s `emulate_default_handler`, `nix`.
- **If unsafe is unavoidable**: one operation per block with a `// SAFETY:` comment, `# Safety` on every `unsafe fn`, and `unsafe_op_in_unsafe_fn` denied **(CI)**.

## Lint exceptions

Exceptions are source attributes with a reason: `#[expect(clippy::too_many_lines, reason = "…")]`, at the narrowest scope that works. `expect` fails the build once the exception is no longer needed, so stale exceptions don't pile up **(CI: `allow_attributes`, `allow_attributes_without_reason`)**. Members can't override `[workspace.lints]` in their own `Cargo.toml`.

## Formatting and profiles

- **`cargo fmt`** with the stable options in `rustfmt.toml`. **(CI)**
- **Profiles**: `release` uses LTO and one codegen unit, and keeps unwinding; `wasm-release` optimizes for size and aborts. Measure (the I10 overhead benchmark) before tuning allocation or adding an allocator.

## Sources

- [APIG] Rust API Guidelines: <https://rust-lang.github.io/api-guidelines/>
- [MS] Microsoft Pragmatic Rust Guidelines: <https://microsoft.github.io/rust-guidelines/>
- [CARGO] The Cargo Book, workspaces, features, lints and profiles: <https://doc.rust-lang.org/cargo/reference/workspaces.html>, <https://doc.rust-lang.org/cargo/reference/features.html>, <https://doc.rust-lang.org/cargo/reference/profiles.html>
- [BOOK] The Rust Programming Language, packages, modules, tests and errors: <https://doc.rust-lang.org/book/>
- Clippy lints and configuration: <https://rust-lang.github.io/rust-clippy/>, <https://doc.rust-lang.org/clippy/lint_configuration.html>
- rustfmt options: <https://rust-lang.github.io/rustfmt/>
- Edition 2024 guide: <https://doc.rust-lang.org/edition-guide/rust-2024/>
- [MATKLAD-WS] "Large Rust Workspaces": <https://matklad.github.io/2021/08/22/large-rust-workspaces.html>; "Delete Cargo Integration Tests": <https://matklad.github.io/2021/02/27/delete-cargo-integration-tests.html>
- tokio: `spawn_blocking` <https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html>, `select!` cancel safety <https://docs.rs/tokio/latest/tokio/macro.select.html>, `tokio-util` task tracking <https://docs.rs/tokio-util/latest/tokio_util/task/struct.TaskTracker.html>
- `thiserror` <https://docs.rs/thiserror>, `anyhow` <https://docs.rs/anyhow>, `tracing` <https://docs.rs/tracing>
- cargo-nextest <https://nexte.st/>, cargo-deny <https://embarkstudios.github.io/cargo-deny/>
