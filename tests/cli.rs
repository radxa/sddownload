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
        .arg("--unpac-path")
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
        "--list-partitions",
        "--fda",
        "--ospi1",
        "--ospi2",
        "--emmc1",
        "--sd",
        "--ssd",
        "--pac",
        "--nvm",
        "--sn",
        "--b-part",
        "--skip-verify",
        "--verify-first",
        "--auto",
        "--reboot",
        "--debug",
        "--unpac-path",
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

    for flag in [
        "--swd",
        "--erase_chip",
        "--b_part",
        "--skip_verify",
        "--unpac_path",
    ] {
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
        "one",
        "one.bin",
        "--flash",
        "two",
        "two.bin",
        "--read",
        "three",
        "three.bin",
        "--read",
        "four",
        "-",
    ])
    .unwrap();

    assert_eq!(options.ospi1.unwrap(), Path::new("override.pac"));
    assert_eq!(options.sn, "second");
    assert_eq!(options.flash, ["one", "one.bin", "two", "two.bin"]);
    assert_eq!(options.read, ["three", "three.bin", "four", "-"]);
}

#[test]
fn list_partitions_is_standalone_and_conflicts_with_device_operations() {
    assert!(
        Cli::try_parse_from(["sddownload", "--list-partitions"])
            .unwrap()
            .list_partitions
    );
    assert!(Cli::try_parse_from(["sddownload", "--list-partitions", "--fda", "fda.bin"]).is_ok());
    for operation in [
        vec!["--flash", "boot", "image.bin"],
        vec!["--read", "boot", "-"],
        vec!["--erase", "boot"],
        vec!["--reboot"],
        vec!["--unpac-path", "out"],
    ] {
        let mut args = vec!["sddownload", "--list-partitions"];
        args.extend(operation);
        let error = Cli::try_parse_from(args).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}

#[test]
fn list_partitions_from_pac_is_offline_and_lists_original_resources() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("firmware.pac");
    let nested = packet(
        0,
        &[
            ("kernel_a", 6, b"image"),
            ("FDA", 0, b"\x7fELFunsupported"),
            ("misc", 6, b""),
            ("empty_a", 6, b""),
            ("emptyFDA", 0, b""),
        ],
        true,
    );
    fs::write(
        &source,
        packet(0, &[("nested", 7, &nested), ("da", 2, b"agent")], true),
    )
    .unwrap();
    let result = cargo_bin_cmd!("sddownload")
        .args([
            "--list-partitions",
            "--auto",
            "--fda",
            "missing-fda.bin",
            "--emmc1",
        ])
        .arg(&source)
        .timeout(Duration::from_secs(3))
        .assert()
        .success();
    let output = String::from_utf8_lossy(&result.get_output().stdout);
    assert!(
        output.starts_with(&format!("PAC: {} (input --emmc1)\n", source.display())),
        "{output}"
    );
    assert_eq!(output.matches(source.to_str().unwrap()).count(), 1);
    assert!(!output.split_whitespace().any(|field| field == "SOURCE") && !output.contains('\t'));
    let rows: Vec<Vec<_>> = output
        .lines()
        .skip(2)
        .map(|line| line.split_whitespace().collect())
        .collect();
    for (name, size, kind, id) in [
        ("emmc1$nested$kernel_a", "5", "IMG", "(6)"),
        ("emmc1$nested$misc", "0", "IMG", "(6)"),
        ("emmc1$nested$empty_a", "0", "IMG", "(6)"),
        ("emmc1$nested$emptyFDA", "0", "FDA", "(0)"),
        ("emmc1$nested$FDA", "15", "FDA", "(0)"),
        ("emmc1$$da", "5", "DLOADER", "(2)"),
    ] {
        assert!(
            rows.iter().any(|row| row[..4] == [name, size, kind, id]),
            "missing {name}: {output}"
        );
    }
    assert!(!output.contains("kernel_b") && !output.contains("empty_b"));
    assert!(output.contains("FDA (bootstrap; not a partition)"));
    assert!(output.contains("download agent (bootstrap; not a partition)"));
    assert!(output.contains("PAC container"));
    let error = String::from_utf8_lossy(&result.get_output().stderr);
    for forbidden in [
        "ADB auto-reboot",
        "ROM FDA:",
        "Caching",
        "Flash ",
        "Download successfully",
    ] {
        assert!(!error.contains(forbidden), "{error}");
    }
    assert!(error.contains("not device partition capacity"), "{error}");
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn list_partitions_shows_sdk_file_type_names_and_unknown_numeric_ids() {
    let types = [
        "FDA", "OSPI_DA", "DLOADER", "SPL", "SFS", "GPT", "IMG", "PAC", "PACK_FBL", "PACK_SBL",
        "PACK_TBL", "PACK_NFS", "PACK_RFD",
    ];
    let child = packet(0, &[], true);
    let names: Vec<_> = (0..types.len())
        .map(|kind| format!("resource_{kind}"))
        .collect();
    let mut entries: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(kind, name)| {
            (
                name.as_str(),
                kind as u32,
                if kind == 7 {
                    child.as_slice()
                } else {
                    b"x".as_slice()
                },
            )
        })
        .collect();
    entries.push(("future_resource", 99, b"x"));
    let result = cargo_bin_cmd!("sddownload")
        .args(["--list-partitions", "--emmc1", "-"])
        .write_stdin(packet(0, &entries, true))
        .timeout(Duration::from_secs(3))
        .assert()
        .success();
    let output = String::from_utf8_lossy(&result.get_output().stdout);
    for (kind, expected) in types.iter().enumerate() {
        let name = format!("emmc1$$resource_{kind}");
        let row = output
            .lines()
            .find(|line| line.split_whitespace().next() == Some(name.as_str()))
            .unwrap();
        let fields: Vec<_> = row.split_whitespace().collect();
        assert_eq!(fields[2], *expected, "{row}");
        assert_eq!(fields[3], format!("({kind})"), "{row}");
    }
    let row = output
        .lines()
        .find(|line| line.starts_with("emmc1$$future_resource "))
        .unwrap();
    assert!(row.contains("UNKNOWN (99)"), "{row}");
}

#[test]
fn list_partitions_from_stdin_handles_metadata_and_explicit_crc_verification() {
    let nested = packet(
        0,
        &[
            ("kernel_a", 6, b"image"),
            ("actual_b", 6, b"b"),
            ("FDA", 0, b"ROM"),
        ],
        true,
    );
    let mut bytes = packet(
        1,
        &[("emmc1", 7, &nested), ("ospi1", 7, b""), ("sd3", 6, b"")],
        true,
    );
    let result = cargo_bin_cmd!("sddownload")
        .args(["--list-partitions", "--pac", "-"])
        .write_stdin(bytes.clone())
        .timeout(Duration::from_secs(3))
        .assert()
        .success();
    let output = String::from_utf8_lossy(&result.get_output().stdout);
    let rows: Vec<Vec<_>> = output
        .lines()
        .skip(2)
        .map(|line| line.split_whitespace().collect())
        .collect();
    for name in [
        "emmc1",
        "ospi1",
        "sd3",
        "emmc1$$kernel_a",
        "emmc1$$actual_b",
        "emmc1$$FDA",
    ] {
        assert!(
            rows.iter().any(|row| row[0] == name),
            "missing {name}: {output}"
        );
    }
    assert!(!output.contains("kernel_b") && !output.contains("actual_a"));
    *bytes.last_mut().unwrap() ^= 1;
    cargo_bin_cmd!("sddownload")
        .args(["--list-partitions", "--pac", "-"])
        .write_stdin(bytes.clone())
        .timeout(Duration::from_secs(3))
        .assert()
        .success();
    let result = cargo_bin_cmd!("sddownload")
        .args(["--list-partitions", "--pac", "-", "--verify-first"])
        .write_stdin(bytes.clone())
        .timeout(Duration::from_secs(3))
        .assert()
        .code(1);
    assert!(result.get_output().stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.get_output().stderr).contains("CRC mismatch"));
    cargo_bin_cmd!("sddownload")
        .args(["--list-partitions", "--pac", "-", "--skip-verify"])
        .write_stdin(bytes)
        .timeout(Duration::from_secs(3))
        .assert()
        .success();
}

#[test]
fn file_listing_is_metadata_only_unless_full_verification_is_requested() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("corrupt-image.pac");
    let mut bytes = packet(0, &[("kernel_a", 6, b"image")], true);
    *bytes.last_mut().unwrap() ^= 1;
    fs::write(&path, bytes).unwrap();
    let result = cargo_bin_cmd!("sddownload")
        .args(["--list-partitions", "--emmc1"])
        .arg(&path)
        .timeout(Duration::from_secs(3))
        .assert()
        .success();
    assert!(String::from_utf8_lossy(&result.get_output().stdout).contains("emmc1$$kernel_a"));
    assert!(String::from_utf8_lossy(&result.get_output().stderr)
        .contains("metadata only; CRCs not checked"));
    let result = cargo_bin_cmd!("sddownload")
        .args(["--list-partitions", "--verify-first", "--emmc1"])
        .arg(&path)
        .timeout(Duration::from_secs(3))
        .assert()
        .code(1);
    assert!(result.get_output().stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.get_output().stderr).contains("CRC mismatch"));
}

#[test]
fn list_partitions_groups_multiple_pacs_without_repeating_source_per_row() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first.pac");
    let second = directory.path().join("second.pac");
    fs::write(
        &first,
        packet(0, &[("FDA", 0, b"ROM"), ("spl", 6, b"spl")], true),
    )
    .unwrap();
    fs::write(
        &second,
        packet(0, &[("kernel_a", 6, b"a"), ("kernel_b", 6, b"b")], true),
    )
    .unwrap();
    let result = cargo_bin_cmd!("sddownload")
        .args(["--list-partitions", "--ospi1"])
        .arg(&first)
        .arg("--emmc1")
        .arg(&second)
        .timeout(Duration::from_secs(3))
        .assert()
        .success();
    let output = String::from_utf8_lossy(&result.get_output().stdout);
    for path in [&first, &second] {
        assert_eq!(
            output.matches(path.to_str().unwrap()).count(),
            1,
            "{output}"
        );
    }
    assert_eq!(output.matches("RESOURCE").count(), 2);
    for name in [
        "ospi1$$FDA",
        "ospi1$$spl",
        "emmc1$$kernel_a",
        "emmc1$$kernel_b",
    ] {
        assert!(output.contains(name), "{output}");
    }
    assert!(
        !output.split_whitespace().any(|field| field == "SOURCE") && !output.contains("inferred")
    );
}

#[test]
fn list_partitions_rejects_wrong_pac_kind_without_device_access() {
    let result = cargo_bin_cmd!("sddownload")
        .args(["--list-partitions", "--pac", "-"])
        .write_stdin(packet(0, &[("kernel", 6, b"image")], true))
        .timeout(Duration::from_secs(3))
        .assert()
        .code(1);
    assert!(result.get_output().stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.get_output().stderr).contains("expects version 1"));
}

#[test]
fn stdin_extraction_and_verify_first_reject_bad_crc_without_usb() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("images");
    let mut bytes = packet(0, &[("kernel", 6, b"payload")], true);
    cargo_bin_cmd!("sddownload")
        .args(["--emmc1", "-", "--unpac-path"])
        .arg(&output)
        .write_stdin(bytes.clone())
        .timeout(Duration::from_secs(3))
        .assert()
        .success();
    assert_eq!(fs::read(output.join("kernel.bin")).unwrap(), b"payload");
    *bytes.last_mut().unwrap() ^= 1;
    let result = cargo_bin_cmd!("sddownload")
        .args(["--auto", "--emmc1", "-", "--verify-first"])
        .write_stdin(bytes)
        .timeout(Duration::from_secs(3))
        .assert()
        .code(1);
    let error = String::from_utf8_lossy(&result.get_output().stderr);
    assert!(error.contains("CRC mismatch"));
    assert!(!error.contains("ADB auto-reboot"));
    assert!(Cli::try_parse_from(["sddownload", "--verify-first", "--skip-verify"]).is_err());
    assert!(sddownload::cli::run_from(["sddownload", "--emmc1", "-", "--ospi1", "-"]) != 0);
}

#[test]
fn normal_stdin_elf_rejection_uses_binary_input_before_device_access() {
    let bytes = packet(0, &[("FDA", 0, b"\x7fELF\r\n\x1a\xff")], true);
    let result = cargo_bin_cmd!("sddownload")
        .args(["--auto", "--emmc1", "-"])
        .write_stdin(bytes)
        .timeout(Duration::from_secs(5))
        .assert()
        .code(1);
    let error = String::from_utf8_lossy(&result.get_output().stderr);
    assert!(error.contains("ELF FDA"), "{error}");
    assert!(!error.contains("ADB auto-reboot"));
}

#[test]
fn explicit_fda_stdin_rejects_invalid_images_and_duplicate_stdin_before_usb() {
    for strict in [false, true] {
        for (bytes, expected) in [
            (b"".as_slice(), "FDA image is empty"),
            (b"\x7fELF\r\n\x1a\xff".as_slice(), "ELF FDA"),
        ] {
            let mut command = cargo_bin_cmd!("sddownload");
            command.args(["--auto", "--fda", "-"]);
            if strict {
                command.arg("--verify-first");
            }
            let result = command
                .write_stdin(bytes)
                .timeout(Duration::from_secs(5))
                .assert()
                .code(1);
            let error = String::from_utf8_lossy(&result.get_output().stderr);
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("ADB auto-reboot"), "{error}");
        }
    }
    let result = cargo_bin_cmd!("sddownload")
        .args(["--auto", "--fda", "-", "--emmc1", "-"])
        .timeout(Duration::from_secs(3))
        .assert()
        .code(1);
    let error = String::from_utf8_lossy(&result.get_output().stderr);
    assert!(
        error.contains("stdin can only be used for one input"),
        "{error}"
    );
    assert!(!error.contains("ADB auto-reboot"));
    assert!(
        Cli::try_parse_from(["sddownload", "--fda", "fda.bin", "--unpac-path", "out"]).is_err()
    );
}

#[test]
fn explicit_fda_file_or_stdin_keeps_verify_first_pac_validation_before_usb() {
    let directory = tempfile::tempdir().unwrap();
    let fda = directory.path().join("fda.bin");
    let pac = directory.path().join("bad.pac");
    fs::write(&fda, b"ROM").unwrap();
    let mut bytes = packet(
        0,
        &[("FDA", 0, b"\x7fELFignored"), ("kernel", 6, b"image")],
        true,
    );
    *bytes.last_mut().unwrap() ^= 1;
    fs::write(&pac, &bytes).unwrap();
    for fda_stdin in [false, true] {
        let mut command = cargo_bin_cmd!("sddownload");
        command.args(["--auto", "--verify-first", "--fda"]);
        if fda_stdin {
            command
                .arg("-")
                .arg("--emmc1")
                .arg(&pac)
                .write_stdin(b"ROM\r\n\x1a\xff");
        } else {
            command
                .arg(&fda)
                .args(["--emmc1", "-"])
                .write_stdin(bytes.clone());
        }
        let result = command.timeout(Duration::from_secs(5)).assert().code(1);
        let error = String::from_utf8_lossy(&result.get_output().stderr);
        assert!(error.contains("CRC mismatch"), "{error}");
        assert!(!error.contains("ELF FDA"), "{error}");
        assert!(!error.contains("ADB auto-reboot"), "{error}");
    }
}

#[test]
fn invalid_arguments_fail_without_device_access() {
    for args in [
        vec![],
        vec!["--unknown"],
        vec!["--auto", "--flash", "x", "missing-file.bin"],
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
fn flash_and_read_require_two_arguments_per_occurrence() {
    for option in ["--flash", "--read"] {
        for arguments in [
            vec!["sddownload", "--auto", option],
            vec!["sddownload", "--auto", option, "boot"],
            vec!["sddownload", "--auto", option, "boot|image.bin"],
            vec!["sddownload", "--auto", option, "boot", "image.bin", "extra"],
            vec![
                "sddownload",
                "--auto",
                option,
                "boot",
                option,
                "system",
                "image.bin",
            ],
        ] {
            assert!(
                Cli::try_parse_from(&arguments).is_err(),
                "accepted {arguments:?}"
            );
        }
        let options =
            Cli::try_parse_from(["sddownload", "--auto", option, "emmc1$$boot", "-"]).unwrap();
        let values = if option == "--flash" {
            options.flash
        } else {
            options.read
        };
        assert_eq!(values, ["emmc1$$boot", "-"]);
        let options = Cli::try_parse_from([
            "sddownload",
            "--auto",
            option,
            "boot",
            "path with spaces.bin",
        ])
        .unwrap();
        let values = if option == "--flash" {
            options.flash
        } else {
            options.read
        };
        assert_eq!(values, ["boot", "path with spaces.bin"]);
    }
}

#[test]
fn flash_stdin_conflicts_fail_before_input_or_device_access() {
    for arguments in [
        vec!["--auto", "--flash", "boot", "-", "--fda", "-"],
        vec![
            "--auto", "--flash", "boot", "-", "--emmc1", "-", "--ssd", "-",
        ],
        vec!["--auto", "--flash", "boot", "-", "--flash", "system", "-"],
    ] {
        let result = cargo_bin_cmd!("sddownload")
            .args(arguments)
            .timeout(Duration::from_secs(3))
            .assert()
            .code(1);
        let error = String::from_utf8_lossy(&result.get_output().stderr);
        assert!(
            error.contains("stdin can only be used for one input"),
            "{error}"
        );
        assert!(!error.contains("ADB auto-reboot"));
        assert!(!error.contains("Staging flash image"));
    }
}

#[test]
fn pac_flash_shared_stdin_reports_missing_selection_without_reopening_input() {
    for strict in [false, true] {
        let mut command = cargo_bin_cmd!("sddownload");
        command.args(["--auto", "--emmc1", "-", "--flash", "emmc1$$missing", "-"]);
        if strict {
            command.arg("--verify-first");
        }
        let result = command
            .write_stdin(packet(0, &[("other", 6, b"image")], true))
            .timeout(Duration::from_secs(3))
            .assert()
            .code(1);
        let error = String::from_utf8_lossy(&result.get_output().stderr);
        assert!(error.contains("PAC --flash selection not found"), "{error}");
        assert!(
            !error.contains("stdin can only")
                && !error.contains("Staging flash image")
                && !error.contains("ADB auto-reboot"),
            "{error}"
        );
    }
}

#[test]
fn pac_flash_shared_stdin_verify_first_rejects_crc_before_device_access() {
    let mut bytes = packet(0, &[("kernel_a", 6, b"image")], true);
    *bytes.last_mut().unwrap() ^= 1;
    let result = cargo_bin_cmd!("sddownload")
        .args([
            "--auto",
            "--emmc1",
            "-",
            "--flash",
            "emmc1$$kernel_a",
            "-",
            "--verify-first",
        ])
        .write_stdin(bytes)
        .timeout(Duration::from_secs(3))
        .assert()
        .code(1);
    let error = String::from_utf8_lossy(&result.get_output().stderr);
    assert!(error.contains("CRC mismatch"), "{error}");
    assert!(
        !error.contains("stdin can only")
            && !error.contains("Staging flash image")
            && !error.contains("ADB auto-reboot"),
        "{error}"
    );
}

#[test]
fn empty_flash_stdin_and_multiple_stdout_reads_fail_before_usb() {
    let result = cargo_bin_cmd!("sddownload")
        .args(["--auto", "--flash", "boot", "-"])
        .write_stdin(b"")
        .timeout(Duration::from_secs(3))
        .assert()
        .code(1);
    let error = String::from_utf8_lossy(&result.get_output().stderr);
    assert!(error.contains("flash image from stdin is empty"), "{error}");
    assert!(!error.contains("ADB auto-reboot"));
    assert!(result.get_output().stdout.is_empty());

    let result = cargo_bin_cmd!("sddownload")
        .args(["--auto", "--read", "boot", "-", "--read", "system", "-"])
        .timeout(Duration::from_secs(3))
        .assert()
        .code(1);
    let error = String::from_utf8_lossy(&result.get_output().stderr);
    assert!(
        error.contains("stdout can only be used for one --read output"),
        "{error}"
    );
    assert!(!error.contains("ADB auto-reboot"));
    assert!(result.get_output().stdout.is_empty());
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
        .arg("--skip-verify")
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
        .arg("--unpac-path")
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
        "--b-part",
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
