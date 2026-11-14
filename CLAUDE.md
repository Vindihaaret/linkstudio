# Link Studio – notes for Claude

Rust GTK 4 / libadwaita control app for the Insta360 Link webcam on Linux. The owner sometimes calls it
"instalink". Public repo: github.com/Vindihaaret/linkstudio (GPL-3.0). The owner is not a programmer:
explain in plain Norwegian, and do the work rather than describing it.

## Rules that always apply

- **Commit, push and release only when the owner asks** ("send til git"). One approval covers one release.
- **Never view or publish the owner's face or personal data.** Do not open the live preview or take
  snapshots to test. Screenshots are taken with the preview off; no personal paths or the camera's serial
  number in the repo.
- **Do not move the real camera in tests without asking** – it may be in a meeting.
- The Claude integration must stay fully opt-in and clearly described in the app and the README.
  "Privacy mode always wins" is a promise: every path that lets Claude see or move must go through
  `claude_check`, also after a delay.
- User-facing text is English in the source, Norwegian in `po/nb.po`. Keep both complete
  (`translator-credits` is the only entry left empty).
- Keep `README.md` in step with every user-visible change.

## Layout

| File | Contents |
|---|---|
| `src/main.rs` | Start-up, `mcp` subcommand, `--minimized` |
| `src/camera.rs` | V4L2 controls and Insta360 vendor (XU) commands |
| `src/preview.rs` | Capture thread: MJPEG stream, decode, snapshot for Claude |
| `src/vcam.rs` | Virtual camera (v4l2loopback) writer and its setup through pkexec |
| `src/config.rs` | `~/.config/linkstudio/config.json` |
| `src/tray.rs` | Status icon (ksni) – menu and the `Command`s it sends |
| `src/mcp.rs` | `linkstudio mcp`: MCP server that forwards to the app over D-Bus |
| `src/i18n.rs`, `build.rs` | gettext catalogs embedded at build time |
| `src/ui.rs` | `App` struct, window construction, signals, shortcuts, connect/refresh, tray wiring |
| `src/ui/sidebar.rs` | Settings pages: image, tracking, effects, profiles, camera |
| `src/ui/gimbal.rs` | Moves, zoom, AI tracking, privacy mode, screen-lock privacy |
| `src/ui/positions.rs` | Saved position tiles and their menu |
| `src/ui/stream.rs` | Start/stop stream, retries, hide to tray, close, photos |
| `src/ui/dialogs.rs` | About, Preferences, uninstall, shortcut list |
| `src/ui/claude.rs` | D-Bus object for Claude, permission checks, Claude preferences page |
| `src/ui/update.rs` | Update check and in-app updater (shell script in `INSTALL_SCRIPT`) |

The `src/ui/*.rs` files are `impl App` blocks with `use super::*;`; methods called from another file are
`pub(super)`.

## Things that are easy to get wrong

- The camera **sleeps when nothing streams** and then ignores gimbal moves and tracking. Anything that
  moves it while the window is hidden must wake it first (`claude_keep_awake` + `claude_when_awake`).
  Privacy mode turned on while asleep is applied in `stream_started`.
- Restarting the stream drops AI tracking in the camera; `resume_tracking` turns it back on.
- `force_close()` really closes (destroys) the window; `window.close()` hides it when "keep in the status
  icon" is on and stops at an open dialog.
- GTK/libadwaita: wrapped labels in the sidebar need `max_width_chars(1)`; `icon_group` hides the group's
  own title widget; a `PopoverMenu` parented to a widget must be unparented on destroy, and lists that own
  the menu are rebuilt from an idle callback (`rebuild_positions_later`).
- xgettext reads the Rust files as C: `tr("…")`/`trf("…")` with a literal is extracted, also in tests.

## Checks before saying something works

```bash
cargo clippy --all-targets -- -D warnings
cargo test
./po/update-po.sh          # then translate new strings in po/nb.po; fix "fuzzy" guesses by hand
```

Trying the app without touching the owner's settings (it still opens the real camera, so keep
`"preview_autostart": false` in the test config):

```bash
dbus-run-session -- sh -c '
  python3 -I tools/fake-tray-host.py /tmp/item &          # only needed for the status icon
  sleep 1
  XDG_CONFIG_HOME=/path/to/test-config \
  LINKSTUDIO_SCREENSHOT=/path/out.png LINKSTUDIO_PAGE=preferences \
  target/release/linkstudio'
```

`LINKSTUDIO_PAGE` takes a sidebar page (`image`, `tracking`, `effects`, `profiles`, `camera`),
`preferences`, `preferences-claude` or `about`. The app renders the window to the PNG after about 8 s and
quits. Now and then the PNG is not written ("screenshot: node=false"); run it again. The status icon menu
can be read with `gdbus call --session -d <item name> -o /MenuBar -m com.canonical.dbusmenu.GetLayout -- 0 -1 '[]'`.

`install -Dm755 target/release/linkstudio ~/.local/bin/linkstudio` lets the owner try a build; they must
quit and restart Link Studio.

## Release

1. Bump `version` in `Cargo.toml`, `cargo build --release` (updates `Cargo.lock`), run the checks.
2. Commit and push to `main`.
3. `gh release create vX.Y.Z --draft --title "Link Studio X.Y.Z" --notes-file …` (English notes, what is
   new for the user, ending with how to update).
4. `git tag vX.Y.Z && git push origin vX.Y.Z`. The Release workflow builds the package, attaches it with
   checksum and attestation, and publishes the draft. Wait for it and for CI, then check that
   `releases/latest` shows the new tag.

The `target/` build files are kept on purpose.
