#!/bin/sh
# Praxec — LLM provider key setup (POSIX sh, optional, idempotent).
#
# Praxec's governed agents need an LLM provider credential. This is the shell
# twin of `px set-provider-keys` for boxes that have only the `praxec` gateway
# binary. It writes the key to the same file the engine reads
# (`~/.config/praxec/providers.env`, XDG-first — env vars still override it),
# with a 0600 file in a 0700 dir, and (with curl) validates the key against the
# provider's models endpoint before writing.
#
#   sh configure-providers.sh                    # interactive
#   sh configure-providers.sh --list             # show configured providers (masked)
#   OPENROUTER_API_KEY=sk-... sh configure-providers.sh --provider openrouter --from-env
#   printf '%s' "$KEY" | sh configure-providers.sh --provider anthropic --key-stdin
#
# Env: PRAXEC_PROVIDER_KEYS_FILE overrides the key-file path (mirrors the engine).
set -eu
umask 077

say()  { printf '\033[1;36m▸\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }

# Providers that take a single API key: "slug ENV_VAR". (bedrock is multi-var;
# ollama/llamacpp are keyless — out of scope for this quick setup.)
providers() {
  cat <<'EOF'
anthropic ANTHROPIC_API_KEY
openai OPENAI_API_KEY
gemini GEMINI_API_KEY
openrouter OPENROUTER_API_KEY
fireworks FIREWORKS_API_KEY
EOF
}
env_var_for() { providers | while read -r slug var; do [ "$slug" = "$1" ] && echo "$var"; done; }

# Key-file path — mirror the engine's resolution (provider_keys.rs): explicit
# override, else an existing XDG file, else an existing legacy file, else XDG.
resolve_keyfile() {
  if [ -n "${PRAXEC_PROVIDER_KEYS_FILE:-}" ]; then echo "$PRAXEC_PROVIDER_KEYS_FILE"; return; fi
  xdg="${XDG_CONFIG_HOME:-$HOME/.config}/praxec/providers.env"
  legacy="$HOME/.praxec/providers.env"
  if   [ -f "$xdg" ];    then echo "$xdg"
  elif [ -f "$legacy" ]; then echo "$legacy"
  else echo "$xdg"; fi
}
KEYFILE="$(resolve_keyfile)"

mask() { # first 7 + *** + last 4, like the engine's mask_value
  v="$1"; n=${#v}
  if [ "$n" -le 11 ]; then echo "***"; else
    printf '%s***%s\n' "$(printf '%s' "$v" | cut -c1-7)" "$(printf '%s' "$v" | cut -c$((n-3))-)"
  fi
}

# Is a provider already configured? (env var set, or a VAR= line in the file)
key_in_file() { [ -f "$KEYFILE" ] && grep -q "^$1=" "$KEYFILE"; }
value_in_env() { eval "v=\${$1:-}"; [ -n "$v" ]; }

list_configured() {
  # Collect into `out` (a pipe `while` runs in a subshell, so a `found=1` inside
  # it would not survive — capture the output instead).
  out="$(providers | while read -r slug var; do
    if value_in_env "$var"; then eval "v=\$$var"; printf '%s: %s (env)\n' "$slug" "$(mask "$v")"
    elif key_in_file "$var"; then v="$(grep "^$var=" "$KEYFILE" | head -1 | cut -d= -f2-)"; printf '%s: %s (%s)\n' "$slug" "$(mask "$v")" "$KEYFILE"; fi
  done)"
  if [ -n "$out" ]; then printf '%s\n' "$out" | while IFS= read -r l; do say "$l"; done
  else say "No provider keys configured yet."; fi
}

# Fetch tool for validation (curl gives us a status code; busybox wget can't
# portably, so with wget-only we skip the ping and advise `praxec doctor`).
CURL=0; command -v curl >/dev/null 2>&1 && CURL=1

validate() { # $1=slug $2=key -> 0 verified / 1 rejected / 2 unknown(warn)
  [ "$CURL" = 1 ] || return 2
  case "$1" in
    anthropic)  code=$(curl -s -o /dev/null -w '%{http_code}' -H "x-api-key: $2" -H "anthropic-version: 2023-06-01" https://api.anthropic.com/v1/models) ;;
    openai)     code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $2" https://api.openai.com/v1/models) ;;
    gemini)     code=$(curl -s -o /dev/null -w '%{http_code}' "https://generativelanguage.googleapis.com/v1beta/models?key=$2") ;;
    openrouter) code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $2" https://openrouter.ai/api/v1/models) ;;
    *) return 2 ;;  # fireworks/other: no probe wired
  esac
  case "$code" in
    2*) return 0 ;;
    401|403) return 1 ;;
    *) return 2 ;;  # timeout/5xx/DNS: warn, still write (never block on a flaky net)
  esac
}

# Atomic upsert of VAR=value into KEYFILE (0700 dir, 0600 file).
write_key() { # $1=VAR $2=value
  dir="$(dirname "$KEYFILE")"; mkdir -p "$dir"; chmod 700 "$dir" 2>/dev/null || true
  tmp="$dir/.providers.env.$$.tmp"; : > "$tmp"; chmod 600 "$tmp"
  [ -f "$KEYFILE" ] && grep -v "^$1=" "$KEYFILE" >> "$tmp" 2>/dev/null || true
  printf '%s=%s\n' "$1" "$2" >> "$tmp"
  mv -f "$tmp" "$KEYFILE"
}

set_provider() { # $1=slug $2=key
  var="$(env_var_for "$1")"; [ -n "$var" ] || die "unknown provider '$1' (try: $(providers | awk '{printf "%s ", $1}'))"
  [ -n "$2" ] || die "empty key for $1"
  case "$(validate "$1" "$2"; echo $?)" in
    0) say "Key verified against $1." ;;
    1) die "$1 rejected the key (401/403). Not saving." ;;
    2) warn "could not verify $1 (no curl / network / no probe) — saving anyway; check later with: praxec doctor --config <cfg>" ;;
  esac
  write_key "$var" "$2"
  say "Saved $var to $KEYFILE"
  say "Note: an exported \$$var in your shell overrides this file."
}

# ── arg parse ────────────────────────────────────────────────────────────────
PROVIDER=""; KEY_STDIN=0; FROM_ENV=0
while [ $# -gt 0 ]; do
  case "$1" in
    --list)      list_configured; exit 0 ;;
    --check-only) list_configured; exit 0 ;;
    --provider)  PROVIDER="${2:?--provider needs a slug}"; shift ;;
    --key-stdin) KEY_STDIN=1 ;;
    --from-env)  FROM_ENV=1 ;;
    --yes)       : ;;  # accepted for CI symmetry; no interactive prompts to suppress here
    -h|--help)   sed -n '2,20p' "$0"; exit 0 ;;
    *) die "unknown flag '$1' (see --help)" ;;
  esac
  shift
done

# Non-interactive paths.
if [ -n "$PROVIDER" ]; then
  var="$(env_var_for "$PROVIDER")"; [ -n "$var" ] || die "unknown provider '$PROVIDER'"
  if [ "$FROM_ENV" = 1 ]; then eval "k=\${$var:-}"; [ -n "$k" ] || die "$var is not set in the environment"; set_provider "$PROVIDER" "$k"; exit 0; fi
  if [ "$KEY_STDIN" = 1 ]; then IFS= read -r k || true; set_provider "$PROVIDER" "$k"; exit 0; fi
  die "with --provider, pass --key-stdin or --from-env (interactive prompt is the no-flag mode)"
fi

# Idempotency: if anything is already configured and this is an interactive run,
# report and confirm before adding another (never silently reconfigure).
any_configured() {
  providers | while read -r slug var; do
    if value_in_env "$var" || key_in_file "$var"; then echo yes; return; fi
  done
}
if [ -n "$(any_configured)" ] && [ -t 0 ]; then
  say "A provider key is already configured:"; list_configured
  printf 'Configure another? [y/N] '; read -r ans || ans=n
  case "$ans" in y|Y) : ;; *) exit 0 ;; esac
fi

# Interactive menu.
[ -t 0 ] || die "no TTY and no --provider — set a key non-interactively, e.g.:
  OPENROUTER_API_KEY=... sh $0 --provider openrouter --from-env
  or just: export OPENROUTER_API_KEY=... before starting praxec."
say "Which provider? (openrouter reaches many models with one key.)"
i=0; providers | while read -r slug var; do i=$((i+1)); printf '  %s) %s (%s)\n' "$i" "$slug" "$var"; done
printf 'Choice (slug or number): '; read -r pick || die "no input"
case "$pick" in
  ''|*[!0-9]*) sel="$pick" ;;  # a slug
  *) sel="$(providers | sed -n "${pick}p" | awk '{print $1}')" ;;  # a number
esac
[ -n "$sel" ] || die "invalid choice"
env_var_for "$sel" >/dev/null || die "unknown provider '$sel'"

printf 'Paste the API key for %s (input hidden): ' "$sel"
if command -v stty >/dev/null 2>&1; then
  stty -echo 2>/dev/null || true
  trap 'stty echo 2>/dev/null || true' EXIT INT TERM
  IFS= read -r key || true
  stty echo 2>/dev/null || true; trap - EXIT INT TERM; printf '\n'
else
  IFS= read -r key || true
fi
set_provider "$sel" "$key"
