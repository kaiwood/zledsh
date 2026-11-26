pub(crate) struct Step {
    pub index: usize,
    pub offset: usize,
    pub option: char,
    pub argument: Option<String>,
    pub diagnostic: Option<String>,
    pub status: u8,
}

pub(crate) fn next(spec: &str, parameters: &[String], index: usize, offset: usize) -> Step {
    let mut step = Step {
        index,
        offset: 0,
        option: '?',
        argument: None,
        diagnostic: None,
        status: 1,
    };
    let Some(word) = parameters.get(index - 1) else {
        step.index = parameters.len() + 1;
        return step;
    };
    if word == "--" {
        step.index += 1;
        return step;
    }
    let Some(options) = word.strip_prefix('-').filter(|options| !options.is_empty()) else {
        return step;
    };
    let Some((byte, option)) = options.char_indices().nth(offset) else {
        return step;
    };
    let rest = &options[byte + option.len_utf8()..];
    if rest.is_empty() {
        step.index += 1;
    } else {
        step.offset = offset + 1;
    }
    step.status = 0;
    let silent = spec.starts_with(':');
    let mut letters = spec.trim_start_matches(':').chars().peekable();
    let supported = option != ':' && option != '?' && letters.any(|letter| letter == option);
    if !supported {
        if silent {
            step.argument = Some(option.to_string());
        } else {
            step.diagnostic = Some(format!("unknown option: -{option}"));
        }
    } else if letters.peek() != Some(&':') {
        step.option = option;
    } else if !rest.is_empty() {
        step.index += 1;
        step.offset = 0;
        step.option = option;
        step.argument = Some(rest.into());
    } else if let Some(argument) = parameters.get(index) {
        step.index += 1;
        step.option = option;
        step.argument = Some(argument.clone());
    } else if silent {
        step.option = ':';
        step.argument = Some(option.to_string());
    } else {
        step.diagnostic = Some(format!("option -{option} requires an argument"));
    }
    step
}
