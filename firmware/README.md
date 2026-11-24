# Winslow Pico firmware

Zephyr + Rust firmware for a Raspberry Pi Pico (RP2040). It scans buttons,
rotary encoders and pots and reports them to the PocketBeagle over UART. The wire
format lives in [`crates/protocol`](../crates/protocol/src/lib.rs); the
debouncing, encoder decoding and pot smoothing in [`crates/scan`](../crates/scan/src/lib.rs),
both shared/testable on the laptop:

```sh
cargo test -p winslow-protocol -p winslow-scan
```

## Inputs and wiring

Inputs are defined in [`boards/rpi_pico.overlay`](boards/rpi_pico.overlay);
adding one there is all it takes (no code changes). Each kind is numbered in
the order listed, max 16. Buttons 1-6 switch to 3.3 V (internal pull-downs);
the encoders and encoder 2's push switch switch to ground (internal pull-ups); pots are wired across ADC_VREF (pin 35) and AGND (pin 33).

| Protocol index | Input            | Pins                    |
|----------------|------------------|-------------------------|
| button 0-2     | buttons 1-3      | GP4, GP3, GP2           |
| button 3-5     | buttons 4-6      | GP18, GP19, GP20        |
| button 6       | encoder 2 push   | GP8                     |
| encoder 0      | encoder 1        | GP14, GP15 (A = GP15)   |
| encoder 1      | encoder 2        | CLK GP9, DT GP7         |
| encoder 2      | encoder 3        | CLK GP5, DT GP6         |
| pot 0-2        | pots             | GP26, GP27, GP28 (ADC0-2) |

If an encoder counts backwards, swap its `a-gpios` and `b-gpios`.

| Pico         | PocketBeagle     |
|--------------|------------------|
| GP0 (pin 1)  | P2.05 (UART4 RX) |
| GP1 (pin 2)  | P2.07 (UART4 TX) |
| GND          | GND              |

Logs go to the Pico's USB port as a serial device (`/dev/ttyACM0` on the
host, e.g. `picocom /dev/ttyACM0`). The on-board LED blinks every 500 ms
while the firmware is running.

## One-time setup

Rust support is an optional Zephyr module. In the west workspace
(`~/zephyrproject`):

```sh
source .venv/bin/activate
west config manifest.project-filter -- +zephyr-lang-rust
west update zephyr-lang-rust
rustup target add thumbv6m-none-eabi
```

The revision Zephyr pins (June 2025) lacks the UART and `gpio-keys`
bindings, so check out a newer one (tested with `6823b69`, Oct 2026):

```sh
cd modules/lang/rust
git fetch https://github.com/zephyrproject-rtos/zephyr-lang-rust main
git checkout 6823b69
```

Note that `west update` resets it to the pinned revision; repeat the checkout
afterwards.

## Build and flash

```sh
cd ~/zephyrproject && source .venv/bin/activate
west build -b rpi_pico -d ~/Projects/winslow/firmware/build ~/Projects/winslow/firmware
```

Hold BOOTSEL while plugging the Pico in, then copy
`build/zephyr/zephyr.uf2` to the `RPI-RP2` drive that appears.
