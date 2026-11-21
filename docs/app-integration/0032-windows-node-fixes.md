# 0032 · Windows node W2g: the fix menu

Node: **0.37.0** (W2g of [WINDOWS.md](../WINDOWS.md)). Status: **proposed** for the app. Builds on [0026](0026-windows-node-w2a.md) and [0031](0031-windows-node-heal.md). Additive: no method, field, frame, cap or link request changed.

## What the person gets

`nightgrid-node fix` works on a Windows host with the same names as on Linux: restart a listed service, clean the Windows Update download cache (`packages`), fix OpenSSH key permissions (`ssh`), reboot, and run a runbook. `fix auto` offers the presets that apply on Windows.

## What the app sees

- `actions.log` on a Windows host may now show `fix.package-cache`, `fix.ssh-permissions` and `fix.runbook`, with the source `menu (...)` or `autofix <id>`, in the fields that were always free strings (as on Linux since the fix menu).
- Self-healing rules (`heal.rules.set`) may use `fix.package-cache` with `disk-full` on Windows once root allows it. `fix.journal-vacuum`, `fix.logrotate` and `fix.unit-reset` are refused on Windows with "not available on Windows yet".
- `actions.policy` is unchanged: it lists only protocol action types.

## What to change in the app

- Nothing is required. Where the app shows disk-full rule actions for a Windows host, it may offer `fix.package-cache` ("Clean the Windows Update cache") and leave out the journal and logrotate fixes.
