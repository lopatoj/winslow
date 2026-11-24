# PocketBeagle setup

Tested with the BeagleBoard.org Debian 12 (Bookworm) IoT image, kernel 6.18
(`bone`), 2026-10-09. The image's own BeagleBone Black build also boots the
PocketBeagle.

## First boot

1. Write the image to a microSD card (balenaEtcher) and push the card in
   until it clicks. On first boot the board resizes its filesystem and
   reboots once or twice; give it a few minutes.
2. `ssh debian@192.168.7.2`, password `temppwd`. You must change it: enter
   `temppwd` again at "Current password", then the new one twice.
3. From the laptop: `ssh-copy-id debian@192.168.7.2`.

libiio, ALSA and libsystemd are already installed on this image.

## Pico on UART0 (P1.30/P1.32)

UART0 (`/dev/ttyS0`) is the board's serial console by default: the kernel
logs to it and a login prompt runs on it, both of which would fight the
daemon for the port. Move the console to tty0 and stop the login prompt
there. A login console over USB (`/dev/ttyGS0` on the board, `/dev/ttyACM0`
on the laptop) remains.

```sh
sudo sed -i -e 's/^console=ttyS0,115200n8$/console=tty0/' \
            -e 's/^\(cmdline=.*\) earlycon /\1 /' /boot/uEnv.txt
sudo systemctl mask serial-getty@ttyS0.service
sudo reboot
```

U-Boot still prints to UART0 at power-on; the Pico ignores that, and stays
silent itself until the daemon talks to it, so it can't interrupt the boot.

If the board doesn't come back: connect over the USB serial console
(`picocom -b 115200 /dev/ttyACM0` on the laptop), or put the SD card in the
laptop and fix `/boot/uEnv.txt` on its root partition.

UART4 (`/dev/ttyS4`, P2.05 RX / P2.07 TX) needs none of this: it's enabled
and muxed by the PocketBeagle device tree.

## AIN7 (P2.36)

P2.36 isn't wired to the ADC directly: it goes to the TPS65217 PMIC's
MUX_IN, and the PMIC's MUX_OUT feeds AIN7. The multiplexer is off at boot,
so AIN7 floats (reads a drifting ~250) until it's set to pass MUX_IN
through: register 0x09 = 0x05 on I2C bus 0, address 0x24. `winslowd` does
this itself at startup when the config uses `voltage7`. By hand:
`sudo i2cset -y -f 0 0x24 0x09 0x05` (needs `i2c-tools`).

## Checking the Pico link by hand

With the daemon stopped (`sudo systemctl stop winslowd`), as `debian`
(which is in the `dialout` group):

```sh
stty -F /dev/ttyS0 115200 raw -echo
( sleep 0.3; printf '\xf2' > /dev/ttyS0 ) &
timeout 2 od -An -tx1 /dev/ttyS0
```

Expect `f0 01` (hello), a `bN mm ll` per pot, and `9N` for held buttons. It
goes quiet 2 s later, since nothing keeps sending heartbeats.

## Install

From the laptop: `scripts/deploy.sh`. Logs: `journalctl -u winslowd -b`.
