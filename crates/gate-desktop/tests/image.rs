//! The built `gate-desktop-helper.exe` as a PE image: it imports no VC runtime and loads its static imports from
//! System32 only, so a DLL planted beside the exe is never loaded in place of a system one.

#![cfg(windows)]
#![expect(clippy::expect_used, reason = "a test that cannot read the built helper must fail loudly")]

/// The tests, in a module so their names start `image::` like the gate suite's.
mod image {
    use std::mem::offset_of;

    use object::LittleEndian as LE;
    use object::pe::{IMAGE_DIRECTORY_ENTRY_LOAD_CONFIG, ImageLoadConfigDirectory64};
    use object::read::pe::PeFile64;

    /// The desktop helper under test.
    const HELPER: &str = env!("CARGO_BIN_EXE_gate-desktop-helper");
    /// `LOAD_LIBRARY_SEARCH_SYSTEM32`, the `DependentLoadFlags` the linker's `/DEPENDENTLOADFLAG:0x800` writes.
    const SYSTEM32_ONLY: u16 = 0x800;
    /// The lower-cased name prefixes of the VC runtime's DLLs, which a static CRT leaves out of the import directory.
    const VC_RUNTIME_PREFIXES: [&str; 4] = ["vcruntime", "msvcp", "ucrtbase", "api-ms-win-crt-"];

    /// The lower-cased names of the DLLs the image's import directory names.
    fn imported_dlls(image: &PeFile64<'_>) -> Vec<String> {
        let table = image
            .import_table()
            .expect("the import directory reads")
            .expect("the exe has an import directory");
        let mut descriptors = table.descriptors().expect("the import descriptors read");
        let mut names = Vec::new();
        while let Some(descriptor) = descriptors.next().expect("an import descriptor reads") {
            let name = table.name(descriptor.name.get(LE)).expect("an imported DLL's name reads");
            names.push(String::from_utf8_lossy(name).to_ascii_lowercase());
        }
        names
    }

    /// The `DependentLoadFlags` field of the image's load config directory.
    fn dependent_load_flags(image: &PeFile64<'_>, data: &[u8]) -> u16 {
        let directory = image
            .data_directory(IMAGE_DIRECTORY_ENTRY_LOAD_CONFIG)
            .expect("the exe has a load config directory");
        let bytes = image
            .section_table()
            .pe_data_at(data, directory.virtual_address.get(LE))
            .expect("the load config lies in a section");
        let size: [u8; 4] = bytes
            .get(..4)
            .and_then(|field| field.try_into().ok())
            .expect("the load config's Size field reads");
        let declared = usize::try_from(u32::from_le_bytes(size)).expect("the load config size fits a usize");
        let offset = offset_of!(ImageLoadConfigDirectory64, dependent_load_flags);
        let needed = offset + size_of::<u16>();
        assert!(
            declared >= needed,
            "the load config is {declared} bytes, too short to hold DependentLoadFlags ({needed})"
        );
        let flags: [u8; 2] = bytes
            .get(offset..needed)
            .and_then(|field| field.try_into().ok())
            .expect("the load config's section holds DependentLoadFlags");
        u16::from_le_bytes(flags)
    }

    #[test]
    fn the_desktop_helper_loads_its_static_imports_from_system32_only() {
        let data = std::fs::read(HELPER).expect("the built helper reads");
        let image = PeFile64::parse(&*data).expect("the built helper is a 64-bit PE image");

        let dlls = imported_dlls(&image);
        let runtime: Vec<&String> = dlls
            .iter()
            .filter(|dll| VC_RUNTIME_PREFIXES.iter().any(|prefix| dll.starts_with(prefix)))
            .collect();
        assert!(runtime.is_empty(), "the helper imports the VC runtime {runtime:?}; it imports {dlls:?}");

        let flags = dependent_load_flags(&image, &data);
        assert_eq!(
            flags, SYSTEM32_ONLY,
            "the helper's DependentLoadFlags is {flags:#x}, not LOAD_LIBRARY_SEARCH_SYSTEM32"
        );
    }
}
