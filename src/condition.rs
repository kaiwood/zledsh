use std::{
    ffi::CString,
    fs,
    mem::MaybeUninit,
    os::unix::fs::{FileTypeExt, MetadataExt},
};

pub(crate) fn unary(op: &str, value: &str) -> bool {
    match op {
        "-n" => !value.is_empty(),
        "-z" => value.is_empty(),
        "-L" | "-h" => fs::symlink_metadata(value).is_ok_and(|m| m.is_symlink()),
        "-r" | "-w" | "-x" => CString::new(value).is_ok_and(|path| {
            let mode = match op {
                "-r" => libc::R_OK,
                "-w" => libc::W_OK,
                _ => libc::X_OK,
            };
            unsafe { libc::faccessat(libc::AT_FDCWD, path.as_ptr(), mode, libc::AT_EACCESS) == 0 }
        }),
        _ => fs::metadata(value).is_ok_and(|m| match op {
            "-e" | "-a" => true,
            "-f" => m.is_file(),
            "-d" => m.is_dir(),
            "-s" => m.len() != 0,
            "-b" => m.file_type().is_block_device(),
            "-c" => m.file_type().is_char_device(),
            "-p" => m.file_type().is_fifo(),
            "-S" => m.file_type().is_socket(),
            _ => false,
        }),
    }
}

pub(crate) fn binary(op: &str, left: &str, right: &str) -> bool {
    if op == "<" {
        return left < right;
    }
    if op == ">" {
        return left > right;
    }
    let left = fs::metadata(left).ok();
    let right = fs::metadata(right).ok();
    match (op, left, right) {
        ("-ef", Some(a), Some(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        ("-nt", Some(a), Some(b)) => (a.mtime(), a.mtime_nsec()) > (b.mtime(), b.mtime_nsec()),
        ("-ot", Some(a), Some(b)) => (a.mtime(), a.mtime_nsec()) < (b.mtime(), b.mtime_nsec()),
        ("-nt", Some(_), None) | ("-ot", None, Some(_)) => true,
        _ => false,
    }
}

pub(crate) fn quote_regex(text: &str) -> String {
    let mut quoted = String::new();
    for ch in text.chars() {
        if ".^$*+?()[]{}|\\".contains(ch) {
            quoted.push('\\');
        }
        quoted.push(ch);
    }
    quoted
}

pub(crate) fn regex(text: &str, pattern: &str) -> Result<bool, String> {
    let text = CString::new(text).map_err(|_| "conditional operand contains a NUL byte")?;
    let pattern = CString::new(pattern).map_err(|_| "regular expression contains a NUL byte")?;
    let mut compiled = MaybeUninit::uninit();
    // POSIX regex state is initialized only on success and freed exactly once.
    unsafe {
        if libc::regcomp(
            compiled.as_mut_ptr(),
            pattern.as_ptr(),
            libc::REG_EXTENDED | libc::REG_NOSUB,
        ) != 0
        {
            return Err("invalid regular expression".into());
        }
        let mut compiled = compiled.assume_init();
        let status = libc::regexec(&compiled, text.as_ptr(), 0, std::ptr::null_mut(), 0);
        libc::regfree(&mut compiled);
        match status {
            0 => Ok(true),
            libc::REG_NOMATCH => Ok(false),
            _ => Err("regular expression match failed".into()),
        }
    }
}
