use emu_core::bus::Bus as _;

/// IDF's btdm_controller_mem_init copies .data_btdm (0x3ffae6e0) from the ROM pointer at
/// 0x4000d4f4; its r_modules_funcs_p (0x3ffafd68) must come through non-zero, or
/// btdm_r_modules_func_p_get returns NULL and the controller faults at EXCVADDR 0x19c.
#[test]
#[ignore = "set ESP32SIM_ROM_DIR to the directory containing esp32_rev300_rom.elf"]
fn bt_controller_rom_initialiser_is_back_filled() {
    let rom = std::path::PathBuf::from(std::env::var_os("ESP32SIM_ROM_DIR").expect("ESP32SIM_ROM_DIR must contain esp32_rev300_rom.elf")).join("esp32_rev300_rom.elf");
    let mut m = esp32::machine([0; 6], 4 << 20);
    m.load_rom(&std::fs::read(rom).expect("a readable esp32_rev300_rom.elf")).unwrap();
    let source = m.bus.read32(0x4000_d4f4).unwrap();
    assert_eq!(source, 0x4000_d890, "the ELF leaves 0x4000d890..0x4000f0c0 out");
    assert_ne!(m.bus.read32(source + (0x3ffa_fd68 - 0x3ffa_e6e0)).unwrap(), 0, "r_modules_funcs_p's initialiser");
}
