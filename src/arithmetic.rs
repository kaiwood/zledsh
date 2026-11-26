//! Signed integer expressions for POSIX shell arithmetic expansion.

#[derive(Clone, Copy)]
enum Token<'a> {
    Number(i64),
    Name(&'a str),
    Op(&'a str),
    End,
}

fn token(source: &str) -> Result<(Token<'_>, &str), &'static str> {
    let source = source.trim_start();
    let Some(first) = source.bytes().next() else {
        return Ok((Token::End, source));
    };
    if first.is_ascii_alphanumeric() || first == b'_' {
        let end = source
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
            .count();
        let (text, rest) = source.split_at(end);
        return if first.is_ascii_digit() {
            Ok((
                Token::Number(integer(text).ok_or("invalid integer constant")?),
                rest,
            ))
        } else {
            Ok((Token::Name(text), rest))
        };
    }
    for op in [
        "<<=", ">>=", "||", "&&", "==", "!=", "<=", ">=", "<<", ">>", "+=", "-=", "*=", "/=", "%=",
        "&=", "^=", "|=", "(", ")", "?", ":", ",", "=", "|", "^", "&", "<", ">", "+", "-", "*",
        "/", "%", "!", "~",
    ] {
        if let Some(rest) = source.strip_prefix(op) {
            return Ok((Token::Op(op), rest));
        }
    }
    Err("invalid arithmetic token")
}

fn integer(text: &str) -> Option<i64> {
    let (sign, digits) = match text.as_bytes().first() {
        Some(b'-' | b'+') => (&text[..1], &text[1..]),
        _ => ("", text),
    };
    let (radix, digits) = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        (16, hex)
    } else if digits.starts_with('0') {
        (8, digits)
    } else {
        (10, digits)
    };
    if sign.is_empty() {
        i64::from_str_radix(digits, radix).ok()
    } else {
        i64::from_str_radix(&format!("{sign}{digits}"), radix).ok()
    }
}

struct Value<'a> {
    number: i64,
    name: Option<&'a str>,
}

impl Value<'_> {
    fn number(number: i64) -> Self {
        Self { number, name: None }
    }
}

pub(crate) fn evaluate(
    source: &str,
    variable: impl FnMut(&str, Option<i64>) -> Result<Option<String>, &'static str>,
) -> Result<i64, &'static str> {
    let mut parser = Parser {
        rest: source,
        variable,
    };
    let value = parser.expression(0, true, 0)?;
    if !matches!(parser.peek()?, Token::End) {
        return Err("unexpected arithmetic token");
    }
    parser.resolve(value, true)
}

struct Parser<'a, F> {
    rest: &'a str,
    variable: F,
}

impl<'a, F: FnMut(&str, Option<i64>) -> Result<Option<String>, &'static str>> Parser<'a, F> {
    fn peek(&self) -> Result<Token<'a>, &'static str> {
        Ok(token(self.rest)?.0)
    }

    fn next(&mut self) -> Result<Token<'a>, &'static str> {
        let (token, rest) = token(self.rest)?;
        self.rest = rest;
        Ok(token)
    }

    fn expect(&mut self, expected: &str) -> Result<(), &'static str> {
        match self.next()? {
            Token::Op(op) if op == expected => Ok(()),
            _ => Err("missing arithmetic delimiter"),
        }
    }

    fn resolve(&mut self, value: Value<'_>, active: bool) -> Result<i64, &'static str> {
        if !active {
            return Ok(0);
        }
        let Some(name) = value.name else {
            return Ok(value.number);
        };
        let Some(text) = (self.variable)(name, None)? else {
            return Ok(value.number);
        };
        if text.trim().is_empty() {
            Ok(0)
        } else {
            integer(text.trim()).ok_or("arithmetic variable is not an integer")
        }
    }

    fn expression(
        &mut self,
        minimum: u8,
        active: bool,
        depth: u8,
    ) -> Result<Value<'a>, &'static str> {
        if depth >= 64 {
            return Err("arithmetic expression nested too deeply");
        }
        let mut left = match self.next()? {
            Token::Number(number) => Value::number(number),
            Token::Name(name) => Value {
                number: 0,
                name: Some(name),
            },
            Token::Op("(") => {
                let value = self.expression(0, active, depth + 1)?;
                self.expect(")")?;
                value
            }
            Token::Op(op @ ("+" | "-" | "!" | "~")) => {
                let operand = self.expression(13, active, depth + 1)?;
                let number = self.resolve(operand, active)?;
                Value::number(match op {
                    "+" => number,
                    "-" => number.wrapping_neg(),
                    "!" => i64::from(number == 0),
                    _ => !number,
                })
            }
            _ => return Err("expected arithmetic operand"),
        };
        while let Token::Op(op) = self.peek()? {
            let power = match op {
                "," => 0,
                "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "<<=" | ">>=" | "&=" | "^=" | "|=" => 1,
                "?" => 2,
                "||" => 3,
                "&&" => 4,
                "|" => 5,
                "^" => 6,
                "&" => 7,
                "==" | "!=" => 8,
                "<" | "<=" | ">" | ">=" => 9,
                "<<" | ">>" => 10,
                "+" | "-" => 11,
                "*" | "/" | "%" => 12,
                _ => break,
            };
            if power < minimum {
                break;
            }
            self.next()?;
            let name = if power == 1 {
                Some(
                    left.name
                        .ok_or("arithmetic assignment requires a variable")?,
                )
            } else {
                None
            };
            let lhs = if op == "=" {
                0
            } else {
                self.resolve(left, active)?
            };
            if op == "?" {
                let yes = self.expression(0, active && lhs != 0, depth + 1)?;
                let yes = self.resolve(yes, active && lhs != 0)?;
                self.expect(":")?;
                let no = self.expression(2, active && lhs == 0, depth + 1)?;
                let no = self.resolve(no, active && lhs == 0)?;
                left = Value::number(if lhs != 0 { yes } else { no });
                continue;
            }
            let rhs_active = active && !(op == "&&" && lhs == 0 || op == "||" && lhs != 0);
            let right = self.expression(power + u8::from(power != 1), rhs_active, depth + 1)?;
            let rhs = self.resolve(right, rhs_active)?;
            let result = if !active {
                0
            } else if op == "=" {
                rhs
            } else {
                binary(if power == 1 { &op[..op.len() - 1] } else { op }, lhs, rhs)?
            };
            if let Some(name) = name.filter(|_| active) {
                (self.variable)(name, Some(result))?;
            }
            left = Value::number(result);
        }
        Ok(left)
    }
}

fn binary(op: &str, lhs: i64, rhs: i64) -> Result<i64, &'static str> {
    Ok(match op {
        "," => rhs,
        "+" => lhs.wrapping_add(rhs),
        "-" => lhs.wrapping_sub(rhs),
        "*" => lhs.wrapping_mul(rhs),
        "/" => lhs.checked_div(rhs).ok_or("invalid arithmetic division")?,
        "%" => lhs.checked_rem(rhs).ok_or("invalid arithmetic remainder")?,
        "<<" | ">>" => {
            let shift = u32::try_from(rhs)
                .ok()
                .filter(|&value| value < i64::BITS)
                .ok_or("invalid shift count")?;
            if op == "<<" {
                lhs << shift
            } else {
                lhs >> shift
            }
        }
        "&" => lhs & rhs,
        "|" => lhs | rhs,
        "^" => lhs ^ rhs,
        "&&" => i64::from(lhs != 0 && rhs != 0),
        "||" => i64::from(lhs != 0 || rhs != 0),
        "==" => i64::from(lhs == rhs),
        "!=" => i64::from(lhs != rhs),
        "<" => i64::from(lhs < rhs),
        "<=" => i64::from(lhs <= rhs),
        ">" => i64::from(lhs > rhs),
        ">=" => i64::from(lhs >= rhs),
        _ => unreachable!("unknown arithmetic operator"),
    })
}
