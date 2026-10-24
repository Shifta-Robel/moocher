# Dashboard (ui/web)

SolidJS + Tailwind CSS v4 + Kobalte, built with Vite into `../../supervisor/ui-dist/`,
which the supervisor embeds. The built files are checked in, so building the
Rust workspace does not need Node.

```bash
npm ci
npm run dev      # http://localhost:5173, proxies /api and /auth to a supervisor on 127.0.0.1:7700
npm run build    # type-checks, then writes ../../supervisor/ui-dist (rebuild the supervisor afterwards)
```

- `src/store.ts`: the cluster state, loaded once and kept current by `/api/v1/stream` (SSE).
- `src/api.ts`: fetch with the session cookie and the CSRF header.
- `src/components/ui.tsx`: the small UI kit (status badges carry colour, an icon and a word).
- `src/pages/*`: one file per view; all but Login and Overview load on demand.
- `public/sw.js`: the service worker for Web Push; `public/theme.js` sets dark/light before first paint
  (the CSP allows no inline scripts).

Browser tests: `tests/e2e/ui.sh` (Playwright, headless Chromium).
