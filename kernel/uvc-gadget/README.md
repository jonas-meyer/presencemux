# presencemux-uvc-dkms

The `usb_f_uvc` module of the Raspberry Pi kernel, with one patch for dwc2.
This directory is GPL-2.0, not MIT like the rest of the repository.

## Why

The dwc2 UDC of the Pi 5 completes a missed isochronous request with
`-ENODATA`. dwc3 uses `-EXDEV`. f_uvc handles only `-EXDEV` as a missed
transfer and cancels the whole queue on any other status. The host then shows
a torn frame. The patch handles `-ENODATA` like `-EXDEV`: the cut frame gets
`UVC_STREAM_ERR`, and the host drops it. See `docs/cam.md`, systemd units and
package.

## Source

The `.c` and `.h` files are unchanged copies of
`drivers/usb/gadget/function/` at raspberrypi/linux commit
`cff533aec2fa601846766b32ff57204e0a61bed7`. That commit is the source of the
kernel package `linux-image-6.18.50+rpt-rpi-2712` (1:6.18.50-1+rpt1). The
`Linux commit` line in `/usr/share/doc/linux-image-*/changelog.Debian.gz`
gives it. `Kbuild` has the `usb_f_uvc` lines of the kernel Makefile. DKMS
applies the patch in `patches/` at build time.

`BUILD_EXCLUSIVE_KERNEL` in `dkms.conf` allows only this kernel. On another
kernel, DKMS skips the build, and the stock module loads.

## Build and install

```bash
sudo apt install dkms dh-dkms debhelper
(cd kernel/uvc-gadget && dpkg-buildpackage -b --no-sign)
sudo apt install ./kernel/presencemux-uvc-dkms_*_all.deb
```

The new module loads at the next boot or after the gadget is removed:
stop `presencemux-cam`, stop `presencemux-gadget`, `modprobe -r usb_f_uvc`,
start both again. `modinfo -n usb_f_uvc` shows `updates/dkms`.

## After a kernel update

1. Find the commit of the new kernel in its changelog.
2. Copy the 14 files of that commit, and check that the patch applies.
3. Change `BUILD_EXCLUSIVE_KERNEL`, the commit above and
   `debian/changelog`.

Remove this package when the fix is in the Raspberry Pi kernel.

## Upstream

The patch is in `git format-patch` form, for linux-usb@vger.kernel.org. The
USB webcam gadget has no maintainer. Send it to Greg Kroah-Hartman and Cc
the dwc2 maintainer. Search lore.kernel.org for an existing patch first.
Not sent yet (2026-10-15).

The gadget API gives no status for a missed isochronous transfer. dwc3
completes it with `-EXDEV`, dwc2 with `-ENODATA`, and other UDCs can use other
codes. A more general upstream form treats each error of the video endpoint
other than `-ESHUTDOWN` and `-ECONNRESET` as a missed transfer. The other form
changes dwc2 to `-EXDEV`, which changes it for all functions on dwc2.
