use usb_gadget::{
    Class, Config, Gadget, Id, Strings, default_udc, function::midi::Midi, remove_all,
};

pub fn setup_gadget() -> std::io::Result<usb_gadget::RegGadget> {
    remove_all()?;

    let mut midi = Midi::builder();
    midi.id = Some("winslow".into());
    midi.in_ports = Some(1);
    midi.out_ports = Some(1);
    midi.index = Some(1);
    let (_midi, handle) = midi.build();

    let udc = default_udc()?;
    Gadget::new(
        Class::new(0, 0, 0),
        Id::new(0x1209, 0x0001),
        Strings::new("Winslow", "Winslow MIDI", "0001"),
    )
    .with_config(Config::new("MIDI").with_function(handle))
    .bind(&udc)
}
