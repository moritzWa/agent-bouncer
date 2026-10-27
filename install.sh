#!/usr/bin/env bash
set -euo pipefail

repo="${AGENT_BOUNCER_REPO:-moritzWa/agent-bouncer}"
install_dir="${AGENT_BOUNCER_INSTALL_DIR:-${HOME}/.local/bin}"
version="${AGENT_BOUNCER_VERSION:-latest}"

fail() {
  printf 'agent-bouncer: error: %s\n' "$*" >&2
  exit 1
}

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  Darwin-arm64) target=aarch64-apple-darwin ;;
  *) fail "no prebuilt binary for $(uname -s) $(uname -m); try: cargo install --git https://github.com/${repo}" ;;
esac

if [[ "$version" == latest ]]; then
  tag="$(curl -fsSL "https://api.github.com/repos/${repo}/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)"
  [[ -n "$tag" ]] || fail "could not find the latest release"
else
  tag="v${version#v}"
fi

stem="agent-bouncer-${tag}-${target}"
base="https://github.com/${repo}/releases/download/${tag}"
temp_dir="$(mktemp -d)"
trap 'rm -rf "$temp_dir"' EXIT

curl -fsSL -o "${temp_dir}/${stem}.tar.gz" "${base}/${stem}.tar.gz"
curl -fsSL -o "${temp_dir}/${stem}.tar.gz.sha256" "${base}/${stem}.tar.gz.sha256"

expected="$(awk 'NR == 1 { print $1 }' "${temp_dir}/${stem}.tar.gz.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "${temp_dir}/${stem}.tar.gz" | awk '{ print $1 }')"
else
  actual="$(shasum -a 256 "${temp_dir}/${stem}.tar.gz" | awk '{ print $1 }')"
fi
[[ "$actual" == "$expected" ]] || fail "checksum verification failed"

tar -xzf "${temp_dir}/${stem}.tar.gz" -C "$temp_dir"
mkdir -p "$install_dir"
install -m 755 "${temp_dir}/${stem}/agent-bouncer" "${install_dir}/agent-bouncer"
printf 'agent-bouncer: installed %s to %s/agent-bouncer\n' "$tag" "$install_dir"
case ":${PATH}:" in
  *":${install_dir}:"*) ;;
  *) printf 'agent-bouncer: add %s to your PATH\n' "$install_dir" ;;
esac
