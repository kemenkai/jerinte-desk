# Custom client variants

A variant is a branded build of this client with its server settings compiled in.

```
res/custom/
  make_assets.py          generates a variant's images from its logo sources
  <variant>/
    logo/mark.png         the mark alone (app icons), transparent background
    logo/lockup.png       mark + wordmark side by side (home-page logo)
    overlay/              files copied over the checkout before building, at their repo paths
      res/custom-client.json
      ...generated images...
  <variant extending another>/
    overlay/res/custom-client.json   "extends" plus the settings it changes; images come from the base
                                     unless it sets "inherit-overlay": false
```

Variants: `jerinte` (staff), `jerinte-customer` (customers' machines: incoming only), and
`jerinte-rustdesk` (staff with RustDesk's name and look).

## Building

`flutter-ci.yml` takes a `variant` input. When it is set, every desktop build job runs
`res/custom/apply.sh <variant>` right after checking out: it copies `res/custom/<variant>/overlay/`
over the checkout and fails the build unless `res/custom-client.json` then overrides the server and
the key. The packages go to the release `<variant>-<run number>` instead of `private-<run number>`.
A variant cannot be combined with `targets=all`, which also runs jobs that do not apply it. A
variant ships the Windows exe without the MSI (`apply.sh` sets `CUSTOM_VARIANT` for the later
steps). Private Linux builds add an AppImage, made from the deb, to the deb and the rpms.

`sciter` in `targets` builds the 32-bit Sciter client (`rustdesk-<version>-x86-sciter.exe`) for
Windows 7 machines whose display driver is not WDDM: the Flutter UI needs Direct3D 11 and crashes
in `flutter_windows.dll` there. Its UI reads the same `hide-*` options from `custom-client.json`;
its icons come from `res/icon.ico`, `res/tray-icon.ico` and, for the title bar of every window,
`res/sciter-window-icon.png` (128 px, no padding).

## Compiled-in settings

`res/custom-client.json` is empty in the stock tree. `src/common.rs` embeds it with
`include_str!`; when it is not empty, `load_custom_client` applies it in every process instead of
reading a signed `custom.txt`, and the Windows exe-name server settings are ignored. The format is
the one `custom.txt` carries once decoded:

- `override-settings`: values the user cannot change (UI, `--option`, config files). Keys are
  routed like RustDesk's custom clients: display, local, then general settings, then builtin UI
  options such as `hide-server-settings`.
- `default-settings`: initial values the user can change.
- `app-name`: must stay `RustDesk`, which `apply.sh` enforces. It also names the config folder and
  files, the IPC names, the service, the install folder, the files and the packages: a client
  renamed this way would be a new install beside the existing one, with its own config and so a
  new ID, instead of an upgrade that keeps it.
- `app-display-name`: the name users see (letters, digits and hyphens, at most 64, and not a name
  Windows reserves such as `Startup` or `CON`). Texts that name RustDesk name it instead, except
  "Powered by RustDesk" (hidden with the builtin option `hide-powered-by-me`), upstream's server
  products, and the remote printer, which is installed as "RustDesk Printer". Window titles, the
  tray, notifications, the About page and the elevation prompts use it, and on Windows so do the
  shortcuts, the Start menu folder, the Programs entry and the service's display name. Shortcuts
  left under the RustDesk name are removed; a Start menu folder goes only once it holds nothing
  else. `apply.sh` writes the name into the exe version resources (Cargo.toml for Sciter and the
  installer, Runner.rc for Flutter), the Linux desktop entries, systemd description, GTK title and
  AppImage, and the macOS Info.plist; the macOS job adds the localized name Finder shows and names
  the dmg volume after it. Custom clients hide the About page's links to rustdesk.com and keep the
  copyright.
- `extends`: another variant this one builds on. `apply.sh` copies that variant's overlay first,
  then this one's, and merges the two `custom-client.json` files: sections such as
  `override-settings` key by key, anything else this variant sets wins. One level only; the base
  must exist. `jerinte-customer` extends `jerinte` this way and only adds what a customer's
  machine needs: it accepts connections and never makes them (`conn-type: incoming`, which also
  refuses switching sides), shows the ID and the one-time password with nothing to configure,
  and accepts passwords only, so a request without one does not open a window on its screen.
  It keeps `jerinte`'s `server-entries`, so a machine on our intranet registers through the native
  ports there and one outside through the public entry. A machine outside probes the intranet
  entry too, so it connects to whatever holds that address on its own network; only our ID
  server's signed key exchange counts as an answer, so that machine cannot take the entry over.
- `inherit-overlay` (with `extends`, default `true`): `false` takes the base's settings and none of
  its files. `jerinte-rustdesk` uses it to be `jerinte` in all but the look: RustDesk's own name,
  icons and home page (no logo), since an overlay can replace a base's file but not remove one the
  base adds (`flutter/assets/logo_*.png`). Its settings come from `jerinte`, secrets included.
- `server-entries` (an override): several ways to the same servers as a JSON list of
  `{"name", "id", "relay"}`, in order of preference. The service registers through the first that
  answers, else the quickest (`src/server_entries.rs`), and they take the place of
  `custom-rendezvous-server` and `relay-server`, which stay for builds that do not know entries.
  An `id` given as a `ws://`/`wss://` URL is reached over WebSocket, `host:port` over the native
  ports. Write each `relay` the way the ID server hands it out (its `-r`, or the public relay URL),
  since it rewrites only relays it recognizes for a peer that came in another way. `jerinte` lists
  the intranet's native ports first, then the public WebSocket entry.
- `dns-fallback` (an override): `name=address` pairs, comma separated, for names the system may
  not resolve. Our intranet has no route out, so nothing there resolves the public name, and the
  server behind it answers at a private address. When the system fails to resolve a name listed
  here (within 3 s), the client resolves it to the address given, and goes on doing so for a
  minute before asking the system again; once the system resolves it, its answer is used
  (`libs/hbb_common/src/dns_fallback.rs`). Only the lookup changes: the URL, SNI and the
  certificate check keep the name, and it applies only to `https://` and `wss://` with the
  certificate checked, so a host at that address on another network cannot pass for the server.
  It covers the WebSocket entries and the API; with it set, Flutter sends its API requests
  through Rust. Not covered: a proxy (it resolves the name), avatars (Flutter loads them itself),
  and a name that resolves to an address which is then unreachable.
- other top-level string keys: hard settings such as `disable-settings`.

Every custom client hides the links to rustdesk.com (About page, install page agreement, help
links on the home page cards), and a client with compiled-in settings never downloads stock
updates, which would drop those settings. The Sciter UI honours `hide-general-settings` like the
Flutter one: no audio input, enhancements, theme, language or update items.

## Images

`python3 res/custom/make_assets.py res/custom/<variant>` (needs Pillow) rewrites the images under
`overlay/`. It fills the areas the mark encloses with white so the icons read on dark backgrounds.
`flutter/assets/icon.png` is deliberately not produced: the tray would load it instead of the tray
icons, and macOS would draw it as a solid template silhouette.
