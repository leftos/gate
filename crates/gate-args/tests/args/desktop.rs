//! `gate desktop hold` and the box's `gate box desktop-hold`: the options of the PC's verb, its hidden seams, and the
//! holder text both take, judged by one rule.

use std::path::PathBuf;

use gate_args::{BoxRequest, BoxSeams, BoxVerb, DesktopHoldRequest, DesktopHoldSeams, HoldRequest, Invocation};

use crate::support::{invocation, problems};

/// The line the box refuses a holder with.
const BOX_REFUSED: &str = "gate box desktop-hold: --holder must be 1 to 80 characters of letters, digits, spaces and - _ . : ( ) / @ + ,";
/// The line the PC refuses a holder with.
const PC_REFUSED: &str = "gate desktop hold: --holder must be 1 to 80 characters of letters, digits, spaces and - _ . : ( ) / @ + ,";

/// `gate box desktop-hold` of `holder` under `C:\r\work` for a ceiling of 60 s.
fn box_words(holder: &str) -> Vec<&str> {
    vec!["box", "desktop-hold", "--root", r"C:\r\work", "--holder", holder, "--ceiling", "60"]
}

/// `gate desktop hold` of `holder` on `box-1` for a ceiling of 60 s.
fn pc_words(holder: &str) -> Vec<&str> {
    vec!["desktop", "hold", "--builder", "box-1", "--holder", holder, "--ceiling", "60"]
}

/// The box's desktop hold [`box_words`] asks for.
fn box_hold(holder: &str) -> Invocation {
    Invocation::Box(BoxRequest {
        verb: BoxVerb::DesktopHold,
        seams: BoxSeams::default(),
        root: Some(PathBuf::from(r"C:\r\work")),
        keys: Vec::new(),
        hold: Some(HoldRequest {
            holder: holder.to_owned(),
            ceiling: 60,
            wait: 0,
            command: Vec::new(),
        }),
    })
}

/// The PC's desktop hold of `holder` on `box-1` for `ceiling` s, waiting `wait` s, with `seams`.
fn pc_hold(holder: &str, ceiling: u64, wait: u64, seams: DesktopHoldSeams) -> Invocation {
    Invocation::DesktopHold(DesktopHoldRequest {
        builder: "box-1".to_owned(),
        holder: holder.to_owned(),
        ceiling,
        wait,
        seams,
    })
}

#[test]
fn a_holder_outside_the_allowed_characters_is_refused() {
    let long = "a".repeat(81);
    for holder in ["", long.as_str(), "a&b", "say \"hi\"", "a\tb", "\u{e9}"] {
        assert_eq!(problems(&box_words(holder)), [BOX_REFUSED], "the box: {holder:?}");
        assert_eq!(problems(&pc_words(holder)), [PC_REFUSED], "the PC: {holder:?}");
    }
}

#[test]
fn a_holder_of_letters_digits_spaces_and_the_allowed_marks_is_taken() {
    let longest = "a".repeat(80);
    for holder in ["builder-desktop MCP (world-of-intrigue)", "a_b.c:d/e@f+g,h", longest.as_str()] {
        assert_eq!(invocation(&box_words(holder)), box_hold(holder), "the box: {holder:?}");
        assert_eq!(
            invocation(&pc_words(holder)),
            pc_hold(holder, 60, 0, DesktopHoldSeams::default()),
            "the PC: {holder:?}"
        );
    }
}

#[test]
fn gate_desktop_hold_parses_its_options() {
    let none = DesktopHoldSeams::default;
    assert_eq!(invocation(&pc_words("mcp 1")), pc_hold("mcp 1", 60, 0, none()), "no wait is a wait of 0");
    assert_eq!(
        invocation(&["desktop", "hold", "--wait=30", "--ceiling=86400", "--holder=mcp 1", "--builder=box-1"]),
        pc_hold("mcp 1", 86_400, 30, none()),
        "in any order, each form"
    );
    assert_eq!(
        invocation(&[
            "desktop",
            "hold",
            "--holder",
            "mcp 1",
            "--builder",
            "box-1",
            "--ceiling",
            "1",
            "--wait",
            "86400"
        ]),
        pc_hold("mcp 1", 1, 86_400, none())
    );
    assert_eq!(invocation(&["desktop", "hold", "--help"]), Invocation::Help, "the usage");
    assert_eq!(
        problems(&[
            "desktop",
            "hold",
            "--builder",
            "box-1",
            "--builder",
            "box-2",
            "--holder",
            "a",
            "--ceiling",
            "9"
        ]),
        ["gate: --builder is given twice"]
    );
    assert_eq!(
        problems(&[
            "desktop",
            "hold",
            "--builder",
            "box-1",
            "--holder",
            "a",
            "--holder",
            "b",
            "--ceiling",
            "9"
        ]),
        ["gate: --holder is given twice"]
    );
    assert_eq!(
        problems(&[
            "desktop",
            "hold",
            "--builder",
            "box-1",
            "--holder",
            "a",
            "--ceiling",
            "9",
            "--wait",
            "1",
            "--wait",
            "2"
        ]),
        ["gate: --wait is given twice"]
    );
    assert_eq!(
        problems(&["desktop", "hold", "--builder", "box-1", "--holder", "a", "--ceiling", "9", "now"]),
        ["gate: unexpected argument now"]
    );
    assert_eq!(
        problems(&["desktop", "hold", "--builder", "box-1", "--holder", "a", "--ceiling", "9", "--slot", "x"]),
        ["gate: unknown option --slot", "gate: unexpected argument x"]
    );
    assert_eq!(problems(&["desktop"]), ["gate: unknown subcommand desktop"]);
    assert_eq!(problems(&["desktop", "drop"]), ["gate: unknown subcommand desktop drop"]);
}

#[test]
fn gate_desktop_hold_needs_builder_holder_and_ceiling() {
    assert_eq!(
        problems(&["desktop", "hold"]),
        [
            "gate: missing --builder <host>",
            "gate: missing --holder \"<text>\"",
            "gate: missing --ceiling <seconds>"
        ]
    );
    assert_eq!(
        problems(&["desktop", "hold", "--holder", "a", "--ceiling", "9"]),
        ["gate: missing --builder <host>"]
    );
    assert_eq!(
        problems(&["desktop", "hold", "--builder", "box-1", "--ceiling", "9"]),
        ["gate: missing --holder \"<text>\""]
    );
    assert_eq!(
        problems(&["desktop", "hold", "--builder", "box-1", "--holder", "a"]),
        ["gate: missing --ceiling <seconds>"]
    );
    assert_eq!(
        problems(&["desktop", "hold", "--builder", "--holder", "a", "--ceiling", "9"]),
        ["gate: --builder needs a value", "gate: missing --builder <host>"]
    );
}

#[test]
fn a_ceiling_of_zero_or_above_a_day_is_refused() {
    let with_ceiling = |ceiling| problems(&["desktop", "hold", "--builder", "box-1", "--holder", "a", "--ceiling", ceiling]);
    for ceiling in ["0", "86401", "-1", "soon", "1.5", "99999999999999999999999"] {
        assert_eq!(
            with_ceiling(ceiling),
            [format!("gate: --ceiling must be a whole number from 1 to 86400, got '{ceiling}'")],
            "{ceiling:?}"
        );
    }
    let with_wait = |wait| problems(&["desktop", "hold", "--builder", "box-1", "--holder", "a", "--ceiling", "9", "--wait", wait]);
    for wait in ["86401", "-1", "soon"] {
        assert_eq!(
            with_wait(wait),
            [format!("gate: --wait must be a whole number from 0 to 86400, got '{wait}'")],
            "{wait:?}"
        );
    }
}

#[cfg(feature = "test-seams")]
#[test]
fn the_interrupt_after_held_seam_parses() {
    let mut words = pc_words("mcp 1");
    words.extend([
        "--test-desktop-hold-interrupt-after-held",
        "--test-remote-transport",
        r"local:C:\b",
        "--test-remote-ssh",
        r"C:\ssh.exe",
    ]);
    let seams = DesktopHoldSeams {
        remote_transport: Some(r"local:C:\b".to_owned()),
        remote_ssh: Some(PathBuf::from(r"C:\ssh.exe")),
        interrupt_after_held: true,
    };
    assert_eq!(invocation(&words), pc_hold("mcp 1", 60, 0, seams));
    let mut twice = pc_words("mcp 1");
    twice.extend(["--test-desktop-hold-interrupt-after-held", "--test-desktop-hold-interrupt-after-held"]);
    assert_eq!(problems(&twice), ["gate: --test-desktop-hold-interrupt-after-held is given twice"]);
    let mut valued = pc_words("mcp 1");
    valued.push("--test-desktop-hold-interrupt-after-held=yes");
    assert_eq!(
        problems(&valued),
        ["gate: unknown option --test-desktop-hold-interrupt-after-held=yes"],
        "a switch takes no value"
    );
}
