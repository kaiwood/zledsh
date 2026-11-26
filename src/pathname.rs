use std::ffi::{CStr, CString};
use std::mem;

// Translate shell quoting into the pattern notation understood by glob(3).
// Return no pattern if there is nothing to expand (POSIX 2.14.3).
pub(crate) fn pattern(chars: impl Iterator<Item = (char, bool)>) -> Option<String> {
    let mut pattern = String::new();
    let mut active = false;
    let mut chars = chars;
    while let Some((ch, quoted)) = chars.next() {
        if ch == '\\' && !quoted {
            // A backslash introduced by expansion can itself quote the next character.
            let (escaped, _) = chars.next()?;
            if escaped != '/' {
                pattern.push('\\');
            }
            pattern.push(escaped);
        } else {
            active |= !quoted && matches!(ch, '*' | '?');
            if (quoted && matches!(ch, '*' | '?' | '\\')) || ch == '[' {
                pattern.push('\\');
            }
            pattern.push(ch);
        }
    }
    active.then_some(pattern)
}

pub(crate) fn expand(pattern: String) -> Result<Vec<String>, String> {
    let pattern = CString::new(pattern).map_err(|_| "pathname pattern contains a NUL byte")?;
    // SAFETY: glob_t is a C structure of integers and pointers. Starting with a
    // zeroed record also makes it safe to free when glob returns an error.
    let mut paths = Glob(unsafe { mem::zeroed() });
    // SAFETY: pattern is NUL-terminated, and paths is a writable glob_t.
    let result = unsafe { libc::glob(pattern.as_ptr(), 0, None, &mut paths.0) };
    match result {
        0 => (0..paths.0.gl_pathc)
            .map(|i| {
                // SAFETY: on success, glob provides gl_pathc non-null pointers
                // to NUL-terminated strings, all owned by paths.
                let path = unsafe { CStr::from_ptr(*paths.0.gl_pathv.add(i)) };
                path.to_str()
                    .map(str::to_owned)
                    .map_err(|_| "pathname match is not UTF-8".into())
            })
            .collect(),
        libc::GLOB_NOMATCH => Ok(Vec::new()),
        _ => Err("pathname expansion failed".into()),
    }
}

struct Glob(libc::glob_t);

impl Drop for Glob {
    fn drop(&mut self) {
        // SAFETY: this record was passed to glob and is freed exactly once.
        unsafe { libc::globfree(&mut self.0) };
    }
}
