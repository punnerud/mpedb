#!/usr/bin/env bash
#
# Run the portable crates' tests as WINDOWS binaries, under Wine, on Linux.
#
# ## What this is for, and what it is not
#
# The Windows CI job runs nightly and does two things: compile the portable
# crates for Windows, and run their tests there. The first half is already
# covered on every push — `.github/workflows/linux.yml` cross-compiles the same
# crates for `x86_64-pc-windows-gnu`, which is what catches the failure mode we
# have actually had twice (a portable crate acquiring a Unix dependency;
# `cannot find 'unix' in 'os'`). That needs no Wine at all: `check`/`clippy` do
# not link or run.
#
# This script covers the OTHER half — Windows *runtime* behaviour: `\r\n`, path
# separators, float and integer formatting, locale, anything where the same code
# computes a different answer on a different OS.
#
# ## Three caveats, and they are not small
#
# 1. **Wine is not Windows.** A pass here does not prove a pass on Windows, and a
#    failure may be Wine's bug rather than ours. Treat a red as "go look",
#    never as "the code is wrong".
# 2. **This is the GNU toolchain; CI is MSVC.** `windows-latest` builds with
#    MSVC, which has a different CRT and different codegen. `-msvc` cannot be
#    built here at all — a dependency's assembly needs `ml64.exe`.
# 3. **Only the portable crates.** The engine is Unix-only BY CONSTRUCTION
#    (mmap, flock, robust pthread mutexes, /proc); see the header of
#    `.github/workflows/windows.yml`. Pointing this at the workspace would fail
#    on line one forever.
#
# So: a screening tool for the nightly job, not a replacement for it, and not a
# gate. The nightly Windows run remains the thing that decides.
#
# ## Disk
#
# The Wine prefix goes on an external disk, not `/` — the root filesystem here
# is 38 GB and goes full under benchmarks. Build artifacts already land on
# `/mnt/xfs` via `~/.cargo/config.toml`.
#
# ## Setup (once)
#
#     sudo apt install --no-install-recommends wine64 mingw-w64   # ~1.9 GB on /
#     rustup target add x86_64-pc-windows-gnu
#
set -euo pipefail

WINE_ROOT="${MPEDB_WINE_ROOT:-/mnt/ext4/wine}"
export WINEPREFIX="${WINEPREFIX:-$WINE_ROOT/mpedb}"
# Wine is chatty on stderr about things that are not our problem; a real test
# failure comes back on the exit code regardless.
export WINEDEBUG="${WINEDEBUG:--all}"

for tool in wine x86_64-w64-mingw32-gcc; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "error: $tool not found." >&2
        echo "  sudo apt install --no-install-recommends wine64 mingw-w64" >&2
        exit 127
    fi
done

if ! rustup target list --installed | grep -qx x86_64-pc-windows-gnu; then
    echo "error: the windows-gnu target is not installed." >&2
    echo "  rustup target add x86_64-pc-windows-gnu" >&2
    exit 127
fi

mkdir -p "$WINEPREFIX"

# Rust's windows-gnu binaries link against libgcc/libwinpthread, which live in
# the mingw runtime directory rather than anywhere Wine looks by default. Adding
# it to WINEPATH is what turns "exited with code 0xc0000135" (DLL not found,
# which reads like a crash) into a test run.
MINGW_DLLS=$(dirname "$(x86_64-w64-mingw32-gcc -print-libgcc-file-name)")
for d in /usr/lib/gcc/x86_64-w64-mingw32/*/ /usr/x86_64-w64-mingw32/lib/; do
    [ -d "$d" ] && MINGW_DLLS="$MINGW_DLLS;$(winepath -w "$d" 2>/dev/null || echo "$d")"
done
export WINEPATH="$MINGW_DLLS"

# `cargo test` builds native and runs the result; the runner is what makes it
# hand the Windows binary to Wine instead of exec'ing it.
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=wine

echo "WINEPREFIX = $WINEPREFIX"
echo "target     = x86_64-pc-windows-gnu (mingw), runner = wine"
echo

cargo test -p mpedb-types -p mpedb-sql --target x86_64-pc-windows-gnu "$@"
