//! The textual front-end: a hand-written lexer and recursive-descent parser.
//!
//! This is the only textual layer in the crate. It turns bounded query text into an [`Ast`]; a
//! future JSON-AST wire form would emit the same tree, so nothing downstream depends on the query
//! having been text. Input is length-capped and predicate nesting is depth-capped, so the parser
//! cannot be driven into unbounded work.
//!
//! Parameters are bound out of band: a `:name` reference lexes to [`Literal::Param`] and is replaced
//! with a supplied value by [`bind`] after parsing, so a caller value can never alter the query's
//! structure.

use std::collections::BTreeMap;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::ast::{
    Aggregate, AggregateFunc, AggregateTerm, Ast, CompareOp, Join, Literal, OrderKey, Predicate, Selection,
};
use crate::error::PqlError;
use crate::value::Value;

/// The largest query text the parser will accept, so it never receives an unbounded string.
pub const MAX_QUERY_BYTES: usize = 4096;
/// The deepest the `where` predicate may nest, so parsing cannot recurse without bound.
pub const MAX_PREDICATE_DEPTH: usize = 32;

/// Supplied parameter values, keyed by name without the leading colon.
pub type Params = BTreeMap<String, Value>;

/// Parse query text into an unbound [`Ast`] whose parameters are still [`Literal::Param`] holes.
///
/// # Errors
/// Returns [`PqlError::Parse`] for text that is too long, does not tokenize, or does not match the
/// grammar.
pub fn parse(text: &str) -> Result<Ast, PqlError> {
    if text.len() > MAX_QUERY_BYTES {
        return Err(PqlError::Parse("query text exceeds the size limit".to_owned()));
    }
    let tokens = lex(text)?;
    let mut parser = Parser {
        tokens: &tokens,
        position: 0,
    };
    let ast = parser.query()?;
    if parser.position != tokens.len() {
        return Err(PqlError::Parse("unexpected trailing input".to_owned()));
    }
    Ok(ast)
}

/// Replace every [`Literal::Param`] in an [`Ast`] with a supplied value.
///
/// # Errors
/// Returns [`PqlError::MissingParameter`] when the query references a parameter the caller did not
/// supply.
pub fn bind(mut ast: Ast, params: &Params) -> Result<Ast, PqlError> {
    if let Some(predicate) = ast.predicate.take() {
        ast.predicate = Some(bind_predicate(predicate, params)?);
    }
    Ok(ast)
}

fn bind_predicate(predicate: Predicate, params: &Params) -> Result<Predicate, PqlError> {
    match predicate {
        Predicate::Or(left, right) => Ok(Predicate::Or(
            Box::new(bind_predicate(*left, params)?),
            Box::new(bind_predicate(*right, params)?),
        )),
        Predicate::And(left, right) => Ok(Predicate::And(
            Box::new(bind_predicate(*left, params)?),
            Box::new(bind_predicate(*right, params)?),
        )),
        Predicate::Not(inner) => Ok(Predicate::Not(Box::new(bind_predicate(*inner, params)?))),
        Predicate::Compare { field, op, value } => Ok(Predicate::Compare {
            field,
            op,
            value: bind_literal(value, params)?,
        }),
        Predicate::In { field, values } => Ok(Predicate::In {
            field,
            values: values
                .into_iter()
                .map(|value| bind_literal(value, params))
                .collect::<Result<_, _>>()?,
        }),
        Predicate::StartsWith { field, prefix } => Ok(Predicate::StartsWith {
            field,
            prefix: bind_literal(prefix, params)?,
        }),
    }
}

fn bind_literal(literal: Literal, params: &Params) -> Result<Literal, PqlError> {
    let Literal::Param(name) = literal else {
        return Ok(literal);
    };
    match params.get(&name) {
        Some(Value::Bool(value)) => Ok(Literal::Bool(*value)),
        Some(Value::Int(value)) => Ok(Literal::Int(*value)),
        Some(Value::Str(value)) => Ok(Literal::Str(value.clone())),
        Some(Value::Timestamp(value)) => Ok(Literal::Timestamp(*value)),
        Some(Value::Null) | None => Err(PqlError::MissingParameter(name)),
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Str(String),
    Int(i64),
    Timestamp(i64),
    Param(String),
    Op(CompareOp),
    Star,
    LParen,
    RParen,
    Comma,
}

fn lex(text: &str) -> Result<Vec<Token>, PqlError> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            b' ' | b'\t' | b'\r' | b'\n' => index += 1,
            b'(' => push(&mut tokens, Token::LParen, &mut index),
            b')' => push(&mut tokens, Token::RParen, &mut index),
            b',' => push(&mut tokens, Token::Comma, &mut index),
            b'*' => push(&mut tokens, Token::Star, &mut index),
            b'=' | b'!' | b'<' | b'>' => index = lex_operator(bytes, index, &mut tokens)?,
            b'"' => index = lex_string(text, index, &mut tokens)?,
            b'@' => index = lex_timestamp(text, index, &mut tokens)?,
            b':' => index = lex_param(text, index, &mut tokens)?,
            b'-' | b'0'..=b'9' => index = lex_number(text, index, &mut tokens)?,
            _ if is_ident_start(byte) => index = lex_ident(text, index, &mut tokens),
            _ => return Err(PqlError::Parse("unexpected character".to_owned())),
        }
    }
    Ok(tokens)
}

fn push(tokens: &mut Vec<Token>, token: Token, index: &mut usize) {
    tokens.push(token);
    *index += 1;
}

fn lex_operator(bytes: &[u8], index: usize, tokens: &mut Vec<Token>) -> Result<usize, PqlError> {
    let next = bytes.get(index + 1).copied();
    let (op, width) = match (bytes[index], next) {
        (b'=', Some(b'=')) => (CompareOp::Eq, 2),
        (b'!', Some(b'=')) => (CompareOp::Ne, 2),
        (b'<', Some(b'=')) => (CompareOp::Le, 2),
        (b'>', Some(b'=')) => (CompareOp::Ge, 2),
        (b'<', _) => (CompareOp::Lt, 1),
        (b'>', _) => (CompareOp::Gt, 1),
        _ => return Err(PqlError::Parse("unexpected operator".to_owned())),
    };
    tokens.push(Token::Op(op));
    Ok(index + width)
}

fn lex_string(text: &str, index: usize, tokens: &mut Vec<Token>) -> Result<usize, PqlError> {
    // Iterate whole `char`s, not bytes: a multibyte UTF-8 char inside the quotes must survive as one
    // codepoint rather than being split into Latin-1 bytes. Escapes only ever introduce ASCII.
    let mut value = String::new();
    let mut chars = text[index + 1..].char_indices();
    while let Some((offset, character)) = chars.next() {
        match character {
            '"' => {
                tokens.push(Token::Str(value));
                return Ok(index + offset + 2);
            }
            '\\' => match chars.next() {
                Some((_, '"')) => value.push('"'),
                Some((_, '\\')) => value.push('\\'),
                _ => return Err(PqlError::Parse("invalid string escape".to_owned())),
            },
            other => value.push(other),
        }
    }
    Err(PqlError::Parse("unterminated string".to_owned()))
}

fn lex_timestamp(text: &str, index: usize, tokens: &mut Vec<Token>) -> Result<usize, PqlError> {
    let end = token_end(text, index + 1);
    let literal = &text[index + 1..end];
    let parsed = OffsetDateTime::parse(literal, &Rfc3339)
        .map_err(|_| PqlError::Parse("invalid timestamp literal".to_owned()))?;
    tokens.push(Token::Timestamp(parsed.unix_timestamp()));
    Ok(end)
}

fn lex_param(text: &str, index: usize, tokens: &mut Vec<Token>) -> Result<usize, PqlError> {
    let end = token_end(text, index + 1);
    let name = &text[index + 1..end];
    if name.is_empty() || !name.bytes().all(is_ident_continue) {
        return Err(PqlError::Parse("invalid parameter name".to_owned()));
    }
    tokens.push(Token::Param(name.to_owned()));
    Ok(end)
}

fn lex_number(text: &str, index: usize, tokens: &mut Vec<Token>) -> Result<usize, PqlError> {
    let end = token_end(text, index);
    let literal = &text[index..end];
    let value = literal
        .parse::<i64>()
        .map_err(|_| PqlError::Parse("invalid integer literal".to_owned()))?;
    tokens.push(Token::Int(value));
    Ok(end)
}

fn lex_ident(text: &str, index: usize, tokens: &mut Vec<Token>) -> usize {
    let end = token_end(text, index);
    tokens.push(Token::Ident(text[index..end].to_owned()));
    end
}

fn token_end(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut cursor = start;
    while cursor < bytes.len() && is_ident_continue(bytes[cursor]) {
        cursor += 1;
    }
    cursor
}

/// One deeper into the predicate, refusing a query whose boolean nesting — `and`, `or`, `not`, or
/// parentheses — would recurse past the cap, so parsing, binding, validation, and evaluation stay
/// within a bounded stack.
fn deepen(depth: usize) -> Result<usize, PqlError> {
    if depth >= MAX_PREDICATE_DEPTH {
        return Err(PqlError::Parse("predicate nests too deeply".to_owned()));
    }
    Ok(depth + 1)
}

const fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

const fn is_ident_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'.' || byte == b'-' || byte == b':' || byte == b'+'
}

struct Parser<'tokens> {
    tokens: &'tokens [Token],
    position: usize,
}

impl Parser<'_> {
    fn query(&mut self) -> Result<Ast, PqlError> {
        self.keyword("from")?;
        let domain = self.name()?;
        let join = self.join_clause()?;
        let predicate = self.where_clause()?;
        let (selection, aggregate) = self.projection()?;
        let order_by = self.order_clause()?;
        let limit = self.limit_clause()?;
        Ok(Ast {
            domain,
            join,
            predicate,
            selection,
            aggregate,
            order_by,
            limit,
        })
    }

    fn join_clause(&mut self) -> Result<Option<Join>, PqlError> {
        if !self.take_keyword("join") {
            return Ok(None);
        }
        let domain = self.name()?;
        self.keyword("on")?;
        let mut on = vec![self.name()?];
        while self.take(&Token::Comma) {
            on.push(self.name()?);
        }
        Ok(Some(Join { domain, on }))
    }

    fn where_clause(&mut self) -> Result<Option<Predicate>, PqlError> {
        if !self.take_keyword("where") {
            return Ok(None);
        }
        Ok(Some(self.predicate(0)?))
    }

    fn projection(&mut self) -> Result<(Selection, Option<Aggregate>), PqlError> {
        if self.take_keyword("aggregate") {
            let aggregate = self.aggregate()?;
            return Ok((Selection::All, Some(aggregate)));
        }
        if !self.take_keyword("select") {
            return Ok((Selection::All, None));
        }
        if self.take(&Token::Star) {
            return Ok((Selection::All, None));
        }
        let mut columns = vec![self.name()?];
        while self.take(&Token::Comma) {
            columns.push(self.name()?);
        }
        Ok((Selection::Columns(columns), None))
    }

    fn aggregate(&mut self) -> Result<Aggregate, PqlError> {
        let mut terms = vec![self.aggregate_term()?];
        while self.take(&Token::Comma) {
            terms.push(self.aggregate_term()?);
        }
        self.keyword("by")?;
        let mut group_by = vec![self.name()?];
        while self.take(&Token::Comma) {
            group_by.push(self.name()?);
        }
        Ok(Aggregate { terms, group_by })
    }

    fn aggregate_term(&mut self) -> Result<AggregateTerm, PqlError> {
        let func = match self.name()?.as_str() {
            "sum" => AggregateFunc::Sum,
            "count" => AggregateFunc::Count,
            "min" => AggregateFunc::Min,
            "max" => AggregateFunc::Max,
            _ => return Err(PqlError::Parse("unknown aggregate function".to_owned())),
        };
        self.take(&Token::LParen)
            .then_some(())
            .ok_or_else(|| PqlError::Parse("aggregate needs parentheses".to_owned()))?;
        let column = if self.take(&Token::Star) || self.peek() == Some(&Token::RParen) {
            None
        } else {
            Some(self.name()?)
        };
        self.expect(&Token::RParen)?;
        self.keyword("as")?;
        let alias = self.name()?;
        Ok(AggregateTerm { func, column, alias })
    }

    fn order_clause(&mut self) -> Result<Vec<OrderKey>, PqlError> {
        if !self.take_keyword("order") {
            return Ok(Vec::new());
        }
        self.keyword("by")?;
        let mut keys = vec![self.order_key()?];
        while self.take(&Token::Comma) {
            keys.push(self.order_key()?);
        }
        Ok(keys)
    }

    fn order_key(&mut self) -> Result<OrderKey, PqlError> {
        let field = self.name()?;
        let descending = if self.take_keyword("desc") {
            true
        } else {
            let _ = self.take_keyword("asc");
            false
        };
        Ok(OrderKey { field, descending })
    }

    fn limit_clause(&mut self) -> Result<Option<u32>, PqlError> {
        if !self.take_keyword("limit") {
            return Ok(None);
        }
        match self.advance() {
            Some(Token::Int(value)) if u32::try_from(*value).is_ok() => {
                Ok(Some(u32::try_from(*value).expect("bounded above by u32::MAX")))
            }
            _ => Err(PqlError::Parse("limit must be a non-negative integer".to_owned())),
        }
    }

    fn predicate(&mut self, mut depth: usize) -> Result<Predicate, PqlError> {
        let mut left = self.and_expr(depth)?;
        while self.take_keyword("or") {
            depth = deepen(depth)?;
            let right = self.and_expr(depth)?;
            left = Predicate::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and_expr(&mut self, mut depth: usize) -> Result<Predicate, PqlError> {
        let mut left = self.unary(depth)?;
        while self.take_keyword("and") {
            depth = deepen(depth)?;
            let right = self.unary(depth)?;
            left = Predicate::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn unary(&mut self, depth: usize) -> Result<Predicate, PqlError> {
        if self.take_keyword("not") {
            return Ok(Predicate::Not(Box::new(self.unary(deepen(depth)?)?)));
        }
        if self.take(&Token::LParen) {
            let inner = self.predicate(deepen(depth)?)?;
            self.expect(&Token::RParen)?;
            return Ok(inner);
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Predicate, PqlError> {
        let field = self.name()?;
        if self.take_keyword("in") {
            return self.membership(field);
        }
        if self.take_keyword("starts_with") {
            let prefix = self.literal()?;
            return Ok(Predicate::StartsWith { field, prefix });
        }
        let op = match self.advance() {
            Some(Token::Op(op)) => *op,
            _ => return Err(PqlError::Parse("expected a comparison operator".to_owned())),
        };
        let value = self.literal()?;
        Ok(Predicate::Compare { field, op, value })
    }

    fn membership(&mut self, field: String) -> Result<Predicate, PqlError> {
        self.expect(&Token::LParen)?;
        let mut values = vec![self.literal()?];
        while self.take(&Token::Comma) {
            values.push(self.literal()?);
        }
        self.expect(&Token::RParen)?;
        Ok(Predicate::In { field, values })
    }

    fn literal(&mut self) -> Result<Literal, PqlError> {
        match self.advance() {
            Some(Token::Str(value)) => Ok(Literal::Str(value.clone())),
            Some(Token::Int(value)) => Ok(Literal::Int(*value)),
            Some(Token::Timestamp(value)) => Ok(Literal::Timestamp(*value)),
            Some(Token::Param(name)) => Ok(Literal::Param(name.clone())),
            Some(Token::Ident(word)) if word == "true" => Ok(Literal::Bool(true)),
            Some(Token::Ident(word)) if word == "false" => Ok(Literal::Bool(false)),
            _ => Err(PqlError::Parse("expected a literal".to_owned())),
        }
    }

    fn name(&mut self) -> Result<String, PqlError> {
        match self.advance() {
            Some(Token::Ident(word)) => Ok(word.clone()),
            _ => Err(PqlError::Parse("expected an identifier".to_owned())),
        }
    }

    fn keyword(&mut self, expected: &str) -> Result<(), PqlError> {
        if self.take_keyword(expected) {
            Ok(())
        } else {
            Err(PqlError::Parse(format!("expected `{expected}`")))
        }
    }

    fn take_keyword(&mut self, expected: &str) -> bool {
        if matches!(self.peek(), Some(Token::Ident(word)) if word == expected) {
            self.position += 1;
            return true;
        }
        false
    }

    fn take(&mut self, expected: &Token) -> bool {
        if self.peek() == Some(expected) {
            self.position += 1;
            return true;
        }
        false
    }

    fn expect(&mut self, expected: &Token) -> Result<(), PqlError> {
        if self.take(expected) {
            Ok(())
        } else {
            Err(PqlError::Parse("unexpected token".to_owned()))
        }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn advance(&mut self) -> Option<&Token> {
        let token = self.tokens.get(self.position);
        if token.is_some() {
            self.position += 1;
        }
        token
    }
}
