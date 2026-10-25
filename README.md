# Link Studio

Control panel for the Insta360 Link webcam on Linux, built with Rust, GTK 4
and libadwaita. Works on GNOME and KDE.

- Live preview (720p, 1080p or 4K) with photo capture to `~/Pictures/Link Studio`
- Gimbal: pan, tilt, zoom, centering, privacy mode and saved positions
- AI tracking with framing (head, upper body, full body)
- Image settings: brightness, contrast, saturation, hue, sharpness,
  white balance, focus and anti-flicker
- Profiles, optionally applied whenever the camera is connected
- Preferences (Ctrl+,): light/dark style and language, both following the
  system by default
- Virtual camera "Link Studio Camera" (Effects tab) for Teams, Zoom, OBS and
  browsers, so the preview and a meeting can use the camera at the same time
- Status icon with quick access to the virtual camera and AI tracking
- **Claude integration (MCP)**, off by default: let Claude tune the picture
  and framing for you – optionally looking through the camera

On KDE Plasma the app keeps its libadwaita (GNOME) look and its own title
bar; light/dark and the accent color are taken from Plasma through the
desktop portal.

## Virtual camera

The virtual camera uses the `v4l2loopback` kernel module. The Effects tab
sets it up with the administrator password (through `pkexec`), running
`data/setup-virtual-camera.sh`, which:

- installs the module (openSUSE, Debian/Ubuntu, Arch; Fedora via RPM Fusion),
- writes `/etc/modprobe.d/linkstudio-v4l2loopback.conf` and
  `/etc/modules-load.d/linkstudio-v4l2loopback.conf`,
- loads the module, creating `/dev/video42` named "Link Studio Camera".

`data/remove-virtual-camera.sh` (also offered in the Effects tab and when
uninstalling) removes those two files. The module package is left installed.

While the virtual camera is on, closing the window hides it; choose Quit in
the menu (Ctrl+Q) to stop.

## Status icon

Link Studio shows a status icon (StatusNotifierItem) with Show, Virtual
camera, AI tracking and Quit. KDE Plasma shows it out of the box; GNOME needs
the [AppIndicator extension](https://extensions.gnome.org/extension/615/appindicator-support/).

Under Preferences → General → Window:

- **Keep in the status icon when the window is closed** – closing the window
  leaves Link Studio in the status icon, ready to use. The camera is released
  while the window is hidden, so other programs can use it.
- **Start at login** – adds an autostart entry that runs `linkstudio --minimized`,
  which starts in the status icon without opening the window.

## Claude integration (MCP)

Link Studio includes an [MCP](https://modelcontextprotocol.io) server, so
Claude (in Claude Code or Claude Desktop) can work with your camera: read and
change image settings, point the gimbal, turn AI tracking on or off, and save
positions and profiles. If you also let it see through the camera, it can take
a picture, judge lighting and framing, adjust, and check again – like a
photographer getting you ready for a meeting.

**Everything is off until you turn it on**, in Link Studio → Preferences →
Claude, and can be turned off again at any time:

- **Allow control from Claude** – settings, gimbal, AI tracking, positions,
  profiles and the virtual camera.
- **Allow Claude to see through the camera** – a separate permission for
  pictures.

### Privacy

- Pictures Claude takes are sent to Anthropic to be analyzed, the same as an
  image you paste into a chat. Only turn on "see through the camera" if you are
  comfortable with that.
- Every picture shows a **"Claude is looking"** badge in the preview and a
  notice (a desktop notification while the window is hidden). Every change
  Claude makes shows a notice too.
- **Privacy mode always wins:** while it is on, Claude can neither see through
  nor move the camera. Claude can turn privacy mode on, but only you can turn it
  off. Privacy mode is remembered across restarts.
- These rules are enforced by Link Studio itself, not by Claude or the MCP
  process, so they apply however Claude connects. Pictures are never stored by
  Link Studio.

### Connecting

Click **Add** under Preferences → Claude, or run:

```sh
claude mcp add --scope user linkstudio -- linkstudio mcp
```

For Claude Desktop, add this to `claude_desktop_config.json` (use the full
path if `~/.local/bin` is not on Claude Desktop's PATH):

```json
{ "mcpServers": { "linkstudio": { "command": "linkstudio", "args": ["mcp"] } } }
```

Start a new Claude session and ask, for example:

- "Make me look good for my meeting."
- "Is anything off with my camera picture?" – backlight, framing, clutter.
- "Save a position that shows the whiteboard."
- "Make a warmer profile for evening calls and use it automatically."

If Link Studio is not running, `linkstudio mcp` starts it, so you always see
what Claude is doing. The camera sleeps (gimbal parked) when nothing streams;
Link Studio wakes it for Claude and lets it sleep again 10 seconds after
Claude's last request.

Tools: `get_status`, `take_snapshot`, `set_image_settings`,
`reset_image_settings`, `move_gimbal`, `center_gimbal`, `set_tracking`,
`goto_position`, `save_position`, `save_profile`, `apply_profile`,
`set_virtual_camera`, `set_privacy`.

## Install / uninstall

Needs `cargo`, `gtk4-devel` and `libadwaita-devel` (openSUSE package names).

```sh
./install.sh             # builds and installs into ~/.local
./uninstall.sh           # removes the program, keeps settings
./uninstall.sh --purge   # also removes ~/.config/linkstudio
```

The app can also uninstall itself from its main menu; that also removes the
Claude Code registration (`claude mcp remove linkstudio`).

## Translations

English is the source language. Translations are gettext catalogs in `po/`
and are embedded into the binary at build time; the language follows the
desktop locale (`LANGUAGE`, `LC_MESSAGES`, `LANG`).

```sh
./po/update-po.sh              # refresh po/linkstudio.pot and merge into all .po files
cp po/linkstudio.pot po/de.po  # start a new language, then fill in msgstr
```

The language can also be picked in Preferences; the window is rebuilt in the
new language right away. To try one from a terminal: `LANGUAGE=nb linkstudio`.

## Camera notes

The vendor features use the camera's UVC extension unit 9 (GUID
`faf1672d-b71b-4793-8c91-7b1c9b7f95f8`), mapped by
[vrwallace/Insta360-Link-1-and-2-Controller-for-Linux](https://github.com/vrwallace/Insta360-Link-1-and-2-Controller-for-Linux).
On firmware 1.4.5.8:

- The mode selector reads back `0xff` after a mode command, so the app keeps
  track of the tracking state itself. Changes made by hand gesture are
  reported properly, so the switch follows the palm gesture.
- The camera drops AI tracking whenever its video stream stops; Link Studio
  turns it back on after restarting its own preview.
- Group tracking (unit 10, selector 1) is read-only, so that option is hidden.
- Selector 9 is exposure compensation: signed 16-bit little-endian, about
  256 per EV. Its GET_MIN/GET_MAX replies are not usable. The value survives
  stream restarts.
- uvcvideo caches the V4L2 pan/tilt values, so moves made by AI tracking are
  invisible there. Selector 26 holds the live position as (tilt, pan) in
  arcsec and is used instead.
- No live zoom value was found, so zoom changed with the "L" gesture is not
  reflected in the zoom slider.
- Writing selector 17 made the camera reset and re-enumerate. Selector 7
  stops tracking when written, but did not measurably change tracking speed.

## License

Copyright © 2026 Jan Ivar Sørensen

Link Studio is free software: you can redistribute it and/or modify it under the terms of the
[GNU General Public License](LICENSE) as published by the Free Software Foundation, either version 3 of
the License, or (at your option) any later version.

Link Studio is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the
implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License
for more details.
