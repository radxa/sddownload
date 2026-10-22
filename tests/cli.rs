use assert_cmd::cargo::cargo_bin_cmd;
use clap::Parser;
use sddownload::{cli::Cli, config::Config, plan::Plan};
use sdimage::pac::{MAGIC, TABLE_END};
use std::{fs, path::Path, time::Duration};

fn packet(version: u32, entries: &[(&str, u32, &[u8])], checksum: bool) -> Vec<u8> {
    let mut bytes = vec![0; TABLE_END as usize];
    bytes[..4].copy_from_slice(&MAGIC.to_le_bytes());
    bytes[4..8].copy_from_slice(&version.to_le_bytes());
    bytes[8..12].copy_from_slice(&[b'E', 0, b'3', 0]);
    bytes[528..532].copy_from_slice(&(entries.len() as u32).to_le_bytes());

    for (index, (name, kind, data)) in entries.iter().enumerate() {
        let table = 1024 + index * 128;

        for (index, word) in name.encode_utf16().enumerate() {
            bytes[table + index * 2..table + index * 2 + 2].copy_from_slice(&word.to_le_bytes());
        }

        let offset = bytes.len() as u64;
        bytes[table + 72..table + 80].copy_from_slice(&(data.len() as u64).to_le_bytes());
        bytes[table + 80..table + 88].copy_from_slice(&offset.to_le_bytes());
        bytes[table + 88..table + 92].copy_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(data);
    }

    if checksum {
        let mut digest = sdimage::BZIP2.digest();
        digest.update(&bytes[..1020]);
        digest.update(&bytes[1024..]);

        bytes[1020..1024].copy_from_slice(&digest.finalize().to_le_bytes());
    }

    bytes
}

fn unpack(source: &Path, output: &Path) -> assert_cmd::Command {
    let mut command = cargo_bin_cmd!("sddownload");
    command
        .args(["--ospi1"])
        .arg(source)
        .arg("--unpac_path")
        .arg(output);
    command.timeout(Duration::from_secs(5));

    command
}

#[test]
fn supported_options_are_visible_and_removed_options_are_rejected() {
    let output = cargo_bin_cmd!("sddownload")
        .arg("--help")
        .assert()
        .success();
    let help = String::from_utf8_lossy(&output.get_output().stdout);

    for flag in [
        "--ospi1",
        "--ospi2",
        "--emmc1",
        "--sd",
        "--ssd",
        "--pac",
        "--nvm",
        "--sn",
        "--b_part",
        "--skip_verify",
        "--auto",
        "--reboot",
        "--debug",
        "--unpac_path",
        "--flash",
        "--erase",
        "--read",
        "--config",
        "--adb-key",
    ] {
        assert!(help.contains(flag), "missing {flag}");
    }

    assert!(!help.contains("--jlink-config"));
    assert!(!help.contains("--jlink-library"));

    for flag in ["--swd", "--erase_chip"] {
        assert!(
            !help.contains(flag),
            "removed option {flag} is still visible"
        );

        let error = Cli::try_parse_from(["sddownload", flag]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }
}

#[test]
fn repeated_scalar_options_override_and_actions_append() {
    let options = Cli::try_parse_from([
        "sddownload",
        "--ospi1",
        "default.pac",
        "--ospi1",
        "override.pac",
        "--sn",
        "first",
        "--sn",
        "second",
        "--flash",
        "one|one.bin",
        "--flash",
        "two|two.bin",
    ])
    .unwrap();

    assert_eq!(options.ospi1.unwrap(), Path::new("override.pac"));
    assert_eq!(options.sn, "second");
    assert_eq!(options.flash, ["one|one.bin", "two|two.bin"]);
}

#[test]
fn invalid_arguments_fail_without_device_access() {
    for args in [
        vec![],
        vec!["--unknown"],
        vec!["--auto", "--flash", "x|missing-file.bin"],
        vec!["--auto", "--read", "x|out|__import__('os')"],
        vec!["--auto", "--erase", "x|1//0"],
        vec!["--auto", "--swd"],
        vec!["--auto", "--erase_chip"],
        vec!["--auto", "--read", "x|out|4096"],
    ] {
        cargo_bin_cmd!("sddownload")
            .args(args)
            .timeout(Duration::from_secs(3))
            .assert()
            .code(1);
    }
}

#[test]
fn defaults_do_not_search_for_configuration_in_working_directory() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("firmware.pac");
    let output = directory.path().join("images");
    let fake_config = directory.path().join("configure");

    fs::write(&source, packet(0, &[("kernel_a", 6, b"payload")], true)).unwrap();
    fs::create_dir(&fake_config).unwrap();
    fs::write(fake_config.join("app.json"), "not valid JSON").unwrap();

    unpack(&source, &output)
        .current_dir(directory.path())
        .assert()
        .success();

    assert_eq!(fs::read(output.join("kernel.bin")).unwrap(), b"payload");
}

#[test]
fn explicit_config_path_is_used() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("firmware.pac");
    let config = directory.path().join("chosen.json");

    fs::write(&source, packet(0, &[("image", 6, b"data")], false)).unwrap();
    fs::write(
        &config,
        r#"{"timeout":{"detect_dev":1},"auto_erase":{"emmc":[]}}"#,
    )
    .unwrap();

    unpack(&source, &directory.path().join("first"))
        .arg("--config")
        .arg(&config)
        .assert()
        .success();

    fs::write(&config, "bad JSON").unwrap();

    unpack(&source, &directory.path().join("second"))
        .arg("--config")
        .arg(&config)
        .assert()
        .code(1);
}

#[test]
fn crc_failure_and_explicit_bypass() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("firmware.pac");
    let mut bytes = packet(0, &[("image", 6, b"data")], true);
    *bytes.last_mut().unwrap() ^= 1;
    fs::write(&source, &bytes).unwrap();

    let rejected = directory.path().join("rejected");
    unpack(&source, &rejected).assert().code(1);
    assert!(!rejected.exists());

    let accepted = directory.path().join("accepted");
    unpack(&source, &accepted)
        .arg("--skip_verify")
        .assert()
        .success();
    assert_eq!(fs::read(accepted.join("image.bin")).unwrap(), b"dat`");
}

#[test]
fn nested_and_all_in_one_layouts() {
    let directory = tempfile::tempdir().unwrap();
    let leaf = packet(0, &[("kernel_a", 6, b"kernel")], true);
    let inner = packet(0, &[("linux", 7, &leaf)], true);
    let source = directory.path().join("nested.pac");
    let output = directory.path().join("nested");

    fs::write(&source, packet(0, &[("ap1", 7, &inner)], true)).unwrap();
    unpack(&source, &output).assert().success();
    assert_eq!(
        fs::read(output.join("ap1/linux/kernel.bin")).unwrap(),
        b"kernel"
    );

    let source = directory.path().join("combined.pac");
    let output = directory.path().join("combined");
    fs::write(&source, packet(1, &[("emmc1", 7, &leaf)], true)).unwrap();

    cargo_bin_cmd!("sddownload")
        .arg("--pac")
        .arg(source)
        .arg("--unpac_path")
        .arg(&output)
        .assert()
        .success();

    assert_eq!(
        fs::read(output.join("emmc1/kernel.bin")).unwrap(),
        b"kernel"
    );
}

#[test]
fn extraction_preserves_existing_files_and_rejects_traversal() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("firmware.pac");
    let output = directory.path().join("images");
    fs::create_dir(&output).unwrap();
    fs::write(output.join("image.bin"), "keep me").unwrap();
    fs::write(&source, packet(0, &[("image", 6, b"replacement")], false)).unwrap();

    unpack(&source, &output).assert().code(1);
    assert_eq!(fs::read(output.join("image.bin")).unwrap(), b"keep me");

    fs::write(&source, packet(0, &[("../outside", 6, b"bad")], false)).unwrap();
    unpack(&source, &output).assert().code(1);
    assert!(!directory.path().join("outside.bin").exists());
}

#[test]
fn elf_loader_can_be_extracted_but_never_selects_a_jlink_backend() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("elf-loader.pac");
    fs::write(&source, packet(0, &[("fda", 0, b"\x7fELF")], false)).unwrap();

    unpack(&source, &directory.path().join("images"))
        .assert()
        .success();

    let result = cargo_bin_cmd!("sddownload")
        .arg("--ospi1")
        .arg(&source)
        .timeout(Duration::from_secs(3))
        .assert()
        .code(1);

    assert!(String::from_utf8_lossy(&result.get_output().stderr).contains("J-Link"));
}

#[test]
fn plan_preserves_bootstrap_order_ab_expansion_and_auto_erase() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("firmware.pac");
    let combined = packet(
        0,
        &[
            ("fda", 0, b"ROM"),
            ("ospi-da", 1, b"ospi loader"),
            ("emmc-da", 2, b"emmc loader"),
            ("ssb-fda", 0, b"ssb loader"),
            ("ssb-da", 2, b"ssb emmc"),
            ("kernel_a", 6, b"kernel"),
            ("misc", 6, b""),
        ],
        true,
    );
    fs::write(&source, combined).unwrap();

    let options = Cli::try_parse_from([
        "sddownload",
        "--emmc1",
        source.to_str().unwrap(),
        "--b_part",
    ])
    .unwrap();
    let settings = Config::load(None).unwrap();
    let plan = Plan::prepare(&options, &settings, &directory.path().join("temporary")).unwrap();
    let names: Vec<_> = plan
        .images
        .iter()
        .map(|image| image.partition.as_str())
        .collect();

    assert_eq!(
        names,
        [
            "ospi1$da",
            "emmc1$da",
            "ssb_fda",
            "ssb_emmc1$da",
            "emmc1$$kernel_a",
            "emmc1$$kernel_b",
        ]
    );
    assert_eq!(plan.auto_erase, ["emmc1$$misc"]);
    assert_eq!(fs::read(plan.fda.unwrap()).unwrap(), b"ROM");
}
