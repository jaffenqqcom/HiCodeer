#!/bin/bash
# Build the QEMU guest golden disk (golden.qcow2) shipped in the HAP resfile.
#
# The golden is an immutable mother disk: it already holds the guest userland
# plus the qemu-init boot scripts, and the runtime copies it to disk.qcow2 (the
# writable working disk) on first provision. Building it is:
#   1) unpack the Alpine base image (images/alpine-rootfs.qcow2) to a raw disk
#   2) mount it and inject the qemu-init boot scripts (repo qemu-mngt/guest-init)
#   3) add the glibc runtime the host's prebuilt language servers need
#   4) add tzdata so the runtime clock sync has Asia/Shanghai to point at
#   5) recompress into a qcow2 and install it at OUT_QCW2
#
# Every step aborts on failure (non-zero exit) so a bundle never ships a stale
# or half-built golden. Run as the normal build user; sudo is used for the loop
# mount and for writing into the (root-owned) mounted tree.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
QEMU_INIT_SRC="$SCRIPT_DIR/../guest-init"
BASE_IMAGE="$SCRIPT_DIR/../images/alpine-rootfs.qcow2"

if [ "$#" -ne 2 ]; then
  echo "usage: $0 OUT_QCW2 CACHE_ROOT" >&2
  exit 2
fi
OUT_QCW2="$1"
CACHE_ROOT="$2"

command -v qemu-img >/dev/null 2>&1 || { echo "ERROR: qemu-img not found" >&2; exit 1; }
command -v losetup >/dev/null 2>&1 || { echo "ERROR: losetup not found" >&2; exit 1; }
[ -f "$BASE_IMAGE" ] || { echo "ERROR: base image missing: $BASE_IMAGE" >&2; exit 1; }

# CACHE_ROOT must stay owned by the build user: the raw/qcow2 intermediates are
# produced as the normal user, so the top-level cache dir is created here
# without sudo.
mkdir -p "$CACHE_ROOT"
RAW="$CACHE_ROOT/golden.raw"
TMP_QCW2="$CACHE_ROOT/golden.qcow2"
MNT="$CACHE_ROOT/golden.mnt"

# 1) Unpack the base qcow2 to a raw disk, once. The raw file is sparse (the
#    base image is a 128 GiB virtual disk holding ~30 MiB of data), so this
#    costs almost no space.
if [ ! -f "$RAW" ]; then
  echo "[golden] unpacking $BASE_IMAGE -> $RAW"
  qemu-img convert -f qcow2 -O raw "$BASE_IMAGE" "$RAW"
fi

# 2) Mount it. A stale mount from an interrupted run is reused and unmounted
#    at the end, so a killed build never leaves the tree mounted forever.
mkdir -p "$MNT"
if ! mountpoint -q "$MNT"; then
  echo "[golden] loop-mounting $RAW at $MNT"
  sudo mount -o loop "$RAW" "$MNT"
fi
cleanup() {
  if mountpoint -q "$MNT"; then
    sync
    sudo umount "$MNT" || true
  fi
}
trap cleanup EXIT

# 2a) Inject the qemu-init boot scripts from the repo on every run, so editing
#     qemu-mngt/guest-init takes effect on the next (forced) golden rebuild.
#     The kernel command line points init= at /usr/lib/qemu-init/init, so this
#     directory is what PID 1 actually is.
echo "[golden] injecting qemu-init boot scripts"
# Clear first: the cached raw disk is reused across runs, so a script deleted or
# renamed in the repo would otherwise survive in the image forever and still be
# started by the rcS glob.
sudo rm -rf "$MNT/usr/lib/qemu-init"
sudo mkdir -p "$MNT/usr/lib/qemu-init"
# Driven by the directory rather than a fixed list: a boot script added to the
# repo must land in the image automatically, or it is silently missing at boot
# while the build still reports success.
for src in "$QEMU_INIT_SRC"/*; do
  [ -f "$src" ] || continue
  name=$(basename "$src")
  sudo cp "$src" "$MNT/usr/lib/qemu-init/$name"
  sudo chmod 755 "$MNT/usr/lib/qemu-init/$name"
done
echo "[golden] injected $(ls -1 "$QEMU_INIT_SRC" | wc -l) boot script(s): $(ls -1 "$QEMU_INIT_SRC" | tr '\n' ' ')"

# 3) glibc runtime. The Alpine userland is musl, which is what makes the guest
#    fast under TCG, but the host's prebuilt language servers are not all
#    musl-aware: rust-analyzer probes /lib and picks its musl build, while the
#    JSON and Python adapters hardcode an `unknown-linux-gnu` (glibc) asset.
#    Carrying a real glibc next to musl - the two coexist because their loaders
#    and libc sonames differ (ld-linux-aarch64.so.1 / libc.so.6 vs
#    ld-musl-aarch64.so.1 / libc.musl-aarch64.so.1) - keeps those servers
#    working without giving up the musl userland. The host is the same
#    architecture and the same glibc family the servers were built against, so
#    the files are taken straight from the host rather than rebuilt.
GLIBC_LIBS="libBrokenLocale.so.1 libanl.so.1 libc.so.6 libc_malloc_debug.so.0 \
libdl.so.2 libm.so.6 libmvec.so.1 libnss_compat.so.2 libnss_dns.so.2 \
libnss_files.so.2 libpthread.so.0 libresolv.so.2 librt.so.1 libthread_db.so.1 \
libutil.so.1 libgcc_s.so.1"
echo "[golden] adding glibc runtime for gnu language servers"
sudo mkdir -p "$MNT/lib" "$MNT/lib64" "$MNT/usr/lib64"
if [ -f /lib/ld-linux-aarch64.so.1 ]; then
  sudo cp -L /lib/ld-linux-aarch64.so.1 "$MNT/lib/ld-linux-aarch64.so.1"
elif [ -f /usr/lib/ld-linux-aarch64.so.1 ]; then
  sudo cp -L /usr/lib/ld-linux-aarch64.so.1 "$MNT/lib/ld-linux-aarch64.so.1"
else
  echo "[golden] ERROR: no glibc loader on this host" >&2
  exit 1
fi
for lib in $GLIBC_LIBS; do
  src=""
  for dir in /lib64 /usr/lib64 /lib /usr/lib; do
    if [ -e "$dir/$lib" ]; then src="$dir/$lib"; break; fi
  done
  if [ -z "$src" ]; then
    echo "[golden] ERROR: glibc library not found on host: $lib" >&2
    exit 1
  fi
  sudo cp -L "$src" "$MNT/lib64/$lib"
done
sudo cp -L /usr/lib64/libstdc++.so.6 "$MNT/usr/lib64/libstdc++.so.6"
# A loader without a cache falls back to its built-in search path, which for
# this glibc build is /lib64 and /usr/lib64 - exactly where the files landed.
# Run ldconfig anyway so the cache is present and the lookup is explicit.
if [ -x /usr/sbin/ldconfig ]; then
  sudo /usr/sbin/ldconfig -r "$MNT" 2>/dev/null || \
    echo "[golden] note: ldconfig -r failed; relying on the default search path"
fi

# 4) tzdata. The runtime clock sync does
#      ln -sf /usr/share/zoneinfo/Asia/Shanghai /etc/localtime; date -s @<epoch>
#    and the Alpine base image ships no timezone database at all, so without
#    this the symlink dangles and the guest prints UTC. The zoneinfo format is
#    stable, so the host's copy is used as-is; the `right/` and `posix/`
#    subtrees are duplicates of the top level and are skipped.
if [ ! -e "$MNT/usr/share/zoneinfo/Asia/Shanghai" ]; then
  echo "[golden] adding tzdata (Asia/Shanghai)"
  [ -f /usr/share/zoneinfo/Asia/Shanghai ] || {
    echo "[golden] ERROR: host has no /usr/share/zoneinfo/Asia/Shanghai" >&2
    exit 1
  }
  sudo mkdir -p "$MNT/usr/share/zoneinfo"
  if command -v rsync >/dev/null 2>&1; then
    sudo rsync -a --exclude 'right/' --exclude 'posix/' \
      /usr/share/zoneinfo/ "$MNT/usr/share/zoneinfo/"
  else
    ( cd /usr/share/zoneinfo && sudo tar cf - --exclude=right --exclude=posix . ) \
      | sudo tar xf - -C "$MNT/usr/share/zoneinfo"
  fi
fi

# 5) Unmount before converting: qemu-img must read a quiescent image.
cleanup
trap - EXIT

# 6) Compressed qcow2 (compression keeps the shipped golden small) and install
#    at OUT_QCW2 (the bundle passes the resfile path).
echo "[golden] qemu-img convert -> $TMP_QCW2"
rm -f "$TMP_QCW2"
qemu-img convert -c -f raw -O qcow2 "$RAW" "$TMP_QCW2"

echo "[golden] installing golden at $OUT_QCW2"
mkdir -p "$(dirname "$OUT_QCW2")"
cp "$TMP_QCW2" "$OUT_QCW2"
echo "[golden] golden ready: $OUT_QCW2 ($(du -h "$OUT_QCW2" | cut -f1))"
