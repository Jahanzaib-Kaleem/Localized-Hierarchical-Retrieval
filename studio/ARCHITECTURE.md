# LHR Studio UI architecture

LHR Studio is a static React/TypeScript client for the Rust/Axum LHR service. Production Studio must not require a Node.js process. Vite is a development/build dependency only; the compiled assets will ultimately be served by LHR itself.

## Visual contract

The palette is intentionally closed. Do not introduce arbitrary accent, status, brand, chart, or semantic colors.

- `#000000` — database/background plane
- `#151515` — primary surfaces
- `#1e1e1e` — raised/interactive surfaces
- `#353535` — borders/dividers/strong separation
- `#e7e7e7` — foreground/content

State is communicated through density, opacity, weight, line treatment, iconography, and text. Transparency derived from the five palette colors is allowed; adding another hue is not.

The product should feel like durable database tooling, not a marketing site. Avoid gradients, glow, glass effects, oversized radii, decorative animation, large empty cards, novelty dashboards, and ornamental charts.

## CSS contract

Do not scatter component-specific one-off utility strings throughout JSX. Layout and surface behavior is declared once in semantic CSS classes and reused.

Foundation classes include:

- `.stack`, `.cluster` — repeated flow primitives
- `.panel` — bounded content surface
- `.metric-grid`, `.metric` — compact numerical summaries
- `.content-grid` — repeatable panel ratios
- `.data-table` — dense database tables
- `.status-strip` — compact runtime state
- `.button` — common control surface

Color values live only in `styles/tokens.css`. Feature CSS should consume those variables instead of declaring raw colors.

## Performance contract

The UI must remain safe when the database is much larger than browser memory or server RAM.

- Never request or render an unbounded row set.
- Use stable cursor pagination from the LHR API.
- Do not poll expensive database endpoints when their screen is not active.
- Cache schema/index/generation metadata with deliberate stale times.
- Mutations and admin operations never retry automatically.
- Large exports must stream/download rather than accumulate in React state.
- Prefer same-origin requests; avoid an extra frontend server hop in production.
- Keep optional dependencies rare and justified.

TanStack Query owns server-state caching and cancellation. TanStack Router owns client navigation. Correctness and permission enforcement remain server-side in Rust.

## Responsive contract

Desktop uses a persistent dense sidebar. Mid-width layouts collapse it to an icon rail. Small screens use a bottom navigation rail and single-column content. No route may rely on a desktop-only viewport to remain operable.

## Deployment direction

Development: Vite on `127.0.0.1:5173`, proxying LHR endpoints to `127.0.0.1:8787`.

Production target: the Studio build is embedded/copied into the LHR runtime image and served by Axum from the same origin as `/v1`, `/healthz`, `/readyz`, and `/metrics`.

Docker/GHCR publishing is deliberately deferred until the initial Studio implementation is coherent enough that image builds provide useful validation rather than wasting CI runs.
