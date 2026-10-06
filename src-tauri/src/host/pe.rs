//! The VST2 pre-filter: what a `.dll` is, read from its headers and its export names, without
//! loading it. A VST2 plugin has no extension of its own, so a folder of them holds every other kind
//! of DLL too; the scan walks them all and only a file that names a VST2 entry among its exports
//! (`VSTPluginMain`, or `main`) is worth a scan child. Every read is bounded and every offset
//! checked: the file is foreign data, and a file this cannot make sense of is `Inconclusive`, which
//! the scan then loads only inside its isolated child.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// What a `.dll` is to the scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Class {
    /// A 64-bit x86 DLL that exports a VST2 entry by name.
    Candidate,
    /// A 32-bit x86 DLL that exports one: a plugin, for a host this build is not.
    PossiblyVst32,
    /// Not a PE image, not a DLL, another architecture, or a DLL without a VST2 entry among its
    /// named exports.
    NotAPlugin,
    /// A PE image whose exports could not be read reliably; why.
    Inconclusive(String),
}

/// The names a VST2 entry is exported under, each with its terminator.
const ENTRY_NAMES: [&[u8]; 2] = [b"VSTPluginMain\0", b"main\0"];
/// The longest of them: what is read of each exported name.
const NAME_READ: usize = 14;

/// A PE image has at most 96 sections; one that claims more is not read further.
const MAX_SECTIONS: usize = 96;
/// Export names compared per file. A plugin exports a handful; a DLL with more than this and no
/// entry among the first is left to the scan child.
const MAX_NAMES: usize = 16_384;
/// How much of the export data is read in one piece (the name table and the names usually sit in
/// it); what lies outside is read name by name.
const MAX_EXPORT_BLOB: u32 = 1 << 20;

const MACHINE_I386: u16 = 0x014c;
const MACHINE_AMD64: u16 = 0x8664;
const FILE_DLL: u16 = 0x2000;
const MAGIC_PE32: u16 = 0x010b;
const MAGIC_PE32_PLUS: u16 = 0x020b;

/// Classify the file at `path`. A file that cannot be opened is `Inconclusive`.
pub(crate) fn classify(path: &Path) -> Class {
    let opened = std::fs::File::open(path).and_then(|file| Ok((file.metadata()?.len(), file)));
    match opened {
        Ok((len, mut file)) => classify_image(&mut file, len),
        Err(e) => Class::Inconclusive(format!("unreadable: {e}")),
    }
}

/// `len` bytes at `offset`, or `None` when the image does not hold them.
fn read_at(image: &mut (impl Read + Seek), image_len: u64, offset: u64, len: usize) -> Option<Vec<u8>> {
    if offset.checked_add(len as u64)? > image_len {
        return None;
    }
    let mut bytes = vec![0u8; len];
    image.seek(SeekFrom::Start(offset)).ok()?;
    image.read_exact(&mut bytes).ok()?;
    Some(bytes)
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// One section's mapping: where its bytes sit in memory and in the file.
struct Section {
    virtual_address: u32,
    raw_size: u32,
    raw_offset: u32,
}

/// Where the byte at the address `rva` sits in the file, and how many bytes its section has on
/// disk from there on.
fn file_span(sections: &[Section], rva: u32) -> Option<(u64, u32)> {
    sections.iter().find_map(|s| {
        let within = rva.checked_sub(s.virtual_address)?;
        (within < s.raw_size).then(|| (u64::from(s.raw_offset) + u64::from(within), s.raw_size - within))
    })
}

/// The file offset of `len` bytes at the address `rva`: inside one section's bytes on disk.
fn file_offset(sections: &[Section], rva: u32, len: u32) -> Option<u64> {
    file_span(sections, rva).filter(|(_, available)| len <= *available).map(|(offset, _)| offset)
}

/// Classify a PE image of `image_len` bytes (the file, or a test's buffer).
pub(crate) fn classify_image(image: &mut (impl Read + Seek), image_len: u64) -> Class {
    let inconclusive = |why: &str| Class::Inconclusive(why.to_string());

    // The DOS header, then the PE signature and the COFF header it points at. Without both this is
    // no PE image at all.
    let Some(dos) = read_at(image, image_len, 0, 64) else { return Class::NotAPlugin };
    if &dos[..2] != b"MZ" {
        return Class::NotAPlugin;
    }
    let pe_offset = u64::from(u32_at(&dos, 0x3c));
    let Some(coff) = read_at(image, image_len, pe_offset, 24) else { return Class::NotAPlugin };
    if &coff[..4] != b"PE\0\0" {
        return Class::NotAPlugin;
    }
    let machine = u16_at(&coff, 4);
    let section_count = usize::from(u16_at(&coff, 6));
    let optional_len = usize::from(u16_at(&coff, 20));
    let characteristics = u16_at(&coff, 22);
    if characteristics & FILE_DLL == 0 || (machine != MACHINE_AMD64 && machine != MACHINE_I386) {
        return Class::NotAPlugin;
    }

    // The optional header: its magic must be the machine's, and it holds the export directory's
    // address and size as its first data directory.
    let optional_offset = pe_offset + 24;
    let Some(optional) = read_at(image, image_len, optional_offset, optional_len) else {
        return inconclusive("truncated optional header");
    };
    if optional.len() < 2 {
        return inconclusive("no optional header");
    }
    let (expected_magic, directories_at) = if machine == MACHINE_AMD64 { (MAGIC_PE32_PLUS, 112) } else { (MAGIC_PE32, 96) };
    if u16_at(&optional, 0) != expected_magic {
        return inconclusive("optional header does not match the machine");
    }
    if optional.len() < directories_at {
        return inconclusive("optional header too short");
    }
    if u32_at(&optional, directories_at - 4) == 0 {
        return Class::NotAPlugin; // No data directories: no exports.
    }
    if optional.len() < directories_at + 8 {
        return inconclusive("optional header ends inside the export directory entry");
    }
    let export_rva = u32_at(&optional, directories_at);
    let export_size = u32_at(&optional, directories_at + 4);
    if export_rva == 0 || export_size == 0 {
        return Class::NotAPlugin;
    }

    // The section table maps addresses to file offsets.
    if section_count > MAX_SECTIONS {
        return inconclusive("too many sections");
    }
    let Some(table) = read_at(image, image_len, optional_offset + optional_len as u64, section_count * 40) else {
        return inconclusive("truncated section table");
    };
    let sections: Vec<Section> = table
        .chunks_exact(40)
        .map(|s| Section { virtual_address: u32_at(s, 12), raw_size: u32_at(s, 16), raw_offset: u32_at(s, 20) })
        .collect();

    // The export directory: how many exports have a name, and where the table of name addresses is.
    let Some(directory) = file_offset(&sections, export_rva, 40).and_then(|at| read_at(image, image_len, at, 40)) else {
        return inconclusive("export directory outside the file");
    };
    let name_count = u32_at(&directory, 24) as usize;
    let names_rva = u32_at(&directory, 32);
    if name_count == 0 {
        return Class::NotAPlugin; // Exports by ordinal only: an entry is never guessed.
    }
    let read_count = name_count.min(MAX_NAMES);

    // The export data in one read, as far as it goes; anything outside it is read on its own.
    let blob_len = export_size.min(MAX_EXPORT_BLOB);
    let blob = file_offset(&sections, export_rva, blob_len)
        .and_then(|at| read_at(image, image_len, at, blob_len as usize))
        .unwrap_or_default();
    let mut bytes_at = |rva: u32, len: usize| -> Option<Vec<u8>> {
        let inside = rva.checked_sub(export_rva).map(|at| at as usize).filter(|at| at.checked_add(len).is_some_and(|end| end <= blob.len()));
        match inside {
            Some(at) => Some(blob[at..at + len].to_vec()),
            None => read_at(image, image_len, file_offset(&sections, rva, len as u32)?, len),
        }
    };

    let Some(name_table) = bytes_at(names_rva, read_count * 4) else {
        return inconclusive("export name table outside the file");
    };
    for entry in name_table.chunks_exact(4) {
        let name_rva = u32_at(entry, 0);
        // A name at the very end of its section is shorter than the longest entry name: what the
        // section holds from there is read, and must be in the file.
        let available = file_span(&sections, name_rva).map_or(0, |(_, available)| available as usize);
        let Some(name) = Some(NAME_READ.min(available)).filter(|len| *len > 0).and_then(|len| bytes_at(name_rva, len)) else {
            return inconclusive("an export name outside the file");
        };
        if ENTRY_NAMES.iter().any(|entry_name| name.starts_with(entry_name)) {
            return if machine == MACHINE_AMD64 { Class::Candidate } else { Class::PossiblyVst32 };
        }
    }
    if name_count > read_count {
        return inconclusive("more exported names than are read");
    }
    Class::NotAPlugin
}

/// Test-only: PE images built byte by byte, for this module's tests and the scan walker's.
#[cfg(test)]
pub(crate) mod fixture {
    pub(crate) const AMD64: u16 = super::MACHINE_AMD64;
    pub(crate) const I386: u16 = super::MACHINE_I386;

    /// Where the parts a test corrupts sit in an `image`.
    pub(crate) struct Image {
        pub(crate) bytes: Vec<u8>,
        /// The optional header's first data directory entry (the export directory's address, size).
        pub(crate) export_entry: usize,
        /// The section header.
        pub(crate) section: usize,
        /// The export directory, the first bytes of the one section.
        pub(crate) directory: usize,
        /// The table of name addresses, then the names.
        pub(crate) name_table: usize,
    }

    /// The address the one section is mapped at, and its offset in the file.
    pub(crate) const SECTION_RVA: u32 = 0x1000;
    const SECTION_OFFSET: usize = 0x400;

    /// A DLL for `machine` (with the optional header that belongs to it) whose one section holds
    /// an export directory naming `names`.
    pub(crate) fn image(machine: u16, names: &[&str]) -> Image {
        let plus = machine == AMD64;
        let optional_len: usize = if plus { 240 } else { 224 };
        let mut bytes = vec![0u8; SECTION_OFFSET];
        let put16 = |bytes: &mut Vec<u8>, at: usize, v: u16| bytes[at..at + 2].copy_from_slice(&v.to_le_bytes());
        let put32 = |bytes: &mut Vec<u8>, at: usize, v: u32| bytes[at..at + 4].copy_from_slice(&v.to_le_bytes());
        bytes[..2].copy_from_slice(b"MZ");
        put32(&mut bytes, 0x3c, 64);
        bytes[64..68].copy_from_slice(b"PE\0\0");
        put16(&mut bytes, 68, machine);
        put16(&mut bytes, 70, 1); // sections
        put16(&mut bytes, 84, optional_len as u16);
        put16(&mut bytes, 86, 0x2000 | 0x0002); // a DLL, executable image
        let optional = 88;
        put16(&mut bytes, optional, if plus { 0x020b } else { 0x010b });
        let directories = optional + if plus { 112 } else { 96 };
        put32(&mut bytes, directories - 4, 16);

        // The section's bytes: the directory, the name address table, the names.
        let name_table = 40;
        let mut section = vec![0u8; name_table + names.len() * 4];
        for (i, name) in names.iter().enumerate() {
            let at = section.len() as u32;
            section[name_table + i * 4..name_table + i * 4 + 4].copy_from_slice(&(SECTION_RVA + at).to_le_bytes());
            section.extend_from_slice(name.as_bytes());
            section.push(0);
        }
        section[24..28].copy_from_slice(&(names.len() as u32).to_le_bytes());
        section[32..36].copy_from_slice(&(SECTION_RVA + name_table as u32).to_le_bytes());
        put32(&mut bytes, directories, SECTION_RVA);
        put32(&mut bytes, directories + 4, section.len() as u32);

        let header = optional + optional_len;
        bytes[header..header + 6].copy_from_slice(b".edata");
        put32(&mut bytes, header + 8, section.len() as u32); // virtual size
        put32(&mut bytes, header + 12, SECTION_RVA);
        put32(&mut bytes, header + 16, section.len() as u32); // size on disk
        put32(&mut bytes, header + 20, SECTION_OFFSET as u32);
        bytes.extend_from_slice(&section);
        Image {
            bytes,
            export_entry: directories,
            section: header,
            directory: SECTION_OFFSET,
            name_table: SECTION_OFFSET + name_table,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{image, Image, AMD64, I386, SECTION_RVA};
    use super::*;
    use std::io::Cursor;

    fn class(bytes: &[u8]) -> Class {
        classify_image(&mut Cursor::new(bytes), bytes.len() as u64)
    }

    fn put32(image: &mut Image, at: usize, v: u32) {
        image.bytes[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn inconclusive(bytes: &[u8]) -> bool {
        matches!(class(bytes), Class::Inconclusive(_))
    }

    #[test]
    fn a_64_bit_dll_that_names_a_vst2_entry_is_a_candidate() {
        assert_eq!(class(&image(AMD64, &["VSTPluginMain"]).bytes), Class::Candidate);
        assert_eq!(class(&image(AMD64, &["main"]).bytes), Class::Candidate, "the older entry name");
        assert_eq!(class(&image(AMD64, &["DllGetClassObject", "VSTPluginMain", "main", "zeta"]).bytes), Class::Candidate);
        // The last name, at the very end of the file, is still read.
        assert_eq!(class(&image(AMD64, &["a_long_exported_name", "main"]).bytes), Class::Candidate);
    }

    #[test]
    fn a_32_bit_dll_that_names_one_is_a_possible_32_bit_plugin() {
        assert_eq!(class(&image(I386, &["VSTPluginMain"]).bytes), Class::PossiblyVst32);
        assert_eq!(class(&image(I386, &["alpha", "main"]).bytes), Class::PossiblyVst32);
        assert_eq!(class(&image(I386, &["alpha"]).bytes), Class::NotAPlugin);
    }

    #[test]
    fn a_well_formed_image_without_a_named_entry_is_not_a_plugin() {
        assert_eq!(class(&image(AMD64, &["GetPluginFactory", "InitDll"]).bytes), Class::NotAPlugin);
        // A name that only starts like an entry, or only ends like one.
        assert_eq!(class(&image(AMD64, &["VSTPluginMainX", "mainly", "domain", "Main"]).bytes), Class::NotAPlugin);
        // Exports by ordinal only.
        assert_eq!(class(&image(AMD64, &[]).bytes), Class::NotAPlugin);
        // No export directory.
        let mut bare = image(AMD64, &["VSTPluginMain"]);
        let entry = bare.export_entry;
        put32(&mut bare, entry, 0);
        assert_eq!(class(&bare.bytes), Class::NotAPlugin);
        // No data directories at all.
        let mut none = image(AMD64, &["VSTPluginMain"]);
        put32(&mut none, entry - 4, 0);
        assert_eq!(class(&none.bytes), Class::NotAPlugin);
        // An executable, and a DLL for another machine (ARM64).
        let mut exe = image(AMD64, &["main"]);
        exe.bytes[86..88].copy_from_slice(&0x0002u16.to_le_bytes());
        assert_eq!(class(&exe.bytes), Class::NotAPlugin);
        let mut arm = image(AMD64, &["VSTPluginMain"]);
        arm.bytes[68..70].copy_from_slice(&0xaa64u16.to_le_bytes());
        assert_eq!(class(&arm.bytes), Class::NotAPlugin);
    }

    #[test]
    fn a_file_that_is_no_pe_image_is_not_a_plugin() {
        assert_eq!(class(b""), Class::NotAPlugin);
        assert_eq!(class(b"MZ"), Class::NotAPlugin);
        assert_eq!(class(&[0x7fu8; 4096]), Class::NotAPlugin);
        let good = image(AMD64, &["VSTPluginMain"]).bytes;
        // No PE signature where the DOS header points; a pointer past the end; one that overflows.
        let mut unsigned = good.clone();
        unsigned[64..68].copy_from_slice(b"NE\0\0");
        assert_eq!(class(&unsigned), Class::NotAPlugin);
        for pointer in [good.len() as u32, u32::MAX] {
            let mut far = good.clone();
            far[0x3c..0x40].copy_from_slice(&pointer.to_le_bytes());
            assert_eq!(class(&far), Class::NotAPlugin);
        }
        // Cut inside the DOS header and inside the COFF header.
        assert_eq!(class(&good[..40]), Class::NotAPlugin);
        assert_eq!(class(&good[..80]), Class::NotAPlugin);
    }

    #[test]
    fn a_truncated_image_is_inconclusive_at_every_cut_past_its_headers() {
        let good = image(AMD64, &["alpha", "VSTPluginMain"]);
        let headers = 88; // The DOS header, the signature and the COFF header.
        for cut in headers..good.bytes.len() {
            // The cut that only takes the last name's terminator leaves "VSTPluginMain" unterminated.
            assert!(inconclusive(&good.bytes[..cut]), "cut at {cut}: {:?}", class(&good.bytes[..cut]));
        }
        assert_eq!(class(&good.bytes), Class::Candidate);
    }

    #[test]
    fn addresses_that_lead_nowhere_are_inconclusive() {
        let good = || image(AMD64, &["alpha", "VSTPluginMain"]);
        // The export directory at an address no section maps, and one that overflows.
        for rva in [0x9000, u32::MAX, SECTION_RVA - 1] {
            let mut bad = good();
            let entry = bad.export_entry;
            put32(&mut bad, entry, rva);
            assert!(inconclusive(&bad.bytes), "export directory at {rva:#x}");
        }
        // The name table, and one name, likewise.
        for rva in [0, 0x9000, u32::MAX, SECTION_RVA + 0x10_0000] {
            let mut bad = good();
            let directory = bad.directory;
            put32(&mut bad, directory + 32, rva);
            assert!(inconclusive(&bad.bytes), "name table at {rva:#x}");
            let mut bad = good();
            let name_table = bad.name_table;
            put32(&mut bad, name_table, rva);
            assert!(inconclusive(&bad.bytes), "a name at {rva:#x}");
        }
        // A name count the table cannot hold: the names past the real ones are not names.
        for count in [3, 100_000, u32::MAX] {
            let mut bad = image(AMD64, &["alpha", "beta"]);
            let directory = bad.directory;
            put32(&mut bad, directory + 24, count);
            assert!(inconclusive(&bad.bytes), "{count} names");
        }
        // A section that claims bytes the file does not have, or sits past its end.
        let mut bad = good();
        let section = bad.section;
        put32(&mut bad, section + 16, u32::MAX);
        put32(&mut bad, section + 20, u32::MAX);
        assert!(inconclusive(&bad.bytes));
        // More sections than an image can have, and an optional header for the other machine.
        let mut bad = good();
        bad.bytes[70..72].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(inconclusive(&bad.bytes));
        let mut bad = good();
        bad.bytes[88..90].copy_from_slice(&0x010bu16.to_le_bytes());
        assert!(inconclusive(&bad.bytes));
    }

    #[test]
    fn only_a_bounded_number_of_names_is_compared() {
        let mut names: Vec<String> = (0..MAX_NAMES).map(|i| format!("export_{i:05}")).collect();
        names.push("VSTPluginMain".to_string());
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        // The entry lies past the cap: the file is left to the scan child rather than read on.
        assert!(inconclusive(&image(AMD64, &refs).bytes));
        // At the cap exactly, every name was compared.
        assert_eq!(class(&image(AMD64, &refs[..MAX_NAMES]).bytes), Class::NotAPlugin);
        assert_eq!(class(&image(AMD64, &refs[1..]).bytes), Class::Candidate);
    }

    #[test]
    fn a_file_that_cannot_be_opened_is_inconclusive() {
        assert!(matches!(classify(Path::new(r"C:\no-such-folder\missing.dll")), Class::Inconclusive(_)));
    }

    /// Runs where ReaPlugs is installed (the owner's rig); a no-op elsewhere.
    #[test]
    fn the_installed_reajs_is_a_candidate() {
        let reajs = Path::new(r"C:\Program Files\VSTPlugins\ReaPlugs\reajs.dll");
        if reajs.is_file() {
            assert_eq!(classify(reajs), Class::Candidate);
        }
    }
}
