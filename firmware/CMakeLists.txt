# SPDX-License-Identifier: Apache-2.0
cmake_minimum_required(VERSION 3.20.0)

# Console/logs over the USB port (/dev/ttyACM0 on the host), since UART0 on
# GP0/GP1 is the link to the PocketBeagle.
set(SNIPPET cdc-acm-console CACHE STRING "")

find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(winslow_pico)

# Collects the inputs and link UART from the devicetree, and reads the ADC.
target_sources(app PRIVATE src/board.c)

rust_cargo_application()
