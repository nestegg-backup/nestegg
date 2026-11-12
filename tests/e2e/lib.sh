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
  DOCKER_BUILDKIT=0 docker build -q -t "${NESTEGG_E2E_PBS_IMAGE:-nestegg-e2e-pbs}" -f "$here/pbs.Dockerfile" "$here" >/dev/null
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
      --network "$net" --network-alias pbs.e2e.test "${NESTEGG_E2E_PBS_IMAGE:-nestegg-e2e-pbs}" >/dev/null
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

# Images for the package tests: the Ubuntu client and, on top of it, the
# same client with systemd as PID 1.
build_systemd_images() {
  DOCKER_BUILDKIT=0 docker build -q -t nestegg-e2e-client:ubuntu-24.04 -f "$here/client.Dockerfile" "$here" >/dev/null
  DOCKER_BUILDKIT=0 docker build -q -t "${NESTEGG_E2E_SYSTEMD_IMAGE:-nestegg-e2e-systemd}" -f "$here/systemd-client.Dockerfile" "$here" >/dev/null
}

# Start "$client" with systemd as PID 1 and wait until it has booted. It needs
# the host cgroup namespace (--cgroupns=host, writable /sys/fs/cgroup), so
# only run this on a disposable test machine. Extra arguments go to
# `docker run` (e.g. -v mounts for the packages).
# CAP_SYS_ADMIN and no seccomp/AppArmor confinement let systemd create mount
# namespaces. Without them systemd silently ignores the units' mount
# sandboxing options, and sandbox checks would pass even with options that
# hide paths (see docs/decisions/0007-systemd-unit-sandboxing.md).
start_systemd_client() {
  docker run -d -t --name "$client" --network "$net" --cgroupns=host \
    --cap-add SYS_ADMIN --security-opt seccomp=unconfined --security-opt apparmor=unconfined \
    -v /sys/fs/cgroup:/sys/fs/cgroup:rw --tmpfs /run --tmpfs /run/lock \
    "$@" "${NESTEGG_E2E_SYSTEMD_IMAGE:-nestegg-e2e-systemd}" >/dev/null
  local state
  for _ in $(seq 1 30); do
    state="$(on_client systemctl is-system-running 2>/dev/null || true)"
    [[ "$state" == running || "$state" == degraded ]] && break
    sleep 1
  done
}

result() {
  echo
  echo "== result: $pass passed, $fail failed"
  [[ $fail -eq 0 ]]
}
