#!/bin/sh
# Never trace secret expansion, including when invoked with sh -x.
set +x
set -eu
umask 077
LC_ALL=C
export LC_ALL

# Only these four well-known names support _FILE. Custom provider env references
# must be injected directly and separately authorized by
# GATEWAY_SECRET_ENV_ALLOWLIST. Never interpret arbitrary names or secret values
# as shell code. All diagnostics below contain fixed names/reasons, not values.
_secret_dir=''
cleanup() {
    if [ -n "$_secret_dir" ]; then
        rm -rf -- "$_secret_dir"
        _secret_dir=''
    fi
}
trap cleanup 0
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

fail() {
    printf 'container-entrypoint: %s: %s\n' "$1" "$2" >&2
    exit 1
}

# Arguments: fixed name, env presence, env value, file presence, file path.
import_secret() {
    if [ "$2" = x ] && [ "$4" = x ]; then
        fail "$1" 'set either the variable or its _FILE companion, not both'
    fi
    if [ "$2" != x ] && [ "$4" != x ]; then
        return
    fi
    _value=$3
    if [ "$4" = x ]; then
        [ -n "$5" ] && [ -f "$5" ] && [ -r "$5" ] \
            || fail "$1" 'secret file must be a readable regular file'
        if [ -z "$_secret_dir" ]; then
            _secret_dir=$(mktemp -d /tmp/gateway-secrets.XXXXXXXXXX) \
                || fail "$1" 'could not create private temporary storage'
        fi
        # Bound reads even if the file changes after validation. Check head's
        # status separately, without a pipeline that could mask a read error.
        head -c 4097 -- "$5" > "$_secret_dir/value" 2>/dev/null \
            || fail "$1" 'could not read secret file'
        # Translate NUL to CR so shells cannot silently discard invalid bytes.
        # A sentinel preserves trailing newlines through command substitution.
        _value=$(tr '\000' '\015' < "$_secret_dir/value" && printf '.') \
            || fail "$1" 'could not read secret file'
        _value=${_value%.}
        [ "${#_value}" -le 4096 ] || fail "$1" 'secret exceeds 4096 bytes'
        # Accept one conventional terminal LF, never embedded/repeated LF or CR.
        _value=${_value%"
"}
    fi
    [ -n "$_value" ] || fail "$1" 'secret must not be empty'
    [ "${#_value}" -le 4096 ] || fail "$1" 'secret exceeds 4096 bytes'
    case $_value in
        *"
"*|*"$(printf '\r')"*) fail "$1" 'secret must be a single line without CR or NUL bytes' ;;
    esac
    export "$1=$_value"
    unset "${1}_FILE"
    _value=''
}

import_secret DATABASE_URL "${DATABASE_URL+x}" "${DATABASE_URL-}" "${DATABASE_URL_FILE+x}" "${DATABASE_URL_FILE-}"
import_secret GATEWAY_OIDC_CLIENT_SECRET "${GATEWAY_OIDC_CLIENT_SECRET+x}" "${GATEWAY_OIDC_CLIENT_SECRET-}" "${GATEWAY_OIDC_CLIENT_SECRET_FILE+x}" "${GATEWAY_OIDC_CLIENT_SECRET_FILE-}"
import_secret OPENAI_API_KEY "${OPENAI_API_KEY+x}" "${OPENAI_API_KEY-}" "${OPENAI_API_KEY_FILE+x}" "${OPENAI_API_KEY_FILE-}"
import_secret ANTHROPIC_API_KEY "${ANTHROPIC_API_KEY+x}" "${ANTHROPIC_API_KEY-}" "${ANTHROPIC_API_KEY_FILE+x}" "${ANTHROPIC_API_KEY_FILE-}"

cleanup
trap - 0 HUP INT TERM
if [ "$#" -eq 0 ]; then
    set -- serve
fi
# No implicit migration or bootstrap. exec replaces this wrapper so SIGTERM
# reaches Rust directly (or through Compose's init). CLI arguments are intact.
exec open-model-gateway "$@"
