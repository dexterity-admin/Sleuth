//! Windows text filters (IFilter): the readers Windows Search uses, which
//! Office installs for `.doc` / `.ppt` and MicroStation for `.dgn`.
//!
//! A filter is third-party code loaded into this process. It runs on a
//! helper thread that is abandoned at the deadline; the engine runs
//! extraction in a disposable child process, so a filter that hangs or
//! crashes costs one file, not the program.

use anyhow::Result;
use std::path::Path;
use std::time::Instant;

/// More lines than any file is allowed to give; stops a filter that never
/// reports its end.
const MAX_FILTER_LINES: usize = 2_000_000;

/// Whether a real text filter is registered for this extension (no dot):
/// the persistent handler exists, is not the null or plain-text default,
/// and names a filter whose DLL is registered. Nothing is loaded.
pub fn registered(ext: &str) -> bool {
    imp::registered(ext)
}

/// The text of a file as its registered filter gives it, one string per
/// line or paragraph. `None` when no filter is registered for the file type
/// on this PC. An error when the filter refuses the file (password,
/// damaged) or does not finish by the deadline.
pub fn ifilter_text(path: &Path, deadline: Instant) -> Result<Option<Vec<String>>> {
    imp::ifilter_text(path, deadline)
}

/// Split filter text into lines on line and paragraph breaks.
fn split_lines(text: &str, lines: &mut Vec<String>) {
    for l in text.split(['\n', '\r', '\u{b}', '\u{c}', '\u{85}', '\u{2028}', '\u{2029}']) {
        let l = crate::out::clean(l);
        if !l.is_empty() {
            lines.push(l);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub fn registered(_ext: &str) -> bool {
        false
    }

    pub fn ifilter_text(_path: &Path, _deadline: Instant) -> Result<Option<Vec<String>>> {
        Ok(None)
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use windows::Win32::Storage::IndexServer::{
        CHUNK_EOC, CHUNK_EOP, CHUNK_EOS, CHUNK_EOW, CHUNK_TEXT, FILTER_E_ACCESS, FILTER_E_EMBEDDING_UNAVAILABLE, FILTER_E_END_OF_CHUNKS, FILTER_E_LINK_UNAVAILABLE, FILTER_E_NO_MORE_TEXT, FILTER_E_NO_TEXT,
        FILTER_E_PASSWORD, FILTER_E_UNKNOWNFORMAT, FILTER_S_LAST_TEXT, IFILTER_INIT_APPLY_INDEX_ATTRIBUTES, IFILTER_INIT_APPLY_OTHER_ATTRIBUTES, IFILTER_INIT_CANON_HYPHENS, IFILTER_INIT_CANON_PARAGRAPHS,
        IFILTER_INIT_CANON_SPACES, IFILTER_INIT_HARD_LINE_BREAKS, IFilter, LoadIFilter, STAT_CHUNK,
    };
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
    use windows::Win32::System::Registry::{HKEY_CLASSES_ROOT, RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::{HRESULT, IUnknown, Interface, PCWSTR, PWSTR};

    /// "No filter": what Windows registers for types it cannot read.
    const NULL_HANDLER: &str = "{098f2470-bae0-11cd-b579-08002b30bfeb}";
    /// The plain-text filter, which reads any file as raw bytes.
    const TEXT_HANDLER: &str = "{5e941d80-bf96-11cd-b579-08002b30bfeb}";
    const IFILTER_IID: &str = "{89BCB740-6119-101A-BCB7-00DD010655AF}";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// The default value of a key under HKEY_CLASSES_ROOT.
    fn class_value(key: &str) -> Option<String> {
        let key = wide(key);
        let mut buf = [0u16; 1024];
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: the key is NUL-terminated and `bytes` is the size of `buf`.
        let rc = unsafe { RegGetValueW(HKEY_CLASSES_ROOT, PCWSTR(key.as_ptr()), PCWSTR::null(), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr().cast()), Some(&mut bytes)) };
        if !rc.is_ok() {
            return None;
        }
        let len = (bytes as usize / 2).min(buf.len());
        let s = String::from_utf16_lossy(&buf[..len]);
        let s = s.trim_end_matches('\0').trim().to_string();
        (!s.is_empty()).then_some(s)
    }

    pub fn registered(ext: &str) -> bool {
        if ext.is_empty() || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
            return false;
        }
        // The handler of the extension, or of the class the extension opens.
        let handler = class_value(&format!(".{ext}\\PersistentHandler")).or_else(|| {
            let prog = class_value(&format!(".{ext}"))?;
            let clsid = class_value(&format!("{prog}\\CLSID"))?;
            class_value(&format!("CLSID\\{clsid}\\PersistentHandler"))
        });
        let Some(handler) = handler else { return false };
        if handler.eq_ignore_ascii_case(NULL_HANDLER) || handler.eq_ignore_ascii_case(TEXT_HANDLER) {
            return false;
        }
        let Some(filter) = class_value(&format!("CLSID\\{handler}\\PersistentAddinsRegistered\\{IFILTER_IID}")) else { return false };
        class_value(&format!("CLSID\\{filter}\\InprocServer32")).is_some()
    }

    /// The filter cannot be created on this PC (as opposed to: it refused
    /// this file).
    fn not_here(hr: HRESULT) -> bool {
        // REGDB_E_CLASSNOTREG, CO_E_DLLNOTFOUND, module not found, E_NOINTERFACE
        matches!(hr.0 as u32, 0x8004_0154 | 0x8004_01F8 | 0x8007_007E | 0x8000_4002)
    }

    fn refusal(hr: HRESULT) -> anyhow::Error {
        if hr == FILTER_E_PASSWORD {
            anyhow::anyhow!("password protected")
        } else if hr == FILTER_E_UNKNOWNFORMAT {
            anyhow::anyhow!("damaged, or not the format its name says (the text filter does not recognise it)")
        } else if hr == FILTER_E_ACCESS {
            anyhow::anyhow!("the text filter could not open the file")
        } else {
            anyhow::anyhow!("the text filter failed: {}", hr.message())
        }
    }

    /// Runs on the helper thread, start to finish.
    fn filter_text(path: &Path, stop: &AtomicBool) -> Result<Option<Vec<String>>> {
        let wpath: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: the path is NUL-terminated; `raw` receives an owned COM
        // reference on success.
        if let Err(e) = unsafe { LoadIFilter(PCWSTR(wpath.as_ptr()), None::<&IUnknown>, &mut raw) } {
            return if not_here(e.code()) { Ok(None) } else { Err(refusal(e.code())) };
        }
        if raw.is_null() {
            return Ok(None);
        }
        // SAFETY: LoadIFilter returned a COM object; we take its one reference.
        let unknown = unsafe { IUnknown::from_raw(raw) };
        let filter: IFilter = match unknown.cast() {
            Ok(f) => f,
            Err(_) => return Ok(None),
        };
        let flags = IFILTER_INIT_CANON_PARAGRAPHS.0
            | IFILTER_INIT_HARD_LINE_BREAKS.0
            | IFILTER_INIT_CANON_HYPHENS.0
            | IFILTER_INIT_CANON_SPACES.0
            | IFILTER_INIT_APPLY_INDEX_ATTRIBUTES.0
            | IFILTER_INIT_APPLY_OTHER_ATTRIBUTES.0;
        let mut got = 0u32;
        // SAFETY: no attribute list is passed; `got` outlives the call.
        let hr = HRESULT(unsafe { filter.Init(flags as u32, &[], &mut got) });
        if hr.is_err() {
            return Err(refusal(hr));
        }
        let mut lines: Vec<String> = Vec::new();
        let mut text = String::new();
        let mut buf = vec![0u16; 16 * 1024];
        loop {
            if stop.load(Ordering::Relaxed) {
                anyhow::bail!("timed out");
            }
            let mut stat = STAT_CHUNK::default();
            // SAFETY: `stat` is a valid STAT_CHUNK for the filter to fill.
            let hr = HRESULT(unsafe { filter.GetChunk(&mut stat) });
            if hr == FILTER_E_END_OF_CHUNKS {
                break;
            }
            if hr == FILTER_E_EMBEDDING_UNAVAILABLE || hr == FILTER_E_LINK_UNAVAILABLE {
                continue;
            }
            if hr.is_err() {
                // Nothing read so far is handed over as if it were the file.
                return Err(refusal(hr));
            }
            if stat.flags.0 & CHUNK_TEXT.0 == 0 {
                continue;
            }
            // How this chunk is separated from the one before it.
            if stat.breakType == CHUNK_EOP || stat.breakType == CHUNK_EOC {
                split_lines(&std::mem::take(&mut text), &mut lines);
            } else if stat.breakType == CHUNK_EOW || stat.breakType == CHUNK_EOS {
                text.push(' ');
            }
            loop {
                if stop.load(Ordering::Relaxed) {
                    anyhow::bail!("timed out");
                }
                let mut n = buf.len() as u32;
                // SAFETY: `n` is the capacity of `buf` in characters.
                let hr = HRESULT(unsafe { filter.GetText(&mut n, PWSTR(buf.as_mut_ptr())) });
                if hr == FILTER_E_NO_MORE_TEXT || hr == FILTER_E_NO_TEXT {
                    break;
                }
                if hr.is_err() {
                    return Err(refusal(hr));
                }
                let n = (n as usize).min(buf.len());
                text.push_str(&String::from_utf16_lossy(&buf[..n]));
                // Whole lines leave the buffer as they complete.
                if let Some(cut) = text.rfind(['\n', '\r']) {
                    let tail = text.split_off(cut + 1);
                    split_lines(&text, &mut lines);
                    text = tail;
                }
                if hr == FILTER_S_LAST_TEXT || n == 0 {
                    break;
                }
            }
            if lines.len() > MAX_FILTER_LINES {
                break;
            }
        }
        split_lines(&text, &mut lines);
        Ok(Some(lines))
    }

    pub fn ifilter_text(path: &Path, deadline: Instant) -> Result<Option<Vec<String>>> {
        let ext = sleuth_core::kinds::extension(&path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        if !registered(&ext) {
            return Ok(None);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (path, flag) = (path.to_path_buf(), stop.clone());
        let spawned = std::thread::Builder::new().name("ifilter".into()).spawn(move || {
            // SAFETY: COM is initialised and released on this thread only,
            // and every COM object is dropped before CoUninitialize.
            let init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            let result = filter_text(&path, &flag);
            if init.is_ok() {
                unsafe { CoUninitialize() };
            }
            let _ = tx.send(result);
        });
        if let Err(e) = spawned {
            anyhow::bail!("the text filter could not be started: {e}");
        }
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // The thread is left behind; it ends when the filter returns.
                stop.store(true, Ordering::Relaxed);
                anyhow::bail!("timed out")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => anyhow::bail!("the text filter failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_text_splits_into_lines() {
        let mut lines = Vec::new();
        split_lines("ISO-1001-01\r\n\r\n  PSV-1203   set pressure\u{2029}last", &mut lines);
        assert_eq!(lines, ["ISO-1001-01", "PSV-1203 set pressure", "last"]);
    }

    #[test]
    fn unknown_types_have_no_filter() {
        assert!(!registered(""));
        assert!(!registered("no-such-extension"));
        assert!(!registered("zz9qx"));
    }

    #[test]
    fn a_type_without_a_filter_is_not_here() {
        let r = ifilter_text(Path::new("nothing.zz9qx"), Instant::now() + std::time::Duration::from_secs(5));
        assert!(matches!(r, Ok(None)));
    }
}
