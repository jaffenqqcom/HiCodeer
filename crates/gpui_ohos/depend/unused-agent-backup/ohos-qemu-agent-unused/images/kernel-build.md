# QEMU Guest Kernel Build

Self-built aarch64 kernel for the in-process QEMU guest (zcoder).

- Kernel version: **6.18.7**
- Host is aarch64 (OpenEuler VM), so the kernel is built **natively** (no `CROSS_COMPILE`).
- Artifacts: `Image` (7.7 MB, uncompressed), `Image.gz` (3.2 MB, gzip self-extracting).
  Either is a valid QEMU `-kernel` payload.

## Reproducing the exact same kernel

```sh
# 1. Source
#    Download https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.18.7.tar.xz
#    Extract to LOCAL disk, NOT the shared virtiofs mount (file ops fail there).

# 2. Configuration
#    linux-6.18.7.config is the exact .config used. Copy it into the tree.
cp images/linux-6.18.7.config /tmp/linux-6.18.7/.config
make -C /tmp/linux-6.18.7 ARCH=arm64 olddefconfig

# 3. Build
make -C /tmp/linux-6.18.7 ARCH=arm64 -j6 Image Image.gz

# 4. Deploy: copy Image to images/Image (bundle-ohos copies it into the HAP).
```

## Configuration notes

Started from `make ARCH=arm64 allnoconfig`, then enabled only what the guest
needs (headless QEMU virt machine running BusyBox initramfs + cmd-agentd):

- PCIe hotplug (the reason this kernel was rebuilt):
  `CONFIG_PCIEPORTBUS`, `CONFIG_HOTPLUG_PCI`, `CONFIG_HOTPLUG_PCI_PCIE` (pciehp).
- virtio transport/devices: `CONFIG_VIRTIO`, `CONFIG_VIRTIO_PCI`,
  `CONFIG_VIRTIO_NET`, `CONFIG_VIRTIO_BLK`, `CONFIG_VIRTIO_CONSOLE` (virtio-serial),
  plus `CONFIG_VIRTIO_MENU`.
- 9p: `CONFIG_NET_9P`, `CONFIG_NET_9P_VIRTIO`, `CONFIG_9P_FS`,
  `CONFIG_9P_FS_POSIX_ACL`, `CONFIG_FS_POSIX_ACL`.
- Network for npm: `CONFIG_INET` (TCP/IP), `CONFIG_PACKET`, `CONFIG_UNIX`,
  `CONFIG_NET_CORE`, `CONFIG_NETDEVICES`, `CONFIG_NET_FAILOVER`.
- Console: `CONFIG_SERIAL_AMBA_PL011`, `CONFIG_SERIAL_CORE`,
  `CONFIG_SERIAL_CORE_CONSOLE`.
- **`CONFIG_BINFMT_SCRIPT` is mandatory** - without it the kernel cannot exec
  shebang scripts (e.g. `/etc/init.d/rcS` -> `Exec format error`).
- Boot: `CONFIG_BLK_DEV_INITRD`, `CONFIG_DEVTMPFS`(+MOUNT), `CONFIG_PROC_FS`,
  `CONFIG_SYSFS`, `CONFIG_TMPFS`, `CONFIG_BINFMT_ELF`.
- Platform: `CONFIG_SMP`, `CONFIG_ARM_GIC`/`ARM_GIC_V3`/`ARM_GIC_V3_ITS` (PCI MSI),
  `CONFIG_ARM_ARCH_TIMER`, `CONFIG_ARM_PSCI_FW`, `CONFIG_PCI_HOST_GENERIC`.
- Process: `CONFIG_FUTEX`, `CONFIG_EPOLL`, `CONFIG_EVENTFD`, `CONFIG_UNIX98_PTYS`,
  `CONFIG_NAMESPACES`, `CONFIG_CGROUPS` (kept for node/npm).
- Deliberately off: DRM/FB (no display), USB, SOUND, WIRELESS, BT, STAGING,
  disk filesystems (VFAT/SQUASHFS/ISO9660/MSDOS), SCSI/ATA, FW_LOADER,
  I2C/SPI/PINCTRL/GPIOLIB/REGULATOR/DMA_ENGINE, MODULES, DEBUG_INFO.

## Build tools required (once per VM)

```sh
sudo dnf install -y flex bison   # kconfig generator
sudo dnf install -y openssl-devel # certs/extract-cert
```
