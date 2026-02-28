#!/usr/bin/env bash
set -euo pipefail

# Bump the Oxydra version across all crates, configs, and docs.
#
# Usage:
#   scripts/bump-version.sh <new-version>
#
# Examples:
#   scripts/bump-version.sh 0.2.0
#   scripts/bump-version.sh 1.0.0

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
  cat <<'EOF'
Usage: bump-version.sh <new-version>

The version should be in semver format without a leading "v" (e.g. 0.2.0).
EOF
  exit 1
}

[[ $# -eq 1 ]] || usage

NEW_VERSION="$1"

if ! [[ "$NEW_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  printf 'Error: version "%s" does not match semver format (X.Y.Z)\n' "$NEW_VERSION" >&2
  exit 1
fi

# Detect current version from the runner crate (source of truth)
CURRENT_VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO_ROOT/crates/runner/Cargo.toml" | head -1)"

if [[ -z "$CURRENT_VERSION" ]]; then
  printf 'Error: could not detect current version from crates/runner/Cargo.toml\n' >&2
  exit 1
fi

if [[ "$CURRENT_VERSION" == "$NEW_VERSION" ]]; then
  printf 'Already at version %s, nothing to do.\n' "$NEW_VERSION"
  exit 0
fi

printf 'Bumping version: %s → %s\n' "$CURRENT_VERSION" "$NEW_VERSION"

# 1) Crate Cargo.toml files
find "$REPO_ROOT/crates" -name Cargo.toml -print0 | while IFS= read -r -d '' f; do
  if grep -q "version = \"$CURRENT_VERSION\"" "$f"; then
    sed -i.bak "s/version = \"$CURRENT_VERSION\"/version = \"$NEW_VERSION\"/" "$f"
    rm -f "$f.bak"
    printf '  Updated %s\n' "${f#"$REPO_ROOT/"}"
  fi
done

# 2) Example config — docker image tags
RUNNER_CONFIG="$REPO_ROOT/examples/config/runner.toml"
if grep -q "$CURRENT_VERSION" "$RUNNER_CONFIG"; then
  sed -i.bak "s/$CURRENT_VERSION/$NEW_VERSION/g" "$RUNNER_CONFIG"
  rm -f "$RUNNER_CONFIG.bak"
  printf '  Updated %s\n' "examples/config/runner.toml"
fi

# 3) README.md — example tag
README="$REPO_ROOT/README.md"
if grep -q "v$CURRENT_VERSION" "$README"; then
  sed -i.bak "s/v$CURRENT_VERSION/v$NEW_VERSION/g" "$README"
  rm -f "$README.bak"
  printf '  Updated %s\n' "README.md"
fi

# 4) Regenerate Cargo.lock
printf '  Updating Cargo.lock...\n'
(cd "$REPO_ROOT" && cargo generate-lockfile --quiet 2>/dev/null || cargo check --quiet 2>/dev/null || true)

printf 'Done. Version is now %s\n' "$NEW_VERSION"
printf 'Next steps:\n'
printf '  1) Review changes: git diff\n'
printf '  2) Commit and tag:  git tag v%s\n' "$NEW_VERSION"
