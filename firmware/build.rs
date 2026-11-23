fn main() {
    // Enables `#[cfg(dt = "aliases::led0")]` and similar for devicetree nodes.
    zephyr_build::dt_cfgs();
}
