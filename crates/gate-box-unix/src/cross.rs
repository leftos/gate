//! A cross build's outputs on a macOS builder: the output path map, which lists a file a cross build for a runtime
//! identifier wrote at the path a native Windows build writes it, and the portability rules, which keep the builder's
//! own programs off the PC. Pure: the walk and the pack in [`crate::outputs`] call them, so they build and are tested
//! on every target.

use std::collections::HashSet;
use std::io;
use std::path::Path;

use gate_inputs::manifest::Chosen;
use gate_inputs::walk::Found;
use gate_wire::{escaped, ordinal_upper};

/// The bytes of a file's start the portability check reads: a fat Mach-O's magic and its arch count.
pub const HEAD_BYTES: u64 = 8;
/// The first four bytes of a program the PC cannot run, whatever follows them: Mach-O 64-bit and 32-bit in either byte
/// order, a 64-bit fat Mach-O, and ELF.
const NATIVE_MAGIC: [[u8; 4]; 6] = [
    [0xCF, 0xFA, 0xED, 0xFE],
    [0xCE, 0xFA, 0xED, 0xFE],
    [0xFE, 0xED, 0xFA, 0xCE],
    [0xFE, 0xED, 0xFA, 0xCF],
    [0xCA, 0xFE, 0xBA, 0xBF],
    [0x7F, 0x45, 0x4C, 0x46],
];
/// A 32-bit fat Mach-O's magic, which a Java `.class` file shares: told apart by the big-endian count after it.
const FAT_MAGIC: [u8; 4] = [0xCA, 0xFE, 0xBA, 0xBE];
/// The most arches a fat Mach-O's count is read as; a `.class` file has its version there, 45 or more.
const FAT_ARCHES_MAX: u32 = 30;
/// Why a run's outputs are not brought back when its cross build did not pass.
pub const FAILED_REASON: &str = "the cross build failed";
/// Why a run's outputs are not brought back when its cross build left a program the PC cannot run among them.
pub const REFUSED_REASON: &str = "the cross build left a non-Windows file";

/// The `/`-separated `path` without the `rid` segment of its first run of segments `bin`, any two, then `rid`, followed
/// by at least `rest` more; `None` when it has no such run.
fn without_rid(path: &str, rid: &str, rest: usize) -> Option<String> {
    let mut segments: Vec<&str> = path.split('/').collect();
    let bin = segments.windows(4 + rest).position(|run| run[0] == "bin" && run[3] == rid)?;
    segments.remove(bin + 3);
    Some(segments.join("/"))
}

/// The PC path of the box file `path`, `/`-separated, that a cross build for `rid` wrote: a path whose segments run
/// `…/bin/{c}/{tfm}/{rid}/{rest}`, exactly `bin`, any two segments, `rid`, then at least one more, without its `rid`
/// segment, `…/bin/{c}/{tfm}/{rest}`; `None` for a path of any other shape. The first such run of segments is mapped.
#[must_use]
pub fn mapped_path(path: &str, rid: &str) -> Option<String> {
    without_rid(path, rid, 1)
}

/// The PC path of the box folder `path` a cross build for `rid` wrote to, as [`mapped_path`] maps a file in it: the
/// `rid` folder itself, `…/bin/{c}/{tfm}/{rid}`, is `…/bin/{c}/{tfm}`; `None` for a folder of any other shape.
#[must_use]
pub fn mapped_folder(path: &str, rid: &str) -> Option<String> {
    without_rid(path, rid, 0)
}

/// Whether the walk listed `found` at a path the output path map gave it: its PC path is not the end of its full path
/// on the box, as a file the walk lists at its own relative path always is.
#[must_use]
pub fn is_mapped(found: &Found) -> bool {
    !found.full.ends_with(Path::new(&found.path))
}

/// `found`, the files a walk listed at their PC paths, in their order, less each native file at a mapped file's PC path
/// ([`is_mapped`]), compared as the PC compares names ([`ordinal_upper`]): the mapped one is the only file at it.
#[must_use]
pub fn crossed(mut found: Vec<Found>) -> Vec<Found> {
    let taken: HashSet<String> = found
        .iter()
        .filter(|file| is_mapped(file))
        .map(|file| ordinal_upper(&file.path))
        .collect();
    found.retain(|file| is_mapped(file) || !taken.contains(&ordinal_upper(&file.path)));
    found
}

/// Whether `head`, a file's first [`HEAD_BYTES`] bytes or all of a shorter one, opens a program the PC cannot run:
/// Mach-O or ELF. A 32-bit fat Mach-O counts only with 1 to 30 arches after its magic, so a Java `.class` file, and a
/// file too short to hold the count, is none.
#[must_use]
pub fn is_native_binary(head: &[u8]) -> bool {
    let Some(magic) = head.get(..4) else {
        return false;
    };
    if NATIVE_MAGIC.iter().any(|native| native.as_slice() == magic) {
        return true;
    }
    magic == FAT_MAGIC
        && head
            .get(4..8)
            .and_then(|count| <[u8; 4]>::try_from(count).ok())
            .is_some_and(|count| (1..=FAT_ARCHES_MAX).contains(&u32::from_be_bytes(count)))
}

/// What a cross build's chosen outputs bring back.
#[derive(Debug, PartialEq, Eq)]
pub enum Portable<'f> {
    /// These, in their order.
    Kept(Vec<Chosen<'f>>),
    /// Nothing: the mapped file at this PC path is a program the PC cannot run, so the cross build did not build for
    /// the PC.
    Refused(String),
}

/// The files of `chosen`, the files picked to send, that the PC can take, each's start read with `head`
/// ([`is_native_binary`]): a native one that is a Mach-O or ELF program is left out; a mapped one ([`is_mapped`])
/// refuses them all, naming its PC path. Only the files chosen are read.
///
/// # Errors
///
/// The first read of `head` that failed.
pub fn portable(chosen: Vec<Chosen<'_>>, mut head: impl FnMut(&Found) -> io::Result<Vec<u8>>) -> io::Result<Portable<'_>> {
    let mut kept = Vec::with_capacity(chosen.len());
    for file in chosen {
        if !is_native_binary(&head(file.file)?) {
            kept.push(file);
        } else if is_mapped(file.file) {
            return Ok(Portable::Refused(file.file.path.clone()));
        }
    }
    Ok(Portable::Kept(kept))
}

/// The line a cross build for `rid` that passed in `seconds` says.
#[must_use]
pub fn passed_line(rid: &str, seconds: f64) -> String {
    format!("gate: the cross build for {rid} took {seconds:.1} s")
}

/// The line a cross build for `rid` that ended with the status `status` says.
#[must_use]
pub fn failed_line(rid: &str, status: u8) -> String {
    format!("gate: the cross build for {rid} failed with exit {status}; the Windows outputs were not brought back")
}

/// The line a cross build for `rid` that left the program the PC cannot run at the PC path `path` says, the path shown
/// with its control characters escaped.
#[must_use]
pub fn refused_line(rid: &str, path: &str) -> String {
    format!(
        "gate: the cross build for {rid} left a non-Windows file {}; the Windows outputs were not brought back",
        escaped(path)
    )
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a head or a glob a case cannot read must fail the case loudly")]
mod tests {
    use std::collections::HashMap;
    use std::io;
    use std::path::PathBuf;

    use gate_inputs::glob::Globs;
    use gate_inputs::manifest::{Chosen, Stamp};
    use gate_inputs::walk::Found;

    use super::{Portable, crossed, failed_line, is_mapped, is_native_binary, mapped_folder, mapped_path, passed_line, portable, refused_line};

    const RID: &str = "win-x64";

    /// The file a walk finds at the box path `path` below `/work`, at its PC path.
    fn found(path: &str) -> Found {
        Found {
            path: mapped_path(path, RID).unwrap_or_else(|| path.to_owned()),
            full: PathBuf::from("/work").join(path),
            stamp: Stamp { size: 1, modified: 2 },
        }
    }

    #[test]
    fn the_map_takes_the_rid_folder_out_below_bin_and_two_folders() {
        for (path, pc) in [
            ("src/A/bin/Debug/net10.0/win-x64/A.exe", "src/A/bin/Debug/net10.0/A.exe"),
            (
                "tools/M/bin/Release/net10.0-windows/win-x64/recorder/R.exe",
                "tools/M/bin/Release/net10.0-windows/recorder/R.exe",
            ),
            ("bin/Debug/net10.0/win-x64/A.exe", "bin/Debug/net10.0/A.exe"),
        ] {
            assert_eq!(mapped_path(path, RID).as_deref(), Some(pc), "{path}");
        }
    }

    #[test]
    fn the_map_leaves_a_path_of_another_shape_alone() {
        for path in [
            "src/A/bin/Debug/win-x64/A.dll",
            "src/A/obj/Debug/net10.0/win-x64/A.dll",
            "src/A/bin/Debug/net10.0/osx-arm64/A.dll",
            "src/A/bin/Debug/net10.0/win-x64",
            "src/A/bin/Debug/net10.0/A.dll",
            "src/A/Bin/Debug/net10.0/win-x64/A.dll",
        ] {
            assert_eq!(mapped_path(path, RID), None, "{path}");
        }
    }

    #[test]
    fn the_folder_map_takes_the_rid_folder_out_of_the_folder_itself_and_below_it() {
        assert_eq!(
            mapped_folder("src/A/bin/Debug/net10.0/win-x64", RID).as_deref(),
            Some("src/A/bin/Debug/net10.0")
        );
        assert_eq!(
            mapped_folder("src/A/bin/Debug/net10.0/win-x64/recorder", RID).as_deref(),
            Some("src/A/bin/Debug/net10.0/recorder")
        );
        assert_eq!(mapped_folder("src/A/bin/Debug/win-x64", RID), None);
        assert_eq!(mapped_folder("src/A/obj/Debug/net10.0/win-x64", RID), None);
    }

    #[test]
    fn a_glob_on_the_pc_path_matches_a_cross_built_file_and_enters_its_folder() {
        let globs = Globs::new(&["tests/*/bin/*/*/*.dll".to_owned()]).expect("the glob is fine");
        let folder = mapped_folder("tests/T/bin/Debug/net10.0/win-x64", RID).expect("the rid folder maps");
        assert!(globs.enters(&folder), "{folder}");
        let file = mapped_path("tests/T/bin/Debug/net10.0/win-x64/T.dll", RID).expect("the file maps");
        assert_eq!(file, "tests/T/bin/Debug/net10.0/T.dll");
        assert!(globs.matches(&file), "{file}");
    }

    #[test]
    fn a_cross_mapped_file_wins_over_a_native_file_at_its_path_and_a_lone_native_file_stays() {
        let files = vec![
            found("bin/Debug/net10.0/A"),
            found("bin/Debug/net10.0/A.dll"),
            found("bin/Debug/net10.0/win-x64/A.dll"),
            found("bin/Debug/net10.0/win-x64/A.exe"),
            found("bin/Debug/net10.0/b.json"),
            found("bin/Debug/net10.0/win-x64/B.JSON"),
        ];
        let kept: Vec<(String, PathBuf, bool)> = crossed(files)
            .into_iter()
            .map(|found| {
                let mapped = is_mapped(&found);
                (found.path, found.full, mapped)
            })
            .collect();
        assert_eq!(
            kept,
            [
                ("bin/Debug/net10.0/A".to_owned(), PathBuf::from("/work/bin/Debug/net10.0/A"), false),
                (
                    "bin/Debug/net10.0/A.dll".to_owned(),
                    PathBuf::from("/work/bin/Debug/net10.0/win-x64/A.dll"),
                    true
                ),
                (
                    "bin/Debug/net10.0/A.exe".to_owned(),
                    PathBuf::from("/work/bin/Debug/net10.0/win-x64/A.exe"),
                    true
                ),
                (
                    "bin/Debug/net10.0/B.JSON".to_owned(),
                    PathBuf::from("/work/bin/Debug/net10.0/win-x64/B.JSON"),
                    true
                ),
            ],
            "each read from its own file; a name differing only in case is the PC's same file"
        );
    }

    #[test]
    fn each_mach_o_and_elf_magic_is_a_native_binary_and_a_pe_or_short_file_is_not() {
        for magic in [
            [0xCF, 0xFA, 0xED, 0xFE],
            [0xCE, 0xFA, 0xED, 0xFE],
            [0xFE, 0xED, 0xFA, 0xCE],
            [0xFE, 0xED, 0xFA, 0xCF],
            [0xCA, 0xFE, 0xBA, 0xBF],
            [0x7F, 0x45, 0x4C, 0x46],
        ] {
            assert!(is_native_binary(&magic), "{magic:02X?}");
            assert!(is_native_binary(&[magic.as_slice(), b"rest"].concat()), "{magic:02X?} and more");
        }
        for portable in [b"MZ\x90\x00".as_slice(), &[0xCF, 0xFA, 0xED], b"", b"{}\n\n"] {
            assert!(!is_native_binary(portable), "{portable:02X?}");
        }
    }

    #[test]
    fn a_fat_mach_o_is_told_from_a_java_class_by_its_arch_count() {
        for count in [1_u8, 2, 30] {
            assert!(is_native_binary(&[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, count]), "{count} arches");
        }
        for head in [
            [0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 0x41].as_slice(),
            &[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 0],
            &[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 31],
            &[0xCA, 0xFE, 0xBA, 0xBE, 0, 1, 0, 1],
            &[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0],
            &[0xCA, 0xFE, 0xBA, 0xBE],
        ] {
            assert!(!is_native_binary(head), "{head:02X?}");
        }
    }

    /// What [`portable`] makes of `files` all chosen, each's first bytes as given.
    fn portable_of(files: &[(&str, &[u8])]) -> io::Result<Result<Vec<String>, String>> {
        let heads: HashMap<PathBuf, Vec<u8>> = files.iter().map(|(path, head)| (found(path).full, head.to_vec())).collect();
        let walked: Vec<Found> = files.iter().map(|(path, _)| found(path)).collect();
        let chosen: Vec<Chosen<'_>> = walked.iter().map(Chosen::from).collect();
        Ok(match portable(chosen, |file| Ok(heads[&file.full].clone()))? {
            Portable::Kept(kept) => Ok(kept.iter().map(|chosen| chosen.file.path.clone()).collect()),
            Portable::Refused(path) => Err(path),
        })
    }

    #[test]
    fn a_native_mach_o_or_elf_is_left_behind_and_a_short_file_or_pe_is_kept() {
        let kept = portable_of(&[
            ("bin/Debug/net10.0/A", &[0xCF, 0xFA, 0xED, 0xFE, 0]),
            ("bin/Debug/net10.0/libA.so", &[0x7F, 0x45, 0x4C, 0x46]),
            ("bin/Debug/net10.0/win-x64/A.exe", b"MZ\x90\x00"),
            ("bin/Debug/net10.0/x.txt", b"ab\n"),
            ("bin/Debug/net10.0/TestResults/a.trx", b"<Tes"),
        ])
        .expect("every head reads");
        assert_eq!(
            kept,
            Ok(vec![
                "bin/Debug/net10.0/A.exe".to_owned(),
                "bin/Debug/net10.0/x.txt".to_owned(),
                "bin/Debug/net10.0/TestResults/a.trx".to_owned()
            ]),
            "the chosen files kept in their order"
        );
    }

    #[test]
    fn a_mapped_mach_o_refuses_the_cross_build_s_outputs_naming_its_pc_path() {
        let refused = portable_of(&[
            ("bin/Debug/net10.0/win-x64/A.exe", b"MZ\x90\x00"),
            ("bin/Debug/net10.0/win-x64/A", &[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 2]),
        ])
        .expect("every head reads");
        assert_eq!(refused, Err("bin/Debug/net10.0/A".to_owned()));
    }

    #[test]
    fn a_head_that_cannot_be_read_fails_the_portability_check() {
        let walked = [found("bin/Debug/net10.0/win-x64/A.exe")];
        let failed = portable(walked.iter().map(Chosen::from).collect(), |_| Err(io::Error::other("denied")));
        assert_eq!(failed.map(drop).map_err(|error| error.to_string()), Err("denied".to_owned()));
    }

    #[test]
    fn the_cross_build_lines_name_the_rid() {
        assert_eq!(passed_line(RID, 12.34), "gate: the cross build for win-x64 took 12.3 s");
        assert_eq!(
            failed_line(RID, 143),
            "gate: the cross build for win-x64 failed with exit 143; the Windows outputs were not brought back"
        );
        assert_eq!(
            refused_line(RID, "bin/Debug/net10.0/A\u{1b}"),
            r"gate: the cross build for win-x64 left a non-Windows file bin/Debug/net10.0/A\u{1b}; the Windows outputs were not brought back"
        );
    }
}
