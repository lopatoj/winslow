/*
 * Gathers the devicetree's buttons, encoders and link UART into plain arrays
 * the Rust code can use. Devicetree access is all compile-time C macros, so
 * this is the simplest bridge.
 */

#include <zephyr/device.h>
#include <zephyr/drivers/adc.h>
#include <zephyr/devicetree.h>
#include <zephyr/drivers/gpio.h>
#include <zephyr/sys/util.h>

/* Must match `Encoder` in src/board.rs. */
struct winslow_encoder {
	struct gpio_dt_spec a;
	struct gpio_dt_spec b;
	uint32_t steps_per_detent;
};

#define BUTTONS DT_COMPAT_GET_ANY_STATUS_OKAY(winslow_buttons)
#define ENCODERS DT_COMPAT_GET_ANY_STATUS_OKAY(winslow_encoders)

#define BUTTON(node) GPIO_DT_SPEC_GET(node, gpios),
#define ENCODER(node)                                                          \
	{                                                                      \
		.a = GPIO_DT_SPEC_GET(node, a_gpios),                          \
		.b = GPIO_DT_SPEC_GET(node, b_gpios),                          \
		.steps_per_detent = DT_PROP(node, steps_per_detent),           \
	},

#if DT_NODE_EXISTS(BUTTONS)
static const struct gpio_dt_spec buttons[] = {DT_FOREACH_CHILD_STATUS_OKAY(BUTTONS, BUTTON)};
#define BUTTON_COUNT ARRAY_SIZE(buttons)
#else
static const struct gpio_dt_spec buttons[1];
#define BUTTON_COUNT 0
#endif

#if DT_NODE_EXISTS(ENCODERS)
static const struct winslow_encoder encoders[] = {DT_FOREACH_CHILD_STATUS_OKAY(ENCODERS, ENCODER)};
#define ENCODER_COUNT ARRAY_SIZE(encoders)
#else
static const struct winslow_encoder encoders[1];
#define ENCODER_COUNT 0
#endif

#define POTS DT_COMPAT_GET_ANY_STATUS_OKAY(winslow_pots)
#define POT(node, prop, idx) ADC_DT_SPEC_GET_BY_IDX(node, idx),

#if DT_NODE_EXISTS(POTS)
static const struct adc_dt_spec pots[] = {DT_FOREACH_PROP_ELEM(POTS, io_channels, POT)};
#define POT_COUNT ARRAY_SIZE(pots)
#else
static const struct adc_dt_spec pots[1];
#define POT_COUNT 0
#endif

BUILD_ASSERT(POT_COUNT <= 16, "at most 16 pots");
BUILD_ASSERT(BUTTON_COUNT <= 16, "at most 16 buttons");
BUILD_ASSERT(ENCODER_COUNT <= 16, "at most 16 encoders");

const struct gpio_dt_spec *winslow_buttons(size_t *count)
{
	*count = BUTTON_COUNT;
	return buttons;
}

const struct winslow_encoder *winslow_encoders(size_t *count)
{
	*count = ENCODER_COUNT;
	return encoders;
}

const struct device *winslow_link_uart(void)
{
	return DEVICE_DT_GET(DT_CHOSEN(winslow_link_uart));
}

size_t winslow_pot_count(void)
{
	return POT_COUNT;
}

/* Configure every pot's ADC channel. Returns 0 or a negative errno. */
int winslow_pots_init(void)
{
	for (size_t i = 0; i < POT_COUNT; i++) {
		if (!adc_is_ready_dt(&pots[i])) {
			return -ENODEV;
		}
		int err = adc_channel_setup_dt(&pots[i]);
		if (err) {
			return err;
		}
	}
	return 0;
}

/* One conversion of pot `i`: the raw 12-bit value, or a negative errno. */
int winslow_pot_read(size_t i)
{
	if (i >= POT_COUNT) {
		return -EINVAL;
	}
	uint16_t sample;
	struct adc_sequence sequence = {
		.buffer = &sample,
		.buffer_size = sizeof(sample),
	};
	int err = adc_sequence_init_dt(&pots[i], &sequence);
	if (!err) {
		err = adc_read(pots[i].dev, &sequence);
	}
	return err ? err : sample;
}
