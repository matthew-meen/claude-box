# Milestone 5 — Image Management

## Goal

Robust, user-friendly image lifecycle: pull OCI images with a progress bar,
import `.ipsw` files into named tart images, and cache base images so repeated
runs don't re-pull. A `claude-box images` subcommand lets users manage images.

---

## Design notes (from review)

- **Binary is now `claude-box`** (renamed in M2), so subcommands like
  `claude-box images list` are unambiguous — no conflict with claude args.
- **tart already caches pulled images** locally. Our cache layer adds value
  by tracking digest → local name mapping and supporting a `--pull` policy,
  but we should not duplicate what tart already does. The cache manifest is
  a thin metadata layer, not a second copy of the image.

---

## Tasks

### 5.1 — OCI pull with progress display

`tart pull` doesn't emit structured progress. Wrap it with an `indicatif`
spinner that shows elapsed time, then transitions to "done" with image name.

Files touched:
- `src/vm/image.rs` — `pull_oci` gains progress display
- `Cargo.toml` — add `indicatif`

### 5.2 — IPSW import

`tart create --from-ipsw <path> <name>` is slow (10–30 min). Improvements:

- Check if derived image name already exists in `tart list` — skip if present.
- Show progress spinner with elapsed time.
- Validate `.ipsw` path is readable and is a plausible macOS IPSW (zip
  structure check) before starting.

Files touched:
- `src/vm/image.rs` — `import_ipsw` with existence check + progress

### 5.3 — Local image cache

Thin metadata layer over tart's native cache:

- Manifest at `~/.cache/claude-box/images.json`:
  `image_ref → { local_name, pulled_at, digest }`.
- On `resolve_image`, check manifest. If `local_name` exists in `tart list`
  output, skip the pull.
- Add `--pull=always|missing|never` flag (default: `missing`).

This avoids the `tart pull` network roundtrip on repeated runs (tart pull
still checks the registry even for cached images).

Files touched:
- `src/vm/image_cache.rs` (new) — read/write cache manifest
- `src/vm/image.rs` — consult cache before pulling
- `src/main.rs` / `src/config.rs` — `--pull` flag

### 5.4 — `claude-box images` subcommand

```
claude-box images list              # show cached images + tart list
claude-box images pull <ref>        # pull without running
claude-box images rm <ref|name>     # remove from cache + tart delete
claude-box images prune             # remove all claude-box cached images
```

With the binary named `claude-box`, subcommand dispatch is straightforward:
use clap's `#[command(subcommand)]` with an optional enum. If `argv[1]`
matches a subcommand, handle it. Otherwise fall through to the existing
pass-through-to-claude behaviour.

Files touched:
- `src/main.rs` — subcommand dispatch
- `src/images.rs` (new) — `images` subcommand handlers

### 5.5 — Base image validation

Before cloning, optionally verify the image boots:

```bash
tart run --no-graphics <name> -- true
```

Quick smoke-boot that exits immediately. Guarded by `--validate-image` flag
(off by default — adds ~30 s to cold start).

Files touched:
- `src/vm/image.rs` — `validate_image(name)` step
- `src/sandbox.rs`, `src/main.rs`, `src/config.rs` — `--validate-image` flag

---

## New dependencies

```toml
indicatif = "0.17"           # progress bars / spinners
```

---

## Verification

```bash
# First pull — should show progress spinner
CLAUDE_BOX_IMAGE=ghcr.io/cirruslabs/macos-sequoia-base:latest \
  claude-box echo hello

# Second run — should skip pull (cache hit)
CLAUDE_BOX_IMAGE=ghcr.io/cirruslabs/macos-sequoia-base:latest \
  claude-box echo hello
# logs should show "using cached image"

# Image management
claude-box images list
claude-box images pull ghcr.io/cirruslabs/macos-sequoia-base:latest
claude-box images prune
```
