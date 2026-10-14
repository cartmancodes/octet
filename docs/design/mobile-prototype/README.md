# Octet mobile prototype

`octet-mobile-prototype.dc.html` is the source of the Claude Design prototype the phone app is rebuilt
to match. It comes from the project "Octet Terminal Prototype", file "Octet Mobile Prototype.html"
(https://claude.ai/design/p/d0eacaaf-6012-4b35-9fcd-216d8b1d9d85). It needs that project's
`support.js` to run, so read it here as a reference for layout, copy and behaviour.

The app departs from the prototype in five places, by design
(`docs/superpowers/specs/2026-10-15-mobile-prototype-design.md`):
- there is no "Session" allow, because Allow is one request, once;
- the Settings footer describes the tailnet, because there is no relay;
- the Usage screen has no context-% bar, because neither vendor reports it;
- there is no image chip;
- the Dynamic Island never receives commands or names from APNs.
