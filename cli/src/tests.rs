use super::*;

#[test]
fn stopwatch_memory_defaults_preserve_other_boards_and_explicit_overrides() {
    for (board, flash, psram) in [("stopwatch", 16, 8), ("waveshare-amoled18-v2", 8, 2)] {
        let options = parse(&["esp32sim".into(), "--board".into(), board.into()], "esp32s3");
        let machine = setup_s3(&options);
        assert_eq!(machine.bus.flash.len(), flash << 20);
        assert_eq!(machine.bus.psram.len(), psram << 20);
    }
    let options = parse(&["esp32sim".into(), "--board".into(), "stopwatch".into(), "--flash-mb".into(), "8".into(), "--psram-mb".into(), "2".into()], "esp32s3");
    let machine = setup_s3(&options);
    assert_eq!(machine.bus.flash.len(), 8 << 20);
    assert_eq!(machine.bus.psram.len(), 2 << 20);
}

#[test]
fn ble_is_opt_in_and_requires_a_supported_adapter_and_symbols() {
    assert!(!parse(&["esp32sim".into()], "esp32s3").ble);
    assert!(parse(&["esp32sim".into(), "--ble".into()], "esp32s3").ble);
    let symbols = esp_soc::elf::Elf::default();
    assert!(esp32s3::machine([0; 6]).bus.enable_ble(&symbols).is_err());
    assert!(esp32c3::machine([0; 6], 4 << 20).bus.enable_ble(&symbols).is_err());
    assert!(esp32c6::machine([0; 6], 4 << 20).bus.enable_ble(&symbols).is_err());
}


#[test]
fn function_trace_patterns_share_prefix_and_exact_matching() {
    let mut m = esp32c6::machine([0; 6], 4 << 20);
    m.symbols.extend([(1, "foo".into()), (2, "foobar".into()), (3, "other".into())]);
    assert_eq!(m.trace_fns("foo"), 2);
    assert_eq!(m.fn_probes.len(), 2);
    assert!(m.fn_probes.contains_key(&1));
    assert!(m.fn_probes.contains_key(&2));
    m.fn_probes.clear();
    assert_eq!(m.trace_fns("foo$"), 1);
    assert_eq!(m.fn_probes.keys().copied().collect::<Vec<_>>(), [1]);
    assert_eq!(m.trace_fns("missing$"), 0);
}

#[test]
fn stub_values_are_explicit() {
    for (spec, expected) in [("func", 0), ("func=0", 0), ("func=true", 1), ("func=false", 0), ("func=42", 42), ("func=0x2a", 42)] {
        assert_eq!(stub_spec(spec), Ok(("func", expected)));
    }
    for spec in ["func=typo", "func=", "func=-1", "func=4294967296"] {
        assert!(stub_spec(spec).is_err(), "{spec}");
    }
}

#[test]
fn reset_loop_spends_one_run_budget() {
    let mut m = esp32c6::machine([0; 6], 4 << 20);
    // lui t0,0x600b1; lui t1,0x10000; sw t1,0x38(t0): request CPU reset.
    let program: Vec<u8> = [0x600b12b7u32, 0x10000337, 0x0262ac23].into_iter().flat_map(u32::to_le_bytes).collect();
    m.bus.load_bytes(0x4000_0000, &program).unwrap();
    // A watchdog for the regression itself: the old per-boot budget reaches this cap.
    m.max_cycles = 10_000;
    assert!(matches!(run_with_reboots(&mut m, 10, true, false, false), Stop::MaxInsns));
    assert!(m.reboots > 0 && m.reboots < 10, "reboots: {}", m.reboots);
    assert!(m.insns() < 100, "instructions: {}", m.insns());
}

/// A C6 with a one-segment app image at the usual offset: an instruction that jumps to itself, in
/// HP SRAM, entered by `boot_app` the way `--boot app` does it.
fn app_mode_machine() -> esp32c6::Machine {
    let mut m = esp32c6::machine([0; 6], 4 << 20);
    let mut image = vec![0xe9, 1, 0, 0];
    image.extend_from_slice(&0x4080_0000u32.to_le_bytes());              // entry
    image.resize(24, 0);
    image.extend_from_slice(&0x4080_0000u32.to_le_bytes());              // the segment: load address, length, data
    image.extend_from_slice(&4u32.to_le_bytes());
    image.extend_from_slice(&0x0000_006fu32.to_le_bytes());              // j .
    m.bus.write_flash(0x10000, &image).unwrap();
    m.boot_app(0x10000).unwrap();
    m.web = Some(esp_soc::web::WebServer::queued());
    m.max_cycles = 100_000;                                              // a test that never ends is not a result
    m
}

/// The page's Restart on an app-mode run, which is the S3's default boot: the chip resets and the
/// app is entered again. It used to end the run, because only a ROM-booted run was brought back up.
#[test]
fn the_pages_restart_brings_an_app_mode_run_back_up() {
    let mut m = app_mode_machine();
    m.web_restart = true;
    m.web.as_ref().unwrap().push_incoming(r#"{"t":"reset"}"#.into());
    let stop = run_with_reboots(&mut m, 2000, false, true, false);
    assert!(!matches!(stop, Stop::SwReset), "the run ended at the reset: {stop:?}");
    assert_eq!(m.reboots, 1, "one chip reset, then the app again");
    assert_eq!(m.cores[0].pc(), 0x4080_0000, "back in the app");
}

/// With `--no-reboot` the front-end does not offer a restart, and the message is ignored: the page
/// cannot end a run that is meant to stop at a chip reset. A reset the firmware asks for still ends
/// an app-mode run, as before.
#[test]
fn a_run_that_cannot_restart_ignores_the_pages_restart() {
    let mut m = app_mode_machine();
    m.web.as_ref().unwrap().push_incoming(r#"{"t":"reset"}"#.into());
    assert!(matches!(run_with_reboots(&mut m, 2000, false, true, false), Stop::MaxInsns));
    assert_eq!(m.reboots, 0);

    let mut m = app_mode_machine();
    m.web_restart = true;
    esp_soc::SocBus::request_reset(&mut m.bus, esp_periph::RST_SW_CPU);   // not the button: the firmware
    assert!(matches!(run_with_reboots(&mut m, 2000, false, true, false), Stop::SwReset));
}

#[test]
fn approximate_options_reject_unsupported_combinations() {
    for chip in ["c3", "c6", "esp32c3", "esp32c6"] {
        for flag in ["--approximate-timing", "--approximate-cache", "--approximate-memory"] {
            let mut args = vec!["esp32sim".into(), flag.into()];
            if flag == "--approximate-memory" { args.push("3".into()); }
            assert!(validate_timing(&parse(&args, chip)).unwrap_err().contains("require --chip s3"));
        }
    }
    let mut o = Opts { chip: "s3".into(), memory_contention: true, ..Default::default() };
    assert!(validate_timing(&o).unwrap_err().contains("requires --approximate-memory"));
    o.approximate_timing = true;
    o.approximate_memory = Some(3);
    assert!(validate_timing(&o).unwrap_err().contains("requires --boot rom"));
    o.boot = Some("rom".into());
    assert!(validate_timing(&o).is_ok());
}

#[test]
fn timing_cycle_values_report_usage_errors() {
    for name in ["--approximate-memory", "ESP32SIM_CACHE_FILL", "ESP32SIM_CACHE_WRITEBACK"] {
        for value in ["", "wrong", "-1", "4294967296"] {
            assert!(timing_cycles(value, name).unwrap_err().starts_with(name));
        }
        assert_eq!(timing_cycles("0", name), Ok(0));
        assert_eq!(timing_cycles("4294967295", name), Ok(u32::MAX));
    }
}

#[test]
fn ble_scripts_validate_commands_and_require_enable_at_parse_time() {
    let mut m = esp32c3::machine([0; 6], 4 << 20);
    assert!(m.load_script("0 ble connect").unwrap_err().contains("--ble"));
    for command in ["conect", "read", "read 0", "read 65536", "write 3 zz", "write 3 f", "discover extra"] {
        let error = m.load_script(&format!("0 ble {command}")).unwrap_err();
        assert!(error.contains("line 1"));
        assert!(!error.contains("--ble"), "syntax must be checked independently of enable state");
    }
}

#[test]
fn pwm_pins_are_observation_options() {
    let o = parse(&["esp32sim".into(), "--pwm".into(), "4".into(), "--pwm".into(), "21".into()], "s3");
    assert_eq!(o.pwm_pins, [4, 21]);
}

#[test]
fn adc_pins_and_raw_counts_are_host_options() {
    use emu_core::Bus;
    let o = parse(&["esp32sim".into(), "--adc".into(), "1".into(), "--adc".into(), "20".into(),
        "--adc-raw".into(), "1=1234".into(), "--adc-raw".into(), "20=4095".into()], "s3");
    assert_eq!(o.adc_pins, [1, 20]);
    assert_eq!(o.adc_raw, [(1, 1234), (20, 4095)]);
    let mut m = setup_s3(&o);
    apply_adc_raw(&mut m, &o.adc_raw);
    m.bus.write32(0x6000_880c, 1 << 19).unwrap();
    m.bus.write32(0x6000_880c, (1 << 19) | (1 << 17)).unwrap();
    let s = m.bus.adc_observation(1).unwrap();
    assert_eq!(s.raw, 1234);
    assert_eq!(s.generation, 1);
}

#[test]
fn hostfwd_is_repeatable_and_kept_with_wifi() {
    let o = parse(&["esp32sim".into(), "--wifi".into(), "ssid=demo".into(),
        "--hostfwd".into(), "tcp:127.0.0.1:0-:80".into(),
        "--hostfwd".into(), "tcp::0-:443".into()], "s3");
    assert_eq!(o.hostfwd, ["tcp:127.0.0.1:0-:80", "tcp::0-:443"]);
}

#[test]
fn ap_client_and_ap_fwd_default_to_the_hotspot_gateway() {
    let o = parse(&["esp32sim".into(), "--ap-client".into(), "ssid=NerdMinerAP,psk=MineYourCoins".into(),
        "--ap-fwd".into(), "tcp:127.0.0.1:0-:80".into(), "--ap-fwd".into(), "tcp::0-10.0.0.7:23".into()], "esp32");
    assert_eq!((o.ap_client.as_deref(), o.ap_fwd.len()), (Some("ssid=NerdMinerAP,psk=MineYourCoins"), 2));
    assert_eq!(ap_fwd_spec(&o.ap_fwd[0]).unwrap().guest_ip, [192, 168, 4, 1]);
    assert_eq!(ap_fwd_spec(&o.ap_fwd[1]).unwrap().guest_ip, [10, 0, 0, 7]);
    assert!(ap_fwd_spec("udp::0-:80").is_err());
}

/// The control channel's `ap_client` makes a phone at run time and `ap_status` reports it; a
/// forward binds a host port. cyd-lab drives a setup portal this way without restarting the run.
#[test]
fn the_control_channel_makes_and_reports_the_softap_phone() {
    let mut m = esp32::machine([0; 6], 4 << 20);
    let web = esp_soc::web::WebServer::queued();
    m.web = Some(web.clone());
    web.push_incoming(r#"{"t":"ap_status"}"#.into());
    web.push_incoming(r#"{"t":"ap_client","ssid":"NerdMinerAP","psk":"MineYourCoins","fwd":"tcp:127.0.0.1:0-:80"}"#.into());
    web.push_incoming(r#"{"t":"ap_client","psk":"short"}"#.into());
    m.run(1);
    let out: Vec<String> = web.take_outbox().into_iter().filter(|(k, _)| *k == 1).map(|(_, d)| String::from_utf8(d).unwrap()).filter(|s| s.contains("ap_status")).collect();
    assert_eq!(out[0], r#"{"t":"ap_status","bound":null,"phone":null}"#);
    assert!(out[1].contains(r#""bound":"127.0.0.1:"#) && out[1].contains(r#""phone":{"state":"scanning","ip":null,"join":"NerdMinerAP","hotspots":[]}"#), "{}", out[1]);
    assert!(out[2].contains("psk must be 8..=63"), "{}", out[2]);
    let link = esp_soc::SocBus::station_link(&mut m.bus).unwrap();
    assert_eq!(link.phone().unwrap().cfg.psk.as_deref(), Some("MineYourCoins"));
}

/// A change of the phone is pushed once as `ap_status` (no `bound`) without being asked; an
/// unchanged phone is not re-sent, and a reply counts as told.
#[test]
fn the_softap_phone_state_is_pushed_on_change() {
    let mut m = esp32::machine([0; 6], 4 << 20);
    let web = esp_soc::web::WebServer::queued();
    m.web = Some(web.clone());
    const PUSH: u64 = 10_000_000;   // instructions: past one 50 Hz push interval
    let statuses = |web: &esp_soc::web::WebServer| -> Vec<String> { web.take_outbox().into_iter().filter(|(k, _)| *k == 1).map(|(_, d)| String::from_utf8(d).unwrap()).filter(|s| s.contains("ap_status")).collect() };
    m.run(PUSH);
    assert!(statuses(&web).is_empty(), "no phone, nothing to push");
    web.push_incoming(r#"{"t":"ap_client","ssid":"openHASP-1a2b"}"#.into());
    m.run(PUSH);
    assert_eq!(statuses(&web).len(), 1, "the reply alone");
    let link = esp_soc::SocBus::station_link(&mut m.bus).unwrap();
    link.phone_mut().unwrap().configure(esp_soc::wifi::phone::PhoneConfig::parse("ssid=NerdMinerAP").unwrap());
    m.run(PUSH);
    let out = statuses(&web);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].starts_with(r#"{"t":"ap_status","bound":null,"phone":{"state":"scanning","ip":null,"join":"NerdMinerAP""#), "{}", out[0]);
    m.run(PUSH);
    assert!(statuses(&web).is_empty());
}

/// The network the firmware asks for is pushed as `wifi_wanted` on each change, and a late
/// joiner gets the latest.
#[test]
fn the_wanted_network_is_pushed_on_change() {
    let mut m = esp32::machine([0; 6], 4 << 20);
    let web = esp_soc::web::WebServer::queued();
    m.web = Some(web.clone());
    const PUSH: u64 = 10_000_000;
    let wanted = |web: &esp_soc::web::WebServer| -> Vec<String> { web.take_outbox().into_iter().filter(|(k, _)| *k == 1).map(|(_, d)| String::from_utf8(d).unwrap()).filter(|s| s.contains("wifi_wanted")).collect() };
    let link = esp_soc::SocBus::station_link(&mut m.bus).unwrap();
    link.attach(Some(esp_soc::wifi::VirtualAp::new(esp_soc::wifi::ApConfig::parse("any").unwrap(), false)), None);
    m.run(PUSH);
    assert!(wanted(&web).is_empty());
    let mut probe = vec![0x40, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 2, 3, 4, 5, 6, 7, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0, 0];
    probe.extend_from_slice(&[0, 7]); probe.extend_from_slice(b"cyd-lab");
    let link = esp_soc::SocBus::station_link(&mut m.bus).unwrap();
    link.ap_mut().unwrap().on_station_tx(&probe, 0);
    m.run(PUSH);
    assert_eq!(wanted(&web), [r#"{"t":"wifi_wanted","ssid":"cyd-lab","secure":null,"served":true}"#]);
    let mut auth = probe[..24].to_vec();
    auth[0] = 0xb0; auth[4..10].copy_from_slice(&esp_soc::wifi::secure_bssid(&[2, 0x53, 0x49, 0x4d, 0, 1])); auth.extend_from_slice(&[0, 0, 1, 0, 0, 0]);
    let link = esp_soc::SocBus::station_link(&mut m.bus).unwrap();
    link.ap_mut().unwrap().on_station_tx(&auth, 0);
    m.run(PUSH);
    assert_eq!(wanted(&web), [r#"{"t":"wifi_wanted","ssid":"cyd-lab","secure":true,"served":false}"#]);
    m.run(PUSH);
    assert!(wanted(&web).is_empty(), "unchanged: not re-sent");
}

/// `{"t":"wifi"}` changes the emulated access point at run time: answered with `wifi_status`, a
/// bad value with its error, and a `wifi_wanted` that reads the same as before is pushed again.
#[test]
fn the_control_channel_changes_the_access_point_live() {
    let mut m = esp32::machine([0; 6], 4 << 20);
    let web = esp_soc::web::WebServer::queued();
    m.web = Some(web.clone());
    const PUSH: u64 = 10_000_000;
    let texts = |web: &esp_soc::web::WebServer| -> Vec<String> { web.take_outbox().into_iter().filter(|(k, _)| *k == 1).map(|(_, d)| String::from_utf8(d).unwrap()).filter(|s| s.contains("wifi_")).collect() };
    web.push_incoming(r#"{"t":"wifi","ssid":"other-net"}"#.into());
    m.run(PUSH);
    assert_eq!(texts(&web), [r#"{"t":"wifi_status","error":"no emulated access point: run with --wifi"}"#]);
    let link = esp_soc::SocBus::station_link(&mut m.bus).unwrap();
    link.attach(Some(esp_soc::wifi::VirtualAp::new(esp_soc::wifi::ApConfig::parse("ssid=cyd-lab").unwrap(), false)), None);
    let mut probe = vec![0x40, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 2, 3, 4, 5, 6, 7, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0, 0];
    probe.extend_from_slice(&[0, 9]); probe.extend_from_slice(b"other-net");
    link.ap_mut().unwrap().on_station_tx(&probe, 0);
    m.run(PUSH);
    assert_eq!(texts(&web), [r#"{"t":"wifi_wanted","ssid":"other-net","secure":null,"served":false}"#]);
    web.push_incoming(r#"{"t":"wifi","ssid":"x","psk":"short"}"#.into());
    web.push_incoming(r#"{"t":"wifi","ssid":"other-net","psk":"","any":false}"#.into());
    m.run(PUSH);
    let out = texts(&web);
    assert!(out[0].contains("invalid WiFi psk: expected 8 to 63 bytes"), "{out:?}");
    assert_eq!(out[1], r#"{"t":"wifi_status","ssid":"other-net","secure":false,"any":false,"disconnected":false}"#);
    assert_eq!(out.len(), 2, "wanted was cleared: nothing to push");
    let link = esp_soc::SocBus::station_link(&mut m.bus).unwrap();
    assert!(link.ap().unwrap().wanted.is_none());
    link.ap_mut().unwrap().on_station_tx(&probe, 0);
    m.run(PUSH);
    assert_eq!(texts(&web), [r#"{"t":"wifi_wanted","ssid":"other-net","secure":null,"served":true}"#], "the next probe is reported, served now");
    web.push_incoming(r#"{"t":"wifi","psk":"esp32sim-pass","any":true}"#.into());
    m.run(PUSH);
    assert_eq!(texts(&web), [r#"{"t":"wifi_status","ssid":"other-net","secure":true,"any":true,"disconnected":false}"#]);
    // a wanted that reads as before is pushed again after a change
    web.push_incoming(r#"{"t":"wifi","ssid":"other-net"}"#.into());
    m.run(PUSH);
    let link = esp_soc::SocBus::station_link(&mut m.bus).unwrap();
    link.ap_mut().unwrap().on_station_tx(&probe, 0);
    m.run(PUSH);
    let out = texts(&web);
    assert_eq!(out.len(), 2, "{out:?}");
    assert_eq!(out[1], r#"{"t":"wifi_wanted","ssid":"other-net","secure":null,"served":true}"#);
}

#[test]
fn analog_scripts_validate_inputs_and_keep_panel_touch_separate() {
    use esp_soc::ScriptAction;
    let mut m = esp32::machine([0; 6], 4 << 20);
    m.load_script("0.30 adc 34 1.650 # volts\n0 touchpad 4 0\n0.20 touchpad 4 1 # touched\n0.40 touch 450 30 1").unwrap();
    assert!(matches!(m.script.events[0], (0, ScriptAction::TouchPad(4, false))));
    assert!(matches!(m.script.events[1], (48_000_000, ScriptAction::TouchPad(4, true))));
    assert!(matches!(m.script.events[2], (72_000_000, ScriptAction::Analog(34, esp_periph::AnalogSource::Const(1.650)))));
    assert!(matches!(m.script.events[3], (96_000_000, ScriptAction::Touch(450, 30, true))));
    for command in ["adc 34", "adc 34 nope", "adc 256 1", "adc -1 1", "touchpad 4", "touchpad 4 2", "touchpad 4 1 extra", "touchpad 64 0", "touchpad 5 1", "touchpad 34 1"] {
        assert!(m.load_script(&format!("0 {command}")).is_err(), "accepted {command}");
    }
    assert!(esp32s3::machine([0; 6]).load_script("0 touchpad 4 1").is_err());
    assert!(esp32c3::machine([0; 6], 4 << 20).load_script("0 touchpad 4 1").is_err());
    assert!(esp32c6::machine([0; 6], 4 << 20).load_script("0 touchpad 4 1").is_err());
    m.load_script("0 adc 34 0\n0 adc 35 3.3\n0 adc 36 1.6504").unwrap();
    assert!(matches!(m.script.events[0].1, ScriptAction::Analog(34, esp_periph::AnalogSource::Const(0.0))));
    assert!(matches!(m.script.events[1].1, ScriptAction::Analog(35, esp_periph::AnalogSource::Const(3.3))));
    assert!(matches!(m.script.events[2].1, ScriptAction::Analog(36, esp_periph::AnalogSource::Const(1.6504))));
}

#[test]
fn analog_scripts_reach_classic_adc_and_touch_registers() {
    let mut m = esp32::machine([0; 6], 4 << 20);
    m.bus.write32(0x3ff4_8800, 1 << 28).unwrap(); // ADC1 data inversion
    m.bus.write32(0x3ff4_8834, 3 << 12).unwrap(); // channel 6, 11 dB
    m.bus.write32(0x3ff4_8494, 4 << 23).unwrap(); // T0 slope, GPIO mux as configured by IDF 5.5
    m.bus.write32(0x3ff4_888c, 1).unwrap();
    m.bus.write32(0x3ff4_8018, 1 << 23).unwrap(); // touch timer
    for (volts, touched, adc, touch) in [("1.650", 1, 1872, 300), ("0.800", 0, 817, 1000)] {
        m.load_script(&format!("0 adc 34 {volts}\n0 touchpad 4 {touched}\n0 stop")).unwrap();
        assert!(matches!(m.run(1), Stop::Halted));
        let start = (1 << 31) | (1 << 18) | (1 << 25);
        m.bus.write32(0x3ff4_8854, start).unwrap();
        m.bus.write32(0x3ff4_8854, start | (1 << 17)).unwrap();
        assert_eq!(m.bus.read32(0x3ff4_8854).unwrap() & 0xffff, adc);
        assert_eq!(m.bus.read32(0x3ff4_8870).unwrap() >> 16, touch);
    }
}
