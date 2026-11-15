#!/usr/bin/env bash
#
# Drive traffic through a running Mira proxy.
#
# The unit tests cover framing and protocol handling against in-memory
# peers. This exercises the parts they cannot: real TLS, real ALPN, real
# origins, and response shapes that only show up on the wire. Run it with
# the TUI open and watch what lands there — a passing run means the
# request completed, not that Mira captured it sensibly.
#
# Usage:
#   ./test.sh                 # every group
#   ./test.sh http2 bodies    # only the named groups
#   ./test.sh --list          # show the groups
#
# Environment:
#   PROXY=127.0.0.1:8888      # where Mira is listening
#   ECHO=https://httpbingo.org # origin used for most checks; point this at
#                             # a local httpbin to run without internet:
#                             #   docker run -p 8080:80 mccutchen/go-httpbin
#                             #   ECHO=http://127.0.0.1:8080 ./test.sh
#   CA=/path/to/ca-cert.pem   # override the CA Mira signs with
#   VERBOSE=1                 # show each curl command and its output

set -uo pipefail

PROXY="${PROXY:-127.0.0.1:8888}"
ECHO="${ECHO:-https://httpbingo.org}"
VERBOSE="${VERBOSE:-}"

if [[ -t 1 ]]; then
    BOLD=$'\033[1m'; RED=$'\033[31m'; GREEN=$'\033[32m'
    YELLOW=$'\033[33m'; DIM=$'\033[2m'; RESET=$'\033[0m'
else
    BOLD=''; RED=''; GREEN=''; YELLOW=''; DIM=''; RESET=''
fi

PASSED=0
FAILED=0
SKIPPED=0
FAILURES=()

TMPDIR_RUN="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_RUN"' EXIT

# ---------------------------------------------------------------------------
# Harness
# ---------------------------------------------------------------------------

group() {
    printf '\n%s== %s ==%s\n' "$BOLD" "$1" "$RESET"
}

skip() {
    SKIPPED=$((SKIPPED + 1))
    printf '  %sSKIP%s %-52s %s\n' "$YELLOW" "$RESET" "$1" "${DIM}$2${RESET}"
}

# check <name> <expected-status> <curl args...>
#
# Runs curl through the proxy and compares the response status. Several
# URLs may be given, in which case *every* response has to match — curl
# reports one status per URL, and accepting the concatenation would let a
# failure hide among its neighbours. An expected status of "-" accepts
# whatever completed, for the few cases where the point is that the
# transfer worked at all.
check() {
    local name="$1" expect="$2"; shift 2

    local out="$TMPDIR_RUN/out" err="$TMPDIR_RUN/err"
    local rc raw
    local -a ca=()
    mapfile -t ca < <(ca_args "$@")
    raw="$(curl --silent --show-error --proxy "$PROXY" \
        --max-time 30 --write-out '%{http_code}\n' \
        --output "$out" "${ca[@]}" "$@" 2>"$err")"
    rc=$?

    if [[ -n "$VERBOSE" ]]; then
        printf '  %s$ curl --proxy %s %s%s\n' "$DIM" "$PROXY" "$*" "$RESET"
        [[ -s "$err" ]] && sed 's/^/    /' "$err"
        head -c 200 "$out" | sed 's/^/    /'
        printf '\n'
    fi

    if (( rc != 0 )); then
        FAILED=$((FAILED + 1))
        FAILURES+=("$name: curl exit $rc — $(tr -d '\n' <"$err" | head -c 120)")
        printf '  %sFAIL%s %-52s %s\n' "$RED" "$RESET" "$name" "${DIM}curl exit $rc${RESET}"
        return 1
    fi

    local -a codes=()
    mapfile -t codes <<<"$raw"

    local summary code all_same=1
    for code in "${codes[@]}"; do
        [[ "$code" == "${codes[0]}" ]] || all_same=0
    done
    if (( ${#codes[@]} > 1 )) && (( all_same )); then
        summary="${#codes[@]}×${codes[0]}"
    else
        summary="${codes[*]}"
    fi

    if [[ "$expect" != "-" ]]; then
        for code in "${codes[@]}"; do
            if [[ "$code" != "$expect" ]]; then
                FAILED=$((FAILED + 1))
                FAILURES+=("$name: expected HTTP $expect, got $summary")
                printf '  %sFAIL%s %-52s %s\n' "$RED" "$RESET" "$name" \
                    "${DIM}got $summary, want $expect${RESET}"
                return 1
            fi
        done
    fi

    PASSED=$((PASSED + 1))
    printf '  %sok%s   %-52s %s\n' "$GREEN" "$RESET" "$name" "${DIM}$summary${RESET}"
    return 0
}

# Like check, but also requires the response body to contain a string.
check_body() {
    local name="$1" expect="$2" needle="$3"; shift 3

    if check "$name" "$expect" "$@"; then
        if ! grep -qF -- "$needle" "$TMPDIR_RUN/out"; then
            PASSED=$((PASSED - 1))
            FAILED=$((FAILED + 1))
            FAILURES+=("$name: response did not contain '$needle'")
            printf '  %s  ^ body missing %q%s\n' "$RED" "$needle" "$RESET"
        fi
    fi
}

curl_supports() {
    curl --help all 2>/dev/null | grep -q -- "$1"
}

# Mira signs intercepted certificates itself, so any HTTPS target needs
# its CA. Detected from the arguments so no caller has to remember.
ca_args() {
    [[ -n "$CA_FILE" ]] || return 0
    local arg
    for arg in "$@"; do
        if [[ "$arg" == https://* || "$arg" == wss://* ]]; then
            printf -- '--cacert\n%s\n' "$CA_FILE"
            return 0
        fi
    done
}

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

require_proxy() {
    if ! curl --silent --max-time 5 --proxy "$PROXY" -o /dev/null http://mira.proxy/ 2>/dev/null; then
        printf '%sMira is not answering on %s%s\n\n' "$RED" "$PROXY" "$RESET"
        printf 'Start it in another terminal first — it takes over this one with its TUI:\n'
        printf '  %scargo run --release -p mira-cli -- --intercept%s\n\n' "$BOLD" "$RESET"
        printf 'Then re-run this script. Override the address with PROXY=host:port.\n'
        exit 1
    fi
}

# Mira signs intercepted certificates with its own CA, so curl needs it.
# Fetching through the proxy exercises the magic domain and avoids
# assuming where the CA lives on disk.
setup_ca() {
    CA_FILE="${CA:-}"
    if [[ -n "$CA_FILE" ]]; then
        return
    fi

    CA_FILE="$TMPDIR_RUN/mira-ca.pem"
    if curl --silent --max-time 5 --proxy "$PROXY" -o "$CA_FILE" http://mira.proxy/ca.pem \
        && [[ -s "$CA_FILE" ]] && grep -q "BEGIN CERTIFICATE" "$CA_FILE"; then
        return
    fi

    if [[ -f "$HOME/.mira/ca-cert.pem" ]]; then
        CA_FILE="$HOME/.mira/ca-cert.pem"
        return
    fi

    CA_FILE=''
}

# HTTPS through Mira only works if interception is on and we trust the CA.
# Everything HTTPS is skipped rather than failed when it is not.
intercept_ready() {
    [[ -n "$CA_FILE" ]] || return 1
    curl --silent --max-time 10 --proxy "$PROXY" --cacert "$CA_FILE" \
        -o /dev/null "$ECHO/status/200" 2>/dev/null
}

# ---------------------------------------------------------------------------
# Groups
# ---------------------------------------------------------------------------

group_magic() {
    group "Mira's own endpoints"
    check "index page over plain HTTP"        200 http://mira.proxy/
    check_body "CA in PEM form"               200 "BEGIN CERTIFICATE" http://mira.proxy/ca.pem
    check "CA in DER form"                    200 http://mira.proxy/ca.crt
    check "unknown path falls back to index"  200 http://mira.proxy/nonsense
}

group_plain() {
    group "Plain HTTP (absolute-form through the proxy)"
    check "GET"                       200 "$PLAIN/get"
    check "404"                       404 "$PLAIN/status/404"
    check "500"                       500 "$PLAIN/status/500"
    check "keep-alive reuse (3 on one connection)" 200 \
        -o /dev/null -o /dev/null \
        "$PLAIN/get" "$PLAIN/get" "$PLAIN/get"
}

group_methods() {
    group "Methods"
    check "HEAD (no body by definition)"  200 --head "$BASE/get"
    check "POST form"                     200 --data 'a=1&b=2' "$BASE/post"
    check "POST JSON"                     200 -H 'Content-Type: application/json' \
        --data '{"hello":"world"}' "$BASE/post"
    check "PUT"                           200 -X PUT --data 'payload' "$BASE/put"
    check "DELETE"                        200 -X DELETE "$BASE/delete"
    check "PATCH"                         200 -X PATCH --data 'p' "$BASE/patch"
    check "OPTIONS"                       200 -X OPTIONS "$BASE/get"
}

group_status() {
    group "Status codes"
    check "204 No Content (no body)"      204 "$BASE/status/204"
    check "301 (not followed)"            301 "$BASE/status/301"
    check "302 followed to 200"           200 --location "$BASE/redirect/1"
    check "redirect chain of 3"           200 --location "$BASE/redirect/3"
    check "304 Not Modified"              304 "$BASE/status/304"
    check "418"                           418 "$BASE/status/418"
    check "401 with auth challenge"       401 "$BASE/basic-auth/user/pass"
    check "401 then 200 with credentials" 200 --user user:pass "$BASE/basic-auth/user/pass"
}

group_bodies() {
    group "Response body shapes"
    check "Content-Length framed"     200 "$BASE/bytes/1024"
    check "chunked (no Content-Length)" 200 "$BASE/stream/5"
    check "empty body"                200 "$BASE/bytes/0"
    check "gzip"                      200 --compressed "$BASE/gzip"
    # Pinned to HTTP/1.1: curl 8.7.1 (macOS) fails to decode deflate over
    # HTTP/2 with exit 23, even talking to the origin directly. The body is
    # valid zlib either way.
    check "deflate"                   200 --http1.1 --compressed "$BASE/deflate"
    check "brotli (if the origin offers it)" - --compressed "$BASE/brotli"
    check "binary (PNG)"              200 "$BASE/image/png"
    check "512 KB download"           200 "$BASE/bytes/524288"
    check "UTF-8 payload"             200 "$BASE/encoding/utf8"
    check "slow drip (streaming)"     200 "$BASE/drip?duration=2&numbytes=64&delay=0"
}

group_requests() {
    group "Request body shapes"
    check "Content-Length request body" 200 --data-binary @"$TMPDIR_RUN/payload" "$BASE/post"
    check "chunked request body"        200 -H 'Transfer-Encoding: chunked' \
        --data-binary @"$TMPDIR_RUN/payload" "$BASE/post"
    check "large request body (256 KB)" 200 --data-binary @"$TMPDIR_RUN/big" "$BASE/post"
    check "many headers"                200 \
        -H 'X-One: 1' -H 'X-Two: 2' -H 'X-Three: 3' -H 'X-Four: 4' -H 'X-Five: 5' \
        -H 'X-Six: 6' -H 'X-Seven: 7' -H 'X-Eight: 8' "$BASE/headers"
    check_body "cookies round-trip"     200 "mira" --cookie 'flavour=mira' "$BASE/cookies"
}

group_https() {
    group "HTTPS interception"
    if ! intercept_ready; then
        skip "all HTTPS checks" "interception off, or CA not trusted"
        return
    fi
    check "TLS GET"                   200 "$SECURE/get"
    check "TLS POST"                  200 --data 'x=1' "$SECURE/post"
    check "TLS keep-alive reuse"      200 -o /dev/null "$SECURE/get" "$SECURE/get"
    check "TLS 512 KB download"       200 "$SECURE/bytes/524288"
    check "TLS chunked"               200 "$SECURE/stream/5"
}

group_http2() {
    group "HTTP/2"
    if ! curl_supports '--http2'; then
        skip "all HTTP/2 checks" "this curl has no HTTP/2 support"
        return
    fi
    if ! intercept_ready; then
        skip "all HTTP/2 checks" "HTTP/2 only applies inside an intercepted session"
        return
    fi

    # curl reports the version it actually used, which is the point: if
    # ALPN did not negotiate h2 these pass as HTTP/1.1 without saying so.
    local version
    version="$(curl --silent --output /dev/null --proxy "$PROXY" --cacert "$CA_FILE" \
        --http2 --write-out '%{http_version}' --max-time 30 "$SECURE/get" 2>/dev/null)"
    if [[ "$version" == "2" ]]; then
        printf '  %sok%s   %-52s %s\n' "$GREEN" "$RESET" "negotiated HTTP/2 end to end" "${DIM}$version${RESET}"
        PASSED=$((PASSED + 1))
    else
        printf '  %sFAIL%s %-52s %s\n' "$RED" "$RESET" "negotiated HTTP/2 end to end" \
            "${DIM}got HTTP/$version${RESET}"
        FAILED=$((FAILED + 1))
        FAILURES+=("HTTP/2 negotiation: client used HTTP/$version, not 2")
    fi

    check "h2 GET"                    200 --http2 "$SECURE/get"
    check "h2 POST"                   200 --http2 --data 'x=1' "$SECURE/post"
    check "h2 chunked response"       200 --http2 "$SECURE/stream/10"
    check "h2 512 KB download"        200 --http2 "$SECURE/bytes/524288"
    check "h2 204 (no body)"          204 --http2 "$SECURE/status/204"
    check "h2 redirect chain"         200 --http2 --location "$SECURE/redirect/3"
    if curl_supports '--parallel'; then
        check "h2 concurrent streams"  200 --http2 --parallel \
            -o /dev/null -o /dev/null -o /dev/null \
            "$SECURE/get" "$SECURE/bytes/4096" "$SECURE/stream/3" "$SECURE/headers"
    else
        skip "h2 concurrent streams" "this curl has no --parallel"
    fi
    check "forced HTTP/1.1 still works" 200 --http1.1 "$SECURE/get"
}

group_limits() {
    group "Limits and error paths"

    # Mira answers an over-long header block with 431 rather than
    # buffering it for the whole read timeout.
    local big_header
    big_header="$(head -c 70000 /dev/zero | tr '\0' 'a')"
    check "oversized headers get 431" 431 -H "X-Enormous: $big_header" "$PLAIN/get"

    check "slow response inside timeout"  200 "$PLAIN/delay/2"

    # An origin Mira cannot reach is answered with a 502 that carries the
    # reason, rather than a bare close.
    check "unresolvable host gets 502"       502 http://this-host-does-not-exist.invalid/
    check "connection refused gets 502"      502 http://127.0.0.1:9/
    check "conflicting Content-Length gets 400" 400 \
        -H 'Content-Length: 1' -H 'Content-Length: 2' --data 'x' "$PLAIN/post"
}

group_websocket() {
    group "WebSocket"

    # An upgrade the origin declines is an ordinary HTTP response, and
    # has to come back whole — headers *and* body. Mira takes its
    # WebSocket path to get here, so this covers the handshake plumbing
    # without needing a WebSocket server.
    check_body "declined upgrade is relayed in full" 200 '"Upgrade"' \
        -H 'Connection: Upgrade' -H 'Upgrade: websocket' \
        -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' \
        -H 'Sec-WebSocket-Version: 13' \
        "$PLAIN/headers"

    # A real ws:// endpoint is left to manual testing: curl holds the
    # connection open after the handshake, so any timeout here would be
    # measuring the echo service rather than Mira.
    skip "ws:// data flow" "needs a live WebSocket server; test by hand"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

ALL_GROUPS=(magic plain methods status bodies requests https http2 limits websocket)

if [[ "${1:-}" == "--list" ]]; then
    printf 'Groups: %s\n' "${ALL_GROUPS[*]}"
    exit 0
fi

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
    sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
fi

command -v curl >/dev/null || { printf 'curl is required\n'; exit 1; }

require_proxy
setup_ca

# Most checks run over whichever scheme ECHO names; a few need one
# specifically, so both forms are derived from it.
BASE="$ECHO"
PLAIN="${ECHO/https:/http:}"
SECURE="${ECHO/http:/https:}"

# Fixtures for request-body checks.
head -c 512 /dev/urandom | base64 >"$TMPDIR_RUN/payload"
head -c 262144 /dev/urandom | base64 >"$TMPDIR_RUN/big"

printf '%sMira smoke test%s\n' "$BOLD" "$RESET"
printf '  proxy   %s\n' "$PROXY"
printf '  origin  %s\n' "$ECHO"
printf '  CA      %s\n' "${CA_FILE:-<none — HTTPS checks will be skipped>}"

# Not named GROUPS: bash keeps that one for the current user's group IDs
# and silently ignores assignments to it.
SELECTED=("$@")
if (( ${#SELECTED[@]} == 0 )); then
    SELECTED=("${ALL_GROUPS[@]}")
fi

for name in "${SELECTED[@]}"; do
    if [[ " ${ALL_GROUPS[*]} " != *" $name "* ]]; then
        printf '%sUnknown group: %s%s (try --list)\n' "$RED" "$name" "$RESET"
        exit 1
    fi
    "group_$name"
done

printf '\n%s%d passed%s' "$GREEN" "$PASSED" "$RESET"
(( FAILED > 0 )) && printf ', %s%d failed%s' "$RED" "$FAILED" "$RESET"
(( SKIPPED > 0 )) && printf ', %s%d skipped%s' "$YELLOW" "$SKIPPED" "$RESET"
printf '\n'

if (( FAILED > 0 )); then
    printf '\n%sFailures:%s\n' "$BOLD" "$RESET"
    for failure in "${FAILURES[@]}"; do
        printf '  - %s\n' "$failure"
    done
    exit 1
fi
