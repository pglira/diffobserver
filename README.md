# diffobserver

A terminal UI that snapshots a repository and **live-monitors the diff** between
your working tree and a chosen baseline. Built in Rust with
[ratatui](https://ratatui.rs).

```
┌ Changes (3) ─────────────┐┌ src/main.rs ─────────────────────────────┐
│▾ src/  +6 -1             ││@@ -1,3 +1,5 @@                            │
│  ▾ util/  +3 -0          ││   1    1   fn main() {                    │
│    A mod.rs  +3 -0       ││   2      -     println!("hello");         │
│  M main.rs  +3 -1        ││        2 +     println!("hello, world");  │
│M README.md  +3 -1        ││        3 +     let answer = 42;           │
│                          ││        4 +     println!("{}", answer);    │
│                          ││   3    5   }                              │
└──────────────────────────┘└───────────────────────────────────────────┘
 base: HEAD (091da26)  ·  3 files  +9 -2  ·  change 1/1   …  ? help · q quit
```

## What it does

- **Take snapshots** of the working tree (press `S`), stored in
  `.snapshots/<name>/` — the same layout as the `snap.sh` script, with unchanged
  files hard-linked to the previous snapshot so each snapshot only costs disk for
  what changed.
- **Monitor diffs live**: an inotify watcher recomputes the changed-file set and
  the open diff as you edit, off the UI thread.
- **Left pane**: a collapsible tree of changed files with `A`/`M`/`D` status
  glyphs and per-file / per-directory `+`/`-` line counts.
- **Right pane**: a unified diff (toggle to side-by-side with `w`) with syntax
  highlighting and word-level intra-line emphasis.
- **Jump around**: file→file and change→change, the latter crossing file
  boundaries so you can sweep every change in order.
- **Baselines**: diff against the latest snapshot, any saved snapshot, or git
  `HEAD` — switch live with `s`.

## Build

```sh
cargo build --release
# binary at target/release/diffobserver
```

Requires Rust 1.85+ (edition 2024). Linux/macOS only (uses Unix symlinks and
permissions).

## Usage

```sh
diffobserver [PATH]      # launch the TUI for PATH (default: current directory)
diffobserver --help
```

The repo root is discovered via git (like `git rev-parse --show-toplevel`);
outside a git repo, `PATH` is used as-is. On first launch with no snapshots, the
baseline defaults to git `HEAD`; press `S` to take a snapshot and the baseline
moves to it.

## Keys

| Key | Action |
| --- | --- |
| `j` / `k`, `↑` / `↓` | move selection (tree) / scroll (diff) |
| `Tab` | switch focus between tree and diff |
| `Enter` / `l` | open file / expand directory |
| `h` | collapse directory |
| `]` / `[` | next / previous file |
| `n` / `N` | next / previous change (crosses file boundaries) |
| `g` / `G` | top / bottom |
| `Ctrl-d` / `Ctrl-u` | half-page down / up in the diff |
| `PageUp` / `PageDown` | full page up / down in the diff |
| `Esc` | return focus to the tree (from the diff) |
| `S` | take a snapshot (prompts for a name) |
| `s` | switch baseline (snapshots / `HEAD`) |
| `e` | open `$EDITOR` at the current change |
| `r` | force a refresh |
| `w` | toggle unified / split view |
| `<` / `>` | narrow / widen the tree pane |
| `?` | help overlay |
| `q` / `Ctrl-c` | quit |

## Excludes

Files ignored by git (`.gitignore`, nested ignores, global excludes,
`.git/info/exclude`) are excluded from both snapshots and diffs — also outside
git repositories, where `.gitignore` files are still honored. `.git/` and
`.snapshots/` are always skipped, and the snapshot store writes a
`.snapshots/.gitignore` so it can never be committed accidentally. Dotfiles are
*not* hidden, so changes to e.g. `.gitignore` or `.github/` are visible.

Files tracked by **Git LFS** (anything matched by a `filter=lfs` rule in the
repo-root `.gitattributes`) are also excluded, from both snapshots and diffs.
HEAD stores only a small LFS *pointer* for these, so comparing it against the
real working-tree file would report a change on every LFS file — exactly what
`git status` does *not* show. Excluding them by the attribute rule (rather than
by detecting a committed pointer) also hides brand-new LFS files that have not
been committed yet. Nested per-directory `.gitattributes` are not consulted.

## Configuration (optional)

diffobserver runs with no configuration. To customize, create
`~/.config/diffobserver/config.toml`:

```toml
# syntect theme name; empty = chosen from `mode`
theme = ""
# "dark" or "light"
mode = "dark"
# files larger than this many bytes are listed but not content-diffed
size_cap_bytes = 2097152
# tree pane width as a percentage of the screen
tree_width_percent = 30
# editor command override (else $VISUAL / $EDITOR)
editor = ""
# syntax highlighting on/off
syntax_highlight = true
```

## Edge cases

- **Added** files show as all-insertions; **deleted** files as all-deletions.
- **Binary** files (NUL byte detected) are listed but show a placeholder instead
  of a content diff.
- **Large** files over the size cap are listed but not diffed; an unchanged one
  is not listed at all (sizes equal on both sides ⇒ treated as unchanged,
  since the content is never read).
- **Unreadable** files (permission denied) are listed with a placeholder; one
  bad file never breaks the rest of the scan.
- **Symlinks** are ignored entirely: not diffed and not snapshotted (unlike
  `snap.sh`'s rsync, which preserves them).
- Renames are not detected (they appear as an add + a delete), matching the
  `diff -rqN` semantics of `snap.sh`.
- Default snapshot names use local time (falling back to UTC if the `date`
  command is unavailable). The names `latest` and anything dot-prefixed are
  reserved.
- Saving a snapshot only advances the baseline when you are already tracking
  `latest`; an explicitly chosen baseline (HEAD or an older snapshot) is kept.
- When the baseline is git `HEAD`, it follows the repo: a new commit (or a
  checkout/reset that moves HEAD) re-resolves the baseline and updates the
  `HEAD (hash)` label automatically.

## Relationship to `snap.sh`

diffobserver reuses `snap.sh`'s on-disk snapshot layout (`.snapshots/<name>` +
a `latest` symlink, hard-linked dedup), so snapshots are interoperable. One
difference: diffobserver excludes files via `.gitignore`, whereas `snap.sh` uses
a fixed exclude list. A snapshot taken by `snap.sh` may therefore contain
gitignored files that diffobserver would report as deletions; for clean results,
take snapshots from within diffobserver (`S`).
