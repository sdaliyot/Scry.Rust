#!/usr/bin/env bash
# End-to-end check: runs both demos as `cargo scry` builds and drives them with `scry-rs`, the way an
# agent or a test does. CI runs it on Linux under Xvfb:
#
#   xvfb-run -a scripts/e2e.sh
#
# It builds what it needs. Run it from the repository root, on Linux, macOS or Windows (Git Bash).
# On a desktop, the dx-demo's window opens while it runs. Screenshots are checked where they are
# supported (Windows and Linux).
set -uo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
exe=""
case "$(uname -s)" in MINGW* | MSYS* | CYGWIN*) exe=".exe" ;; esac
scry="$root/scry/target/debug/scry-rs$exe"
demos="$root/demos/target/scry/debug"
work=$(mktemp -d)
# Descriptors go to a folder of their own, so apps the user runs don't interfere.
export SCRY_RS_TARGETS="$work/targets"
failures=0
pids=()

cleanup() {
  # Written so that bash 3.2 (macOS) accepts an empty array under set -u.
  for pid in ${pids[@]+"${pids[@]}"}; do kill "$pid" 2>/dev/null; done
  rm -rf "$work"
}
trap cleanup EXIT

# check <name> <text the reply must contain> <scry-rs arguments...>
check() {
  local name=$1 expected=$2
  shift 2
  local reply
  reply=$("$scry" "$@" 2>&1)
  if [[ "$reply" == *"$expected"* ]]; then
    echo "ok    $name"
  else
    echo "FAIL  $name"
    echo "      scry-rs $*"
    echo "      expected: $expected"
    echo "      got:      ${reply:0:400}"
    failures=$((failures + 1))
  fi
}

# is_png <name> <file>
is_png() {
  if [[ "$(head -c 8 "$2" 2>/dev/null | od -An -tx1 | tr -d ' \n')" == "89504e470d0a1a0a" ]]; then
    echo "ok    $1"
  else
    echo "FAIL  $1: $2 is not a PNG"
    failures=$((failures + 1))
  fi
}

echo "== Building scry-rs, cargo scry and the demos"
cargo build --quiet --manifest-path "$root/scry/Cargo.toml" -p scry-rs-cli -p scry-rs-cargo || exit 1
(cd "$root/demos" && "$root/scry/target/debug/cargo-scry$exe" scry build --quiet --workspace) || exit 1

echo "== console-demo"
check "no app yet" '"no_target"' --process console-demo roots
# Started a moment after the command, so the command has to wait for it.
(sleep 2 && exec "$demos/console-demo$exe" >"$work/console.log" 2>&1) &
pids+=($!)
# The endpoint starts on main's first line, so APP may still be empty when the app is first listed: wait for
# the state rather than read it straight away.
check "--wait-for-target" '"satisfied":true' --process console-demo --wait-for-target 60s wait timeout=30 'APP.stats.ticks >= 1'
check "roots" 'console_demo::APP' --process console-demo roots
check "set" '"ok":true' --process console-demo set 'APP.rules[0].percent' 50
check "get" '"percent":50' --process console-demo get 'APP.rules[0]'
check "call" '"ok":true' --process console-demo call APP.inventory.total_value_cents
check "eval" '"result":3' --process console-demo eval '1 + 2'
check "wait" '"satisfied":true' --process console-demo wait timeout=15 'APP.stats.ticks >= 2'
check "assert fails" '"assertion_failed"' --process console-demo assert 'APP.stats.ticks < 0'
check "hook add" '"hook":1' --process console-demo hook add Inventory::sell_one
check "hook wait" '"satisfied":true' --process console-demo hook wait 1 timeout=30
case "$(uname -s)" in
  Linux) mode=$(stat -c %a "$SCRY_RS_TARGETS"/*.json) ;;
  Darwin) mode=$(stat -f %Lp "$SCRY_RS_TARGETS"/*.json) ;;
  *) mode=600 ;;
esac
if [[ "$mode" == 600 ]]; then echo "ok    descriptor is private"; else
  echo "FAIL  descriptor mode is $mode, not 600"
  failures=$((failures + 1))
fi

echo "== dx-demo"
"$demos/dx-demo$exe" >"$work/dx.log" 2>&1 &
pids+=($!)
check "--wait-for-target" '"ok":true' --process dx-demo --wait-for-target 180s roots
check "set a global signal" '"ok":true' --process dx-demo set FILTER Done
check "text" 'Install Rust' --process dx-demo text ul
check "click" '"tag":"button"' --process dx-demo click 'text=All'
# The click runs the app's handler after it replies, so wait for its effect rather than read it.
check "the click's effect" '"satisfied":true' --process dx-demo wait timeout=10 'FILTER == "All"'
check "windows" '"window":0' --process dx-demo windows
case "$(uname -s)" in
  Darwin) check "screenshot (not on macOS yet)" '"bad_command"' --process dx-demo screenshot ;;
  *)
    check "screenshot of the page" '"format":"png"' --process dx-demo --out "$work/page.png" screenshot
    is_png "the page's PNG" "$work/page.png"
    check "screenshot of an element" '"tag":"ul"' --process dx-demo --out "$work/list.png" screenshot ul
    is_png "the element's PNG" "$work/list.png"
    ;;
esac

echo
if ((failures)); then
  echo "$failures check(s) failed"
  echo "--- console-demo output"; tail -20 "$work/console.log"
  echo "--- dx-demo output"; tail -20 "$work/dx.log"
  exit 1
fi
echo "All checks passed"
