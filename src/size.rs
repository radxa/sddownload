//! Checked integer expressions matching the legacy size arguments. No eval,
//! floating point, shell expansion, variables or executable expressions.
use anyhow::{bail, ensure, Context, Result};
struct Parser<'a> {
    text: &'a [u8],
    position: usize,
}

impl Parser<'_> {
    fn whitespace(&mut self) {
        while self
            .text
            .get(self.position)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.position += 1
        }
    }

    fn atom(&mut self) -> Result<i128> {
        self.whitespace();
        let c = *self
            .text
            .get(self.position)
            .context("missing size expression")?;

        if c == b'+' || c == b'-' {
            self.position += 1;
            let value = self.atom()?;

            return if c == b'-' {
                value.checked_neg().context("size expression overflow")
            } else {
                Ok(value)
            };
        }

        if c == b'(' {
            self.position += 1;
            let value = self.expression(0)?;
            self.whitespace();

            ensure!(
                self.text.get(self.position) == Some(&b')'),
                "missing closing parenthesis"
            );
            self.position += 1;

            return Ok(value);
        }

        let start = self.position;

        while self
            .text
            .get(self.position)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
        {
            self.position += 1
        }

        ensure!(self.position > start, "invalid size expression");
        let raw = std::str::from_utf8(&self.text[start..self.position])?.replace('_', "");
        let (digits, radix) =
            if let Some(s) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
                (s, 16)
            } else if let Some(s) = raw.strip_prefix("0o").or_else(|| raw.strip_prefix("0O")) {
                (s, 8)
            } else if let Some(s) = raw.strip_prefix("0b").or_else(|| raw.strip_prefix("0B")) {
                (s, 2)
            } else {
                (raw.as_str(), 10)
            };

        i128::from_str_radix(digits, radix).context("invalid size integer")
    }

    fn expression(&mut self, min_precedence: u8) -> Result<i128> {
        let mut left = self.atom()?;

        loop {
            self.whitespace();
            let tail = &self.text[self.position..];
            let (op, precedence, len) = if tail.starts_with(b"<<") {
                (b'<', 1, 2)
            } else if tail.starts_with(b">>") {
                (b'>', 1, 2)
            } else if tail.starts_with(b"//") {
                (b'/', 3, 2)
            } else {
                match tail.first() {
                    Some(b'+') => (b'+', 2, 1),
                    Some(b'-') => (b'-', 2, 1),
                    Some(b'*') => (b'*', 3, 1),
                    _ => break,
                }
            };

            if precedence < min_precedence {
                break;
            }
            self.position += len;
            let right = self.expression(precedence + 1)?;
            left = match op {
                b'+' => left.checked_add(right),
                b'-' => left.checked_sub(right),
                b'*' => left.checked_mul(right),
                b'/' => {
                    ensure!(right != 0, "division by zero in size");
                    let q = left.checked_div(right).context("size division overflow")?;
                    let r = left % right;
                    Some(if r != 0 && (r > 0) != (right > 0) {
                        q - 1
                    } else {
                        q
                    })
                }
                b'<' => {
                    ensure!(
                        (0..64).contains(&right),
                        "size shift must be between 0 and 63"
                    );
                    left.checked_mul(1i128 << right)
                }
                b'>' => {
                    ensure!(
                        (0..64).contains(&right),
                        "size shift must be between 0 and 63"
                    );
                    Some(left >> right)
                }
                _ => unreachable!(),
            }
            .context("size arithmetic overflow")?;
        }

        Ok(left)
    }
}

pub fn parse(text: &str) -> Result<u64> {
    let mut parser = Parser {
        text: text.as_bytes(),
        position: 0,
    };
    let value = parser.expression(0)?;
    parser.whitespace();

    if parser.position != parser.text.len() {
        bail!("unsupported size expression")
    };

    u64::try_from(value).context("size must be between zero and 0xffffffffffffffff")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arithmetic() {
        for (text, value) in [
            ("0xffffffffffffffff", u64::MAX),
            ("2*1024*1024", 2097152),
            ("1+2<<3", 24),
            ("(-3//2)+4", 2),
            ("(1<<32)+4", 0x100000004),
        ] {
            assert_eq!(parse(text).unwrap(), value)
        }
    }

    #[test]
    fn reject_code_and_overflow() {
        for text in [
            "__import__('os')",
            "1/2",
            "1<<64",
            "-1",
            "1//0",
            "0xffffffffffffffff+1",
        ] {
            assert!(parse(text).is_err(), "{text}")
        }
    }
}
