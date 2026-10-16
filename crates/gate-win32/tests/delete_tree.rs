//! A tree deleted through an open folder's handle ([`gate_win32::delete_tree_opened`]) whatever its depth.
#![cfg(windows)]
#![expect(clippy::expect_used, reason = "a test whose folders cannot be made must fail loudly")]

use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use gate_win32::{Make, Share, delete_tree_opened, open_beneath_as};
use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ};

/// How many folders deep the tree is: several thousand, as one the builder's user leaves in a work folder can be.
const DEPTH: usize = 3000;

#[test]
fn a_tree_thousands_of_folders_deep_is_deleted_without_overflowing_the_stack() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("gate-win32-delete-tree-{}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("an earlier run's folder is removed");
    }
    let top = dir.join("top");
    std::fs::create_dir_all(&top).expect("the folder can be made");
    let held: File = OpenOptions::new()
        .access_mode(FILE_GENERIC_READ | DELETE)
        .share_mode(Share::NoDelete.mode())
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(&top)
        .expect("the folder can be opened for deletion");
    // Each level is made beneath the one above it by handle, so no path past MAX_PATH is ever named.
    let mut level = open_beneath_as(&held, "a", FILE_GENERIC_READ, Share::All, Make::NewFolder).expect("a folder is made");
    for _ in 1..DEPTH {
        level = open_beneath_as(&level, "a", FILE_GENERIC_READ, Share::All, Make::NewFolder).expect("a folder is made");
    }
    open_beneath_as(&level, "leaf.txt", FILE_GENERIC_READ, Share::All, Make::NewFile).expect("the leaf file is made");
    drop(level);

    // A thread of 64 KiB of stack, which a walk recursing once per level would overflow in a few hundred levels.
    let deleted = std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(move || {
            let deleted = delete_tree_opened(&held);
            drop(held);
            deleted
        })
        .expect("the thread starts")
        .join()
        .expect("the walk neither panics nor overflows its stack");
    deleted.expect("the whole tree is deleted");

    assert!(!top.exists(), "the top folder is deleted with everything in it");
    assert!(!top.exists(), "the top folder is deleted with everything in it");
    std::fs::remove_dir_all(&dir).expect("the folder can be removed");
}
