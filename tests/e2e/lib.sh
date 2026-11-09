# Shared helpers for the end-to-end scripts. Sourced, not executed.
# Expects: here, net, pbs, client to be set by the caller.

pass=0
fail=0

ok()   { echo "  ok   $*"; pass=$((pass + 1)); }
bad()  { echo "  FAIL $*"; fail=$((fail + 1)); }
check() { local what="$1"; shift; if "$@"; then ok "$what"; else bad "$what"; fi; }
on_pbs()    { docker exec "$pbs" "$@"; }
on_client() { docker exec "$client" "$@"; }

# Negative checks must fail for the expected reason only: an unrelated
# failure (typo, wrong path, server down) must not count as a pass.
# The helpers print what they got when they do not pass.
show() { tail -n 5 <<<"$1" | sed 's/^/      | /'; }

# fails_with CODE PATTERN CMD...: CMD exits with CODE and its combined
# stdout/stderr matches the extended regular expression PATTERN.
fails_with() {
  local want="$1" pattern="$2" out code=0
  shift 2
  out="$("$@" 2>&1)" || code=$?
  if [[ $code -ne $want ]]; then
    echo "    expected exit code $want, got $code:"; show "$out"; return 1
  fi
  if ! grep -Eq -- "$pattern" <<<"$out"; then
    echo "    output does not match /$pattern/:"; show "$out"; return 1
  fi
}

# exits_with CODE CMD...: CMD exits with exactly CODE, e.g. 1 for "grep
# found nothing" (2 would mean a missing file).
exits_with() {
  local want="$1" code=0
  shift
  "$@" >/dev/null 2>&1 || code=$?
  [[ $code -eq $want ]] || { echo "    expected exit code $want, got $code"; return 1; }
}

# no_output CMD...: CMD succeeds and prints nothing on stdout.
no_output() {
  local out
  out="$("$@")" || { echo "    command failed: $*"; return 1; }
  [[ -z "$out" ]] || { echo "    unexpected output:"; show "$out"; return 1; }
}

# absent_in DIR PATH: the client directory DIR exists and PATH below it does
# not, so a wrong DIR cannot make the check pass.
absent_in() {
  on_client test -d "$1" || { echo "    $1 is not a directory"; return 1; }
  exits_with 1 on_client test -e "$1/$2"
}

build_pbs_image() {
  DOCKER_BUILDKIT=0 docker build -q -t nestegg-e2e-pbs -f "$here/pbs.Dockerfile" "$here" >/dev/null
}

# Start a disposable PBS and prepare datastore "store1", namespace "desktops",
# user backup@pbs with token "desktop" (DatastoreBackup on the namespace).
# Sets: fp, admin_pw, token_secret.
start_pbs() {
  echo "== starting PBS"
  docker network create "$net" >/dev/null
  local attempt up=""
  for attempt in 1 2 3; do
    # Short fixed host name: the PBS certificate CN is built from host name
    # and DNS search domain and must not exceed 64 characters.
    docker run -d --tmpfs /run --name "$pbs" --hostname pbs --dns-search e2e.test \
      --network "$net" --network-alias pbs.e2e.test nestegg-e2e-pbs >/dev/null
    for _ in $(seq 1 60); do
      if on_pbs curl -skf https://127.0.0.1:8007/api2/json/ping >/dev/null 2>&1; then up=1; break; fi
      [[ "$(docker inspect -f '{{.State.Running}}' "$pbs" 2>/dev/null)" == true ]] || break
      sleep 1
    done
    [[ -n "$up" ]] && break
    echo "PBS did not come up (attempt $attempt); container log:"
    docker logs "$pbs" 2>&1 | tail -20 | sed 's/^/    /'
    docker rm -f "$pbs" >/dev/null 2>&1 || true
  done
  [[ -n "$up" ]] || { echo "PBS failed to start"; exit 1; }
  on_pbs proxmox-backup-manager versions | head -1

  echo "== preparing PBS: datastore, namespace, user, token, ACL"
  admin_pw="$(head -c 24 /dev/urandom | base64 | tr -d '/+=')"
  on_pbs mkdir -p /datastore
  on_pbs proxmox-backup-manager datastore create store1 /datastore >/dev/null
  on_pbs proxmox-backup-manager user create admin@pbs --password "$admin_pw" >/dev/null
  on_pbs proxmox-backup-manager acl update / Admin --auth-id admin@pbs
  fp="$(on_pbs proxmox-backup-manager cert info | sed -n 's/^Fingerprint (sha256): //p')"
  docker exec -e PBS_PASSWORD="$admin_pw" -e PBS_FINGERPRINT="$fp" "$pbs" \
    proxmox-backup-client namespace create desktops --repository admin@pbs@127.0.0.1:store1 >/dev/null
  on_pbs proxmox-backup-manager user create backup@pbs >/dev/null
  token_secret="$(on_pbs proxmox-backup-manager user generate-token backup@pbs desktop \
    | sed -n 's/.*"value": *"\([^"]*\)".*/\1/p')"
  [[ -n "$token_secret" ]] || { echo "could not create API token"; exit 1; }
  for id in 'backup@pbs' 'backup@pbs!desktop'; do
    on_pbs proxmox-backup-manager acl update /datastore/store1/desktops DatastoreBackup --auth-id "$id"
  done
}

result() {
  echo
  echo "== result: $pass passed, $fail failed"
  [[ $fail -eq 0 ]]
}
