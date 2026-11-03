# 04: Frontend in Rust or TypeScript?

**Question:** Should Fleet Web (spec §8, screens A–G) be written in Rust, and if so, how?
**Date of research:** 2026-11-03
**Status:** Recommendation, ready for decision
**Scope:** the browser dashboard only. This report assumes the agent and server are Rust, since other research tracks cover those. It looks at the frontend only where it touches the server (axum, the live-state protocol, SSO).

> **Evidence labels used throughout**
> - **[M] Measured.** I built or ran it myself in a scratch PoC on this date (4-core Linux VM, Rust 1.97.0, Chromium via Playwright 1.64.1).
> - **[V] Verified.** Read from a primary source on this date: the crates.io API, the npm registry, a GitHub release page, git history from a fresh clone, or an official blog.
> - **[I] Inferred.** My engineering judgment or an estimate. Treat it as an opinion and check it.

---

## 0. TL;DR

1. **Recommendation: Option E.** Write the server and agent in Rust and the dashboard in React + TypeScript. Generate the TypeScript types from the Rust `fleet-protocol` crate with **ts-rs**, and serve the built SPA from the axum binary. This is the spec's original frontend choice, plus one change: the TS types are generated from Rust, not hand-written.
   - It gives you most of the type-sharing benefit people want from "all-Rust", which I put at about 90–95% of it ([I], quantified in §7).
   - Every hard widget the spec asks for has a mature, accessible, maintained library: virtualized filterable tables, live time-series, a swimlane timeline, a log viewer, a date-range filter, comboboxes and dialogs.
   - IT can staff it. About 44–49% of developers use TypeScript, against about 15% for Rust, and only a small slice of the Rust users write Rust for the browser [V, SO survey 2025].
2. **Fallback if "all-Rust" is a hard requirement: Option D.** Render on the server in Rust with axum + askama and push updates with **Datastar 1.0 over SSE**. Add three small JS islands, ideally type-checked against the ts-rs types:
   - uPlot charts;
   - a canvas swimlane timeline;
   - a virtualized log viewer.

   All state and logic stay in Rust. This matches spec §6 (the server owns authoritative state) and gives the simplest build, deploy and audit story for IT: no WASM and no framework.
3. **Choose Option A + F instead of D if** the team is Rust-fluent and wants rich client-side interactivity *in Rust*. That means **Leptos 0.8 CSR** built with Trunk and served by axum as static files (no SSR or hydration), uPlot through a small wasm-bindgen binding, and the timeline and log viewer as native Leptos components.
   - It costs about 4–10 extra engineer-weeks for accessible primitives and interop.
   - Builds are slower.
   - The framework has a bus factor of about 2.
4. **Not recommended here:**
   - **Dioxus.** It is a credible second choice to Leptos (see §4.B for when to prefer it). The dashboard would start on 0.7 while 0.8 is in alpha with breaking changes, and its accessible primitives crate is not on crates.io.
   - **Yew and Sycamore.** Low activity, effectively one maintainer each.
   - **egui.** Canvas-rendered, so poor accessibility, text selection and browser integration.
   - **Topcoat.** Interesting, but it has shipped 11 minor versions since April 2026.

**The honest one-liner:** Rust pays off on the agent and server, where correctness and resource use matter. The frontend is where it pays least and costs most. You would trade a mature ecosystem for type sharing you can mostly get from codegen.

---

## 1. What the dashboard actually demands

These requirements come from spec §6 ("Live state"), §8 (screens A–G), §10 (security) and §16 (acceptance criteria). They are the yardstick for every option below.

| # | Capability | Where it is needed | Notes |
|---|---|---|---|
| R1 | Snapshot + delta live state, reconciled by version or sequence; fresh snapshot after a gap | All live screens (§6 "Live state") | The client needs a small normalized store. Deltas patch it, and any detected gap triggers a re-fetch. The spec warns against rebuilding state purely from telemetry events. |
| R2 | Many dense tables with sort and filter; virtualized long lists | A (live executions, runner health), B (jobs/devices tree), E (history with 11 filters), F (diff tables), G (admin lists) | Screen E needs URL-synchronized filter state, multi-select comboboxes and a time-range picker. |
| R3 | Time-series charts | C (CPU, memory, swap history; correlate spikes with the flow timeline), A (utilization), E (trends, distributions) | At a 5 s cadence, one series is 720 points per hour and 17,280 per day. Shared cursor and zoom are needed across charts and the timeline. |
| R4 | Gantt-style swimlane timeline (rows are devices, bars are flows), updating live | B (execution timeline), C (correlation), D (navigable step timeline) | This is a swimlane, not a project-management Gantt: no dependencies and no dragging. On the order of 10 devices × 200+ flows means 2k+ bars. Needs zoom, pan, tooltips and click-through. |
| R5 | Log viewer for very large logs (spec §15 "enormous logs") with sanitized rendering (§10) | D | Needs virtualization, ANSI colour, search, and jumping to the failed step. Must never inject log text as HTML. |
| R6 | Screenshot and evidence viewer | D | Expiring signed URLs, zoom, and step-to-screenshot linking. |
| R7 | Accessible components | All | Dialog, menu, combobox, tabs, tooltip, toast and date-range picker, following the WAI-ARIA Authoring Practices. |
| R8 | SSO (OIDC), roles, project-level authorization | All (§10) | Best done server-side, which makes it framework-neutral. See §9.3. |
| R9 | Provenance on every value (§18) | All | Observed, provisional, verified, derived, inferred or stale. This is a design-system concern: badges and styles. |
| R10 | Stays usable with many simultaneous active runs (§16.14) | A, B | Needs throttled rendering and batched updates. |

R3, R4 and R5 are where the frontend technology choice matters most. R7 is where the Rust options are weakest.

---

## 2. Ecosystem status on 2026-11-03

### 2.1 Rust UI frameworks [V]

All data comes from the crates.io API and from git history in fresh clones (non-merge commits since 2025-11-03).

| Framework | Stable / pre-release | Last release | Major (0.x.0) cadence | Commits, last 12 mo | Authors, 12 mo | Top 2 contributors' share, 12 mo | Funding |
|---|---|---|---|---|---|---|---|
| **Leptos** | 0.8.23 / **0.9.0-rc1** | 2026-10-31 | 0.6 Jan 2024 → 0.7 Nov 2024 → 0.8 May 2025 → 0.9 RC Oct 2026 | 610 | 73 | Saber Haj Rabiee 238, Greg Johnston 178 (about 68%) | Community and GitHub sponsors |
| **Dioxus** | 0.7.10 / **0.8.0-alpha.3** | 2026-10-16 | 0.5 Mar 2024 → 0.6 Dec 2024 → 0.7 Oct 2025 → 0.8 alpha since May 2026 | 333 (squash-merged) | 107 | Jonathan Kelley 86, Nico Burns 76 (about 49%) | Company: DioxusLabs, a full-time core team. README credits FutureWei, Satellite.im and GitHub Accelerator. The website shows Airbus, ESA, Cognition and YC logos. |
| **Yew** | 0.23.0 | 2026-03-10 | 0.21 Sep 2023 → 0.22 Dec 2025 (a 26-month gap) → 0.23 Mar 2026 | 154 | 25 | Siyuan Yan 86 (56%, plus 30 from dependabot) | Community |
| **Sycamore** | 0.9.4 | 2026-10-06 | 0.8 Aug 2022 → 0.9 Nov 2024 | 34 | 2 | Luke Chu 33 (97%) | Community |
| Topcoat (tokio-rs; server-rendered, no WASM) | 0.11.0 | 2026-10-24 | 0.1 → 0.11 between Apr and Oct 2026 | n/a | n/a | n/a | tokio-rs (Carl Lerche) |

GitHub stars on the repo pages: Leptos 21.5k (72 open issues, 38 PRs) and Dioxus 39.6k (670 open issues, 224 PRs) [V].

Breaking-change history [V, release notes]:

- **Leptos 0.6 → 0.7** was a rewrite of the reactive system and renderer: a new signals crate (`reactive_graph`) and the `tachys` renderer. It was a large migration.
- **0.7 → 0.8** was small.
- **0.8 → 0.9** is listed as minor. Lazy loading moved behind a feature flag; signals no longer implement `Fn` directly, so `impl Fn() -> T` props may need `*signal` or `impl SignalOrFn`; `server_fn` honours the axum body limit, which is 2 MB by default; `ToggleEvent` typing changed.
- **Dioxus 0.4 → 0.5** was a full move to signals, a large migration.
- **Dioxus 0.6 → 0.7** brought the Subsecond hot-patcher, primitives, and `dx` changes.
- **The 0.8 alphas** remove the `dioxus_elements` namespace (so `html::main {}` is needed for name clashes), change how falsy boolean attributes render, upgrade `keyboard-types` (`Key::Enter` becomes `Key::Named(NamedKey::Enter)`), and drop the 0.7 asset-format fallback. 0.7.6, on 2026-04-22, was announced as the *final* 0.7 feature release.

Bus factor:

- **Leptos:** effectively 2. Greg Johnston wrote 58% of all commits ever (3,058 of 5,263).
- **Dioxus:** effectively 2–3 (Kelley, Almloff, Burns), but they are salaried, which is both a strength and a company risk.
- **Yew and Sycamore:** 1.

### 2.2 Rust component, table and chart crates [V]

"Recent dl" is crates.io's 90-day download count.

| Crate | Version (date) | Recent dl | What it gives you | Verdict for Fleet |
|---|---|---|---|---|
| **thaw** (Leptos, Fluent design) | 0.4.8 (2025-08-03); 0.5.0-beta (2025-05) | 16k | Broad styled set. **0.4 targets Leptos 0.7. Leptos 0.8 support exists only as an unreleased 0.5 beta.** Last commit 2026-05-07. The repo makes no accessibility claims. | Stagnating. Risky to base a product on. |
| **leptos-struct-table** (Synphonyte) | 0.19.1 (2026-09-02) | 54k | Virtualized, paginated or infinite tables; sorting; async data providers; Tailwind class preset. The README does not mention filtering. | **Good.** The closest thing to TanStack Table for Leptos. |
| **leptos-use** (Synphonyte) | 0.20.0 (2026-10-20) | 430k | WebSocket, EventSource, resize and intersection observers, infinite scroll, intervals. Split into micro-crates in 0.20. | **Good**, well maintained. |
| biji-ui (headless, Leptos) | 0.5.0 (2026-04-08) | 128 | Headless UI–style primitives | Too small to depend on. |
| leptix-ui / leptix_primitives | 0.1.6 (2026-04) / 0.2.3 (2024-11) | 25 / 108 | "Radix-style" primitives; needs nightly | Too small, and needs nightly. |
| radix-leptos / leptos-shadcn-ui (cloud-shuttle) | 0.9.0 (2025-09) | 103 / 450 | Claims of "100% coverage" and "WCAG 2.1 AA" | Contradictory README counts and signs of heavy AI generation. No release in 13 months. **Avoid.** |
| leptodon (Open Analytics) | 1.7.0 (2026-10-02) | 582 | Tailwind toolkit inspired by Thaw and Flowbite | New and small. No accessibility claims. |
| rust-ui (`leptos_ui`) | 0.3.22 (2026-04) | 22k | shadcn-like registry. Now **Dioxus-focused** per its README. | Direction is drifting away from Leptos. |
| **dioxus-primitives** (DioxusLabs/components) | **crates.io has only a 0.0.0 placeholder.** `primitives/Cargo.toml` on main says 0.0.1, so it is used as a git dependency through `dx components add`. | n/a | First-party, "following the WAI-ARIA Authoring Practices", Playwright-tested. About 35 primitives, including accordion, alert_dialog, combobox, context_menu, date_picker, calendar, dialog, dropdown_menu, popover, select, slider, tabs, toast, tooltip, toggle_group, toolbar and **virtual_list**. Last commit 2026-09-08. | **The best accessibility story in Rust**, but not published, and `dx components` is broken on 0.8.0-alpha.3 (TLS issue noted in that release). |
| leptos-chartistry | 0.2.3 (2026-01-23) | 58k | Native Leptos SVG charts | Fine for simple charts. Not proven for live 17k-point series. |
| dioxus-charts | 0.4.0 (2026-01-06) | 8k | Simple SVG charts | Basic only. |
| charming (ECharts bindings) | 0.6.0 (2025-06-17) | 355k (mostly SSR/image use) | Builds ECharts options in Rust. **Its `WasmRenderer` binds to a global `echarts` JS object**, so echarts.js must be loaded anyway [V, source]. | This is JS interop with extra steps. No release in 17 months. |
| plotters (+ canvas backend) | 0.3.7 (2024-09-08) | 63M (mostly native) | Static plotting | No interactivity (tooltips, zoom). Wrong tool. |
| egui_plot / egui | 0.38 / 0.37 (Oct 2026) | 1.2M / 8.8M | Immediate-mode canvas UI | Not DOM-based, so accessibility, text selection, Ctrl-F and browser zoom are poor. Wrong tool for this product. |
| Gantt or timeline in Rust | n/a | n/a | **No crate exists** for Leptos or Dioxus. | You build it, about 500–800 LOC [I]. |
| TanStack Table/Virtual equivalent | n/a | n/a | leptos-struct-table (tables) and dioxus-primitives `virtual_list`. **There is no headless, framework-agnostic table-state engine** like `@tanstack/table-core`. | You build the grouping, column visibility, faceting and filter logic yourself [I]. |
| **ts-rs** | 12.0.1 (2026-01-31) | 9.7M | Rust → TS type generation with serde-attribute compatibility | **Mature.** Used in option E. |
| specta | 1.0.5 / 2.0.0-rc.27 (2026-10-21) | 1.6M | Rust → TS, used by Tauri | 2.0 has been in RC for a long time. |
| typeshare (1Password) | 1.0.5 (2026-01-02) | 1.8M | Multi-language codegen | Fine. Slow cadence. |

### 2.3 Hypermedia (option D) [V]

| Item | Version (date) | Notes |
|---|---|---|
| askama | 0.16.1 (2026-09-04) | Compile-time-checked Jinja-like templates. 16.7M recent downloads. `askama_web` 0.16.0 provides axum integration. |
| minijinja | 3.0.5 (2026-10-29) | Runtime templates (Armin Ronacher). Very active: five 3.0.x releases in October 2026. |
| maud | 0.27.0 (2025-02-02) | Macro HTML. No release in 21 months. |
| **htmx** | npm `latest` is 2.0.11 (2026-09-22). **4.0 shipped 2026-08-28 under npm's `next` tag** (fetch-based, built-in idiomorph morphing, `hx-partial`, explicit attribute inheritance). htmx 2 is "supported indefinitely". | Starting on 2.x means a migration later; starting on 4.x means early adoption. |
| **Datastar** | **1.0.0 (2026-04-16) → 1.0.4 (2026-09-21).** The Rust SDK crate `datastar` is at 0.4.1 (2026-09-29). | SSE-native "signals + morph" model. Some features are in a paid **Datastar Pro** (for example `data-match-media`; Rocket moved out of Pro in 1.0.4). The npm package is stale at 1.0.0-beta.11, so use the CDN or vendored release. |
| axum-htmx | 0.8.1 (2025-06-05) | Header extractors and responders |
| Topcoat (tokio-rs) | 0.11.0 (2026-10-24) | Server-rendered Rust and Rust-to-JS "reactive instructions"; integrates Datastar and htmx. Announced 2026-07-22 and "early-stage, expect breaking changes". **Too young for an IT-maintained product.** |

### 2.4 TypeScript side (option E) [V, npm registry]

| Package | Latest (date) |
|---|---|
| react / react-dom | 19.3.1 (2026-10-14) |
| typescript | 7.0.2 (2026-07-08). This is the native Go compiler. |
| vite / @vitejs/plugin-react | 8.3.5 (2026-10-21) / 6.2.1 (2026-11-02) |
| tailwindcss / @tailwindcss/vite | 4.3.4 (2026-11-02) |
| @tanstack/react-table | 9.3.1 (2026-10-12). v9 is now `latest`. |
| @tanstack/react-virtual | 3.14.26 (2026-11-03) |
| @tanstack/react-query | 5.105.3 (2026-11-02) |
| @tanstack/react-router | 1.170.46 (2026-10-22) |
| radix-ui / shadcn CLI | 1.8.1 (2026-10-12) / 4.23.0 (2026-10-27) |
| react-aria-components | 1.22.1 (2026-10-09) |
| uplot | 1.6.32 (2025-03-14). The release is old, but the repo is active, with commits on 2026-10-30 by leeoniya. |
| echarts | 6.1.0 (2026-05-19) |
| vis-timeline | 8.5.6 (2026-10-26) |
| @playwright/test / @axe-core/playwright | 1.64.1 / 4.14.0 |
| vitest / @testing-library/react | 5.1.0 / 16.3.3 |

---

## 3. Measurements from the PoC [M]

The scratch PoCs are in `/tmp/claude-0/-home-user/35b53bb7-a211-5ee3-8f5b-0d3a8129de43/scratchpad/web-poc`; none of this code is in the repo. Four apps were built:

| App | What it contains |
|---|---|
| `leptos-hello` | Leptos 0.8.23 CSR counter |
| `dioxus-hello` | Dioxus 0.7.10 web counter |
| `leptos-dash` | Leptos 0.8.23 CSR with `leptos_router` 0.8.17, a serde-typed protocol enum, a web-sys WebSocket, a keyed 500-row table with a text filter, and a derived "failed" count |
| `react-dash` | Functionally the same app in React 19.3.1 + react-router 8.5.1, built with Vite 8.3.5 |

The release profile was `opt-level="z"`, `lto=true`, `codegen-units=1` and `panic="abort"`, followed by `wasm-bindgen 0.2.130 --target web` and `wasm-opt -Oz` (binaryen 132). Sizes are gzip -9.

### 3.1 Bundle size

| Build | WASM raw | WASM gzip | JS glue raw / gzip | **Total gzip** |
|---|---|---|---|---|
| Plain wasm-bindgen baseline (one `<p>`) | 17.2 KB | 8.0 KB | 12.2 / 3.4 KB | **11.4 KB** |
| **Leptos hello** | 67.4 KB | 28.3 KB | 23.0 / 5.3 KB | **33.6 KB** |
| **Dioxus hello**, cargo + wasm-bindgen | 275.7 KB | 118.1 KB | 78.4 KB (glue + 5 snippets) / 14.6 KB | **132.7 KB** |
| Dioxus hello, `dx build --release` (dx 0.7.10) | 242.4 KB | 104.8 KB | 28.9 / 7.7 KB | **112.5 KB** |
| **Leptos dash** (router, serde_json, WebSocket, table) | 288.5 KB | 119.4 KB | 41.5 / 7.5 KB | **126.9 KB** |
| **React dash**, equivalent | n/a | n/a | 257.6 / 81.9 KB | **81.9 KB** |

What this shows:

- Leptos's runtime floor is small, about 34 KB.
- **serde_json, the router and the app code dominate quickly.** The realistic Leptos app is already about 1.5× the size of the React equivalent once both are gzipped.
- For a full Fleet dashboard I would expect **400 KB to 1 MB+ gzipped of WASM** with Leptos, and 250–500 KB gzipped of JS with React plus charts [I].
- Neither size matters much for an internal tool on a corporate network with caching. WASM compiles in a streaming pipeline, so its per-byte startup cost is lower than parsing JS.

### 3.2 Startup in headless Chromium

The table gives the median of 7 cold contexts: time from navigation until the page text appears, measured with `performance.now()`. The rows column shows the number of `<tr>` elements rendered.

| App | CPU 1× (median, min–max) | CPU 4× throttled (median, min–max) |
|---|---|---|
| Leptos hello | 93 ms (82–129) | 306 ms (233–327) |
| Dioxus hello | 112 ms (107–143) | 373 ms (324–566) |
| Leptos dash (renders 500 rows) | 163 ms (147–181) | 694 ms (618–762) |
| React dash (renders 500 rows) | 183 ms (165–194) | 734 ms (662–969) |

Startup is **a wash** between Leptos and React for this app shape. The time is dominated by building 500 rows of DOM, not by WASM instantiation. Dioxus's larger runtime costs about 20–70 ms more at hello-world scale. Startup is not a deciding factor for an internal dashboard.

### 3.3 Live-update throughput

A local Node WebSocket server sends a 500-row snapshot, then 2,000 `flow.updated` JSON messages at 500 msg/s. That is about 100× Fleet's real flow-event rate, a deliberate stress test. Each app parses the JSON, patches its store and re-renders the filtered table. Main-thread time comes from CDP `Performance.getMetrics` (median of 3 runs).

The stream lasts 4 s, so a wall time close to 4.2 s means the app keeps up and anything well above that means it falls behind.

| App | CPU | Wall time to last update | Main-thread task time | Script time | Layout time | JS heap* |
|---|---|---|---|---|---|---|
| Leptos dash, *naive* (one `RwSignal<Vec<Row>>`, `<For>` keyed by `(id, duration)`) | 1× | 4,185 ms | 2,925 ms | 876 ms | 851 ms | 1.7 MB |
| Leptos dash2, *fine-grained* (one signal per mutable cell, keyed by id) | 1× | 4,160 ms | **2,537 ms** | 571 ms | 861 ms | 1.7 MB |
| React dash (`useState` + `map`, automatic batching) | 1× | 4,251 ms | 3,062 ms | 1,131 ms | 827 ms | 7.1 MB |
| Leptos dash, naive | 4× | **6,956 ms (falls behind)** | 6,543 ms | 3,408 ms | 1,083 ms | 2.9 MB |
| Leptos dash2, fine-grained | 4× | 4,807 ms | **4,442 ms** | 1,998 ms | 901 ms | 2.4 MB |
| React dash | 4× | 4,894 ms | 4,609 ms | 1,295 ms | 1,502 ms | 6.8 MB |

\* The JS heap figure excludes WASM linear memory, so the heap columns are not comparable across Leptos and React.

Findings:

1. **Fine-grained Leptos is about 15–20% cheaper on the main thread than naive React at 1×. Under 4× throttling it is roughly equal.**
2. **Naive Leptos is *slower* than naive React under throttling.** React batches multiple `setState` calls into one render, while the naive Leptos version clones and re-diffs the whole 500-row list on every message. Leptos's advantage only appears if you model state as per-field signals or `reactive_stores`.
3. I hit a real Leptos footgun while building dash2. A keyed `<For>` **silently keeps the old row view** when an item is replaced by a new value with the same key. After a snapshot the UI stopped updating and showed no error. The fix is to patch the existing signals in place. This is exactly the snapshot-then-deltas flow in spec §6, so it would bite Fleet.
4. At 500 msg/s every variant spends 60–90% of the main thread on updates. **The real fix is server-side coalescing** (§9.3), not the framework choice.

### 3.4 Compile and iteration times

| Step | Time [M] |
|---|---|
| Leptos dash, **cold dev build** for wasm32 (≈226 crates) | 76 s |
| Leptos dash, incremental dev build after editing view text | 1.9 s, plus 0.8 s wasm-bindgen on the 84 MB debug `.wasm` |
| Leptos dash, incremental **release** build (LTO, codegen-units=1) | 14.1 s |
| Dioxus hello, cold dev build (≈127 crates) | 47 s |
| Dioxus hello, incremental dev build | 0.9 s |
| Dioxus hello, `dx build --release` from warm deps | 75 s |
| Cold release build of the first three Leptos crates, sharing the CPU with two `cargo install` jobs | 5 min 30 s |
| `cargo install dioxus-cli@0.7.10 --locked` (a dev-machine and CI one-off; `cargo binstall` avoids it) | about 35 min, also under contention |
| **Vite 8 production build** of the React dash | **1.5 s** |

Notes:

- dx 0.7 also tries to download esbuild and other tools from npm and GitHub at build time. On a locked-down CI runner I had to put a local esbuild on `PATH` and set `NO_DOWNLOADS=1`. That is relevant for IT-controlled CI.
- These are toy apps. A real Fleet frontend of 15–30k LOC in a single crate would see incremental dev builds of **about 5–20 s** and release WASM builds of minutes [I]. The OXIDT post-mortem (§8) reports **22 min for the WASM build on 6 cores**, though for a fullstack app.
- Vite HMR applies a change in well under a second regardless of app size [I, not measured interactively].
- Dioxus Subsecond hot-patching narrows this gap for Dioxus specifically [V, release notes].

### 3.5 ts-rs output check

`ts-rs 12.0.1` ran on a protocol enum with `#[serde(tag = "type")]` and `rename_all`. The tagged union, string-literal enum and `DateTime` → `string` mappings were all correct. Two real pitfalls showed up:

```ts
// generated
export type FlowRow = { ..., duration_ms: bigint, sequence: number, attempt: number | null, };
export type LiveMsg = { "type": "snapshot", version: bigint, rows: Array<FlowRow>, }
                    | { "type": "flow.updated", version: bigint, row: FlowRow, };
```

1. **`u64` maps to `bigint` by default**, but `JSON.parse` produces `number`. Set `TS_RS_LARGE_INT=number` globally, or use `#[ts(type = "number")]` (verified on `sequence`). Sequence numbers are far below 2^53.
2. **`Option<T>` with `skip_serializing_if = "Option::is_none"` is typed `T | null`, but on the wire the field is absent** (`undefined`). Use `#[ts(optional)]` or a project-wide `#[serde(default)]` convention.

Both can be caught by a single round-trip contract test: serialize Rust fixtures, then type-check and parse them in a Vitest test.

---

## 4. Option assessments

### A. Leptos (0.8.23 stable, 0.9.0-rc1)

**Model.** Fine-grained signals with no virtual DOM: `signal`, `RwSignal`, `Memo` and `Effect`, plus `reactive_stores` for nested store fields. This model suits R1 and R10 very well. A WebSocket delta updates exactly the cells bound to the changed fields. That only holds if you model state per field: in the PoC (§3.3), fine-grained Leptos was 15–20% cheaper than React at 1×, while *naive* Leptos fell behind React under CPU throttling, and a keyed `<For>` gotcha silently froze the table after a snapshot.

**CSR versus SSR with hydration.**

- `cargo-leptos` 0.3.11 with `leptos_axum` gives SSR, hydration, islands and server functions.
- For Fleet, **SSR buys almost nothing.** The app sits behind SSO with no SEO, and the first paint happens on a fast LAN. In exchange it costs:
  - two compilations (server plus wasm32), which is the main CI pain in the Dioxus post-mortem in §8;
  - hydration-mismatch bugs;
  - server functions that bypass the spec's REST and OpenAPI contract (§6 requires REST for history and initial state).
- **Use CSR with Trunk 0.22.1** (released 2026-10-26; serve, proxy and WS proxy support). axum then serves `dist/` as static files.

**Maturity and cadence.** Leptos is active, with 610 commits and 73 authors in 12 months and patch releases every 1–3 weeks. Since 0.7, breaking changes have been modest. The bus factor is about 2 [V].

**Components and accessibility.** **This is the weak point.**

- There is no maintained, widely used, accessible headless primitive library (§2.2).
- thaw, the largest styled library, has not released Leptos 0.8 support.
- You would implement combobox/multiselect (Screen E has 11 filters), a date-range picker, menus, toasts and tooltips yourself, using native `<dialog>`, the Popover API, `<details>`, `<select>` and the WAI-ARIA APG patterns.
- I estimate **3–6 engineer-weeks** to reach the accessibility quality you get on day 1 from Radix or React Aria [I].

**Tailwind.** It works well. Tailwind v4 scans `.rs` files for literal class strings, so it is framework-agnostic. Run the standalone Tailwind CLI, or Trunk's tailwind asset. `tailwind_fuse` (0.3.2, Jan 2025) handles class merging. Dynamic class strings built with `format!` will not be detected; the same is true in TSX [I].

**Charts.** Use JS interop.

- Use uPlot through about 100–150 lines of `#[wasm_bindgen] extern "C"` bindings.
- Or use ECharts, where `charming` adds little because it still needs echarts.js.
- `leptos-chartistry` is fine for small sparklines.

**Timeline (R4).** Write it natively as reactive SVG, or canvas for more than about 2k bars, in Leptos's `view!`. This is actually pleasant in Leptos [I].

**Tables and virtualization.** leptos-struct-table, plus your own filter and URL-state logic.

**Developer experience.**

- `trunk serve` rebuilds on save; see §3.4 for incremental times.
- View-macro hot reload exists (`cargo leptos watch --hot-reload`; console messages say it needs the `nightly` feature).
- Leptos added "minimal support for `subsecond`" in Sep–Oct 2025 [V, git log], but it is not the first-class experience that Dioxus has.
- Panics in WASM need `console_error_panic_hook`. Browser stack traces are mangled unless you ship DWARF or names. Debugging is noticeably worse than with JS sourcemaps [I].
- Compile errors from `view!` and trait-heavy props can be long. 0.9 simplifies some of this by removing the `Fn` impls on signals.

**Testing.** `wasm-bindgen-test` handles unit and component tests in a headless browser. There is no Testing-Library equivalent. Playwright plus `@axe-core/playwright` works unchanged for E2E and accessibility checks.

**Production use.** Several small open-source admin UIs exist, such as `apalis-board`, a Leptos job-queue dashboard with SSE live logs (1.0.0, 75 stars). I found no published case study of a large company dashboard on Leptos. Lemmy's long-running Leptos UI rewrite (`lemmy-ui-leptos`, 135 stars) is still not the official Lemmy UI [V/I].

### B. Dioxus (0.7.10 stable, 0.8.0-alpha.3)

**Model.** React-like components and hooks over a virtual DOM, with signals (`use_signal`) since 0.5. The web renderer applies edits through a small JS interpreter. Version 0.8 reworks rendering internals.

**Strengths.**

1. **The best Rust DX.** `dx serve` hot-patches Rust code at runtime through **Subsecond**; it is on by default in 0.8 and covers web and WASM.
2. **First-party accessible primitives**, Radix-modelled and Playwright-tested, including combobox, date_picker and virtual_list.
3. A salaried core team.
4. `dx` handles Tailwind automatically (0.7 release notes) and asset bundling.

**Weaknesses for this app.**

1. **Timing.** 0.7 is in maintenance (final feature release 0.7.6, April 2026), and 0.8 is alpha with real breaking changes (§2.1). A dashboard started today would do a 0.7 → 0.8 migration in its first year.
2. **dioxus-primitives is not on crates.io** (0.0.0 placeholder). You vendor components by copying them in, as shadcn does. That works, but the alpha.3 `dx components` command is broken.
3. **Bundle floor is about 4× Leptos** (§3.1).
4. Effort is split across desktop, mobile and the native/Blitz renderer, so the web is one of several targets.
5. **You are tied to the `dx` CLI** for the good DX.
6. 670 open issues.
7. Charts still need JS interop. The post-mortem in §8 uses Chart.js through `dioxus-use-js`.

**When to prefer Dioxus over Leptos.** Choose it if the team values hot-patching and first-party accessible primitives above web-only focus and bundle size, and is willing to start on 0.8 alphas or betas and track them to stable [I].

### C. Yew, Sycamore and others

- **Yew 0.23** (March 2026). It went 26 months without a release between 0.21 and 0.22. Activity is now driven by one contributor (56% of 12-month commits). It is maintained, but the ecosystem has largely moved to Leptos and Dioxus [V].
- **Sycamore 0.9.4** (October 2026). One person wrote 97% of the 12-month commits. Not suitable for a product IT must own [V].
- **egui and eframe.** Canvas-only, so no accessible DOM. Wrong for a dashboard of tables and text [V/I].
- **Topcoat.** See D [V].
- **Freya** (Dioxus-based native) is desktop-only and not relevant.

### D. Server-rendered Rust plus hypermedia

The stack would be axum with askama 0.16 (or minijinja 3.0), Datastar 1.0 over SSE (or htmx 2 or 4 with the SSE extension), Tailwind via the standalone CLI, and vendored uPlot plus a few JS islands.

**How live state works.** The browser opens one SSE stream per page. When the materialized view changes, the server re-renders the affected fragment, such as a table row, a counter or a runner card. Datastar or htmx morphs it into the DOM. This **fits spec §6 nicely**: the server owns the authoritative state, a reconnect simply re-sends the fragments, and the browser never reconstructs state from events.

**Strengths.**

- No WASM toolchain, no second compile and no npm (you vendor two JS files).
- The smallest attack surface. Log text is escaped by askama by default.
- Plain HTML, which is easy for any backend engineer or IT to read.
- Native `<dialog>`, `<details>` and Popover API give decent accessibility for free.
- Screens E, F and G (filter forms, paginated tables, admin CRUD) are a **perfect** fit.

**Weaknesses.**

- Screens B, C and D need rich client-side interaction: a zoomable live timeline, cursor-synchronized charts, a virtualized and searchable log viewer, and comboboxes. All of those become hand-written JS islands, so "a little JS" grows to about 1–2k LOC of untyped JS (or TS with a bundler, at which point you have an npm toolchain again).
- Per-viewer server rendering on every 5 s metric tick costs CPU, but that is fine at internal scale.
- Datastar has a paid Pro tier, which is a licensing question for IT.
- htmx is mid-transition from 2 to 4.

### E. React + TypeScript frontend with generated types

The backend and agent stay Rust. The frontend uses the spec's original stack, with ts-rs generating every protocol and DTO type from the Rust `fleet-protocol` crate into `web/src/gen/`. CI fails if the regenerated output differs from what is committed.

**Strengths.** Every requirement R1–R10 has a mature, maintained, accessible library:

| Requirement | Library |
|---|---|
| Tables (R2) | TanStack Table 9 + Virtual 3 |
| Primitives (R7) | Radix or shadcn, or React Aria (best-in-class combobox and date-range picker) |
| Time series (R3) | uPlot |
| Distributions | ECharts 6 |
| Timeline (R4) | d3-scale/d3-zoom with custom canvas/SVG, or vis-timeline |
| Log viewer (R5) | TanStack Virtual + anser (ANSI parsing into spans, no `innerHTML`) |
| Snapshot cache | TanStack Query (WebSocket deltas patch it with `setQueryData`) |

Other strengths:

- Vite HMR in under a second.
- First-class debugging with sourcemaps.
- Vitest and Testing Library for component tests.
- The largest hiring pool. This matches the spec's "IT must maintain it."

**Weaknesses.**

- Two languages and two toolchains (cargo and npm).
- npm supply-chain exposure. For example, `d3-scale` 4.0.3 and `d3-zoom` 3.0.1 were published *today*, 2026-11-03, after four or more years without releases. They were published via GitHub trusted publishing and are probably legitimate, but this is exactly why you need a lockfile and a release-age cooldown policy [V].
- Generated types guarantee shape, not runtime validity. That is acceptable because the server is first-party. Add zod only at the boundaries you need.

### F. Hybrid: Rust WASM with JS libraries for charts and timeline

**This is not really a separate option.** Every Rust-WASM dashboard ends up here, as the post-mortem in §8 shows. The question is only *how much* JS:

- **Minimal F**, the recommended form of A: uPlot only, through hand-written bindings. The timeline and log viewer are native Leptos.
- **Maximal F**: uPlot, ECharts, vis-timeline and a JS virtualizer. At that point you are maintaining wasm-bindgen glue for four JS libraries, without TypeScript's types on the JS side, and gaining little over E.

**Interop costs [I].**

- Hand-written `extern "C"` bindings per library: about 100–300 LOC each.
- Converting `Vec<f64>` to `Float64Array` (cheap with `js_sys::Float64Array::from`).
- Lifetimes of `Closure`s for callbacks, which can leak if they are not dropped.
- No type checking of the JS library's API beyond what you declare.

---

## 5. Comparison matrix

Scores are 1–5, higher is better for Fleet. They are [I] judgments based on the [V]/[M] facts above.

| Criterion (weight) | A. Leptos CSR (+F-min) | B. Dioxus | C. Yew/Sycamore | D. SSR + Datastar/htmx | **E. React + TS + ts-rs** | F-max (Rust + many JS libs) |
|---|---|---|---|---|---|---|
| Maturity, cadence, bus factor (3) | 3: active, 0.x, bus factor ~2 | 3: funded team, but 0.8 alpha churn | 1 | 4: askama/minijinja are mature; htmx 2→4 and Datastar Pro are minor concerns | **5** | 3 |
| Accessible components (3) | 1–2: build them yourself | 3: first-party primitives, unpublished | 1 | 3: native HTML goes far; JS islands for combobox | **5**: Radix, React Aria | 2 |
| Tables + virtualization (2) | 3: leptos-struct-table | 2: virtual_list only | 1 | 3: server paging; virtualization needs a JS island | **5**: TanStack | 3 |
| Time-series charts (2) | 3: uPlot interop | 3: interop | 2 | 3: uPlot island | **5** | 4 |
| Gantt timeline (2) | 4: native SVG/canvas is pleasant | 3 | 2 | 2: JS island | 4: custom with d3-scale, or vis-timeline | 3 |
| Log viewer (1) | 3 | 3 | 2 | 3: server paging + JS virtual scroll | **5** | 3 |
| Live updates and performance (2) | **5**: fine-grained signals | 4 | 3 | 4: SSE morph; server owns state | 4 | 4 |
| Bundle and startup (1) | 4 | 3 | 3 | **5** | 4 | 3 |
| DX: compile, hot reload, debugging (2) | 2–3 | 3–4: Subsecond | 2 | 4 | **5** | 2 |
| Hiring and IT maintainability (3) | 2 | 2 | 1 | 4 | **5** | 1 |
| Type sharing with backend (2) | **5** | **5** | **5** | 4: templates use Rust structs; the JS islands need ts-rs types | 4: ts-rs, with the pitfalls in §3.5 | 4 |
| Testing (1) | 3: wasm-bindgen-test + Playwright | 3: Playwright, primitives tested | 2 | 4: template unit tests + Playwright | **5**: Vitest + RTL + Playwright | 3 |
| Supply-chain surface (1) | 4: crates only | 4 | 4 | **5** | 2: npm | 3 |
| **Weighted total (max 125)** | **78.5** | **78** | **50** | **90** | **115** | **70** |

Where a cell gives a range ("1–2", "2–3", "3–4"), the total uses the midpoint.

How to read this:

- **E wins clearly** on almost every criterion except supply chain and pure type sharing.
- **D comes second, mainly on operability**: maturity, hiring, build time and supply chain. Its weakness is concentrated in the interactive widgets of screens B, C and D.
- **A and B are within noise of each other.** For a Rust-WASM build I prefer Leptos because of its web-only focus, its stable release line (0.8 → 0.9 is minor) and leptos-struct-table. Dioxus wins on accessible primitives and hot-patching DX.

I also tested the matrix with the "live and investigate" criteria weighted double (charts, timeline, log viewer, live updates), because spec §18 makes them the product's core. The ranking does not change: E 146, D 113, A 105.5, B 101 (out of 160). D's operability advantage outweighs A's better widget story. Choose A over D only when the conditions in §6 hold.

---

## 6. Recommendation, fallbacks and when to choose each

| Choose | When |
|---|---|
| **E: React + TS + ts-rs** (recommended) | You want the fastest path to a polished, accessible UI; **IT or other teams will maintain it** (spec §14 Phase 5 "IT handover"); you expect contributors who are not Rust specialists; you want the shortest risk path through the timeline, charts and log viewer. |
| **D: askama + Datastar over SSE + 3 JS islands** (fallback when all-Rust is required) | "All-Rust" means *all logic and state in Rust*, and the team accepts about 1–2k LOC of JS/TS islands for the timeline, charts and log viewer. IT maintainability and a small build and supply-chain surface are top priorities. The team is backend-leaning. |
| **A + F-min: Leptos CSR + uPlot interop** | "All-Rust" means *the UI code itself* is Rust; the team is Rust-fluent and comfortable with reactive UI; you accept writing about 5–10 accessible primitives yourself and keeping about 150 LOC of chart bindings; you are comfortable tracking a 0.x framework with a bus factor of about 2; CI can afford minutes-long WASM release builds; IT accepts maintaining Rust WASM. |
| **B: Dioxus** instead of Leptos | Same as A, but you rank hot-patching DX and first-party accessible primitives above web-only focus and bundle size, and you are willing to start on 0.8 pre-releases rather than 0.7. |
| Not Yew, Sycamore, egui or Topcoat | Bus factor, accessibility or maturity (§4.C). |

**A decision rule [I].** Ask who will open a PR against the frontend in month 18:

- **"IT, or any web developer"**: choose E.
- **"Backend Rust engineers who will touch templates, plus occasionally a few JS widgets"**: choose D.
- **"The original Rust author and one other Rust engineer who enjoy UI work"**: A is defensible.

**A hybrid worth knowing about [I].** D and E share the same server contract: REST, the WS/SSE live channel and ts-rs types. You can therefore build screens E, F and G (history, comparisons, admin) server-rendered first, with the interactive screens as TS islands, and promote to a full SPA later if needed. Do not plan both up front, because two UI paradigms double the design-system work.

---

## 7. Type sharing, quantified

**The type surface implied by the spec [I, counted from §3–§9, §18]:**

- 16 event types plus the envelope (§5)
- the state enums: worker 4, job 8 and flow 7 states (§6)
- 11 failure categories (§9)
- 6 provenance kinds (§18)
- about 10 REST groups × 3–5 DTOs (§6)

That totals about **60–90 shared types**. Expect them to change most often in Phases 1–4.

| Benefit | All-Rust (A, B, D) | E with ts-rs |
|---|---|---|
| Compile-time detection of renamed or removed fields across server and UI | Yes, in one `cargo check` | Yes, at `cargo test` (export) plus `tsc` (TS 7 is fast). CI diff gate. |
| Enum exhaustiveness (`match` versus a TS `switch` with a `never` check) | Yes | Yes, with `never` checks and a lint rule |
| Serde attribute fidelity (tags, renames) | Exact (same code) | Good. Pitfalls: `u64` → `bigint` and optional versus null (§3.5). Fix once with conventions and a round-trip test. |
| Runtime validation of server JSON | serde rejects bad payloads | None by default; add zod where needed |
| Shared *logic* (formatting, state labels, provenance rules, version reconciliation) | Write it once | Write it twice, *or* keep it server-side. **The spec already puts the authoritative logic on the server** (§6), so the client-side share is small, about 300–800 LOC [I]. |
| Setup cost | 0 | About 1 day: ts-rs derives, an export test, a CI diff check and conventions |
| Ongoing cost per schema change | 0 | Run `cargo test -p fleet-protocol` (automatic in the dev script) |

**Estimate [I].**

- ts-rs captures about **90–95% of the type-sharing benefit**.
- The remaining 5–10% is about 300–800 LOC of duplicated presentation logic plus a few edge-case mismatches. That is worth perhaps **1–2 engineer-weeks over the life of v1**.
- The accessible-components and charting gap of a Rust frontend costs, by my estimate, **4–10 engineer-weeks** in v1 alone: 3–6 for primitives, 1–2 for chart and timeline interop, and 1–2 for extra debugging and CI build time.

So type sharing is a real benefit, but it is not big enough to justify the switch by itself.

---

## 8. Real-world evidence and post-mortems

**OXIDT, "One year of Dioxus fullstack in production" (2026-10-26)** [V]. A solo founder runs Dioxus 0.7 fullstack across about 12 crates.

The good:

- one language end to end;
- shared types;
- "refactoring with confidence";
- few runtime bugs.

The painful:

- **The WASM build takes about 22 minutes on 6 cores.**
- **A release image takes about 90 minutes on a 2-core GitHub runner** (about 40 minutes self-hosted).
- clippy ran out of memory on a 7 GB runner.
- They stopped building on every push.
- **"No Rust charting library was trusted for production"**, so they use Chart.js through about 340 LOC of JS.
- Tailwind requires Bun in CI.
- They plan to evaluate the server-rendered **Topcoat** to drop WASM.

This is one anecdote, and fullstack mode means a double compile; CSR avoids that. But it matches what I expect for Fleet on CI-runner hardware.

**`xiaocongyu66/new-api`, issue #356** [V, summary of a Chinese-language issue]. This was an epic to rewrite a React 19 + TS frontend in Dioxus 0.7. It was **abandoned on 2026-10-19 by the maintainer's decision**: the Phase-0 scaffolding PR was closed without merging and the Rust workspace was removed.

**Lemmy** [V/I]. The `lemmy-ui-leptos` rewrite has 895 commits, but the production Lemmy UI is still the TypeScript one. In the rewrite discussion (issue #15), commenters raised Rust's learning curve for frontend contributors.

**apalis-board** [V] is a small, real example of the "Rust admin dashboard with live updates" niche. It is a Leptos UI with SSE live task logs, embeddable in axum, at 1.0.0.

**Dioxus website** [V] shows Airbus, ESA, Cognition, YC and FutureWei logos but publishes **no case studies**.

**Community scale** [V]. The 2025 State of Rust survey had 7,156 responses and its blog post contains no frontend breakdown. The Stack Overflow 2025 survey reports TypeScript at 43.6% of all respondents (48.8% of professionals), Rust at 14.8%, and React at 44.7%. The 2026 survey shows TypeScript at 43.8%.

I found **no write-up of a team moving *to* Leptos or Dioxus for a large internal dashboard and reporting success at team scale**. The positive reports are from solo or very small teams [I, absence of evidence from searches].

---

## 9. Concrete stack for the recommendation (E) and how it plugs into axum

### 9.1 Repository layout

```text
crates/
  fleet-protocol/      # serde types: events (§5), live-state DTOs (§6), REST DTOs; #[derive(TS)]
  fleet-server/        # axum; embeds web/dist
  fleet-agent/
web/                   # Vite + React + TS
  src/gen/             # ts-rs output (committed; CI diff-checked)
```

### 9.2 Frontend packages

Versions are as of 2026-11-03. Pin them exactly in `package.json`, commit the lockfile, and use a 14-day release-age cooldown (pnpm `minimumReleaseAge`, or Renovate `minimumReleaseAge`).

| Role | Package @ version |
|---|---|
| Framework | `react@19.3.1`, `react-dom@19.3.1` |
| Language and tooling | `typescript@7.0.2` (fall back to the 5.x/6.x JS compiler if a tool needs the TS JS API), `vite@8.3.5`, `@vitejs/plugin-react@6.2.1`, `@biomejs/biome@2.6.4` (lint and format; or `eslint@10.14.0`) |
| Styling | `tailwindcss@4.3.4`, `@tailwindcss/vite@4.3.4`, `lucide-react@1.67.0` (icons) |
| Accessible primitives | `shadcn@4.23.0` CLI (copy-in components) on `radix-ui@1.8.1`. Add `react-aria-components@1.22.1` for **DateRangePicker and ComboBox** (Screen E filters). |
| Routing and URL filter state | `@tanstack/react-router@1.170.46` + `@tanstack/router-plugin@1.168.48`, for type-safe search params so Screen E filters live in the URL |
| Server state | `@tanstack/react-query@5.105.3`. REST snapshots load into the query cache; WebSocket deltas patch it with `setQueryData`; a detected sequence gap calls `invalidateQueries`, which re-fetches the snapshot (spec §6 step 5). |
| Tables | `@tanstack/react-table@9.3.1`, `@tanstack/react-virtual@3.14.26` |
| Time series (Screen C, A) | `uplot@1.6.32`, with a thin React wrapper you own (about 80 LOC) and a shared cursor sync key across the CPU, memory and swap charts and the timeline |
| History charts (Screen E, F) | `echarts@6.1.0` (tree-shaken `echarts/core`); bar charts, box plots and histograms |
| Timeline (Screen B, C, D) | Custom canvas component on `d3-scale@4.0.2` (pin; 4.0.3 is one day old) + `d3-zoom@3.0.0` (pin, likewise). This is the recommended route. Alternative: `vis-timeline@8.5.6`, if you accept its DOM weight and styling. |
| Log viewer (Screen D) | `@tanstack/react-virtual` + `anser@2.3.5` (ANSI to JSON spans, rendered as text nodes, never `innerHTML`) |
| Testing | `vitest@5.1.0`, `@testing-library/react@16.3.3`, `@playwright/test@1.64.1`, `@axe-core/playwright@4.14.0` |

### 9.3 Server side: the frontend touchpoints

**Types.** `ts-rs = "12.0.1"` in `fleet-protocol` with the `serde-compat` (default) and `chrono-impl` features. Add `TS_RS_LARGE_INT=number` in `.cargo/config.toml` `[env]` and `#[ts(optional)]` on skipped `Option`s. Also add a `cargo test -p fleet-protocol export_bindings` step and `git diff --exit-code web/src/gen` in CI. Use `utoipa` 6.0.0 for the OpenAPI the spec asks for, aimed at IT and integrators. The frontend uses the ts-rs types, not a second generator.

**Serving the SPA.** `rust-embed` 8.13.0 (or `tower-http` 0.7.1 `ServeDir` + `ServeFile` fallback) embeds `web/dist` into the `fleet-server` binary, so one image serves both, which spec §11 allows. Route the paths as follows:

| Path | Handler |
|---|---|
| `/api/v1/**` | REST |
| `/api/v1/live` | Browser WebSocket upgrade |
| `/api/v1/agent/stream` | Agent WebSocket |
| `/auth/**` | OIDC |
| `/**` | SPA `index.html` fallback with long-cache hashed `/assets/*` |

**SSO, as a backend-for-frontend (BFF), with no tokens in the browser.**

- Use `openidconnect` 4.1.0 (Authorization Code + PKCE) and `tower-sessions` 0.15.0, with an HttpOnly, Secure, SameSite=Lax session cookie.
- The WebSocket upgrade authenticates with the same cookie, plus an **Origin check** against cross-site WebSocket hijacking and a short-lived per-connection subscription scope.
- No `oidc-client-ts` in the SPA. This keeps SSO identical for every frontend option, which is why R8 does not differentiate them.

**The live channel.** The server sends `{type:"snapshot", version, …}` on subscribe and `{type:"…updated", version, …}` deltas, coalesced at 250–1000 ms per subscriber. The client tracks `version` and re-fetches on a gap.

**Artifacts (Screen D).** Screenshots are fetched with short-lived signed URLs. HTML reports are served from a **separate sandbox origin**, or with `Content-Security-Policy: sandbox` + `Content-Disposition: attachment`, never from the app origin (spec §10).

**Build.** A multi-stage Dockerfile: `node:22` runs `npm ci && vite build`, then the Rust stage `cargo build --release` embeds `dist/`, then a distroless runtime image.

### 9.4 Fallback stack D (server-rendered Rust + Datastar)

| Role | Crate or file |
|---|---|
| HTTP server | `axum = "0.8.9"`, `tower-http = "0.7.1"` (compression, `ServeDir` for `/static`) |
| Templates | `askama = "0.16.1"`, compile-time checked and auto-escaping, which is safe for log text (§10). Add `askama_web = "0.16.0"` with the axum feature. Alternative: `minijinja = "3.0.5"` if you want template reload without recompiling. |
| Live updates | `datastar = "0.4.1"` (official Rust SDK, axum support, passes the Datastar conformance suite) with the **Datastar 1.0.4** JS bundle vendored from the GitHub release. The npm package is stale. |
| Live update flow | One SSE stream per page. Handlers subscribe to the live-state broadcast and emit `datastar-patch-elements` events rendered by askama, coalesced every 500–1000 ms. On reconnect, send the full fragment set (spec §6 step 5). |
| Conservative alternative | `htmx.org@2.0.11` + its SSE extension + `axum-htmx = "0.8.1"`. Re-evaluate htmx 4 once it is `latest` on npm. |
| Styling | Tailwind 4.3.4 **standalone CLI** (no npm). Native `<dialog>`, the Popover API, `<details>` and `<select>` cover most accessibility needs. |
| Islands (`static/islands/*.js`, about 1–2k LOC total) | `uplot@1.6.32` (vendored) for charts; a custom canvas swimlane timeline; a log viewer that fetches 1k-line windows by range and renders them as text nodes. Write them as ES modules with JSDoc types that import the ts-rs `.d.ts` output, then check them with `tsc --checkJs --noEmit` in CI, or bundle them with `esbuild@0.28.2` if you prefer TS. |
| Auth | Same BFF as §9.3. A cookie session also covers the SSE stream. |
| Testing | `cargo test` renders templates with fixture state and asserts on the HTML; Playwright + axe for E2E. |

### 9.5 Alternative stack A (Leptos CSR)

| Role | Crate or package |
|---|---|
| Framework and router | `leptos = { version = "=0.8.23", features = ["csr"] }`, `leptos_router = "=0.8.17"`. Plan the 0.9 upgrade once it is final (rc1 was released 2026-10-31). |
| Utilities | `leptos-use = "0.20.0"` (or its micro-crates), `leptos-struct-table = "0.19.1"`, `codee = "0.3.5"` |
| Transport and serialization | `serde`/`serde_json`, `gloo-net = "0.7.0"` |
| Browser bindings | `wasm-bindgen = "=0.2.130"`, `web-sys = "0.3.107"`, `js-sys` |
| Build | `trunk` 0.22.1 with Tailwind 4.3.4 standalone CLI; release profile `opt-level="z"`, LTO, `codegen-units=1`, and `wasm-opt -Oz` |
| Charts | `uplot@1.6.32`, vendored, with hand-written bindings |
| Timeline and log viewer | Native Leptos SVG/canvas |
| Testing | `wasm-bindgen-test`, Playwright + axe |

axum integration is the same as §9.3: `ServeDir` or `rust-embed` over Trunk's `dist/`. Do **not** use `cargo-leptos`/`leptos_axum` SSR.

---

## 10. Per-screen risks (A–G)

| Screen | Hardest parts | Risk with E (React/TS) | Risk with A (Leptos CSR) | Risk with D (SSR + Datastar) |
|---|---|---|---|---|
| **A: Fleet overview** | Live counters, live execution table, runner health cards with data-freshness ("stale" after N s), recent-activity feed | **Low.** Main risk is render storms with many runs, so batch deltas with `requestAnimationFrame` or a 250 ms coalesce. | **Low.** Fine-grained signals are ideal here. | **Low.** This is SSE fragment morphing's sweet spot. Freshness timers need a little JS, or server-sent "stale" transitions. |
| **B: Pipeline detail** | **Gantt swimlane timeline** (live-growing bars, zoom and pan, 2k+ bars, "now" line, click-through), hierarchical jobs/devices tree, live activity stream, failure navigation | **Medium.** Building a custom canvas timeline is about 500–800 LOC. vis-timeline is faster to start but awkward to theme and slow with many items. Use canvas, not SVG, beyond about 2k bars. | **Medium.** Same build effort, and native SVG/canvas in Leptos is fine. The tree view needs an accessible `treegrid` you write yourself. | **High.** The timeline must be a JS island fed by SSE or JSON. Morphing a 2k-element SVG every tick is wasteful. |
| **C: Worker/device detail** | CPU, memory and swap history (17k points per series per day), thermal "unavailable" state (§16.5), **correlating metric spikes with the flow timeline** through a shared time axis and cursor | **Low–medium.** uPlot handles this easily. Cursor sync between uPlot and a custom timeline needs a shared `xScale` store. | **Medium.** uPlot through bindings; cursor sync across the WASM/JS boundary; you own the binding when uPlot changes. | **Medium–high.** JS island; synchronization lives entirely in JS. |
| **D: Flow investigation** | **Log viewer** (huge logs, ANSI, search, jump to step, sanitized), screenshot viewer, step timeline, links to source and previous runs | **Low–medium.** TanStack Virtual + anser. Server-side paging or range requests for very large logs; never `dangerouslySetInnerHTML`. | **Medium.** Write a virtual scroller (dioxus-primitives has one; Leptos needs your own or leptos-struct-table's). ANSI parsing in Rust is easy. Rendering is safe by default. | **Medium.** Server-paged log windows work well; smooth virtual scroll and search need JS. Escaping is safe by default in askama. |
| **E: Test history** | 11 filters (multi-select comboboxes, time-range picker, tags), URL-synced state, large paginated tables, trend charts | **Low.** React Aria ComboBox/DateRangePicker, TanStack Router search params, ECharts. | **High.** Accessible multi-select combobox and date-range picker built by hand. This is the single biggest accessibility cost. | **Low–medium.** Forms and GET params are natural; native `<input type="date">` pairs are acceptable. A rich multi-select needs a JS island. |
| **F: Comparisons** | Diff tables (new, recovered, missing, regressed), "mismatch explanation" banners | **Low** | **Low–medium** | **Low.** Server-rendered diffs are ideal. |
| **G: Administration** | CRUD forms, roles, retention policies, audit-log table, ingestion-health view | **Low** | **Medium.** Form validation and dialogs are hand-built. | **Low.** Classic server forms are the best fit. |

**Cross-cutting risks.**

1. **Accessibility regressions.** Gate CI on `@axe-core/playwright` for every screen, under any option.
2. **Provenance styling (§18)** needs a design-system decision (badges, hatching for provisional, and so on), whichever framework you choose.
3. **Load.** For many simultaneous runs (§16.14), the server must coalesce per subscriber. Do not push every agent event to every browser.

---

## 11. Risks of the recommendation itself, and mitigations

| Risk | Mitigation |
|---|---|
| Schema drift between Rust and TS | ts-rs plus CI diff gate plus a round-trip fixture test (§3.5) |
| npm supply chain | Exact pins, committed lockfile, release-age cooldown, `npm ci --ignore-scripts` where possible, Renovate with grouped updates, and an SBOM in the image |
| "Two languages" friction for the Rust author | Keep the frontend thin: no business logic, only presentation of server-computed state. That is already the spec's architecture (§6). |
| TypeScript 7 (Go-native) tool incompatibilities | Keep the `typescript@5.x/6.x` JS package available for tools that use the compiler API |
| Future pressure to rewrite in Rust | The protocol crate and REST API are frontend-agnostic (spec §6 "Keep backend contracts independent of the frontend framework"). A later Leptos port could reuse `fleet-protocol` directly. |

---

## 12. What I did not verify

- I did not measure hot-reload or hot-patch latency interactively (`dx serve`, `trunk serve`, Vite HMR). The numbers in §3.4 are batch rebuilds.
- I did not build a PoC for option D (Datastar or htmx), so its assessment rests on documentation and experience [I].
- I did not benchmark the full Fleet UI. The PoCs are skeletons, and real-app sizes in §3.1 are extrapolated [I].
- I did not hands-on test dioxus-primitives' accessibility claims. They come from the repo README.
- I did not check Datastar Pro license terms.
- I did not review the htmx 4 SSE extension changes.
- I could not read GitHub API data such as exact star counts per repo through the session's GitHub access. Stars come from repo pages, and contributor statistics come from local blobless clones.

---

## Sources

**Framework releases and repos**
- Leptos releases (0.9.0-rc0/rc1 breaking changes): https://github.com/leptos-rs/leptos/releases
- Leptos repo: https://github.com/leptos-rs/leptos
- cargo-leptos releases: https://github.com/leptos-rs/cargo-leptos/releases
- Dioxus releases (0.7.x, 0.8.0-alpha.0–3): https://github.com/DioxusLabs/dioxus/releases
- Dioxus repo: https://github.com/DioxusLabs/dioxus
- Dioxus site: https://dioxuslabs.com/
- Dioxus components and primitives: https://github.com/DioxusLabs/components
- Dioxus primitives manifest: https://raw.githubusercontent.com/DioxusLabs/components/main/primitives/Cargo.toml
- Dioxus 0.7 coverage: https://alternativeto.net/news/2025/11/dioxus-0-7-brings-hot-patching-wgpu-rendering-and-axum-integration
- Dioxus v0.7.0 release: https://github.com/dioxuslabs/dioxus/releases/tag/v0.7.0-alpha.0
- Yew: https://github.com/yewstack/yew
- Sycamore: https://github.com/sycamore-rs/sycamore
- Trunk releases: https://github.com/trunk-rs/trunk/releases
- Topcoat announcement: https://tokio.rs/blog/2026-07-22-announcing-topcoat
- Topcoat repo: https://github.com/tokio-rs/topcoat/wiki
- crates.io API (versions, dates, downloads for every crate in §2), e.g. https://crates.io/api/v1/crates/leptos

**Component, table and chart libraries**
- thaw: https://github.com/thaw-ui/thaw (commit history: https://github.com/thaw-ui/thaw/commits/main)
- leptos-shadcn-ui: https://github.com/cloud-shuttle/leptos-shadcn-ui
- rust-ui: https://github.com/rust-ui/ui
- leptodon: https://github.com/openanalytics/leptodon
- leptix-ui: https://github.com/RantAI-dev/leptix-ui
- biji-ui: https://docs.rs/crate/biji-ui/latest
- leptos-struct-table: https://github.com/Synphonyte/leptos-struct-table
- leptos-use: https://docs.rs/leptos-use/latest/leptos_use/
- leptos-chartistry: https://github.com/feral-dot-io/leptos-chartistry
- charming: https://github.com/yuankunzhang/charming (WASM renderer source: https://raw.githubusercontent.com/yuankunzhang/charming/main/charming/src/renderer/wasm_renderer.rs)
- uPlot commits: https://github.com/leeoniya/uPlot/commits/master
- ts-rs: https://github.com/Aleph-Alpha/ts-rs
- npm registry metadata for every npm package in §2.4 and §9.2, e.g. https://registry.npmjs.org/@tanstack/react-table

**Hypermedia**
- htmx 4 release coverage: https://www.infoq.com/news/2026/09/htmx-4-released/
- htmx 4 notes: https://flaviocopes.com/htmx-4/
- htmx 4 overview: https://blog.openreplay.com/htmx-4-0-release/
- Datastar releases: https://github.com/starfederation/datastar/releases
- Datastar Rust SDK: https://github.com/starfederation/datastar-rust

**Real-world use and post-mortems**
- One year of Dioxus fullstack in production (OXIDT, 2026-10-26): https://oxidt.com/blog/one-year-of-dioxus-fullstack-in-production-the-good-the-painful
- Abandoned Dioxus rewrite epic: https://github.com/xiaocongyu66/new-api/issues/356
- lemmy-ui-leptos: https://github.com/LemmyNet/lemmy-ui-leptos
- Lemmy rewrite discussion: https://github.com/LemmyNet/lemmy-ui-leptos/issues/15
- apalis-board: https://github.com/geofmureithi/apalis-board

**Surveys**
- 2025 State of Rust Survey results: https://blog.rust-lang.org/2026/03/02/2025-State-Of-Rust-Survey-results
- Stack Overflow Developer Survey 2025, technology: https://survey.stackoverflow.co/2025/technology
- Stack Overflow Developer Survey 2026, technology: https://survey.stackoverflow.co/2026/technology
