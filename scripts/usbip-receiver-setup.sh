#!/usr/bin/env bash
# One-time receiver-side setup for Mousehop's USB/IP injection backend.
#
# The mousehop daemon exports a composite virtual HID device (keyboard +
# mouse) on 127.0.0.1:3240. This script loads the kernel's virtual host
# controller and attaches that device, so the OS sees a regular USB
# keyboard/mouse — no compositor cooperation (libei portal etc.) needed.
#
# Usage (on the RECEIVING machine, as root):
#   scripts/usbip-receiver-setup.sh attach   # default
#   scripts/usbip-receiver-setup.sh detach
#   scripts/usbip-receiver-setup.sh status
set -euo pipefail

BUS_ID="1-1"
HOST="127.0.0.1"
PORT="3240"

die() { echo "error: $*" >&2; exit 1; }
info() { echo "usbip-setup: $*"; }

need_usbip_tool() {
    command -v usbip >/dev/null 2>&1 || die "usbip tool not found. Install it first:
  Debian/Ubuntu/deepin/UOS: sudo apt install usbip-utils
  Fedora:                   sudo dnf install usbip-utils
  Arch:                     sudo pacman -S usbip-utils"
}

load_vhci() {
    if [ -d "/sys/module/vhci_hcd" ]; then
        info "vhci_hcd already loaded"
        return
    fi
    info "loading vhci_hcd kernel module"
    modprobe vhci_hcd 2>/dev/null || die "modprobe vhci_hcd failed. Your kernel may lack
  CONFIG_USBIP_CORE/CONFIG_USBIP_VHCI_HCD (common on stock distro kernels without
  extra module packages, e.g. Debian needs linux-modules-extra-\$(uname -r))."
}

is_attached() {
    usbip port 2>/dev/null | grep -q "Attached Devices: [1-9]"
}

cmd_attach() {
    load_vhci
    need_usbip_tool
    if is_attached; then
        info "device already attached:"
        usbip port
        return
    fi
    info "attaching ${BUS_ID} from ${HOST}:${PORT}"
    # The mousehop daemon must be running on this machine with
    # emulation_backend = "usbip" (or --emulation-backend usbip),
    # otherwise the import is refused.
    usbip attach -r "${HOST}" -b "${BUS_ID}" || die "attach failed. Is mousehop running
  with the usbip emulation backend enabled on this machine?
  Check: RUST_LOG=info mousehop --emulation-backend usbip test-emulation"
    info "attached. The virtual keyboard/mouse should appear in this session now:"
    usbip port
}

cmd_detach() {
    need_usbip_tool
    port="$(usbip port 2>/dev/null | grep -oP 'Port \K[0-9]+' | head -1 || true)"
    if [ -z "${port}" ]; then
        info "nothing attached"
        return
    fi
    info "detaching port ${port}"
    usbip detach -p "${port}"
}

cmd_status() {
    need_usbip_tool
    [ -d "/sys/module/vhci_hcd" ] || info "vhci_hcd not loaded"
    usbip port
}

case "${1:-attach}" in
    attach) cmd_attach ;;
    detach) cmd_detach ;;
    status) cmd_status ;;
    *) die "usage: $0 [attach|detach|status]" ;;
esac
