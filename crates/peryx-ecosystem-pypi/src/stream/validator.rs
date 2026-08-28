//! A byte-at-a-time RFC 8259 grammar validator running alongside the structural rewriter.
//!
//! [`PageTransformer`](super::PageTransformer) rewrites a page's recognized members but passes
//! unrecognized top-level bytes through untouched, so malformed punctuation (`"unknown":,`) or a
//! non-object root (`123`) could reach balanced depth and finish clean, letting the cold path
//! publish bytes that later buffered reads reject. This validator enforces the whole JSON grammar
//! and the PEP 691 requirement that the document root be an object, without buffering the page: its
//! only unbounded state is a container-kind stack proportional to nesting depth, which
//! [`MAX_PAGE_BYTES`](super::MAX_PAGE_BYTES) already caps.

use super::{TransformError, is_json_whitespace};

/// The container a close bracket must match and a comma must continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
    Object,
    Array,
}

/// The keyword a literal is spelling out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keyword {
    True,
    False,
    Null,
}

impl Keyword {
    const fn bytes(self) -> &'static [u8] {
        match self {
            Self::True => b"true",
            Self::False => b"false",
            Self::Null => b"null",
        }
    }
}

/// The escape-decoding position inside a string literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Escape {
    None,
    Backslash,
    Unicode {
        value: u16,
        digits: u8,
        low_surrogate: bool,
    },
    SurrogateBackslash,
    SurrogateU,
    Utf8 {
        value: u32,
        remaining: u8,
        min: u32,
    },
}

/// The number DFA position. Accepting states may end the number; the rest still need input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Number {
    /// Saw a leading `-`; a digit must follow.
    Minus,
    /// Saw a single `0`; only a fraction or exponent may extend it.
    Zero,
    /// Saw `1-9` then digits.
    Int,
    /// Saw `.`; a fraction digit must follow.
    Dot,
    /// Inside fraction digits.
    Frac,
    /// Saw `e`/`E`; a sign or digit must follow.
    Exp,
    /// Saw the exponent sign; a digit must follow.
    ExpSign,
    /// Inside exponent digits.
    ExpDigit,
}

impl Number {
    const fn accepting(self) -> bool {
        matches!(self, Self::Zero | Self::Int | Self::Frac | Self::ExpDigit)
    }
}

/// The next byte the grammar expects, or a scalar token being consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// The start of a value: the document root, an array element, or an object member value.
    Value,
    /// Just after `[`: a value, or `]` to close an empty array.
    ArrayFirst,
    /// Just after `{`: a key string, or `}` to close an empty object.
    ObjectFirst,
    /// After a member separator inside an object: a key string.
    ObjectKey,
    /// After an object key: the `:` separator.
    Colon,
    /// After a value inside a container: `,` or the matching close bracket.
    Comma,
    /// The root value closed: only trailing whitespace remains.
    Done,
    /// Inside a string. `key` marks an object key so its close returns to [`State::Colon`].
    Str { escape: Escape, key: bool },
    /// Inside a number.
    Num(Number),
    /// Inside a `true`/`false`/`null` literal, holding how many bytes it has matched.
    Lit { keyword: Keyword, index: u8 },
}

/// Streaming RFC 8259 validator with a PEP 691 object-root requirement.
#[derive(Debug)]
pub(super) struct JsonValidator {
    stack: Vec<Container>,
    state: State,
    /// The first grammar violation seen, surfaced at [`Self::result`].
    failed: bool,
    /// The root value has been fully consumed.
    complete: bool,
}

impl JsonValidator {
    pub(super) const fn new() -> Self {
        Self {
            stack: Vec::new(),
            state: State::Value,
            failed: false,
            complete: false,
        }
    }

    /// # Errors
    /// Returns [`TransformError::Malformed`] when the grammar was violated or the root value never
    /// closed as a single object.
    pub(super) const fn result(&self) -> Result<(), TransformError> {
        if self.failed || !self.complete {
            return Err(TransformError::Malformed);
        }
        Ok(())
    }

    /// Advance the grammar by one raw upstream byte. A violation is latched and every later byte is
    /// ignored, so the caller can keep streaming without the validator misbehaving.
    pub(super) fn feed(&mut self, byte: u8) {
        if self.failed {
            return;
        }
        // Only a number ends without consuming its terminator, so that byte is replayed in the state
        // the finished number leaves behind; every other transition consumes its byte outright.
        loop {
            match self.state {
                State::Str { escape, key } => {
                    self.feed_string(byte, escape, key);
                    return;
                }
                State::Num(number) => match step_number(number, byte) {
                    NumberStep::Continue(next) => {
                        self.state = State::Num(next);
                        return;
                    }
                    NumberStep::Terminate => {
                        if !number.accepting() {
                            self.failed = true;
                            return;
                        }
                        self.finish_value();
                    }
                    NumberStep::Reject => {
                        self.failed = true;
                        return;
                    }
                },
                State::Lit { keyword, index } => {
                    self.feed_literal(byte, keyword, index);
                    return;
                }
                structural => {
                    self.feed_structural(byte, structural);
                    return;
                }
            }
        }
    }

    fn feed_structural(&mut self, byte: u8, state: State) {
        if is_json_whitespace(byte) {
            return;
        }
        match state {
            State::Value => self.begin_value(byte),
            State::ArrayFirst => {
                if byte == b']' {
                    self.close(Container::Array);
                } else {
                    self.begin_value(byte);
                }
            }
            State::ObjectFirst => match byte {
                b'}' => self.close(Container::Object),
                b'"' => {
                    self.state = State::Str {
                        escape: Escape::None,
                        key: true,
                    }
                }
                _ => self.failed = true,
            },
            State::ObjectKey => {
                if byte == b'"' {
                    self.state = State::Str {
                        escape: Escape::None,
                        key: true,
                    };
                } else {
                    self.failed = true;
                }
            }
            State::Colon => {
                if byte == b':' {
                    self.state = State::Value;
                } else {
                    self.failed = true;
                }
            }
            State::Comma => match byte {
                // A comma is reached only inside a container, so the stack is never empty here; an
                // object member wants a key next, an array element wants a value.
                b',' => {
                    self.state = if self.stack.last() == Some(&Container::Object) {
                        State::ObjectKey
                    } else {
                        State::Value
                    };
                }
                b']' => self.close(Container::Array),
                b'}' => self.close(Container::Object),
                _ => self.failed = true,
            },
            State::Done | State::Str { .. } | State::Num(_) | State::Lit { .. } => self.failed = true,
        }
    }

    fn begin_value(&mut self, byte: u8) {
        if self.stack.is_empty() && byte != b'{' {
            self.failed = true;
            return;
        }
        match byte {
            b'{' => {
                self.stack.push(Container::Object);
                self.state = State::ObjectFirst;
            }
            b'[' => {
                self.stack.push(Container::Array);
                self.state = State::ArrayFirst;
            }
            b'"' => {
                self.state = State::Str {
                    escape: Escape::None,
                    key: false,
                }
            }
            b't' => {
                self.state = State::Lit {
                    keyword: Keyword::True,
                    index: 1,
                }
            }
            b'f' => {
                self.state = State::Lit {
                    keyword: Keyword::False,
                    index: 1,
                }
            }
            b'n' => {
                self.state = State::Lit {
                    keyword: Keyword::Null,
                    index: 1,
                }
            }
            b'-' => self.state = State::Num(Number::Minus),
            b'0' => self.state = State::Num(Number::Zero),
            b'1'..=b'9' => self.state = State::Num(Number::Int),
            _ => self.failed = true,
        }
    }

    fn close(&mut self, expected: Container) {
        if self.stack.pop() == Some(expected) {
            self.finish_value();
        } else {
            self.failed = true;
        }
    }

    const fn finish_value(&mut self) {
        if self.stack.is_empty() {
            self.state = State::Done;
            self.complete = true;
        } else {
            self.state = State::Comma;
        }
    }

    const fn feed_string(&mut self, byte: u8, escape: Escape, key: bool) {
        match escape {
            Escape::None => self.feed_unescaped_string(byte, key),
            Escape::Backslash => self.feed_escaped_string(byte, key),
            Escape::Unicode {
                value,
                digits,
                low_surrogate,
            } => self.feed_unicode_escape(byte, value, digits, low_surrogate, key),
            Escape::SurrogateBackslash => {
                if byte == b'\\' {
                    self.state = State::Str {
                        escape: Escape::SurrogateU,
                        key,
                    };
                } else {
                    self.failed = true;
                }
            }
            Escape::SurrogateU => {
                if byte == b'u' {
                    self.state = State::Str {
                        escape: Escape::Unicode {
                            value: 0,
                            digits: 0,
                            low_surrogate: true,
                        },
                        key,
                    };
                } else {
                    self.failed = true;
                }
            }
            Escape::Utf8 { value, remaining, min } => self.feed_utf8(byte, value, remaining, min, key),
        }
    }

    const fn feed_unescaped_string(&mut self, byte: u8, key: bool) {
        match byte {
            b'"' => {
                if key {
                    self.state = State::Colon;
                } else {
                    self.finish_value();
                }
            }
            b'\\' => {
                self.state = State::Str {
                    escape: Escape::Backslash,
                    key,
                }
            }
            0x20..=0x7F => {}
            0xC2..=0xDF => self.start_utf8(key, (byte & 0x1F) as u32, 1, 0x80),
            0xE0..=0xEF => self.start_utf8(key, (byte & 0x0F) as u32, 2, 0x800),
            0xF0..=0xF4 => self.start_utf8(key, (byte & 0x07) as u32, 3, 0x10000),
            _ => self.failed = true,
        }
    }

    const fn feed_escaped_string(&mut self, byte: u8, key: bool) {
        let escape = match byte {
            b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => Escape::None,
            b'u' => Escape::Unicode {
                value: 0,
                digits: 0,
                low_surrogate: false,
            },
            _ => {
                self.failed = true;
                return;
            }
        };
        self.state = State::Str { escape, key };
    }

    const fn feed_unicode_escape(&mut self, byte: u8, value: u16, digits: u8, low_surrogate: bool, key: bool) {
        let Some(digit) = hex_digit(byte) else {
            self.failed = true;
            return;
        };
        let value = (value << 4) | digit;
        if digits + 1 < 4 {
            self.state = State::Str {
                escape: Escape::Unicode {
                    value,
                    digits: digits + 1,
                    low_surrogate,
                },
                key,
            };
        } else if low_surrogate {
            if value >= 0xDC00 && value <= 0xDFFF {
                self.state = State::Str {
                    escape: Escape::None,
                    key,
                };
            } else {
                self.failed = true;
            }
        } else if value >= 0xD800 && value <= 0xDBFF {
            self.state = State::Str {
                escape: Escape::SurrogateBackslash,
                key,
            };
        } else if value >= 0xDC00 && value <= 0xDFFF {
            self.failed = true;
        } else {
            self.state = State::Str {
                escape: Escape::None,
                key,
            };
        }
    }

    const fn feed_utf8(&mut self, byte: u8, value: u32, remaining: u8, min: u32, key: bool) {
        if byte < 0x80 || byte > 0xBF {
            self.failed = true;
            return;
        }
        let value = (value << 6) | (byte & 0x3F) as u32;
        if remaining > 1 {
            self.start_utf8(key, value, remaining - 1, min);
        } else if value < min || value > 0x0010_FFFF || value >= 0xD800 && value <= 0xDFFF {
            self.failed = true;
        } else {
            self.state = State::Str {
                escape: Escape::None,
                key,
            };
        }
    }

    const fn start_utf8(&mut self, key: bool, value: u32, remaining: u8, min: u32) {
        self.state = State::Str {
            escape: Escape::Utf8 { value, remaining, min },
            key,
        };
    }

    fn feed_literal(&mut self, byte: u8, keyword: Keyword, index: u8) {
        let bytes = keyword.bytes();
        if bytes.get(usize::from(index)) != Some(&byte) {
            self.failed = true;
        } else if usize::from(index) + 1 == bytes.len() {
            self.finish_value();
        } else {
            self.state = State::Lit {
                keyword,
                index: index + 1,
            };
        }
    }
}

const fn hex_digit(byte: u8) -> Option<u16> {
    match byte {
        b'0'..=b'9' => Some((byte - b'0') as u16),
        b'a'..=b'f' => Some((byte - b'a' + 10) as u16),
        b'A'..=b'F' => Some((byte - b'A' + 10) as u16),
        _ => None,
    }
}

enum NumberStep {
    Continue(Number),
    /// The byte is not part of the number; the number ends and the byte is replayed.
    Terminate,
    Reject,
}

const fn step_number(number: Number, byte: u8) -> NumberStep {
    match number {
        Number::Minus => match byte {
            b'0' => NumberStep::Continue(Number::Zero),
            b'1'..=b'9' => NumberStep::Continue(Number::Int),
            _ => NumberStep::Reject,
        },
        Number::Zero => match byte {
            b'.' => NumberStep::Continue(Number::Dot),
            b'e' | b'E' => NumberStep::Continue(Number::Exp),
            _ => NumberStep::Terminate,
        },
        Number::Int => match byte {
            b'0'..=b'9' => NumberStep::Continue(Number::Int),
            b'.' => NumberStep::Continue(Number::Dot),
            b'e' | b'E' => NumberStep::Continue(Number::Exp),
            _ => NumberStep::Terminate,
        },
        Number::Dot => match byte {
            b'0'..=b'9' => NumberStep::Continue(Number::Frac),
            _ => NumberStep::Terminate,
        },
        Number::Frac => match byte {
            b'0'..=b'9' => NumberStep::Continue(Number::Frac),
            b'e' | b'E' => NumberStep::Continue(Number::Exp),
            _ => NumberStep::Terminate,
        },
        Number::Exp => match byte {
            b'+' | b'-' => NumberStep::Continue(Number::ExpSign),
            b'0'..=b'9' => NumberStep::Continue(Number::ExpDigit),
            _ => NumberStep::Terminate,
        },
        Number::ExpSign | Number::ExpDigit => match byte {
            b'0'..=b'9' => NumberStep::Continue(Number::ExpDigit),
            _ => NumberStep::Terminate,
        },
    }
}
