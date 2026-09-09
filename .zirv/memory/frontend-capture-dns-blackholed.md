## Memory
- Key: frontend-capture-dns-blackholed
- Written-by: claude
- Written: 1788979273
- Verified: 1788979273
- Source: explicit

zirv frontend render's headless Chromium capture (frontend_render.rs capture()) launches with --host-resolver-rules "MAP * 0.0.0.0, EXCLUDE localhost, EXCLUDE 127.0.0.1", blackholing every DNS lookup except loopback. It is a deliberate security boundary with no comment at the call site: a captured page may render repository-influenced content and must not reach any external host. Never remove or weaken that flag when touching the launch argv.
