# LHR Studio UI architecture

LHR Studio is the human-facing workspace for developers using LHR. It is a static React/TypeScript client for the Rust/Axum service; production Studio must not require a Node.js process.

The UI is **not** a visual dump of the engine's internal modules. Engine terminology belongs in diagnostics when it helps a developer solve a problem, not in primary navigation merely because the subsystem exists.

## Product contract

A developer opening Studio should be able to answer four questions immediately:

1. Is my database ready?
2. How do I get data into it?
3. How do I query it?
4. How do I call it from my application?

Those questions define the primary information architecture:

- **Home** — state + useful next actions
- **Data** — import and schema
- **Query** — exact query builder and results
- **API** — copyable integration examples and core endpoints

Operational surfaces are secondary and live behind **System**:

- indexes
- workload telemetry
- generations
- maintenance/recovery
- raw metrics

**Settings** contains only settings/session/connection information a user can act on. Design tokens, palette notes, implementation contracts, transport trivia, and similar project metadata must never be presented as user settings.

### UX rules

- Do not create a top-level page solely because a Rust subsystem exists.
- Do not show an internal metric unless it helps the user make a decision or diagnose a problem.
- Prefer progressive disclosure: ordinary defaults first, advanced resource ceilings and engine details second.
- Empty states must lead to a useful action.
- A primary workflow may not require the user to paste raw JSON when a structured UI can express it safely.
- Destructive/replacement actions must explain what changes and preserve LHR's recovery model.
- Diagnostics may be dense; primary workflows should be obvious without reading documentation.
- Use familiar developer vocabulary (`Data`, `Query`, `API`, `System`) before LHR-specific vocabulary.

## Import contract

Studio CSV import is a convenience path, not a browser-memory ingest pipeline.

- The browser reads only a bounded sample for preview/schema inference.
- The full CSV is uploaded as multipart data and streamed to a temporary server-side file.
- The service enforces a dedicated import byte ceiling.
- The existing two-pass `import_csv` pipeline builds dictionaries, canonical data, exact indexes, verification and the immutable generation.
- Publication remains atomic; an existing CURRENT generation is not modified in place.
- Temporary upload files are removed after success or failure.
- Very large/offline imports can continue to use the CLI without changing the Studio information architecture.

## Visual contract

The palette is intentionally closed. Do not introduce arbitrary accent, status, brand, chart, or semantic colors.

- `#000000` — database/background plane
- `#151515` — primary surfaces
- `#1e1e1e` — raised/interactive surfaces
- `#353535` — borders/dividers/strong separation
- `#e7e7e7` — foreground/content

State is communicated through density, opacity, weight, line treatment, iconography, and text. Transparency derived from the five palette colors is allowed; adding another hue is not.

The product should feel like durable developer tooling, not a marketing site. Avoid gradients, glow, glass effects, oversized radii, decorative animation, large empty cards, novelty dashboards, and ornamental charts.

## CSS contract

Do not scatter component-specific one-off utility strings throughout JSX. Layout and surface behavior is declared once in semantic CSS classes and reused.

Foundation classes include:

- `.stack`, `.cluster` — repeated flow primitives
- `.panel` — bounded content surface
- `.metric-grid`, `.metric` — compact numerical summaries
- `.content-grid` — repeatable panel ratios
- `.data-table` — dense database tables
- `.button` — common control surface

Color values live only in `styles/tokens.css`. Feature CSS consumes those variables instead of declaring raw palette colors.

## Performance contract

The UI must remain safe when the database is much larger than browser memory or server RAM.

- Never request or render an unbounded row set.
- Use stable cursor pagination from the LHR API.
- Do not poll expensive database endpoints when their screen is not active.
- Cache schema/index/generation metadata with deliberate stale times.
- Mutations and admin operations never retry automatically.
- Large exports/imports must stream rather than accumulate in React state.
- Prefer same-origin requests; avoid an extra frontend server hop in production.
- Keep optional dependencies rare and justified.

TanStack Query owns server-state caching and cancellation. TanStack Router owns client navigation. Correctness and permission enforcement remain server-side in Rust.

## Responsive contract

Desktop uses a persistent compact sidebar. Mid-width layouts collapse it to an icon rail. Small screens use a bottom navigation rail and single-column content. Primary workflows must remain fully operable at each width; advanced tables may scroll horizontally rather than breaking layout.

## Deployment direction

Development: Vite on `127.0.0.1:5173`, proxying LHR endpoints to `127.0.0.1:8787`.

Production: the Studio build is embedded/copied into the LHR runtime image and served by Axum from the same origin as `/v1`, `/healthz`, `/readyz`, and `/metrics`.

GHCR/image publishing should only be triggered when a coherent Studio slice is ready for meaningful validation, not for every intermediate UI commit.
