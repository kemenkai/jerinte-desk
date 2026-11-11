#!/usr/bin/env bash
# Copies a custom client variant over the checkout, before anything is built (see README.md).
# Fails closed: a build that skipped the overlay would ship pointing at the public servers.
set -euo pipefail

variant="${1:-}"
cd "$(dirname "$0")/../.."
if [[ ! "$variant" =~ ^[a-z0-9-]+$ || ! -d "res/custom/$variant/overlay" ]]; then
  echo "::error::Unknown custom client variant"
  exit 1
fi
# A variant may extend another one ("extends" in its custom-client.json): the base overlay goes
# first, so the variant only carries what it changes. With "inherit-overlay": false it takes the
# base's settings only and none of its files, e.g. to keep RustDesk's own icons and logo: an
# overlay can replace a file but not remove one the base adds.
# A command substitution, not a process substitution: set -e stops on a bad config only here.
# The name is checked here, not only by the shell below: read splits on whitespace, so a value
# such as "jerinte x" would otherwise pass as jerinte and shift into the second field.
parsed="$(python3 -c '
import json, re, sys
config = json.load(open(sys.argv[1], encoding="utf-8"))
base, inherit = config.get("extends", ""), config.get("inherit-overlay", True)
if not isinstance(base, str) or (base and not re.fullmatch(r"[a-z0-9-]+", base)):
    sys.exit("res/custom-client.json: extends must name a variant")
if not isinstance(inherit, bool):
    sys.exit("res/custom-client.json: inherit-overlay must be true or false")
if not inherit and not base:
    sys.exit("res/custom-client.json: inherit-overlay applies only with extends")
print(base or "-", "yes" if inherit else "no")' "res/custom/$variant/overlay/res/custom-client.json")"
read -r base inherit <<< "$parsed"
[[ "$base" == "-" ]] && base=""
if [[ -n "$base" ]]; then
  if [[ ! "$base" =~ ^[a-z0-9-]+$ || "$base" == "$variant" || ! -d "res/custom/$base/overlay" ]]; then
    echo "::error::Unknown base variant $base"
    exit 1
  fi
  if [[ "$inherit" == "yes" ]]; then
    cp -R "res/custom/$base/overlay/." .
  fi
fi
cp -R "res/custom/$variant/overlay/." .
CUSTOM_BASE="$base" CUSTOM_VARIANT="$variant" python3 - <<'EOF'
import json, os, re, sys


def merge(into, over):
    """Sections such as override-settings merge key by key; anything else `over` sets wins."""
    merged = dict(into)
    for key, value in over.items():
        if isinstance(value, dict) and isinstance(merged.get(key), dict):
            merged[key] = {**merged[key], **value}
        else:
            merged[key] = value
    return merged


# The repository is public: the fields that say where our servers are and which keys they use
# read "@secret" in it, and their values come from the CUSTOM_CLIENT_SECRETS secret, keyed by
# variant: {"jerinte": {"override-settings": {...}}, "jerinte-customer": {...}}.
try:
    secrets = json.loads(os.environ.get("CUSTOM_CLIENT_SECRETS") or "{}")
except ValueError:
    sys.exit("CUSTOM_CLIENT_SECRETS is not JSON")
variant, base = os.environ["CUSTOM_VARIANT"], os.environ["CUSTOM_BASE"]


def pieces(value):
    """A secret value and the parts of it a log line could show on their own."""
    if isinstance(value, dict):
        for inner in value.values():
            yield from pieces(inner)
        return
    if not isinstance(value, str):
        return
    # The whole text first, a JSON list (server-entries) included: a log line showing it entire
    # would otherwise be masked only where each entry's parts appear.
    yield value
    if value.startswith("["):
        try:
            for entry in json.loads(value):
                yield from pieces(entry)
            return
        except ValueError:
            pass
    rest = value.split("://", 1)[-1]
    hostport, _, path = rest.partition("/")
    yield hostport
    yield hostport.rsplit(":", 1)[0]
    if path:
        yield path.split("/", 1)[0]


# On GitHub Actions the log of a public repository is public: mask every secret value and its
# parts (the repository masks only the secret's whole text) before anything can print them.
if os.environ.get("GITHUB_ACTIONS") == "true":
    for piece in sorted({p for p in pieces(secrets) if len(p) >= 4}):
        print(f"::add-mask::{piece}")
data = merge(json.load(open("res/custom-client.json", encoding="utf-8")), secrets.get(variant, {}))
if base:
    base_data = json.load(open(f"res/custom/{base}/overlay/res/custom-client.json", encoding="utf-8"))
    if "extends" in base_data:
        sys.exit(f"res/custom/{base}: a base variant cannot extend another one")
    base_data = merge(base_data, secrets.get(base, {}))
    data = merge(base_data, {k: v for k, v in data.items() if k not in ("extends", "inherit-overlay")})


def unset(value, path=""):
    if isinstance(value, dict):
        for key, inner in value.items():
            yield from unset(inner, f"{path}.{key}" if path else key)
    elif value == "@secret":
        yield path


# A field still "@secret" would ship a client without our servers or keys: fail instead.
missing = list(unset(data))
if missing:
    sys.exit("res/custom-client.json: no value for " + ", ".join(missing)
             + " (CUSTOM_CLIENT_SECRETS lacks them)")
with open("res/custom-client.json", "w", encoding="utf-8") as f:
    json.dump(data, f, ensure_ascii=False, indent=2)
    f.write("\n")
settings = data.get("override-settings", {})
if not settings.get("custom-rendezvous-server") or not settings.get("key"):
    sys.exit("res/custom-client.json: override-settings lacks the server or the key")
# The app name also names the config, IPC, service, install folder and files: changing it makes a
# separate install with a new ID instead of upgrading the existing one.
if data.get("app-name", "RustDesk") != "RustDesk":
    sys.exit("res/custom-client.json: keep app-name RustDesk and set app-display-name instead")


def patch(path, pairs):
    with open(path, encoding="utf-8", newline="") as f:
        text = f.read()
    for old, new in pairs:
        if text.count(old) != 1:
            sys.exit(f"{path}: expected one {old!r}")
        text = text.replace(old, new)
    with open(path, "w", encoding="utf-8", newline="") as f:
        f.write(text)


def newline(path):
    with open(path, encoding="utf-8", newline="") as f:
        return "\r\n" if "\r\n" in f.read() else "\n"


env = os.environ.get("GITHUB_ENV")
name = data.get("app-display-name", "")
if name:
    # The same rule as is_valid_app_display_name in src/common.rs, which would otherwise fall back to
    # RustDesk: the name goes into elevated install scripts and names the Start Menu folder.
    if not re.fullmatch(r"[A-Za-z0-9-]{1,64}", name):
        sys.exit("res/custom-client.json: app-display-name may only contain letters, digits and hyphens")
    if re.fullmatch(r"startup|con|prn|aux|nul|(com|lpt)[0-9]", name.lower()):
        sys.exit("res/custom-client.json: app-display-name is a name Windows reserves")
    # What Explorer, Task Manager and the UAC prompt show for the Sciter exe, the installer (the
    # portable packer) and the Flutter runner. File names stay rustdesk.
    for path in ("Cargo.toml", "libs/portable/Cargo.toml"):
        patch(path, [
            ('ProductName = "RustDesk"', f'ProductName = "{name}"'),
            ('FileDescription = "RustDesk Remote Desktop"', f'FileDescription = "{name} Remote Desktop"'),
        ])
    patch("flutter/windows/runner/Runner.rc", [
        ('VALUE "FileDescription", "RustDesk Remote Desktop"', f'VALUE "FileDescription", "{name} Remote Desktop"'),
        ('VALUE "ProductName", "RustDesk"', f'VALUE "ProductName", "{name}"'),
    ])
    # Linux: the application menu, `systemctl status`, and the GTK window before Flutter titles it.
    patch("res/rustdesk.desktop", [("Name=RustDesk", f"Name={name}")])
    patch("res/rustdesk-link.desktop", [("Name=RustDesk", f"Name={name}")])
    patch("res/rustdesk.service", [("Description=RustDesk", f"Description={name}")])
    patch("flutter/linux/my_application.cc", [
        ('gtk_header_bar_set_title(header_bar, "rustdesk");', f'gtk_header_bar_set_title(header_bar, "{name}");'),
        ('gtk_window_set_title(window, "rustdesk");', f'gtk_window_set_title(window, "{name}");'),
    ])
    # The AppImage's menu entry and file name; its id stays rustdesk.
    for arch in ("x86_64", "aarch64"):
        patch(f"appimage/AppImageBuilder-{arch}.yml", [("    name: rustdesk", f"    name: {name}")])
    # macOS: the menu bar takes CFBundleName; Finder and the Dock take the InfoPlist.strings the
    # build adds, but only while CFBundleDisplayName here matches the bundle's file name, as Finder
    # otherwise treats the bundle as renamed by the user. PRODUCT_NAME stays RustDesk: it is the
    # .app path, and MainMenu.xib names it as the module.
    nl = newline("flutter/macos/Runner/Info.plist")
    patch("flutter/macos/Runner/Info.plist", [(
        "<string>$(PRODUCT_NAME)</string>",
        f"<string>{name}</string>{nl}\t<key>CFBundleDisplayName</key>{nl}\t<string>$(PRODUCT_NAME)</string>"
        f"{nl}\t<key>LSHasLocalizedDisplayName</key>{nl}\t<true/>",
    )])
    if env:
        with open(env, "a", encoding="utf-8") as f:
            f.write(f"CUSTOM_APP_DISPLAY_NAME={name}\n")
    print(f"Display name {name}")
# Later steps skip what custom clients do not ship (the MSI).
if env:
    with open(env, "a", encoding="utf-8") as f:
        f.write("CUSTOM_VARIANT=1\n")
EOF
echo "Applied custom client variant $variant${base:+ (extends $base)}"
