#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
SOURCE_DIR="${REPO_ROOT}/skills/pleb"

if [[ ! -f "${SOURCE_DIR}/SKILL.md" ]]; then
    echo "error: canonical skill not found at ${SOURCE_DIR}/SKILL.md" >&2
    exit 1
fi

TARGETS=(
    "${HOME}/.agents/skills/pleb"
    "${HOME}/.gemini/config/skills/pleb"
    "${HOME}/.claude/skills/pleb"
    "${HOME}/.codex/skills/pleb"
    "${HOME}/.config/opencode/skills/pleb"
)

for dest in "${TARGETS[@]}"; do
    parent="$(dirname "${dest}")"
    mkdir -p "${parent}"
    if [[ -L "${dest}" ]]; then
        rm -f "${dest}"
    elif [[ -e "${dest}" ]]; then
        echo "error: refusing to overwrite non-symlink path ${dest}; remove or move it manually" >&2
        exit 1
    fi
    ln -s "${SOURCE_DIR}" "${dest}"
    echo "linked ${dest} -> ${SOURCE_DIR}"
done

# Retire legacy pre-rename symlinks so no dangling links remain.
LEGACY_SYMLINKS=(
    "${HOME}/.agents/skills/mesh-wick"
    "${HOME}/.gemini/config/skills/mesh-wick"
    "${HOME}/.claude/skills/mesh-wick"
    "${HOME}/.codex/skills/mesh-wick"
    "${HOME}/.config/opencode/skills/mesh-wick"
)

for legacy in "${LEGACY_SYMLINKS[@]}"; do
    if [[ -L "${legacy}" ]]; then
        rm -f "${legacy}"
        echo "removed legacy ${legacy}"
    fi
done

LEGACY_CLAUDE_SKILL="${HOME}/.claude/skills/wick"
if [[ -e "${LEGACY_CLAUDE_SKILL}" || -L "${LEGACY_CLAUDE_SKILL}" ]]; then
    rm -rf "${LEGACY_CLAUDE_SKILL}"
    echo "removed legacy ${LEGACY_CLAUDE_SKILL}"
fi
