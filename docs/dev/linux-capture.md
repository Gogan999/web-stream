# Linux capture

On Linux the host captures what the GPU scans out (KMS) and encodes it on
the same GPU. This works the same on the login screen, in any desktop
session (X11 or Wayland) and on a text console, because it reads the
display hardware's state rather than asking a compositor.

## Requirements

- Root, or `CAP_SYS_ADMIN`: the kernel only hands out framebuffer handles
  (`DRM_IOCTL_MODE_GETFB2`, Linux 5.7 or later) to privileged processes.
  The systemd service runs as root.
- A KMS driver: `i915`/`xe`, `amdgpu`, `nouveau`, or `nvidia-drm` with
  `modeset=1` (`nvidia-drm.modeset=1` on the kernel command line).
- An encoder, loaded at run time; the host starts without any of them and
  reports that no encoder was found:
  - Intel and AMD: VA-API (`libva.so.2`, `libva-drm.so.2`) and the driver's
    VA-API backend (`intel-media-va-driver` / `intel-media-driver`, or
    Mesa's `mesa-va-drivers` / `libva-mesa-driver`). It needs video
    processing (VPP) and an H.264, HEVC or AV1 encode entrypoint; `vainfo`
    lists them.
  - NVIDIA: the proprietary driver's `libnvidia-encode.so.1`, `libcuda.so.1`
    and `libEGL.so.1` (EGL device platform). NVENC API 12.1 or later.

## Displays

Every lit connector is a display, named after the monitor's EDID name and
the connector (`DELL U2720Q (DP-1)`). Its id is derived from the GPU's PCI
path and the connector name, so it stays the same across reboots. When the
desktop is one framebuffer spread over several monitors (X11), the
positions come from it; otherwise the displays are placed side by side.

### Headless

KMS can only capture a connector that is lit. On a machine without a
monitor, plug in an HDMI or DisplayPort dummy plug (an EDID emulator): the
desktop and the login screen then drive it like a monitor, at the modes its
EDID lists. Without one, the kernel can be told to light a connector with
`video=HDMI-A-1:1920x1080@60e` (and, for the modes, an EDID file with
`drm.edid_firmware=HDMI-A-1:edid/1920x1080.bin`), but whether the desktop
uses it depends on the compositor.

## What is captured

The primary plane of the CRTC. Overlay planes (some compositors put a
fullscreen video there) and the hardware cursor plane are not part of the
picture; the browser draws its own cursor. Rotated outputs are captured
unrotated.

## Encoding

- VA-API: VPP imports the scanout DMA-BUF, crops, scales, letterboxes and
  converts it to NV12 (BT.709, limited range) on the GPU; the encoder reads
  that surface in place. CBR without B-frames, an infinite GOP, keyframes
  on demand, bitrate changes on the next frame, parameter sets on every
  keyframe. Lost frames are repaired with a keyframe.
- NVENC: EGL imports the scanout into OpenGL, a shader writes the NV12
  planes, and CUDA copies them on the GPU into the encoder's input. The
  NVENC configuration is the one the Windows host uses.

Changing the display mode to match the browser is not supported on Linux.
