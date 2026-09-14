#!/usr/bin/env bash
#
# build-images.sh - recreate the upstream payload that lives in this directory.
#
# Nothing under images/ comes out of the application's own build: the bundle
# script only *consumes* this directory. These are the guest VM payload inputs -
# the Linux guest kernel and its config, the Alpine base disk, the QEMU engine
# shared libraries, and one zlib. This script fetches each one from its recorded
# upstream and, where the artifact is a build of ours rather than a released
# binary, builds it here. A successful run therefore recreates the whole
# directory on a clean machine, with no out-of-tree scratch copy involved.
#
# Provenance:
#   Image                       built here: linux-<KVER>.tar.xz + the HiSH
#                               arm64_virt base + the n_tty resize patch + 13
#                               option deltas (see kernel-build.md for prose)
#   linux-<KVER>.config         the .config that produced that Image
#   arm64_virt.base.config      harmoninux/linux-config, pinned commit
#   n_tty_resize.patch          harmoninux/linux-config, pinned commit
#   alpine-rootfs.qcow2         rootfs_aarch64.qcow2 (release), recompressed -c
#   libqemu-system-aarch64.so   harmoninux/qemu release libs.zip, arm64-v8a
#   libslirp.so.0               same archive
#   libz.so                     zlib-ng-compat from the brew prefix, DT_SONAME
#                               rewritten to the bare name the engine asks for
#
# kernel-build.md is prose, not an artifact, and stays hand-written.
#
# Where to run: on the Linux build host (the same machine the bundle script
# uses). Needed: bash, curl, tar, unzip, patch, flex, bison, bc, python3,
# qemu-img, and an LLVM toolchain (clang / ld.lld / llvm-ar / llvm-nm). The
# shared disk is mounted there too, so the brew prefix below is reachable.
#
# Usage:
#   ./build-images.sh                 build/fetch whatever is missing
#   ./build-images.sh --force         ignore caches, rebuild everything
#   ./build-images.sh --only kernel   one of: kernel rootfs engine libz configs
#   ./build-images.sh --check         verify what is on disk; no network
#   ./build-images.sh --clean         drop the download/source caches, then stop

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IMAGES_DIR="$SCRIPT_DIR"

# Caches live outside the repo. The kernel source tree in particular has to sit
# on a filesystem with working hard links: on the shared (virtio-fs) disk the
# kernel build degrades to copying every file, which is why the default is under
# $HOME. Point IMAGES_KBUILD_DIR at an existing tree to reuse its object files.
CACHE_DIR="${IMAGES_CACHE_DIR:-$HOME/.cache/qemu-mngt-images}"
if [ -n "${IMAGES_KBUILD_DIR:-}" ]; then
  KBUILD_DIR="$IMAGES_KBUILD_DIR"
elif [ -d "$HOME/kbuild/linux-6.12.60" ]; then
  KBUILD_DIR="$HOME/kbuild"          # reuse an existing scratch tree if there is one
else
  KBUILD_DIR="$HOME/qemu-mngt-kbuild"
fi
JOBS="${IMAGES_JOBS:-$(nproc)}"

# ---------------------------------------------------------------------------
# Pinned upstreams
# ---------------------------------------------------------------------------
KVER=6.12.60
KERNEL_URL="https://mirrors.aliyun.com/linux-kernel/v6.x/linux-${KVER}.tar.xz"
KERNEL_SHA=a63096b2147411d683cecbf87622bb2ff4885bac2b3641d3d4f10250c89cdcf8

LCFG_RAW="https://raw.githubusercontent.com/harmoninux/linux-config"
LCFG_ARM64_VIRT_COMMIT=4d69cdf2dbc5b2febd41389dd7db0a5df5e542e8
LCFG_PATCH_COMMIT=43d3ef05086c69d2364ad897f3359da72e205dba
BASE_CONFIG_URL="$LCFG_RAW/$LCFG_ARM64_VIRT_COMMIT/arm64_virt"
BASE_CONFIG_SHA=6500db0365b21ab793a4350c1376627f56c9b5ecf0c1d64f9f358b8b49adf771
NTFY_PATCH_URL="$LCFG_RAW/$LCFG_PATCH_COMMIT/patch/linux-${KVER}/0001-n_tty_resize.patch"
NTFY_PATCH_SHA=daf309d29920f7c4c7a37762b061696d571c9a66bf1cacec6c592e1a9b68e8cd

QEMU_LIBS_URL="https://github.com/harmoninux/qemu/releases/download/hish-20260110/libs.zip"
QEMU_LIBS_SHA=684979be9f5101d124297e5cbbef6da27d1b2873dfc1d9a57eb80cdb9f8f4d74

ROOTFS_URL="https://github.com/harmoninux/linux-config/releases/download/rootfs-20260117/rootfs_aarch64.qcow2"
ROOTFS_SHA=367962e0df197e75dfed6f89cafd231bd4dadcdd5eb77d0a2d2f320cf660c834

# zlib-ng-compat is installed from brew into a prefix; its bare-name libz.so is
# what the engine's DT_NEEDED resolves to. Two builds of 2.3.3 have been seen on
# this machine - both are a working aarch64 zlib, so either is accepted and the
# recorded one is only reported as a note.
ZLIB_SHA_SEEN_1=1a9794ad88d1ec20455ce1f49cd119116e02bfec8e36e8ec9dc20d1f723d97be
ZLIB_SHA_SEEN_2=eba5ca8bb51790699461090750ca74e415ee524fa4b0a0796b0776d5a07babc8

# ---------------------------------------------------------------------------
# Recorded outputs, for --check
# ---------------------------------------------------------------------------
OUT_BASE_CONFIG_SHA=$BASE_CONFIG_SHA
OUT_PATCH_SHA=$NTFY_PATCH_SHA
OUT_ROOTFS_SHA=bd0d17025c08e4037e239ea3a8c3a055640851f22a4239f3977d8c0251533ee5
OUT_ENGINE_SHA=875a5f5e7f5378e1c859df10b35ef8bc0693c73435771e8914baf46f19044ea5
OUT_SLIRP_SHA=40fd99ebba549179ae416837f19c4bd920bdc789ead4b1cfb3434cc1be852f63
OUT_LIBZ_SHA=490966cb435f75a811275be5f192c1a3391229679f2ef549fa47b464dd4c7e7f
OUT_KERNEL_SHA=10a21e8f2884ed250990578861cc67d3d63a4e4844a6071d55580c26dfc339b9
OUT_KCONFIG_SHA=de1d753ba830b412b5e44a5f3c000fafb268599eb3f18b328ca7741803c40cb1

# --- kernel build recipe -----------------------------------------------------
# Single consolidated pass: the HiSH arm64_virt base plus the options this guest
# actually needs. Capability group is what the virtio-fs mounts and runtime
# work-dir hotplug depend on; performance group retunes a base that was sized
# for a 512 MiB single-core guest rather than an 8 GiB multi-vCPU TCG one.
KCFLAGS='-march=armv8.5-a+crc+crypto+lse+rcpc+rng+sm4+sha3+dotprod+fp16 -mtune=neoverse-n1 -O2 -falign-functions=64 -fno-strict-aliasing -mllvm -vectorize-loops -mllvm -force-vector-width=2'

# 23 options that must end up =y, 4 that must end up unset.
MUST_Y="CONFIG_FUSE_FS CONFIG_VIRTIO_FS CONFIG_PCIEPORTBUS CONFIG_HOTPLUG_PCI \
CONFIG_HOTPLUG_PCI_PCIE CONFIG_ARM64_HW_AFDBM CONFIG_ARM64_TLB_RANGE \
CONFIG_TRANSPARENT_HUGEPAGE CONFIG_TRANSPARENT_HUGEPAGE_ALWAYS CONFIG_NO_HZ_FULL \
CONFIG_HIGH_RES_TIMERS CONFIG_VIRTIO_BLK CONFIG_VIRTIO_NET CONFIG_NET_9P_VIRTIO \
CONFIG_9P_FS CONFIG_EXT4_FS CONFIG_DEVTMPFS_MOUNT \
CONFIG_SERIAL_AMBA_PL011_CONSOLE CONFIG_UNIX98_PTYS CONFIG_TMPFS \
CONFIG_ARM_GIC_V3_ITS CONFIG_PCI_MSI CONFIG_HW_RANDOM_VIRTIO"
MUST_NOT="CONFIG_SLUB_TINY CONFIG_HZ_PERIODIC CONFIG_TRANSPARENT_HUGEPAGE_MADVISE \
CONFIG_BLK_DEV_INITRD"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
log()  { printf '[images] %s\n' "$*"; }
warn() { printf '[images] WARNING: %s\n' "$*" >&2; }
die()  { printf '[images] ERROR: %s\n' "$*" >&2; exit 1; }

need() {
  local missing=""
  for c in "$@"; do command -v "$c" >/dev/null 2>&1 || missing="$missing $c"; done
  [ -z "$missing" ] || die "missing tool(s):$missing"
}

sha256_of() { sha256sum "$1" | cut -d' ' -f1; }

# fetch URL SHA256 DEST - download only when the cached copy is absent or wrong,
# and always verify before letting it be used.
# The transfer lands in a sibling "<dest>.part" and is moved into place only once
# the checksum matches. Whatever already sits at DEST is therefore never touched
# by a failed transfer: the old file stays exactly as it was (stale beats gone),
# and the only leftover is the .part, which the next run overwrites.
fetch() {
  local url="$1" want="$2" dest="$3" part="$3.part"
  mkdir -p "$(dirname "$dest")"
  if [ "$FORCE" = 0 ] && [ -f "$dest" ] && [ "$(sha256_of "$dest")" = "$want" ]; then
    log "cached  $(basename "$dest")"
    return 0
  fi
  log "fetch   $(basename "$dest")"
  rm -f "$part"
  if ! curl -fSL --retry 3 --retry-delay 2 --connect-timeout 20 -o "$part" "$url"; then
    rm -f "$part"
    die "download failed: $url"
  fi
  local got; got="$(sha256_of "$part")"
  if [ "$got" != "$want" ]; then
    rm -f "$part"
    die "checksum mismatch on $(basename "$dest"): got $got, want $want"
  fi
  mv -f "$part" "$dest"
}

# install_into SRC DEST MODE - write through a sibling temp file so a failure
# never leaves a half-written artifact in the tree.
# The shared virtio-fs mount reports every file as 0777 and refuses chmod with
# EPERM, so a refused chmod is expected there and must not be fatal; on a normal
# filesystem the mode is set as asked.
install_into() {
  local src="$1" dest="$2" mode="$3"
  cp -f "$src" "$dest.new"
  chmod "$mode" "$dest.new" 2>/dev/null || true
  mv -f "$dest.new" "$dest"
}

# patch_soname SRC DEST NEWNAME - rewrite DT_SONAME in place. The dynamic table
# points at its own string table (sh_link), which is the only unambiguous way to
# find the right one - a shared object has several SHT_STRTAB sections.
patch_soname() {
  python3 - "$1" "$2" "$3" <<'PY'
import struct, sys
src, dst, new = sys.argv[1], sys.argv[2], sys.argv[3]
d = bytearray(open(src, "rb").read())
if d[:4] != b"\x7fELF":
    sys.exit("not an ELF: " + src)
e_shoff, = struct.unpack_from("<Q", d, 0x28)
e_shentsize, e_shnum, _ = struct.unpack_from("<HHH", d, 0x3A)
secs = []
dynamic = None
for i in range(e_shnum):
    off = e_shoff + i * e_shentsize
    _, stype, _, _, offset, size, link, _, _, _ = struct.unpack_from("<IIQQQQIIQQ", d, off)
    secs.append((offset, size))
    if stype == 6:                      # SHT_DYNAMIC
        dynamic = (offset, size, link)
if dynamic is None:
    sys.exit("no SHT_DYNAMIC in " + src)
dyn_off, dyn_size, dynstr_idx = dynamic
str_off, _ = secs[dynstr_idx]
soname = None
for pos in range(dyn_off, dyn_off + dyn_size, 16):
    tag, val = struct.unpack_from("<QQ", d, pos)
    if tag == 0:
        break
    if tag == 14:                       # DT_SONAME
        soname = val
if soname is None:
    sys.exit("no DT_SONAME in " + src)
start = str_off + soname
end = d.index(b"\x00", start)
old = bytes(d[start:end])
new_b = new.encode()
if len(new_b) > len(old):
    sys.exit("new SONAME is longer than the old one (%r > %r)" % (new_b, old))
d[start:start + len(new_b)] = new_b
d[start + len(new_b)] = 0
open(dst, "wb").write(bytes(d))
print("  SONAME %s -> %s" % (old.decode(), new))
PY
}

# --- brew zlib ---------------------------------------------------------------
# Candidate brew roots. The Cellar sits on the shared disk, so the build host can
# read the device-side install directly.
brew_roots() {
  {
    [ -n "${IMAGES_BREW_PREFIX:-}" ] && printf '%s\n' "$IMAGES_BREW_PREFIX"
    command -v brew >/dev/null 2>&1 && brew --prefix 2>/dev/null || true
    printf '%s\n' "$(cd "$SCRIPT_DIR/../../../../../../.." 2>/dev/null && pwd)/.harmonybrew"
    printf '%s\n' "$HOME/.harmonybrew"
    printf '%s\n' "/mnt/linux_share/.harmonybrew"
  } | awk 'NF && !seen[$0]++'
}

# find_zlib_input - pick the zlib-ng-compat build to derive libz.so from.
# Preference: a build whose hash is one we have shipped before, so re-running
# reproduces the exact bytes; otherwise whatever the opt prefix points at (still
# a valid aarch64 zlib, just a different build - the caller warns).
find_zlib_input() {
  local root f sha
  local roots=() files=()
  while IFS= read -r root; do
    [ -d "$root" ] || continue
    roots+=("$root")
  done < <(brew_roots)
  [ "${#roots[@]}" -gt 0 ] || return 1

  for root in "${roots[@]}"; do
    for f in "$root"/Cellar/zlib-ng-compat/*/lib/libz.so.1 \
             "$root"/opt/zlib-ng-compat/lib/libz.so.1 \
             "$root"/opt/zlib-ng-compat/lib/libz.so; do
      [ -f "$f" ] || continue
      files+=("$f")
    done
  done
  [ "${#files[@]}" -gt 0 ] || return 1

  for f in "${files[@]}"; do
    sha="$(sha256_of "$f")"
    if [ "$sha" = "$ZLIB_SHA_SEEN_1" ] || [ "$sha" = "$ZLIB_SHA_SEEN_2" ]; then
      printf '%s\n' "$f"
      return 0
    fi
  done
  printf '%s\n' "${files[0]}"
}

# ---------------------------------------------------------------------------
# Sections
# ---------------------------------------------------------------------------
build_kernel() {
  need make patch flex bison bc python3 clang ld.lld llvm-ar llvm-nm
  fetch "$BASE_CONFIG_URL" "$BASE_CONFIG_SHA" "$CACHE_DIR/arm64_virt.base.config"
  fetch "$NTFY_PATCH_URL"  "$NTFY_PATCH_SHA"  "$CACHE_DIR/n_tty_resize.patch"
  fetch "$KERNEL_URL"      "$KERNEL_SHA"      "$CACHE_DIR/linux-$KVER.tar.xz"

  local tree="$KBUILD_DIR/linux-$KVER"
  if [ "$FORCE" = 1 ] && [ -d "$tree" ]; then
    log "force: discarding $tree"
    rm -rf "$tree"
  fi
  if [ ! -d "$tree" ]; then
    log "extract linux-$KVER.tar.xz -> $KBUILD_DIR"
    mkdir -p "$KBUILD_DIR"
    tar -xf "$CACHE_DIR/linux-$KVER.tar.xz" -C "$KBUILD_DIR" || die "extract failed"
  else
    log "reusing source tree $tree"
  fi

  # The archive is pinned by hash, but a half-extracted tree would still pass
  # that check - so read the version straight out of the Makefile.
  local v
  v="$(awk -F' = ' '/^(VERSION|PATCHLEVEL|SUBLEVEL) =/{printf "%s.", $2}' "$tree/Makefile" | sed 's/\.$//')"
  [ "$v" = "$KVER" ] || die "source tree reports version '$v', expected '$KVER'"

  if grep -q '__process_set_size' "$tree/drivers/tty/n_tty.c"; then
    log "patch already applied"
  else
    log "apply 0001-n_tty_resize.patch"
    ( cd "$tree" && patch -p1 --forward < "$CACHE_DIR/n_tty_resize.patch" ) \
      || die "patch failed"
  fi

  log "reset config to the arm64_virt base + 13 deltas"
  cp -f "$CACHE_DIR/arm64_virt.base.config" "$tree/.config"
  ( cd "$tree" && ./scripts/config \
      --enable FUSE_FS \
      --enable VIRTIO_FS \
      --enable PCIEPORTBUS \
      --enable HOTPLUG_PCI \
      --enable HOTPLUG_PCI_PCIE \
      --enable ARM64_HW_AFDBM \
      --enable ARM64_TLB_RANGE \
      --enable TRANSPARENT_HUGEPAGE \
      --enable TRANSPARENT_HUGEPAGE_ALWAYS \
      --disable TRANSPARENT_HUGEPAGE_MADVISE \
      --enable HIGH_RES_TIMERS \
      --disable SLUB_TINY \
      --set-val NR_CPUS 32 ) || die "scripts/config failed"

  log "olddefconfig"
  ( cd "$tree" && env KCFLAGS="$KCFLAGS" make ARCH=arm64 LLVM=1 LLVM_IAS=1 olddefconfig ) \
    >/dev/null || die "olddefconfig failed"

  local fail=0 k
  for k in $MUST_Y; do
    grep -qE "^$k=y" "$tree/.config" || { warn "option not enabled: $k"; fail=1; }
  done
  for k in $MUST_NOT; do
    grep -qE "^# $k is not set" "$tree/.config" || { warn "option should be off: $k"; fail=1; }
  done
  grep -qE '^CONFIG_NR_CPUS=32$' "$tree/.config" || { warn "NR_CPUS is not 32"; fail=1; }
  [ "$fail" = 0 ] || die "required kernel options are missing after olddefconfig"
  log "all 23 required options set, 4 exclusions confirmed, NR_CPUS=32"

  log "build Image with $JOBS job(s)"
  ( cd "$tree" && env KCFLAGS="$KCFLAGS" make ARCH=arm64 LLVM=1 LLVM_IAS=1 -j"$JOBS" Image ) \
    || die "kernel build failed"

  install_into "$tree/arch/arm64/boot/Image" "$IMAGES_DIR/Image" 644
  install_into "$tree/.config" "$IMAGES_DIR/linux-$KVER.config" 644
  log "installed Image ($(du -h "$IMAGES_DIR/Image" | cut -f1)) and linux-$KVER.config"

  ikconfig_check "$IMAGES_DIR/Image"
}

# ikconfig_check IMAGE - read back the config the kernel itself carries, rather
# than trusting the .config that was on disk when it was built.
ikconfig_check() {
  python3 - "$1" <<'PY' || return 1
import gzip, sys
d = open(sys.argv[1], "rb").read()
s, e = d.find(b"IKCFG_ST"), d.find(b"IKCFG_ED")
if s < 0 or e < 0:
    sys.exit("no embedded ikconfig markers in the Image")
cfg = gzip.decompress(d[s + 8:e]).decode("utf-8", "replace")
want_y = ["CONFIG_VIRTIO_FS", "CONFIG_FUSE_FS", "CONFIG_HOTPLUG_PCI_PCIE",
          "CONFIG_ARM64_TLB_RANGE", "CONFIG_HIGH_RES_TIMERS"]
want_n = ["CONFIG_SLUB_TINY"]
bad = [k for k in want_y if "\n%s=y\n" % k not in "\n" + cfg + "\n"]
bad += [k for k in want_n if "\n%s=y\n" % k in "\n" + cfg + "\n"]
if "\nCONFIG_NR_CPUS=32\n" not in "\n" + cfg + "\n":
    bad.append("CONFIG_NR_CPUS=32")
if bad:
    sys.exit("embedded config disagrees with the build: %s" % ", ".join(bad))
print("  embedded ikconfig agrees with the build")
PY
  log "kernel self-check passed"
}

build_rootfs() {
  need qemu-img
  fetch "$ROOTFS_URL" "$ROOTFS_SHA" "$CACHE_DIR/rootfs_aarch64.qcow2"
  # The released disk is already the right filesystem; the only change is a
  # recompress, which is what takes it from ~46 MiB down to ~17 MiB.
  log "recompress -> alpine-rootfs.qcow2"
  rm -f "$IMAGES_DIR/alpine-rootfs.qcow2.new"
  qemu-img convert -c -f qcow2 -O qcow2 \
    "$CACHE_DIR/rootfs_aarch64.qcow2" "$IMAGES_DIR/alpine-rootfs.qcow2.new" \
    || die "qemu-img convert failed"
  mv -f "$IMAGES_DIR/alpine-rootfs.qcow2.new" "$IMAGES_DIR/alpine-rootfs.qcow2"

  # Self-check: same content as upstream, only the encoding differs.
  if qemu-img compare "$CACHE_DIR/rootfs_aarch64.qcow2" "$IMAGES_DIR/alpine-rootfs.qcow2" >/dev/null 2>&1; then
    log "self-check: content identical to the upstream image"
  else
    die "self-check failed: recompressed image differs in content from upstream"
  fi
}

build_engine() {
  need unzip
  fetch "$QEMU_LIBS_URL" "$QEMU_LIBS_SHA" "$CACHE_DIR/libs.zip"
  local tmpd; tmpd="$(mktemp -d)"
  unzip -qo "$CACHE_DIR/libs.zip" -d "$tmpd" || die "unzip failed"

  local src="$tmpd/libs/arm64-v8a" f
  for f in libqemu-system-aarch64.so libslirp.so.0; do
    [ -f "$src/$f" ] || die "libs.zip does not contain $f"
    install_into "$src/$f" "$IMAGES_DIR/$f" 755
    log "installed $f ($(du -h "$IMAGES_DIR/$f" | cut -f1))"
  done

  if command -v readelf >/dev/null 2>&1; then
    local needed
    needed="$(readelf -d "$IMAGES_DIR/libqemu-system-aarch64.so" | grep -o 'libslirp.so.0\|libz.so' | sort -u | tr '\n' ' ')"
    log "engine DT_NEEDED: $needed"
  fi
  rm -rf "$tmpd"
}

build_libz() {
  need python3
  local src
  if ! src="$(find_zlib_input)"; then
    die "zlib-ng-compat was not found in any known brew root.
    Install it first - its Cellar lives on the shared disk, so this build host
    picks it up straight away:
        brew install zlib-ng-compat
    or point the script at an existing prefix:
        IMAGES_BREW_PREFIX=/path/to/brew $0 --only libz"
  fi

  local in_sha; in_sha="$(sha256_of "$src")"
  log "source  $src"
  log "        sha256 $in_sha"
  case "$in_sha" in
    "$ZLIB_SHA_SEEN_1"|"$ZLIB_SHA_SEEN_2") : ;;
    *) warn "this is not a zlib build recorded before; it is fine as long as it is a working aarch64 zlib" ;;
  esac

  # The engine's DT_NEEDED is the bare name libz.so, and the loader matches on
  # both filename and DT_SONAME, so the shipped file has to be renamed *and*
  # re-sonamed.
  patch_soname "$src" "$IMAGES_DIR/libz.so.new" "libz.so"
  mv -f "$IMAGES_DIR/libz.so.new" "$IMAGES_DIR/libz.so"

  if command -v readelf >/dev/null 2>&1; then
    local arch soname
    arch="$(readelf -h "$IMAGES_DIR/libz.so" | awk -F: '/Machine/{print $2}' | tr -d ' ')"
    soname="$(readelf -d "$IMAGES_DIR/libz.so" | grep -o 'SONAME.*\[.*\]' | sed 's/.*\[\(.*\)\]/\1/')"
    [ "$arch" = "AArch64" ] || die "libz.so is not an aarch64 ELF (got '$arch')"
    [ "$soname" = "libz.so" ] || die "libz.so SONAME is '$soname', expected 'libz.so'"
    log "self-check: AArch64, SONAME=libz.so"
  fi
}

build_configs() {
  fetch "$BASE_CONFIG_URL" "$BASE_CONFIG_SHA" "$IMAGES_DIR/arm64_virt.base.config"
  fetch "$NTFY_PATCH_URL"  "$NTFY_PATCH_SHA"  "$IMAGES_DIR/n_tty_resize.patch"
  log "installed arm64_virt.base.config and n_tty_resize.patch"
}

# ---------------------------------------------------------------------------
# --check: report what is on disk, touch nothing
# ---------------------------------------------------------------------------
check_one() { # LABEL PATH EXPECTED
  local label="$1" path="$2" want="$3" got verdict
  if [ ! -f "$path" ]; then
    printf '  %-26s MISSING\n' "$label"; return 1
  fi
  got="$(sha256_of "$path")"
  if [ "$got" = "$want" ]; then verdict=ok; else verdict=DIFFERS; fi
  printf '  %-26s %-8s %s\n' "$label" "$verdict" "${got:0:16}"
  if [ "$verdict" = ok ]; then return 0; fi
  return 1
}

check_all() {
  local rc=0
  echo "== files with a pinned upstream =="
  check_one arm64_virt.base.config "$IMAGES_DIR/arm64_virt.base.config" "$OUT_BASE_CONFIG_SHA" || rc=1
  check_one n_tty_resize.patch     "$IMAGES_DIR/n_tty_resize.patch"     "$OUT_PATCH_SHA"       || rc=1
  check_one alpine-rootfs.qcow2    "$IMAGES_DIR/alpine-rootfs.qcow2"    "$OUT_ROOTFS_SHA"      || rc=1
  check_one libqemu-system.so     "$IMAGES_DIR/libqemu-system-aarch64.so" "$OUT_ENGINE_SHA"     || rc=1
  check_one libslirp.so.0          "$IMAGES_DIR/libslirp.so.0"          "$OUT_SLIRP_SHA"       || rc=1
  echo "== files derived from a local input =="
  check_one "libz.so (patched)"    "$IMAGES_DIR/libz.so"                "$OUT_LIBZ_SHA"        || rc=1
  local pref
  if pref="$(find_zlib_input)"; then
    echo "  brew libz input            $pref"
    echo "                             sha $(sha256_of "$pref" | cut -c1-16)"
  else
    echo "  brew libz input            NOT FOUND (install zlib-ng-compat)"
  fi
  echo "== files produced by the kernel build =="
  # These are byte-identical only for the same compiler and flags, so a
  # difference is reported but does not fail the check.
  local got
  got="$([ -f "$IMAGES_DIR/Image" ] && sha256_of "$IMAGES_DIR/Image" || echo -)"
  [ "$got" = "$OUT_KERNEL_SHA" ] || echo "  note: Image differs from the recorded build (toolchain dependent)"
  printf '  %-26s %s\n' Image "${got:0:16}"
  got="$([ -f "$IMAGES_DIR/linux-$KVER.config" ] && sha256_of "$IMAGES_DIR/linux-$KVER.config" || echo -)"
  [ "$got" = "$OUT_KCONFIG_SHA" ] || rc=1
  printf '  %-26s %s\n' "linux-$KVER.config" "${got:0:16}"
  echo "== upstream files in the cache =="
  for pair in "linux-$KVER.tar.xz:$KERNEL_SHA" "libs.zip:$QEMU_LIBS_SHA" \
              "rootfs_aarch64.qcow2:$ROOTFS_SHA"; do
    local f="${pair%%:*}" want="${pair##*:}"
    if [ -f "$CACHE_DIR/$f" ] && [ "$(sha256_of "$CACHE_DIR/$f")" = "$want" ]; then
      printf '  %-26s cached\n' "$f"
    else
      printf '  %-26s not cached\n' "$f"
    fi
  done
  echo
  if [ "$rc" = 0 ]; then log "check passed"; else warn "check found mismatches"; fi
  return "$rc"
}

# ---------------------------------------------------------------------------
# Argument handling
# ---------------------------------------------------------------------------
MODE=build
FORCE=0
ONLY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --force) FORCE=1 ;;
    --check) MODE=check ;;
    --clean) MODE=clean ;;
    --only)  shift; ONLY="${1:-}"; [ -n "$ONLY" ] || die "--only needs a section name" ;;
    --only=*) ONLY="${1#--only=}" ;;
    -h|--help) sed -n '2,40p' "$0"; exit 0 ;;
    *) die "unknown argument: $1 (try --help)" ;;
  esac
  shift
done

case "$MODE:$ONLY" in
  check:*) check_all ;;
  clean:*)
    log "removing $CACHE_DIR"
    rm -rf "$CACHE_DIR"
    log "removing $KBUILD_DIR/linux-$KVER"
    rm -rf "$KBUILD_DIR/linux-$KVER"
    log "caches cleared; run without --clean to rebuild"
    ;;
  build:kernel)  build_kernel ;;
  build:rootfs)  build_rootfs ;;
  build:engine)  build_engine ;;
  build:libz)    build_libz ;;
  build:configs) build_configs ;;
  build:)
    log "cache   $CACHE_DIR"
    log "kbuild  $KBUILD_DIR"
    build_configs
    build_engine
    build_rootfs
    build_libz
    build_kernel
    echo
    log "done - images/ now holds:"
    ( cd "$IMAGES_DIR" && for f in Image linux-$KVER.config arm64_virt.base.config \
        n_tty_resize.patch alpine-rootfs.qcow2 libqemu-system-aarch64.so libslirp.so.0 libz.so; do
        [ -f "$f" ] && printf '    %-28s %10s B  %s\n' "$f" "$(stat -c%s "$f")" "$(sha256_of "$f" | cut -c1-16)"
      done )
    ;;
  build:*) die "unknown section: $ONLY (kernel|rootfs|engine|libz|configs)" ;;
esac
