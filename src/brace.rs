use crate::syntax::{Part, Word, WordPart};

const LIMIT: usize = 4096;

pub(crate) fn alternatives(word: &Word) -> Result<Option<Vec<Word>>, String> {
    if !word
        .parts
        .iter()
        .any(|part| !part.quoted && matches!(&part.kind, Part::Literal(text) if text.contains('{')))
    {
        return Ok(None);
    }
    let mut parts = Vec::new();
    for part in &word.parts {
        if let Part::Literal(text) = &part.kind
            && !part.quoted
        {
            parts.extend(text.chars().map(|ch| literal(ch.to_string())));
        } else {
            parts.push(part.clone());
        }
    }
    let mut queue = vec![parts];
    let mut output = Vec::new();
    let mut expanded = false;
    while let Some(mut parts) = queue.pop() {
        if let Some((start, end, choices)) = group(&parts)? {
            expanded = true;
            if output.len() + queue.len() + choices.len() > LIMIT {
                return Err("too many brace alternatives".into());
            }
            for choice in choices.into_iter().rev() {
                let mut next = parts[..start].to_vec();
                next.extend(choice);
                next.extend_from_slice(&parts[end + 1..]);
                queue.push(next);
            }
        } else {
            if parts.is_empty() {
                parts = Word::literal(String::new()).parts;
            }
            output.push(Word {
                parts,
                ..word.clone()
            });
        }
    }
    Ok(expanded.then_some(output))
}

type Group = (usize, usize, Vec<Vec<WordPart>>);

fn group(parts: &[WordPart]) -> Result<Option<Group>, String> {
    let mut open = Vec::new();
    let mut best = None;
    for (i, part) in parts.iter().enumerate() {
        match text(part) {
            Some("{") => open.push((i, Vec::new())),
            Some(",") => {
                if let Some((_, commas)) = open.last_mut() {
                    commas.push(i);
                }
            }
            Some("}") => {
                if let Some((start, commas)) = open.pop() {
                    if best
                        .as_ref()
                        .is_some_and(|&(earlier, _, _)| earlier < start)
                    {
                        continue;
                    }
                    if commas.is_empty() {
                        if let Some(values) = sequence(&parts[start + 1..i])? {
                            best = Some((start, i, values));
                        }
                    } else {
                        let mut from = start + 1;
                        let values = commas
                            .into_iter()
                            .chain([i])
                            .map(|end| {
                                let choice = parts[from..end].to_vec();
                                from = end + 1;
                                choice
                            })
                            .collect();
                        best = Some((start, i, values));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(best)
}

fn text(part: &WordPart) -> Option<&str> {
    match &part.kind {
        Part::Literal(text) if !part.quoted => Some(text),
        _ => None,
    }
}

fn literal(text: String) -> WordPart {
    WordPart {
        kind: Part::Literal(text),
        quoted: false,
    }
}

fn sequence(parts: &[WordPart]) -> Result<Option<Vec<Vec<WordPart>>>, String> {
    let Some(value) = parts.iter().map(text).collect::<Option<String>>() else {
        return Ok(None);
    };
    let fields: Vec<_> = value.split("..").collect();
    let (first, last, step) = match fields.as_slice() {
        [first, last] => (*first, *last, 1u64),
        [first, last, step] => match step.parse::<i64>() {
            Ok(step) => (*first, *last, step.unsigned_abs().max(1)),
            Err(_) => return Ok(None),
        },
        _ => return Ok(None),
    };
    let padded = |s: &str| {
        let digits = s.strip_prefix('-').unwrap_or(s);
        digits.len() > 1 && digits.starts_with('0')
    };
    let (first, last, letters, width) = match (first.parse::<i64>(), last.parse::<i64>()) {
        (Ok(a), Ok(b)) if !first.starts_with('+') && !last.starts_with('+') => (
            a,
            b,
            false,
            if padded(first) || padded(last) {
                first.len().max(last.len())
            } else {
                0
            },
        ),
        _ => match (first.as_bytes(), last.as_bytes()) {
            ([a], [b]) if a.is_ascii_alphabetic() && b.is_ascii_alphabetic() => {
                (i64::from(*a), i64::from(*b), true, 0)
            }
            _ => return Ok(None),
        },
    };
    let count = first.abs_diff(last) / step;
    if count >= LIMIT as u64 {
        return Err("too many brace alternatives".into());
    }
    let direction = if first <= last { 1 } else { -1 };
    Ok(Some(
        (0..=count)
            .map(|offset| {
                let number =
                    (i128::from(first) + direction * i128::from(offset) * i128::from(step)) as i64;
                let value = if letters {
                    char::from(number as u8).to_string()
                } else {
                    format!("{number:0width$}")
                };
                vec![literal(value)]
            })
            .collect(),
    ))
}
