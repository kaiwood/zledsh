pub(crate) fn current() -> libc::mode_t {
    // There is no portable read-only query. Use a restrictive mask briefly.
    let saved = unsafe { libc::umask(0o777) };
    unsafe { libc::umask(saved) };
    saved & 0o777
}

pub(crate) fn symbolic(mask: libc::mode_t) -> String {
    let permissions = |shift: u32| -> String {
        ['r', 'w', 'x']
            .into_iter()
            .zip([4 as libc::mode_t, 2, 1])
            .filter_map(|(letter, bit)| (mask & (bit << shift) == 0).then_some(letter))
            .collect()
    };
    format!(
        "u={},g={},o={}",
        permissions(6),
        permissions(3),
        permissions(0)
    )
}

pub(crate) fn parse(value: &str, mask: libc::mode_t) -> Option<libc::mode_t> {
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return libc::mode_t::from_str_radix(value, 8)
            .ok()
            .filter(|&mode| mode <= 0o777);
    }
    let mut allowed = !mask & 0o777;
    for clause in value.split(',') {
        let mut chars = clause.chars().peekable();
        let mut classes = 0;
        while let Some(class) = chars.next_if(|ch| matches!(ch, 'a' | 'u' | 'g' | 'o')) {
            classes |= match class {
                'u' => 0o700,
                'g' => 0o070,
                'o' => 0o007,
                _ => 0o777,
            };
        }
        if classes == 0 {
            classes = 0o777;
        }
        loop {
            let op = chars.next_if(|ch| matches!(ch, '+' | '-' | '='))?;
            let permissions = if let Some(copy) = chars.next_if(|ch| matches!(ch, 'u' | 'g' | 'o'))
            {
                (allowed
                    >> match copy {
                        'u' => 6,
                        'g' => 3,
                        _ => 0,
                    })
                    & 7
            } else {
                let mut bits = 0;
                while let Some(letter) = chars.next_if(|ch| !matches!(ch, '+' | '-' | '=')) {
                    bits |= match letter {
                        'r' => 4,
                        'w' => 2,
                        'x' => 1,
                        'X' => libc::mode_t::from(allowed & 0o111 != 0),
                        _ => return None,
                    };
                }
                bits
            };
            let bits = (permissions * 0o111) & classes;
            allowed = match op {
                '+' => allowed | bits,
                '-' => allowed & !bits,
                _ => (allowed & !classes) | bits,
            };
            if chars.peek().is_none() {
                break;
            }
        }
    }
    Some(!allowed & 0o777)
}
