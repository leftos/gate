//! The state root: which root is the machine's default, the hash a non-default root carries, and the kernel object names
//! each root gives.

use gate_state::{KernelObject, StateRoot, default_root, key_hash, log_hash, normalize_root, root_hash};

const LOCAL_APP_DATA: &str = r"C:\Users\someone\AppData\Local";
const DEFAULT_ROOT: &str = r"C:\Users\someone\AppData\Local\gate\v1";
/// The first 8 hex digits of the SHA-256 of the UTF-8 bytes of `c:\gate-test\root`, computed with PowerShell's
/// `Get-FileHash`, not with the crate under test.
const TEST_ROOT_HASH: &str = "7ef6faae";
/// The same for `c:\gate-test\other`.
const OTHER_ROOT_HASH: &str = "263cfe66";

fn root(path: &str) -> StateRoot {
    StateRoot::new(path, Some(&default_root(LOCAL_APP_DATA)))
}

#[test]
fn verbatim_unc_prefix_reads_as_a_unc_path() {
    assert_eq!(normalize_root(r"\\?\UNC\srv\share\x"), r"\\srv\share\x");
    assert_eq!(normalize_root(r"\\?\unc\SRV\share\x\"), r"\\srv\share\x");
    assert_eq!(normalize_root(r"\\srv\share\x"), r"\\srv\share\x");
    assert_eq!(root(r"\\?\UNC\srv\share\x").hash(), root(r"\\srv\share\x").hash());
    assert_ne!(root(r"\\srv\share\x").hash(), root(r"\\srv\share\y").hash());
}

#[test]
fn default_root_is_recognised_case_and_separator_insensitively() {
    for path in [
        DEFAULT_ROOT,
        r"c:\users\SOMEONE\appdata\local\GATE\V1\",
        "C:/Users/someone/AppData/Local/gate/v1/",
        r"\\?\C:\Users\someone\AppData\Local\gate\v1",
    ] {
        assert!(root(path).is_default(), "{path} is the default root");
    }
    assert!(
        StateRoot::new(DEFAULT_ROOT, Some(&default_root(r"C:\Users\someone\AppData\Local\"))).is_default(),
        "a trailing separator on LOCALAPPDATA changes nothing"
    );
    for path in [
        r"C:\Users\someone\AppData\Local\gate",
        r"C:\Users\someone\AppData\Local\gate\v2",
        r"D:\Users\someone\AppData\Local\gate\v1",
    ] {
        assert!(!root(path).is_default(), "{path} is not the default root");
    }
    assert!(
        !StateRoot::new(DEFAULT_ROOT, None).is_default(),
        "without LOCALAPPDATA no root is the default"
    );
}

#[test]
fn default_root_names_carry_no_hash() {
    let root = root(DEFAULT_ROOT);

    assert_eq!(root.hash(), None);
    assert_eq!(root.object_name(KernelObject::HeavySlot(1)), r"Local\gate-slot-1");
    assert_eq!(root.object_name(KernelObject::Critical), r"Local\gate-critical");
}

#[test]
fn non_default_root_names_carry_8_hex_of_sha256() {
    let root = root(r"C:\gate-test\root");

    assert_eq!(root_hash(r"c:\gate-test\root"), TEST_ROOT_HASH);
    assert_eq!(root.hash(), Some(TEST_ROOT_HASH));
    assert_eq!(
        root.object_name(KernelObject::HeavySlot(1)),
        format!(r"Local\gate-{TEST_ROOT_HASH}-slot-1")
    );
}

#[test]
fn hash_ignores_trailing_separator_case_and_slashes() {
    for path in [r"C:\GATE-TEST\Root\", "c:/gate-test/root", r"c:\gate-test\root\\", "C:/Gate-Test/ROOT/"] {
        assert_eq!(normalize_root(path), r"c:\gate-test\root", "{path} normalises");
        assert_eq!(root(path).hash(), Some(TEST_ROOT_HASH), "{path} hashes as the test root");
    }
}

#[test]
fn verbatim_prefix_is_stripped() {
    assert_eq!(normalize_root(r"\\?\C:\gate-test\root"), r"c:\gate-test\root");
    assert_eq!(root(r"\\?\C:\gate-test\root\").hash(), Some(TEST_ROOT_HASH));
}

#[test]
fn two_roots_give_two_hashes() {
    let first = root(r"C:\gate-test\root");
    let second = root(r"C:\gate-test\other");

    assert_eq!(second.hash(), Some(OTHER_ROOT_HASH));
    assert_ne!(first.hash(), second.hash());
    assert_ne!(
        first.object_name(KernelObject::Journal),
        second.object_name(KernelObject::Journal),
        "two test roots never share a journal mutex"
    );
}

#[test]
fn claim_files_drop_local_and_the_root_hash() {
    let rows = [
        (KernelObject::HeavySlot(2), "gate-slot-2.json"),
        (KernelObject::LightSlot(0), "gate-light-slot-0.json"),
        (KernelObject::RemoteSlot(1), "gate-remote-slot-1.json"),
        (KernelObject::Critical, "gate-critical.json"),
    ];
    for (object, file_name) in rows {
        assert_eq!(object.claim_file_name().as_deref(), Some(file_name), "{object:?}");
    }

    for object in [
        KernelObject::Journal,
        KernelObject::WakeEvent { pid: 4242, start_ms: 1 },
        KernelObject::Job { gate_pid: 4242 },
        KernelObject::Log { hash: 1 },
        KernelObject::Routed { hash: 1 },
        KernelObject::RemoteCutover,
    ] {
        assert_eq!(object.claim_file_name(), None, "{object:?} has no claim");
    }
}

#[test]
fn a_log_mutex_is_named_by_16_hex_of_its_normalised_path() {
    let hash = log_hash(r"C:\Work\Out.log");

    assert_eq!(hash, log_hash(r"c:/work/out.log"), "case and separators do not matter");
    assert_eq!(hash, log_hash(r"\\?\C:\WORK\OUT.LOG"), "nor does a verbatim prefix");
    assert_ne!(hash, log_hash(r"C:\Work\Other.log"));
    let name = root(r"C:\gate-test\root").object_name(KernelObject::Log { hash: 0xab });
    assert_eq!(name, format!(r"Local\gate-{TEST_ROOT_HASH}-log-00000000000000ab"));
    assert_eq!(
        root(DEFAULT_ROOT).object_name(KernelObject::Log { hash }),
        format!(r"Local\gate-log-{hash:016x}")
    );
}

#[test]
fn object_names_match_the_design_table() {
    let default = root(DEFAULT_ROOT);
    let other = root(r"C:\gate-test\root");
    let rows = [
        (KernelObject::HeavySlot(2), r"Local\gate-slot-2", r"Local\gate-7ef6faae-slot-2"),
        (
            KernelObject::LightSlot(0),
            r"Local\gate-light-slot-0",
            r"Local\gate-7ef6faae-light-slot-0",
        ),
        (
            KernelObject::RemoteSlot(1),
            r"Local\gate-remote-slot-1",
            r"Local\gate-7ef6faae-remote-slot-1",
        ),
        (KernelObject::Critical, r"Local\gate-critical", r"Local\gate-7ef6faae-critical"),
        (KernelObject::Journal, r"Local\gate-journal", r"Local\gate-7ef6faae-journal"),
        (
            KernelObject::WakeEvent {
                pid: 4242,
                start_ms: 1_760_000_000_123,
            },
            r"Local\gate-wake-4242-1760000000123",
            r"Local\gate-7ef6faae-wake-4242-1760000000123",
        ),
        (
            KernelObject::Job { gate_pid: 4242 },
            r"Local\gate-job-4242",
            r"Local\gate-7ef6faae-job-4242",
        ),
        (
            KernelObject::Routed { hash: 0xab },
            r"Local\gate-routed-00000000000000ab",
            r"Local\gate-7ef6faae-routed-00000000000000ab",
        ),
        (
            KernelObject::RemoteCutover,
            r"Local\gate-remote-cutover",
            r"Local\gate-7ef6faae-remote-cutover",
        ),
    ];

    for (object, default_name, other_name) in rows {
        assert_eq!(default.object_name(object), default_name, "{object:?} under the default root");
        assert_eq!(other.object_name(object), other_name, "{object:?} under a non-default root");
    }
}

/// The first 16 hex digits of the SHA-256 of the UTF-8 bytes of `app-1a2b3c4d`, computed with `sha256sum`, not with the
/// crate under test.
const KEY_HASH: u64 = 0x4fa4_4c98_cd5b_3bf3;

#[test]
fn a_routed_mutex_is_named_by_16_hex_of_its_lower_cased_key() {
    assert_eq!(key_hash("app-1a2b3c4d"), KEY_HASH);
    assert_eq!(key_hash("App-1A2B3C4D"), KEY_HASH, "the key's letter case does not matter");
    assert_ne!(key_hash("app-1a2b3c4e"), KEY_HASH, "another key, another hash");
    assert_eq!(
        root(r"C:\gate-test\root").object_name(KernelObject::Routed {
            hash: key_hash("APP-1a2b3c4d")
        }),
        format!(r"Local\gate-{TEST_ROOT_HASH}-routed-{KEY_HASH:016x}")
    );
}
