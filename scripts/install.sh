#!/bin/sh
# llm-hub installer. Installs the binary, a persistent background service, and optionally GitHub Copilot.
#
#   sh scripts/install.sh                 binary + settings (.env) + background service
#   sh scripts/install.sh --copilot       the above + GitHub Copilot setup (gh, .env, gh login, check)
#   curl -fsSL https://raw.githubusercontent.com/barnuri/llm-hub/master/scripts/install.sh | sh -s -- --copilot [flags]
#
# Safe to run again: every step checks what is already done and does only the missing part.
#
# Flags:
#   --copilot           also set up GitHub Copilot
#   --dir PATH          settings folder with .env (default: ~/llm-hub)
#   --profile NAME      Copilot profile name (default: copilot)
#   --auth-flow VALUE   github-copilot (default) or github-copilot-vscode
#   --device-login      sign in with llm-hub's device code instead of gh
#   --skip-gh-login     do not run gh auth login (fail when gh is not logged in)
#   --no-service        do not install llm-hub as a background service (default: installed)
#   --force-env         replace the managed .env values with the defaults
#   --skip-install      do not install gh or llm-hub (the binary must exist already)
#   -h, --help          show this help
#
# Install folder: LLM_HUB_INSTALL_DIR (default /usr/local/bin).

set -eu

REPO="barnuri/llm-hub"
INSTALL_DIR="${LLM_HUB_INSTALL_DIR:-/usr/local/bin}"
COPILOT=0
DIR="$HOME/llm-hub"
PROFILE="copilot"
AUTH_FLOW="github-copilot"
DEVICE_LOGIN=0
SKIP_GH_LOGIN=0
SKIP_INSTALL=0
SERVICE=1
FORCE_ENV=0

step() { printf '\n== %s\n' "$*"; }
ok() { printf '  ok: %s\n' "$*"; }
todo() { printf '  do: %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

usage() {
  sed -n '2,22p' "$0"
  exit "${1:-0}"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --copilot) COPILOT=1; shift ;;
    --dir) [ $# -ge 2 ] || die "--dir needs a path"; DIR="$2"; shift 2 ;;
    --profile) [ $# -ge 2 ] || die "--profile needs a name"; PROFILE="$2"; shift 2 ;;
    --auth-flow) [ $# -ge 2 ] || die "--auth-flow needs a value"; AUTH_FLOW="$2"; shift 2 ;;
    --device-login) DEVICE_LOGIN=1; shift ;;
    --skip-gh-login) SKIP_GH_LOGIN=1; shift ;;
    --no-service) SERVICE=0; shift ;;
    --force-env) FORCE_ENV=1; shift ;;
    --skip-install) SKIP_INSTALL=1; shift ;;
    -h|--help) usage 0 ;;
    *) die "unknown flag: $1 (see --help)" ;;
  esac
done

# ---------------------------------------------------------------- binary
install_binary() {
  os=$(uname -s | tr '[:upper:]' '[:lower:]')
  case "$os" in
    darwin) os="macos" ;;
    linux) os="linux" ;;
    *) die "unsupported OS: $os" ;;
  esac
  arch=$(uname -m)
  case "$arch" in
    arm64|aarch64) arch="aarch64" ;;
    x86_64|amd64) arch="x86_64" ;;
    *) die "unsupported arch: $arch" ;;
  esac
  url="https://github.com/${REPO}/releases/latest/download/llm-hub-${os}-${arch}"
  echo "downloading $url"
  tmp=$(mktemp)
  curl -fsSL "$url" -o "$tmp"
  chmod +x "$tmp"
  if [ -w "$INSTALL_DIR" ]; then
    mv "$tmp" "$INSTALL_DIR/llm-hub"
  else
    echo "writing to $INSTALL_DIR needs sudo"
    sudo mv "$tmp" "$INSTALL_DIR/llm-hub"
  fi
  echo "installed: $("$INSTALL_DIR/llm-hub" --version 2>/dev/null || echo "$INSTALL_DIR/llm-hub")"
}

step "llm-hub binary"
if [ "$SKIP_INSTALL" -eq 1 ]; then
  command -v llm-hub >/dev/null 2>&1 || die "llm-hub is missing and --skip-install is set"
  ok "llm-hub at $(command -v llm-hub) (skipped update)"
else
  # Always the newest published release: an existing install is replaced.
  install_binary
  command -v llm-hub >/dev/null 2>&1 || die "llm-hub not on PATH. Add $INSTALL_DIR to PATH and rerun"
fi

# ---------------------------------------------------------------- settings
step "settings folder and .env"
mkdir -p "$DIR"
ENV_FILE="$DIR/.env"
[ -f "$ENV_FILE" ] || : > "$ENV_FILE"

# Sets KEY=VALUE. Keeps an existing value unless --force-env.
set_key() {
  key="$1"; value="$2"
  if grep -q "^${key}=" "$ENV_FILE"; then
    if [ "$FORCE_ENV" -eq 1 ]; then
      tmp="$ENV_FILE.tmp"
      grep -v "^${key}=" "$ENV_FILE" > "$tmp" || true
      printf '%s=%s\n' "$key" "$value" >> "$tmp"
      mv "$tmp" "$ENV_FILE"
      ok "$key replaced"
    else
      ok "$key already set (use --force-env to replace)"
    fi
  else
    printf '%s=%s\n' "$key" "$value" >> "$ENV_FILE"
    todo "added $key"
  fi
}

set_key "LLM_HUB_PERSISTENT" "true"
ok ".env at $ENV_FILE"

# ---------------------------------------------------------------- copilot
copilot_setup() {
  case "$AUTH_FLOW" in
    github-copilot|github-copilot-vscode) ;;
    *) die "--auth-flow must be github-copilot or github-copilot-vscode" ;;
  esac
  case "$PROFILE" in
    ""|*[!a-z0-9-]*) die "--profile may contain only a-z, 0-9 and '-'" ;;
  esac
  UPPER=$(printf '%s' "$PROFILE" | tr 'a-z-' 'A-Z_')
  PREFIX="LLM_HUB_${UPPER}"

  step "Copilot 1/4 GitHub CLI (gh)"
  if command -v gh >/dev/null 2>&1; then
    ok "gh at $(command -v gh)"
  elif [ "$SKIP_INSTALL" -eq 1 ]; then
    die "gh is missing and --skip-install is set"
  else
    todo "install gh"
    os=$(uname -s)
    if [ "$os" = "Darwin" ] && command -v brew >/dev/null 2>&1; then
      brew install gh
    elif [ "$os" = "Linux" ] && command -v apt-get >/dev/null 2>&1; then
      sudo apt-get update && sudo apt-get install -y gh || die "apt could not install gh. Install it from https://cli.github.com and rerun"
    elif [ "$os" = "Linux" ] && command -v dnf >/dev/null 2>&1; then
      sudo dnf install -y gh || die "dnf could not install gh. Install it from https://cli.github.com and rerun"
    else
      die "no supported package manager (brew, apt-get, dnf). Install gh from https://cli.github.com and rerun"
    fi
    command -v gh >/dev/null 2>&1 || die "gh still not on PATH after install"
    ok "gh installed at $(command -v gh)"
  fi
  GH=$(command -v gh)

  step "Copilot 2/4 profile settings"
  set_key "${PREFIX}_BASE_URL" "https://api.githubcopilot.com"
  set_key "${PREFIX}_DISPLAY_NAME" "GitHub Copilot"
  set_key "${PREFIX}_AUTH" "$AUTH_FLOW"
  set_key "${PREFIX}_GH_PATH" "$GH"
  if [ "$DEVICE_LOGIN" -eq 1 ]; then
    set_key "${PREFIX}_GITHUB_TOKEN_SOURCE" "login"
  else
    set_key "${PREFIX}_GITHUB_TOKEN_SOURCE" "auto"
  fi

  step "Copilot 3/4 GitHub login"
  if [ "$DEVICE_LOGIN" -eq 1 ]; then
    if (cd "$DIR" && llm-hub copilot login --profile "$PROFILE"); then
      ok "device login stored for profile $PROFILE"
    else
      die "device login failed. Rerun to retry, or drop --device-login to use gh"
    fi
  elif "$GH" auth status -h github.com >/dev/null 2>&1; then
    ok "gh is logged in to github.com"
  elif [ "$SKIP_GH_LOGIN" -eq 1 ]; then
    die "gh is not logged in and --skip-gh-login is set. Run: gh auth login -h github.com -s read:user -p https -w"
  elif [ -r /dev/tty ] && [ -w /dev/tty ]; then
    todo "gh auth login (browser code, scope read:user)"
    "$GH" auth login -h github.com -s read:user -p https -w </dev/tty >/dev/tty 2>&1 \
      || die "gh auth login failed or was cancelled. Rerun to retry"
  else
    die "gh is not logged in and no terminal is attached. Run in a terminal: gh auth login -h github.com -s read:user -p https -w"
  fi

  step "Copilot 4/4 GitHub token check"
  if [ "$DEVICE_LOGIN" -eq 0 ]; then
    TOKEN=$("$GH" auth token -h github.com 2>/dev/null || true)
    [ -n "$TOKEN" ] || die "gh gives no token. Run: gh auth login -h github.com -s read:user -p https -w"
    login=$(curl -fsS -m 15 -H "Authorization: Bearer $TOKEN" -H "Accept: application/vnd.github+json" \
      https://api.github.com/user 2>/dev/null | sed -n 's/.*"login": *"\([^"]*\)".*/\1/p' | head -n1 || true)
    [ -n "$login" ] || die "GitHub rejected the gh token (api.github.com/user). Run: gh auth login -h github.com again"
    ok "GitHub user: $login"
  else
    ok "device login mode: token checked when llm-hub starts"
  fi
}

if [ "$COPILOT" -eq 1 ]; then
  copilot_setup
fi

# ---------------------------------------------------------------- service
step "background service"
if [ "$SERVICE" -eq 1 ]; then
  (cd "$DIR" && llm-hub service install) || die "llm-hub service install failed. Rerun, or use --no-service"
  ok "background service installed and running"
else
  ok "skipped (--no-service)"
fi

printf '\nDone.\n'
if [ "$COPILOT" -eq 1 ]; then
  printf '  Start Claude Code on Copilot: cd %s && llm-hub claude %s/auto\n' "$DIR" "$PROFILE"
fi
printf '  Check the service: llm-hub service status\n'
