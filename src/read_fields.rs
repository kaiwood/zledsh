// POSIX read stops splitting before the last variable, which receives the
// original remainder (not a rejoin of the split fields).
pub(crate) fn split(mut input: &[(char, bool)], ifs: &str, count: usize) -> Vec<String> {
    let is_separator = |&(ch, escaped): &(char, bool)| !escaped && ifs.contains(ch);
    let is_white = |item: &(char, bool)| is_separator(item) && " \t\n".contains(item.0);
    let trim_start =
        |input: &[(char, bool)]| input.iter().take_while(|item| is_white(item)).count();
    let text = |input: &[(char, bool)]| input.iter().map(|&(ch, _)| ch).collect::<String>();
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
    fields.resize(count, String::new());
    fields
}
