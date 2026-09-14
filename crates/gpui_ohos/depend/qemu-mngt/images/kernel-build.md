# QEMU Guest Kernel Build

Guest kernel for the in-process QEMU engine (HiCodeer). This kernel is HiSH's,
with a small, deliberate set of deltas.

- Version: **linux 6.12.60**
- Base config: HiSH's **`arm64_virt`** - archived here as
  `arm64_virt.base.config`. It was recovered from the released kernel's embedded
  ikconfig (`IKCFG_ST`/`IKCFG_ED` markers + gzip) and is byte-identical to the
  config HiSH's `mkimg.sh` builds with.
- Patch: HiSH's `0001-n_tty_resize.patch` - archived here as `n_tty_resize.patch`
- Artifact: `Image` (13.29 MB, uncompressed) - a valid QEMU `-kernel` payload
- Toolchain: clang 17.0.6 + LLVM (`LLVM=1 LLVM_IAS=1`), built natively on the
  aarch64 OpenEuler VM (no `CROSS_COMPILE`)

## Why this kernel at all

HiCodeer hot-plugs a virtio-fs device for every mounted working directory
(`mount.rs` -> QMP `device_add` onto a `pcie-root-port`). HiSH's shipped kernel
cannot do either half: its config has `CONFIG_FUSE_FS=n`, no `CONFIG_VIRTIO_FS`
at all, and `CONFIG_PCIEPORTBUS=n` / `CONFIG_HOTPLUG_PCI=n`. Those five options
are therefore re-enabled, and everything else is left exactly as HiSH ships it -
so the guest keeps the boot behaviour, drivers and timings the reference was
validated with.

## Deltas from the `arm64_virt` base

### Capability (required by HiCodeer's mount design)

| option | value | why |
| --- | --- | --- |
| `CONFIG_FUSE_FS` | y | virtio-fs is a FUSE protocol |
| `CONFIG_VIRTIO_FS` | y | the sandbox and every workdir mount |
| `CONFIG_PCIEPORTBUS` | y | `pcie-root-port` slots |
| `CONFIG_HOTPLUG_PCI` | y | hotplug core |
| `CONFIG_HOTPLUG_PCI_PCIE` | y | pciehp: bind the device after `device_add` |

### Performance (the base is tuned for HiSH's 512 MiB single-core guest)

This guest is different in kind: 8 GiB, up to `cores` vCPUs, TCG-only, and
dominated by `exec` + allocation (busybox, git, language servers). Each of these
options costs guest time **under TCG specifically**, which is why they are
restored even though the base leaves them off:

| option | base | here | why |
| --- | --- | --- | --- |
| `CONFIG_SLUB_TINY` | y | **n** | the low-memory slab variant drops the per-CPU fast paths; `SLUB_CPU_PARTIAL` comes back with it |
| `CONFIG_ARM64_HW_AFDBM` | n | **y** | without it the first touch of every page traps to software to set the access flag |
| `CONFIG_ARM64_TLB_RANGE` | n | **y** | one `TLBI RANGE` instead of one `TLBI` per page |
| `CONFIG_TRANSPARENT_HUGEPAGE` `+_ALWAYS` | n | **y** | 2 MiB mappings: fewer levels walked, fewer softmmu TLB fills; also brings `CONFIG_ARM64_CONTPTE` (contiguous PTE folding) |
| `CONFIG_HIGH_RES_TIMERS` | n | **y** | deadlines honoured at ns instead of tick granularity |
| `CONFIG_NR_CPUS` | 512 | **32** | the base sizes per-CPU arrays for 512 CPUs; this guest never exceeds a handful |

`CONFIG_NO_HZ_FULL` needs nothing: the base already builds it, and per its own
Kconfig it behaves as tickless-idle unless a `nohz_full=` CPU list is passed.

`CONFIG_HZ` stays at the base's **100**: a lower tick rate is the same argument
as above (every tick is a guest-to-host exit).

## Reproducing this kernel

```sh
# 1. Source + patch. The patch is archived next to this file.
curl -fsSL -o linux-6.12.60.tar.xz \
  https://mirrors.aliyun.com/linux-kernel/v6.x/linux-6.12.60.tar.xz
tar xf linux-6.12.60.tar.xz
cd linux-6.12.60
patch -p1 --forward < ../images/n_tty_resize.patch

# 2. Config: start from the archived HiSH base, then apply the deltas.
#    (images/linux-6.12.60.config is the resulting .config.)
cp ../images/arm64_virt.base.config .config
./scripts/config \
  --enable FUSE_FS --enable VIRTIO_FS \
  --enable PCIEPORTBUS --enable HOTPLUG_PCI --enable HOTPLUG_PCI_PCIE \
  --enable ARM64_HW_AFDBM --enable ARM64_TLB_RANGE \
  --enable TRANSPARENT_HUGEPAGE --enable TRANSPARENT_HUGEPAGE_ALWAYS \
  --disable TRANSPARENT_HUGEPAGE_MADVISE \
  --enable HIGH_RES_TIMERS --disable SLUB_TINY --set-val NR_CPUS 32

# 3. KCFLAGS: per-feature codegen for TCG (see below).
export KCFLAGS='-march=armv8.5-a+crc+crypto+lse+rcpc+rng+sm4+sha3+dotprod+fp16 -mtune=neoverse-n1 -O2 -falign-functions=64 -fno-strict-aliasing -mllvm -vectorize-loops -mllvm -force-vector-width=2'
make ARCH=arm64 LLVM=1 LLVM_IAS=1 olddefconfig
make ARCH=arm64 LLVM=1 LLVM_IAS=1 -j6 Image

# 4. Deploy: arch/arm64/boot/Image -> images/Image, and .config ->
#    images/linux-6.12.60.config (bundle-ohos copies Image into the HAP).
```

Verify the result actually carries the deltas by extracting the new image's own
ikconfig and grepping it - the same trick that recovered HiSH's base:

```sh
python3 -c "
import gzip,sys
d=open('arch/arm64/boot/Image','rb').read()
open('/tmp/k.conf','wb').write(gzip.decompress(d[d.find(b'IKCFG_ST')+8:d.find(b'IKCFG_ED')]))"
grep -E '^CONFIG_(VIRTIO_FS|PCIEPORTBUS|ARM64_HW_AFDBM|TLB_RANGE)' /tmp/k.conf
```

## KCFLAGS: per-feature code generation for TCG

`-march=armv8.5-a` with the extension list makes clang emit the architecture
extensions **directly** instead of routing every atomic through the kernel's
`alternative` patching. That is a large win under TCG, where a single `ldadd`
translates to one host atomic while an `ldxr`/`stxr` LL/SC pair becomes a
serialised compare-and-swap sequence. Every extension here is implemented by
QEMU's TCG backend behind a feature gate, and `-cpu max` advertises them, so the
generated instructions execute rather than trap:

- `+lse` - `target/arm/tcg` translates `ldadd`/`cas`/`swpa` natively.
- `+rng` - `RNDR`, gated on `aa64_rndr` (`target/arm/helper.c`).
- `+sm4`, `+sha3`, `+crc`, `+crypto`, `+rcpc`, `+dotprod`, `+fp16` - all
  TCG-implemented behind their own feature gates.
- `-mllvm -vectorize-loops -mllvm -force-vector-width=2` vectorises but pins the
  width to 2 elements; wider vectors make TCG emit a long element-by-element
  helper sequence per instruction, which costs more than the scalar code.
- `-mtune=neoverse-n1` is carried over from the reference build. TCG models no
  pipeline, so the scheduling hints buy nothing; it is kept to stay identical.

**Caution:** because `-march` asserts those features unconditionally, the guest
must run on a CPU model that really provides them. `-cpu max` does. On a model
without LSE the guest would take an illegal-instruction fault instead of falling
back to the LL/SC path.

## Kernel command line (what this config actually acts on)

The engine passes `GUEST_CMDLINE` (`qemuctrl/src/lib.rs`). Only these entries do
anything against this config; the reasoning for each omission lives next to the
constant:

- `console=ttyAMA0,115200`, `root=/dev/vda rw`, `init=/usr/lib/qemu-init/init`,
  `TERM=xterm`
- `mitigations=off` (`CONFIG_CPU_MITIGATIONS=y`)
- HiSH's `kpti=off` is **not** carried over: this config has
  `CONFIG_UNMAP_KERNEL_AT_EL0=n`, so the early parameter is unregistered.
- `init_on_alloc=1` is **not** carried over: `CONFIG_INIT_ON_ALLOC_DEFAULT_ON`
  is off here, so the parameter would switch allocation zeroing ON.

## Build tools required (once per VM)

```sh
sudo dnf install -y flex bison     # kconfig generator
sudo dnf install -y openssl-devel  # certs/extract-cert
sudo dnf install -y llvm           # llvm-objcopy/-nm/-ar/-readelf for LLVM=1
```

`llvm` must match the installed `clang` major version (17.0.6 pairs with
llvm 17.0.6). `clang-libs` alone is not enough: `LLVM=1` shells out to
`llvm-objcopy`, `llvm-nm` and `llvm-readelf`, and the build fails in
`vdso_prepare` without them.

If the tree was ever configured for GCC and then switched to `LLVM=1`, check
`include/generated/vdso-offsets.h`. A failed first build can leave it as a
zero-byte file while `.vdso-offsets.h.cmd` still records the target as up to
date - the `$(NM) | $(gen-vdso-sym) | sort > $@` pipeline reports the exit
status of `sort`, so the failure is invisible. The symptom is
`use of undeclared identifier 'vdso_offset_sigtramp'` when compiling
`arch/arm64/kernel/signal.c`. Fix it by touching the dependency to force a
rebuild: `touch arch/arm64/kernel/vdso/vdso.so.dbg`.

## Notes

- `CONFIG_CC_VERSION_TEXT` in `linux-6.12.60.config` records clang 17.0.6
  (this build), where HiSH's shipped config records Ubuntu clang 18.1.3. That is
  the toolchain on hand; the KCFLAGS above are what matter for codegen.
- The base leaves `CONFIG_ARM64_VA_BITS=52` (4-level tables). This guest keeps
  it rather than dropping to 39: it is what the reference runs, and the page
  walk cost is already addressed by `CONFIG_ARM64_CONTPTE`.
