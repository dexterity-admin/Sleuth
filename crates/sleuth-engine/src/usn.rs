//! Incremental discovery on local NTFS volumes through the change journal.
//!
//! The journal records every create, delete, rename and write on a volume.
//! Reading it from a checkpoint (journal id + next USN) tells which folders
//! changed since the last pass, so only those are listed again instead of
//! walking the whole disk.
//!
//! - Reading the journal needs administrator rights (opening `\\.\C:`).
//!   Without them, or on a non-NTFS volume, the volume is walked as before.
//! - A different journal id (journal deleted and recreated) or a checkpoint
//!   older than the journal's first record (it wrapped) means changes were
//!   lost: the volume is walked in full and a new checkpoint saved.
//! - Every record is checked against the buffer before it is trusted; a
//!   malformed record ends that batch instead of being guessed at.
//! - A changed folder is listed again on its own; a folder created or renamed
//!   into place is walked with everything under it, because moving a tree
//!   journals only the top folder.

/// Changes worth a new listing: create, delete, rename, and the close of a
/// write (size and date are taken from the listing).
const REASONS: u32 = 0x0000_0100 /* FILE_CREATE */
    | 0x0000_0200 /* FILE_DELETE */
    | 0x0000_1000 /* RENAME_OLD_NAME */
    | 0x0000_2000 /* RENAME_NEW_NAME */
    | 0x0000_0002 /* DATA_EXTEND */
    | 0x0000_0001 /* DATA_OVERWRITE */
    | 0x0000_0004 /* DATA_TRUNCATION */
    | 0x8000_0000 /* CLOSE */;
const REASON_CREATE: u32 = 0x100;
const REASON_RENAME_NEW: u32 = 0x2000;
const ATTR_DIRECTORY: u32 = 0x10;
/// More changed folders than this in one pass: walk the volume instead.
pub const MAX_DIRS: usize = 5000;

/// One journal record, as far as Sleuth needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// File id of the folder holding the changed entry (64- or 128-bit).
    pub parent: u128,
    /// File id of the changed entry itself.
    pub file: u128,
    pub reason: u32,
    pub attributes: u32,
    pub name: String,
    pub v3: bool,
}

/// Parse the output of `FSCTL_READ_USN_JOURNAL`: the next USN, then records.
/// Stops at the first record that does not fit the buffer or its own length.
pub fn parse_records(buf: &[u8]) -> (i64, Vec<Record>, bool) {
    let mut out = Vec::new();
    if buf.len() < 8 {
        return (0, out, false);
    }
    let next = i64::from_le_bytes(buf[0..8].try_into().unwrap());
    let mut off = 8usize;
    let u16_at = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32_at = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let u64_at = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let mut clean = true;
    while off + 8 <= buf.len() {
        let len = u32_at(buf, off) as usize;
        if len == 0 {
            break;
        }
        if len < 60 || off + len > buf.len() || len % 8 != 0 {
            clean = false;
            break;
        }
        let rec = &buf[off..off + len];
        let major = u16_at(rec, 4);
        let parsed = match major {
            2 => {
                // V2: 64-bit ids; name length/offset at 56/58
                let (nl, no) = (u16_at(rec, 56) as usize, u16_at(rec, 58) as usize);
                (no >= 60 && no + nl <= len && nl % 2 == 0).then(|| Record {
                    file: u64_at(rec, 8) as u128,
                    parent: u64_at(rec, 16) as u128,
                    reason: u32_at(rec, 40),
                    attributes: u32_at(rec, 52),
                    name: utf16(&rec[no..no + nl]),
                    v3: false,
                })
            }
            3 if len >= 76 => {
                // V3: 128-bit ids; name length/offset at 72/74
                let (nl, no) = (u16_at(rec, 72) as usize, u16_at(rec, 74) as usize);
                (no >= 76 && no + nl <= len && nl % 2 == 0).then(|| Record {
                    file: u128::from_le_bytes(rec[8..24].try_into().unwrap()),
                    parent: u128::from_le_bytes(rec[24..40].try_into().unwrap()),
                    reason: u32_at(rec, 56),
                    attributes: u32_at(rec, 68),
                    name: utf16(&rec[no..no + nl]),
                    v3: true,
                })
            }
            // V4 (range tracking) and unknown versions carry no names: skip.
            _ => None,
        };
        match parsed {
            Some(r) => out.push(r),
            None if major == 2 || major == 3 => {
                clean = false;
                break;
            }
            None => {}
        }
        off += len;
    }
    (next, out, clean)
}

fn utf16(b: &[u8]) -> String {
    let v: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&v)
}

/// Folders to list again (`shallow`) and folders created or moved in, to
/// walk with everything under them (`deep`), from a batch of records.
/// Returns parent ids; the caller resolves them to paths.
pub fn plan(records: &[Record]) -> (Vec<u128>, Vec<u128>) {
    let mut shallow: Vec<u128> = Vec::new();
    let mut deep: Vec<u128> = Vec::new();
    for r in records.iter().filter(|r| r.reason & REASONS != 0) {
        if !shallow.contains(&r.parent) {
            shallow.push(r.parent);
        }
        if r.attributes & ATTR_DIRECTORY != 0 && r.reason & (REASON_CREATE | REASON_RENAME_NEW) != 0 && !deep.contains(&r.file) {
            deep.push(r.file);
        }
    }
    (shallow, deep)
}

#[derive(Debug)]
pub enum Outcome {
    /// The journal cannot be used here; walk the volume.
    Unavailable(String),
    /// No usable checkpoint (first pass, journal reset or wrapped): walk the
    /// volume in full, then save this checkpoint (taken before the walk).
    Restart { journal_id: String, next_usn: i64, why: String },
    /// Only these folders changed.
    Changes { shallow: Vec<String>, deep: Vec<String>, journal_id: String, next_usn: i64 },
}

#[cfg(windows)]
mod win {
    use super::*;
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ExtendedFileIdType, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_128, FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdType, GetFinalPathNameByHandleW, OPEN_EXISTING, OpenFileById,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0};
    use windows::core::HSTRING;

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    fn open_volume(letter: &str) -> Result<Handle, String> {
        let path = HSTRING::from(format!(r"\\.\{}", letter.trim_end_matches('\\')));
        unsafe { CreateFileW(&path, GENERIC_READ.0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, None, OPEN_EXISTING, Default::default(), None) }
            .map(Handle)
            .map_err(|e| if e.code().0 as u32 == 0x8007_0005 { "needs administrator rights".to_string() } else { e.message() })
    }

    fn query(h: &Handle) -> Result<USN_JOURNAL_DATA_V0, String> {
        let mut data = USN_JOURNAL_DATA_V0::default();
        let mut got = 0u32;
        unsafe { DeviceIoControl(h.0, FSCTL_QUERY_USN_JOURNAL, None, 0, Some(&mut data as *mut _ as *mut _), std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32, Some(&mut got), None) }
            .map_err(|e| format!("no change journal on this volume ({})", e.message()))?;
        Ok(data)
    }

    fn path_of(vol: &Handle, id: u128, v3: bool) -> Option<String> {
        let desc = if v3 {
            FILE_ID_DESCRIPTOR { dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32, Type: ExtendedFileIdType, Anonymous: FILE_ID_DESCRIPTOR_0 { ExtendedFileId: FILE_ID_128 { Identifier: id.to_le_bytes() } } }
        } else {
            FILE_ID_DESCRIPTOR { dwSize: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32, Type: FileIdType, Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: id as i64 } }
        };
        unsafe {
            let h = OpenFileById(vol.0, &desc, FILE_READ_ATTRIBUTES.0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, None, FILE_FLAG_BACKUP_SEMANTICS).ok()?;
            let h = Handle(h);
            let mut buf = vec![0u16; 1024];
            let n = GetFinalPathNameByHandleW(h.0, &mut buf, FILE_NAME_NORMALIZED) as usize;
            if n == 0 || n > buf.len() {
                return None;
            }
            let p = String::from_utf16_lossy(&buf[..n]);
            Some(p.strip_prefix(r"\\?\").unwrap_or(&p).to_string())
        }
    }

    pub fn changes(letter: &str, checkpoint: Option<(String, i64)>) -> Outcome {
        let vol = match open_volume(letter) {
            Ok(h) => h,
            Err(e) => return Outcome::Unavailable(e),
        };
        let j = match query(&vol) {
            Ok(j) => j,
            Err(e) => return Outcome::Unavailable(e),
        };
        let journal_id = format!("{:016x}", j.UsnJournalID);
        let start = match checkpoint {
            None => return Outcome::Restart { journal_id, next_usn: j.NextUsn, why: "first pass".into() },
            Some((id, _)) if id != journal_id => return Outcome::Restart { journal_id, next_usn: j.NextUsn, why: "the change journal was recreated".into() },
            Some((_, usn)) if usn < j.FirstUsn => return Outcome::Restart { journal_id, next_usn: j.NextUsn, why: "changes were lost (the journal wrapped)".into() },
            Some((_, usn)) => usn,
        };
        let mut records = Vec::new();
        let mut usn = start;
        let mut buf = vec![0u8; 64 * 1024];
        while usn < j.NextUsn {
            let input = READ_USN_JOURNAL_DATA_V0 { StartUsn: usn, ReasonMask: REASONS, ReturnOnlyOnClose: 0, Timeout: 0, BytesToWaitFor: 0, UsnJournalID: j.UsnJournalID };
            let mut got = 0u32;
            let ok = unsafe {
                DeviceIoControl(vol.0, FSCTL_READ_USN_JOURNAL, Some(&input as *const _ as *const _), std::mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as u32, Some(buf.as_mut_ptr() as *mut _), buf.len() as u32, Some(&mut got), None)
            };
            if let Err(e) = ok {
                return Outcome::Restart { journal_id, next_usn: j.NextUsn, why: format!("the journal could not be read ({})", e.message()) };
            }
            let (next, recs, clean) = parse_records(&buf[..got as usize]);
            if !clean {
                return Outcome::Restart { journal_id, next_usn: j.NextUsn, why: "a malformed journal record".into() };
            }
            if next <= usn {
                break;
            }
            records.extend(recs);
            usn = next;
            if records.len() > MAX_DIRS * 20 {
                return Outcome::Restart { journal_id, next_usn: j.NextUsn, why: "too many changes to list one by one".into() };
            }
        }
        let v3 = records.iter().any(|r| r.v3);
        let (shallow_ids, deep_ids) = plan(&records);
        if shallow_ids.len() + deep_ids.len() > MAX_DIRS {
            return Outcome::Restart { journal_id, next_usn: j.NextUsn, why: "too many changed folders".into() };
        }
        // A folder that no longer exists cannot be resolved; its parent was
        // journaled too (the delete or rename record) and is listed instead.
        let shallow = shallow_ids.into_iter().filter_map(|id| path_of(&vol, id, v3)).collect();
        let deep = deep_ids.into_iter().filter_map(|id| path_of(&vol, id, v3)).collect();
        Outcome::Changes { shallow, deep, journal_id, next_usn: usn }
    }
}

#[cfg(windows)]
pub use win::changes;

#[cfg(not(windows))]
pub fn changes(_letter: &str, _checkpoint: Option<(String, i64)>) -> Outcome {
    Outcome::Unavailable("not Windows".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2(parent: u64, file: u64, reason: u32, attrs: u32, name: &str) -> Vec<u8> {
        let n: Vec<u8> = name.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        let len = (60 + n.len()).div_ceil(8) * 8;
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes());
        r[8..16].copy_from_slice(&file.to_le_bytes());
        r[16..24].copy_from_slice(&parent.to_le_bytes());
        r[40..44].copy_from_slice(&reason.to_le_bytes());
        r[52..56].copy_from_slice(&attrs.to_le_bytes());
        r[56..58].copy_from_slice(&(n.len() as u16).to_le_bytes());
        r[58..60].copy_from_slice(&60u16.to_le_bytes());
        r[60..60 + n.len()].copy_from_slice(&n);
        r
    }

    #[test]
    fn records_are_parsed_and_planned() {
        let mut buf = 777i64.to_le_bytes().to_vec();
        buf.extend(v2(5, 100, 0x100 | 0x8000_0000, 0x20, "ISO-1001-01.pdf"));
        buf.extend(v2(5, 101, 0x2000, ATTR_DIRECTORY, "Moved project"));
        buf.extend(v2(9, 102, 0x200, 0x20, "old.pdf"));
        let (next, recs, clean) = parse_records(&buf);
        assert!(clean);
        assert_eq!(next, 777);
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].name, "ISO-1001-01.pdf");
        let (shallow, deep) = plan(&recs);
        assert_eq!(shallow, vec![5, 9]);
        assert_eq!(deep, vec![101], "a folder moved in is walked whole");
    }

    #[test]
    fn malformed_records_stop_the_batch() {
        let mut buf = 1i64.to_le_bytes().to_vec();
        buf.extend(v2(5, 100, 0x100, 0, "a.pdf"));
        let mut bad = v2(5, 101, 0x100, 0, "b.pdf");
        bad[56..58].copy_from_slice(&5000u16.to_le_bytes()); // name runs past the record
        buf.extend(bad);
        let (_, recs, clean) = parse_records(&buf);
        assert_eq!(recs.len(), 1);
        assert!(!clean);
        // a record longer than the buffer
        let mut cut = 1i64.to_le_bytes().to_vec();
        cut.extend(v2(5, 100, 0x100, 0, "a.pdf"));
        cut.truncate(cut.len() - 8);
        assert!(!parse_records(&cut).2);
        // an empty batch
        assert_eq!(parse_records(&9i64.to_le_bytes()).1.len(), 0);
    }
}
