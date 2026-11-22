use super::*;
use crate::logs::parse::parse_all;

#[test]
fn os_release_picks_the_manager() {
    let cases: [(&str, Option<PackageManager>); 12] = [
        ("ID=debian\nVERSION_ID=\"12\"\n", Some(PackageManager::Apt)),
        ("ID=ubuntu\nID_LIKE=debian\n", Some(PackageManager::Apt)),
        (
            "ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n",
            Some(PackageManager::Apt),
        ),
        (
            "NAME=\"Arch Linux\"\nID=arch\n",
            Some(PackageManager::Pacman),
        ),
        (
            "ID=endeavouros\nID_LIKE=arch\n",
            Some(PackageManager::Pacman),
        ),
        ("ID=fedora\nVERSION_ID=42\n", Some(PackageManager::Dnf)),
        (
            "ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n",
            Some(PackageManager::Dnf),
        ),
        (
            "ID=\"almalinux\"\nID_LIKE=\"rhel centos fedora\"\n",
            Some(PackageManager::Dnf),
        ),
        ("ID='Ubuntu'\n", Some(PackageManager::Apt)),
        (
            "ID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\"\n",
            None,
        ),
        ("ID=nixos\n", None),
        ("", None),
    ];
    for (text, want) in cases {
        assert_eq!(PackageManager::from_os_release(text), want, "{text:?}");
    }
}

#[test]
fn os_release_ignores_lookalike_keys_and_comments() {
    let text = "# ID=arch\nVARIANT_ID=arch\nID=fedora\n";
    assert_eq!(
        PackageManager::from_os_release(text),
        Some(PackageManager::Dnf)
    );
}

#[test]
fn install_commands_never_run_anything_but_name_sudo_for_linux() {
    assert_eq!(
        PackageManager::Apt.install_command(&["libpq-dev"]),
        "sudo apt install libpq-dev"
    );
    assert_eq!(
        PackageManager::Pacman.install_command(&["libldap", "libsasl"]),
        "sudo pacman -S --needed libldap libsasl"
    );
    assert_eq!(
        PackageManager::Dnf.install_command(&["libpq-devel"]),
        "sudo dnf install libpq-devel"
    );
    assert_eq!(
        PackageManager::Brew.install_command(&["libpq"]),
        "brew install libpq"
    );
}

#[test]
fn build_failures_are_recognised() {
    let cases: &[(&str, Library)] = &[
        ("Error: pg_config executable not found.", Library::Libpq),
        (
            "      pg_config is required to build psycopg2 from source.  Please add the directory",
            Library::Libpq,
        ),
        (
            "psycopg/psycopg.h:36:10: fatal error: Python.h: No such file or directory",
            Library::PythonHeaders,
        ),
        (
            "Modules/LDAPObject.c:4:10: fatal error: lber.h: No such file or directory",
            Library::Ldap,
        ),
        (
            "Modules/constants.h:7:10: fatal error: ldap.h: No such file or directory",
            Library::Ldap,
        ),
        (
            "Modules/LDAPObject.c:16:10: fatal error: sasl/sasl.h: No such file or directory",
            Library::Sasl,
        ),
        (
            "src/lxml/includes/etree_defs.h:14:10: fatal error: libxml/xmlversion.h: No such file or directory",
            Library::Libxml2,
        ),
        (
            "fatal error: libxslt/xsltconfig.h: No such file or directory",
            Library::Libxml2,
        ),
        (
            "Error: Please make sure the libxml2 and libxslt development packages are installed.",
            Library::Libxml2,
        ),
        (
            "The headers or library files could not be found for jpeg,",
            Library::Jpeg,
        ),
        (
            "The headers or library files could not be found for zlib,",
            Library::Zlib,
        ),
        (
            "c/_cffi_backend.c:15:10: fatal error: ffi.h: No such file or directory",
            Library::Ffi,
        ),
        (
            "error: command 'gcc' failed: No such file or directory",
            Library::Compiler,
        ),
        (
            "      error: command '/usr/bin/x86_64-linux-gnu-gcc' failed: No such file or directory",
            Library::Compiler,
        ),
        (
            "unable to execute 'cc': No such file or directory",
            Library::Compiler,
        ),
        (
            "error: command 'clang' failed: No such file or directory",
            Library::Compiler,
        ),
    ];
    for (line, want) in cases {
        assert_eq!(detect_build_failure(line), Some(*want), "{line}");
    }
}

#[test]
fn unrelated_lines_are_not_build_failures() {
    for line in [
        "all good",
        "Building psycopg2==2.9.10",
        "error: command 'gcc' failed with exit code 1",
        "error: command 'rustc' failed: No such file or directory",
        "error: command 'gcc",
        "unable to execute 'cc'",
        "Python.h found",
    ] {
        assert_eq!(detect_build_failure(line), None, "{line}");
    }
}

fn entry(text: &str) -> Entry {
    let mut entries = parse_all(text);
    assert_eq!(entries.len(), 1, "{text}");
    entries.remove(0)
}

#[test]
fn wkhtmltopdf_log_messages_are_recognised() {
    for msg in [
        "You need Wkhtmltopdf to print a pdf version of the reports.",
        "Wkhtmltopdf seems to be broken.",
        "Upgrade Wkhtmltopdf to (at least) 0.12.0",
    ] {
        let e = entry(&format!(
            "2026-11-22 10:00:00,123 4242 INFO ? odoo.addons.base.models.ir_actions_report: {msg}"
        ));
        assert_eq!(detect_log(&e), Some(Library::Wkhtmltopdf), "{msg}");
    }
}

#[test]
fn other_log_entries_are_not_library_hints() {
    for text in [
        // Found and fine.
        "2026-11-22 10:00:00,123 4242 INFO ? odoo.addons.base.models.ir_actions_report: Will use the Wkhtmltopdf binary at /usr/bin/wkhtmltopdf",
        // Same text from another logger (e.g. a custom module quoting it).
        "2026-11-22 10:00:00,123 4242 INFO ? odoo.addons.my_module.models.x: You need Wkhtmltopdf to print a pdf version of the reports.",
        // Not an Odoo record.
        "You need Wkhtmltopdf to print a pdf version of the reports.",
    ] {
        assert_eq!(detect_log(&entry(text)), None, "{text}");
    }
}

#[test]
fn every_library_has_a_package_or_a_note_for_every_manager() {
    for lib in Library::ALL {
        for m in PackageManager::ALL {
            assert!(
                lib.packages(m).is_some() || lib.note(Some(m)).is_some(),
                "{lib:?} on {m}: nothing to tell the user"
            );
            if let Some(p) = lib.packages(m) {
                assert!(!p.is_empty());
                assert!(
                    p.iter().all(|n| !n.is_empty()
                        && n.chars()
                            .all(|c| c.is_ascii_alphanumeric() || "-_.+".contains(c))),
                    "{lib:?} on {m}: {p:?}"
                );
            }
        }
    }
}

#[test]
fn hint_names_the_host_command() {
    let h = Hint::new(Library::Libpq, Some(PackageManager::Pacman));
    assert_eq!(
        h.command.as_deref(),
        Some("sudo pacman -S --needed postgresql-libs")
    );
    assert_eq!(h.commands.len(), 4);
    assert_eq!(
        h.to_string(),
        "PostgreSQL client headers (pg_config) are missing; install with: sudo pacman -S --needed postgresql-libs"
    );
}

#[test]
fn hint_with_unknown_manager_lists_every_command() {
    let h = Hint::new(Library::Ldap, None);
    assert_eq!(h.command, None);
    let text = h.to_string();
    for want in [
        "sudo apt install libldap2-dev libsasl2-dev",
        "sudo pacman -S --needed libldap libsasl",
        "sudo dnf install openldap-devel cyrus-sasl-devel",
        "brew install openldap cyrus-sasl",
    ] {
        assert!(text.contains(want), "{text}");
    }
}

#[test]
fn hint_without_a_package_gives_the_note() {
    let h = Hint::new(Library::Wkhtmltopdf, Some(PackageManager::Pacman));
    assert_eq!(h.command, None);
    let text = h.to_string();
    assert!(!text.contains("install with"), "{text}");
    assert!(text.contains("wkhtmltopdf/packaging/releases"), "{text}");

    let mac = Hint::new(Library::Compiler, Some(PackageManager::Brew));
    assert!(mac.to_string().contains("xcode-select --install"), "{mac}");
}

#[test]
fn hint_keeps_the_note_next_to_the_command() {
    let h = Hint::new(Library::Libpq, Some(PackageManager::Brew));
    let text = h.to_string();
    assert!(text.contains("install with: brew install libpq"), "{text}");
    assert!(text.contains("keg-only"), "{text}");
}

#[test]
fn hint_serializes_flat() {
    let h = Hint::new(Library::Jpeg, Some(PackageManager::Apt));
    let v = serde_json::to_value(&h).expect("json");
    assert_eq!(v["library"], "jpeg");
    assert_eq!(v["manager"], "apt");
    assert_eq!(v["command"], "sudo apt install libjpeg-dev");
    assert_eq!(v["commands"][0]["manager"], "apt");
}

#[test]
fn host_detection_is_stable() {
    assert_eq!(PackageManager::host(), PackageManager::host());
    if cfg!(target_os = "macos") {
        assert_eq!(PackageManager::host(), Some(PackageManager::Brew));
    }
}
