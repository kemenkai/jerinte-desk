#!/usr/bin/env python3
"""Keeps custom client builds (`inputs.variant`) of flutter-build.yml from publishing anything.

The repository is public, and a custom client has our servers compiled in. So in a variant build:
no step may publish a release or upload an artifact (both public), rust-cache may not save its
target directory (fork pull requests can restore caches), and the packages go to the draft release
flutter-ci.yml creates, which only the repository's collaborators can see.

  variant_guard.py check FILE   fail when a step breaks the rules above
  variant_guard.py apply FILE   rewrite FILE so it keeps them (after merging upstream's workflows)
"""
import re
import sys

GUARD = "inputs.variant == ''"
VARIANT = "inputs.variant != ''"
PUBLISHES = ("softprops/action-gh-release", "actions/upload-artifact")
# The steps a variant build runs that publish its packages: each gets a sibling uploading the same
# files to the draft release. Matched by the files they publish.
DRAFT_FILES = (
    "./SignOutput/rustdesk-*.msi",
    "./SignOutput/rustdesk-*.exe",
    ".dmg",
    "rustdesk-*.deb",
    ".AppImage",
)


def steps(lines):
    """(start, end, dash indent) of every step: a `- ` item whose first key is a step key."""
    out = []
    for i, line in enumerate(lines):
        m = re.match(r"^(\s*)- (name|uses|if|id|run|shell|with|env):", line)
        if not m:
            continue
        indent = len(m.group(1))
        end = i + 1
        while end < len(lines):
            nxt = lines[end]
            if nxt.strip():
                lead = len(nxt) - len(nxt.lstrip())
                # A comment less indented than the step's keys belongs to what follows.
                if lead < indent + 2 and (nxt.lstrip().startswith("#") or lead < indent):
                    break
                if not nxt.lstrip().startswith("#") and (
                        lead < indent + 2 or (lead == indent and nxt.lstrip().startswith("- "))):
                    break
            end += 1
        while end > i + 1 and not lines[end - 1].strip():
            end -= 1
        out.append((i, end, indent))
    return out


def key(lines, start, end, indent, name):
    """The line index of a step key, the first line's own included."""
    for j in range(start, end):
        line = lines[j]
        if j == start:
            line = " " * (indent + 2) + line.lstrip()[2:]
        if re.match(rf"^ {{{indent + 2}}}{name}:", line):
            return j
    return None


def if_value(lines, j):
    return lines[j].split("if:", 1)[1].strip()


def guarded(expr):
    return GUARD in expr


def with_guard(expr, guard=GUARD):
    expr = expr.strip()
    if expr.startswith("${{") and expr.endswith("}}"):
        return expr[:-2].rstrip() + f" && {guard} }}}}"
    return f"({expr}) && {guard}"


def problems(text):
    lines = text.split("\n")
    out = []
    for start, end, indent in steps(lines):
        body = "\n".join(lines[start:end])
        uses = key(lines, start, end, indent, "uses")
        target = lines[uses].split("uses:", 1)[1].strip() if uses is not None else ""
        cond = key(lines, start, end, indent, "if")
        expr = if_value(lines, cond) if cond is not None else ""
        if target.startswith(PUBLISHES) and not guarded(expr):
            out.append(f"line {start + 1}: {target.split('@')[0]} runs in a variant build")
        if target.startswith("Swatinem/rust-cache") and f"save-if: ${{{{ {GUARD} }}}}" not in body:
            out.append(f"line {start + 1}: rust-cache saves in a variant build")
        if "res/custom/apply.sh" in body and "secrets.CUSTOM_CLIENT_SECRETS" not in body:
            out.append(f"line {start + 1}: apply.sh without CUSTOM_CLIENT_SECRETS")
    return out


def draft_step(indent, expr, files):
    pad = " " * indent
    sub = pad + "  "
    globs = " ".join(files)
    return [
        f"{pad}- name: Upload to the draft release (custom client)",
        f"{sub}if: {with_guard(expr, VARIANT)}",
        f"{sub}shell: bash",
        f"{sub}env:",
        f"{sub}  GH_TOKEN: ${{{{ github.token }}}}",
        f"{sub}run: |",
        f"{sub}  shopt -s nullglob",
        f"{sub}  files=({globs})",
        f'{sub}  if [ ${{#files[@]}} -eq 0 ]; then echo "::error::nothing to upload"; exit 1; fi',
        f'{sub}  gh release upload "$TAG_NAME" "${{files[@]}}" --repo "$GITHUB_REPOSITORY" --clobber',
    ]


def apply(text):
    lines = text.split("\n")
    # Bottom up, so the indices of the steps still to do stay right.
    for start, end, indent in reversed(steps(lines)):
        body = "\n".join(lines[start:end])
        uses = key(lines, start, end, indent, "uses")
        target = lines[uses].split("uses:", 1)[1].strip() if uses is not None else ""
        cond = key(lines, start, end, indent, "if")
        expr = if_value(lines, cond) if cond is not None else "true"
        insert = []
        if target.startswith("softprops/action-gh-release") and not guarded(expr):
            files = [l.strip() for l in lines[start:end] if l.strip().startswith(("./", "rustdesk"))]
            if any(f.endswith(DRAFT_FILES) or f in DRAFT_FILES for f in files):
                insert = draft_step(indent, expr, files)
        if target.startswith(PUBLISHES) and not guarded(expr):
            if cond is not None:
                lines[cond] = lines[cond].split("if:", 1)[0] + "if: " + with_guard(expr)
            else:
                lines.insert(start + 1, " " * (indent + 2) + f"if: {GUARD}")
                end += 1
        if target.startswith("Swatinem/rust-cache") and "save-if:" not in body:
            w = key(lines, start, end, indent, "with")
            if w is None:
                lines[end:end] = [" " * (indent + 2) + "with:", " " * (indent + 4) + f"save-if: ${{{{ {GUARD} }}}}"]
            else:
                lines.insert(w + 1, " " * (indent + 4) + f"save-if: ${{{{ {GUARD} }}}}")
            end += 1
        if "res/custom/apply.sh" in body and "secrets.CUSTOM_CLIENT_SECRETS" not in body:
            e = key(lines, start, end, indent, "env")
            line = " " * (indent + 4) + "CUSTOM_CLIENT_SECRETS: ${{ secrets.CUSTOM_CLIENT_SECRETS }}"
            if e is None:
                r = key(lines, start, end, indent, "run")
                lines[r:r] = [" " * (indent + 2) + "env:", line]
            else:
                lines.insert(e + 1, line)
            end += 1
        if insert:
            lines[end:end] = [""] + insert
    return "\n".join(lines)


def main():
    cmd, path = sys.argv[1], sys.argv[2]
    text = open(path, encoding="utf-8").read()
    if cmd == "apply":
        text = apply(text)
        open(path, "w", encoding="utf-8").write(text)
    found = problems(text)
    for p in found:
        print(f"{path}: {p}")
    sys.exit(1 if found else 0)


if __name__ == "__main__":
    main()
