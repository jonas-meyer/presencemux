# presencemux-ffs-dkms

The `usb_f_fs` (FunctionFS) module of the Raspberry Pi kernel, with one patch
for the USB Video Class. This directory is GPL-2.0, not MIT like the rest of
the repository.

## Why

The cam worker implements the USB camera function in userspace through
FunctionFS, with a bulk endpoint. The dwc2 UDC of the Pi 5 misses isochronous
microframes, and the kernel UVC function (f_uvc) has only isochronous
streaming. See `docs/ffs-camera.md`.

The stock `usb_f_fs` accepts class-specific descriptors only for HID, CCID and
DFU, and rejects the UVC descriptors with `EINVAL`. The patch accepts
`CS_INTERFACE` and `CS_ENDPOINT` descriptors when the current interface has
the video class.

f_fs does not renumber the interface numbers and endpoint addresses in the
class-specific descriptors. The camera function must be the first function of
the configuration, so that its interfaces are 0 and 1 and its endpoint is
0x81. The cam worker checks this after the bind.

## Source

The `.c` and `.h` files are unchanged copies from raspberrypi/linux commit
`cff533aec2fa601846766b32ff57204e0a61bed7`: `f_fs.c` and `u_fs.h` from
`drivers/usb/gadget/function/`, `u_os_desc.h` and `configfs.h` from
`drivers/usb/gadget/`. That commit is the source of the kernel package
`linux-image-6.18.50+rpt-rpi-2712` (1:6.18.50-1+rpt1). `Kbuild` has the
`usb_f_fs` lines of the kernel Makefile. DKMS applies the patch in `patches/`
at build time.

`BUILD_EXCLUSIVE_KERNEL` in `dkms.conf` allows only this kernel. On another
kernel, DKMS skips the build, and the stock module loads. It rejects the
camera descriptors, so the gadget does not bind, and the host sees no device.
There is no fallback.

## Build and install

```bash
sudo apt install dkms dh-dkms debhelper
(cd kernel/ffs-gadget && dpkg-buildpackage -b --no-sign)
sudo apt install ./kernel/presencemux-ffs-dkms_*_all.deb
```

The module loads when a gadget uses FunctionFS. `modinfo -n usb_f_fs` shows
`updates/dkms`. If the stock module is already loaded, remove the gadget that
uses it and run `modprobe -r usb_f_fs`.

## After a kernel update

1. Find the commit of the new kernel in its changelog.
2. Copy the 4 files of that commit, and check that the patch applies.
3. Change `BUILD_EXCLUSIVE_KERNEL`, the commit above and
   `debian/changelog`.

## Upstream

The patch is in `git format-patch` form. A version for linux-usb must also
renumber the interface numbers and endpoint addresses in the video
descriptors, and document the rule in `Documentation/usb/functionfs-desc.rst`,
as the DFU patch of 2024 did. A draft series is in
`docs/ffs-camera/patches/ffs/`. Not sent yet (2026-10-20).
