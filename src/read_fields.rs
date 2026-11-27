pub(crate) type Unit = Result<char, u8>;

pub(crate) fn units(bytes: &[u8]) -> impl Iterator<Item = Unit> + '_ {
    bytes.utf8_chunks().flat_map(|chunk| {
        chunk
            .valid()
            .chars()
            .map(Ok)
            .chain(chunk.invalid().iter().copied().map(Err))
    })
}

// POSIX read stops splitting before the last variable, which receives the
// original remainder (not a rejoin of the split fields).
pub(crate) fn split(mut input: &[(Unit, bool)], ifs: &str, count: usize) -> Vec<Vec<u8>> {
    let is_separator =
        |&(ch, escaped): &(Unit, bool)| !escaped && ch.is_ok_and(|ch| ifs.contains(ch));
    let is_white =
        |item: &(Unit, bool)| is_separator(item) && item.0.is_ok_and(|ch| " \t\n".contains(ch));
    let trim_start =
        |input: &[(Unit, bool)]| input.iter().take_while(|item| is_white(item)).count();
    let text = |input: &[(Unit, bool)]| {
        let mut bytes = Vec::new();
        for &(unit, _) in input {
            match unit {
                Ok(ch) => bytes.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes()),
                Err(byte) => bytes.push(byte),
            }
        }
        bytes
    };
    let mut fields = Vec::with_capacity(count);
    if !ifs.is_empty() {
        for _ in 1..count {
            input = &input[trim_start(input)..];
            if input.is_empty() {
                break;
            }
            let end = input.iter().position(is_separator).unwrap_or(input.len());
            fields.push(text(&input[..end]));
            input = &input[end..];
            input = &input[trim_start(input)..];
            if input.first().is_some_and(is_separator) {
                input = &input[1..];
            }
        }
        let start = trim_start(input);
        let end = input
            .iter()
            .rposition(|item| !is_white(item))
            .map_or(start, |index| index + 1);
        input = &input[start..end];
    }
    fields.push(text(input));
    fields.resize(count, Vec::new());
    fields
}
